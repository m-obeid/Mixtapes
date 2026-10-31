//! System media controls on macOS, where `mpris.rs` serves MPRIS on Linux and
//! `smtc.rs` the media flyout on Windows. MPNowPlayingInfoCenter fills the
//! Now Playing widget in Control Center and the menu bar; MPRemoteCommandCenter
//! brings back its buttons, the keyboard's media keys and AirPods taps.

use std::cell::{Cell, RefCell};
use std::ptr::NonNull;
use std::rc::Rc;

use block2::RcBlock;
use gtk::glib;
use gtk::prelude::*;
use objc2::rc::Retained;
use objc2::runtime::AnyObject;
use objc2::{AnyThread, Message};
use objc2_app_kit::NSImage;
use objc2_core_foundation::CGSize;
use objc2_foundation::{NSData, NSDictionary, NSNumber, NSString};
use objc2_media_player::{
    MPChangePlaybackPositionCommandEvent, MPMediaItemArtwork, MPMediaItemPropertyAlbumTitle, MPMediaItemPropertyArtist,
    MPMediaItemPropertyArtwork, MPMediaItemPropertyPlaybackDuration, MPMediaItemPropertyTitle, MPNowPlayingInfoCenter,
    MPNowPlayingInfoPropertyElapsedPlaybackTime, MPNowPlayingInfoPropertyPlaybackRate, MPNowPlayingPlaybackState, MPRemoteCommand,
    MPRemoteCommandCenter, MPRemoteCommandEvent, MPRemoteCommandHandlerStatus,
};

use crate::App;
use crate::model::PlaybackStatus;
use crate::net::NetHandle;
use crate::player::Player;

/// Size of the cover handed to Control Center, in pixels.
const ART_SIZE: u32 = 512;

/// What macOS asks for, sent from its command handlers to the player.
#[derive(Debug)]
enum Command {
    Play,
    Pause,
    Toggle,
    Next,
    Previous,
    Seek(f64),
}

pub struct Mpris {
    player: Rc<Player>,
    net: NetHandle,
    /// The tokens addTargetWithHandler returned, to remove the handlers again.
    targets: RefCell<Vec<(Retained<MPRemoteCommand>, Retained<AnyObject>)>>,
    /// Cover for the playing track, with the video id it belongs to.
    artwork: RefCell<Option<(String, Retained<MPMediaItemArtwork>)>>,
    /// Bumped per cover request, so a slow download cannot land after a newer one.
    art_generation: Cell<u64>,
}

impl Mpris {
    pub fn start(ctx: &Rc<App>) -> Rc<Self> {
        let (tx, rx) = async_channel::unbounded::<Command>();
        let this = Rc::new(Self {
            player: ctx.player.clone(),
            net: ctx.net.clone(),
            targets: RefCell::new(Vec::new()),
            artwork: RefCell::new(None),
            art_generation: Cell::new(0),
        });
        this.register_commands(tx);
        this.pump_commands(rx);
        this.watch_state();
        this.refresh_bounds();
        this.publish();
        tracing::info!("now playing controls registered");
        this
    }

    pub fn shutdown(&self) {
        // SAFETY: plain property calls on the shared objects, on the main thread.
        unsafe {
            for (command, target) in self.targets.borrow_mut().drain(..) {
                command.removeTarget(Some(&target));
            }
            let center = MPNowPlayingInfoCenter::defaultCenter();
            center.setNowPlayingInfo(None);
            center.setPlaybackState(MPNowPlayingPlaybackState::Stopped);
        }
    }

    fn register_commands(&self, tx: async_channel::Sender<Command>) {
        // SAFETY: the shared command center, used from the main thread, which
        // is also where it calls the handlers.
        let center = unsafe { MPRemoteCommandCenter::sharedCommandCenter() };
        let simple = |command: Retained<MPRemoteCommand>, make: fn() -> Command| {
            let tx = tx.clone();
            let handler = RcBlock::new(move |_event: NonNull<MPRemoteCommandEvent>| {
                let _ = tx.try_send(make());
                MPRemoteCommandHandlerStatus::Success
            });
            // SAFETY: the block is copied by the command and lives until removeTarget.
            let target = unsafe {
                command.setEnabled(true);
                command.addTargetWithHandler(&handler)
            };
            self.targets.borrow_mut().push((command, target));
        };
        // SAFETY: getters on the shared command center.
        unsafe {
            simple(center.playCommand(), || Command::Play);
            simple(center.pauseCommand(), || Command::Pause);
            simple(center.togglePlayPauseCommand(), || Command::Toggle);
            simple(center.nextTrackCommand(), || Command::Next);
            simple(center.previousTrackCommand(), || Command::Previous);
        }
        // SAFETY: as above.
        let seek = unsafe { center.changePlaybackPositionCommand() };
        let handler = RcBlock::new(move |event: NonNull<MPRemoteCommandEvent>| {
            // SAFETY: this command only sends position events, and the event is
            // alive for the duration of the call.
            let event = unsafe { event.cast::<MPChangePlaybackPositionCommandEvent>().as_ref() };
            let _ = tx.try_send(Command::Seek(unsafe { event.positionTime() }));
            MPRemoteCommandHandlerStatus::Success
        });
        // SAFETY: as for the simple commands.
        let target = unsafe {
            seek.setEnabled(true);
            seek.addTargetWithHandler(&handler)
        };
        self.targets.borrow_mut().push((Retained::into_super(seek), target));
    }

    fn pump_commands(self: &Rc<Self>, rx: async_channel::Receiver<Command>) {
        let player = self.player.clone();
        glib::spawn_future_local(async move {
            while let Ok(command) = rx.recv().await {
                tracing::debug!(?command, "now playing command");
                match command {
                    // Play on a stopped player starts the staged track, as with MPRIS.
                    Command::Play => match player.state().status() {
                        PlaybackStatus::Stopped => player.toggle_play(),
                        _ => player.play(),
                    },
                    Command::Pause => player.pause(),
                    Command::Toggle => player.toggle_play(),
                    Command::Next => player.next(),
                    Command::Previous => player.previous(),
                    Command::Seek(seconds) => player.seek(seconds),
                }
            }
        });
    }

    fn watch_state(self: &Rc<Self>) {
        let state = self.player.state();
        let connect = |name: &str, f: fn(&Rc<Mpris>)| {
            let weak = Rc::downgrade(self);
            state.connect_notify_local(Some(name), move |_, _| {
                if let Some(this) = weak.upgrade() {
                    f(&this);
                }
            });
        };
        // Position is left out on purpose: macOS moves the clock itself from
        // the elapsed time and the rate, so only jumps need publishing.
        for name in ["status", "title", "artist", "duration"] {
            connect(name, |this| this.publish());
        }
        for name in ["thumbnail-url", "video-id"] {
            connect(name, |this| {
                this.publish();
                this.load_artwork();
            });
        }
        for name in ["current-index", "queue-length"] {
            connect(name, |this| this.refresh_bounds());
        }
        let weak = Rc::downgrade(self);
        state.connect_seeked(move |_| {
            if let Some(this) = weak.upgrade() {
                this.publish();
            }
        });
    }

    /// The whole Now Playing entry. macOS takes it as one dictionary.
    fn publish(&self) {
        let state = self.player.state();
        let status = state.status();
        let playback = match status {
            PlaybackStatus::Playing => MPNowPlayingPlaybackState::Playing,
            PlaybackStatus::Paused | PlaybackStatus::Loading => MPNowPlayingPlaybackState::Paused,
            PlaybackStatus::Stopped => MPNowPlayingPlaybackState::Stopped,
        };
        // SAFETY: the shared center, on the main thread.
        let center = unsafe { MPNowPlayingInfoCenter::defaultCenter() };
        if state.video_id().is_empty() {
            // SAFETY: as above.
            unsafe {
                center.setNowPlayingInfo(None);
                center.setPlaybackState(MPNowPlayingPlaybackState::Stopped);
            }
            return;
        }
        let track = self.player.current_track();
        let artists = match track.as_ref().map(|t| &t.artists).filter(|a| !a.is_empty()) {
            Some(list) => list.iter().map(|a| a.name.as_str()).filter(|n| !n.is_empty()).collect::<Vec<_>>().join(", "),
            None => state.artist(),
        };
        let album = track.as_ref().and_then(|t| t.album.as_ref()).map(|a| a.name.clone()).unwrap_or_default();
        let duration = state.duration().max(0.0);
        let rate = if status == PlaybackStatus::Playing { 1.0 } else { 0.0 };

        // SAFETY: the keys are constant NSStrings MediaPlayer exports.
        let mut entries: Vec<(&NSString, Retained<AnyObject>)> = unsafe {
            vec![
                (MPMediaItemPropertyTitle, any(NSString::from_str(&state.title()))),
                (MPMediaItemPropertyArtist, any(NSString::from_str(&artists))),
                (MPMediaItemPropertyAlbumTitle, any(NSString::from_str(&album))),
                (MPMediaItemPropertyPlaybackDuration, any(NSNumber::new_f64(duration))),
                (MPNowPlayingInfoPropertyElapsedPlaybackTime, any(NSNumber::new_f64(state.position().clamp(0.0, duration)))),
                (MPNowPlayingInfoPropertyPlaybackRate, any(NSNumber::new_f64(rate))),
            ]
        };
        if let Some((video_id, artwork)) = self.artwork.borrow().as_ref()
            && *video_id == state.video_id()
        {
            // SAFETY: as above.
            entries.push((unsafe { MPMediaItemPropertyArtwork }, any(artwork.clone())));
        }
        let (keys, values): (Vec<&NSString>, Vec<Retained<AnyObject>>) = entries.into_iter().unzip();
        let info = NSDictionary::from_retained_objects(&keys, &values);
        // SAFETY: as above.
        unsafe {
            center.setNowPlayingInfo(Some(&info));
            center.setPlaybackState(playback);
        }
    }

    /// Download the cover and wrap it for Control Center.
    fn load_artwork(self: &Rc<Self>) {
        let state = self.player.state();
        let (video_id, thumbnail) = (state.video_id(), state.thumbnail_url());
        if video_id.is_empty() || self.artwork.borrow().as_ref().is_some_and(|(id, _)| *id == video_id) {
            return;
        }
        let generation = self.art_generation.get() + 1;
        self.art_generation.set(generation);
        let net = self.net.clone();
        let weak = Rc::downgrade(self);
        glib::spawn_future_local(async move {
            // A downloaded track's own cover first: offline the remote one cannot be fetched.
            let address = crate::ui::cover::track_cover_address(&net, &video_id, &thumbnail).await;
            if address.is_empty() {
                return;
            }
            let http = net.client().http().clone();
            let auth = net.client().media_auth();
            let bytes = net.spawn(async move { crate::ui::cover::fetch_cover_bytes(&http, auth.as_ref(), &address, Some(ART_SIZE)).await }).await.ok().flatten();
            let Some(this) = weak.upgrade() else { return };
            if this.art_generation.get() != generation {
                return;
            }
            let Some(artwork) = bytes.as_deref().and_then(artwork_from_bytes) else { return };
            this.artwork.replace(Some((video_id, artwork)));
            this.publish();
        });
    }

    fn refresh_bounds(&self) {
        let bounds = self.player.bounds();
        // SAFETY: getters and setters on the shared command center.
        unsafe {
            let center = MPRemoteCommandCenter::sharedCommandCenter();
            center.nextTrackCommand().setEnabled(bounds.can_next);
            // Previous also restarts the current track, so it always has something to do.
            center.previousTrackCommand().setEnabled(bounds.can_previous || self.player.current_track().is_some());
        }
    }
}

fn any<T: Message>(object: Retained<T>) -> Retained<AnyObject> {
    // SAFETY: every Objective-C object is an AnyObject.
    unsafe { Retained::cast_unchecked(object) }
}

/// An MPMediaItemArtwork over the decoded image, which hands the same image
/// back at any size it is asked for.
fn artwork_from_bytes(bytes: &[u8]) -> Option<Retained<MPMediaItemArtwork>> {
    let data = NSData::with_bytes(bytes);
    let image = NSImage::initWithData(NSImage::alloc(), &data)?;
    let size = image.size();
    if size.width <= 0.0 || size.height <= 0.0 {
        return None;
    }
    let handler = RcBlock::new(move |_size: CGSize| -> NonNull<NSImage> { NonNull::from(&*image) });
    // SAFETY: the block keeps the image alive and is retained by the artwork.
    Some(unsafe { MPMediaItemArtwork::initWithBoundsSize_requestHandler(MPMediaItemArtwork::alloc(), size, &handler) })
}

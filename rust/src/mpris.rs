//! System media controls over MPRIS. Port of player/mpris.py, which handed
//! mprisify an adapter onto the player.
//!
//! The split follows the rest of the app: the D-Bus server runs on the tokio
//! runtime, so its implementation must be Send and never touches the player.
//! It answers from a snapshot the GTK thread writes, and posts commands back
//! over a channel the GTK thread drains, the same shape the audio thread uses.

use std::cell::{Cell, RefCell};
use std::path::{Path, PathBuf};
use std::rc::Rc;
use std::sync::{Arc, Mutex};
use std::time::Instant;

use gtk::prelude::*;
use gtk::{gio, glib};
use mpris_server::zbus::{Result as ZbusResult, fdo};
use mpris_server::{
    LoopStatus, Metadata, PlaybackRate, PlaybackStatus as MprisStatus, PlayerInterface, Property,
    RootInterface, Server, Signal, Time, TrackId, Volume,
};

use crate::App;
use crate::model::{HttpAuth, PlaybackStatus, RepeatMode};
use crate::net::NetHandle;
use crate::paths::Paths;
use crate::queue::PREVIOUS_RESTART_THRESHOLD;
use crate::player::Player;
use crate::state::PlayerState;
use crate::ui::cover::fetch_cover_bytes;

/// Owns `org.mpris.MediaPlayer2.Mixtapes`, the name the Python app took.
const BUS_SUFFIX: &str = "Mixtapes";
const IDENTITY: &str = "Mixtapes";
const DESKTOP_ENTRY: &str = "com.pocoguy.Muse";
const TRACK_ID_PREFIX: &str = "/com/pocoguy/Muse/track";
/// Art below this is upscaled: some clients render small covers badly.
const MIN_ART_SIZE: i32 = 512;

/// What the desktop shell asked for. Applied on the GTK thread.
#[derive(Debug, Clone)]
enum Command {
    Play,
    Pause,
    PlayPause,
    Next,
    Previous,
    Stop,
    /// Relative seek in microseconds, what the spec's Seek carries.
    SeekBy(i64),
    /// Absolute position in seconds, from SetPosition.
    SetPosition(f64),
    SetVolume(f64),
    SetShuffle(bool),
    SetLoop(RepeatMode),
    Raise,
    Quit,
}

/// Everything the D-Bus side answers with. The GTK thread is the only writer.
#[derive(Debug, Clone, Default)]
struct Snapshot {
    status: PlaybackStatus,
    video_id: String,
    title: String,
    artists: Vec<String>,
    album: String,
    /// The address the track carries. Used until the local file is written.
    art_url: String,
    /// The squared-off file and the track it belongs to, so a later metadata
    /// refresh cannot put the remote address back.
    art_file: String,
    art_file_video_id: String,
    duration: f64,
    position: f64,
    /// When `position` was sampled, so a poll between ticks reads a live value.
    position_at: Option<Instant>,
    volume: f64,
    shuffle: bool,
    repeat: RepeatMode,
    can_next: bool,
    /// Index of the playing track, or -1. CanGoPrevious reads it live, since
    /// Previous restarts the track once it is past the threshold.
    index: i32,
}

impl Snapshot {
    fn playback_status(&self) -> MprisStatus {
        match self.status {
            // Loading reports as Playing, as get_playstate did, so the shell
            // does not blink to paused between tracks.
            PlaybackStatus::Playing | PlaybackStatus::Loading => MprisStatus::Playing,
            PlaybackStatus::Paused => MprisStatus::Paused,
            PlaybackStatus::Stopped => MprisStatus::Stopped,
        }
    }

    /// Position now: the last sample plus the time since, while playing.
    fn live_position(&self) -> f64 {
        let drift = match (self.status, self.position_at) {
            (PlaybackStatus::Playing, Some(at)) => at.elapsed().as_secs_f64(),
            _ => 0.0,
        };
        let position = self.position + drift;
        if self.duration > 0.0 { position.min(self.duration) } else { position }
    }

    /// A local file if one was written for this track, else the remote address.
    fn art(&self) -> &str {
        if !self.art_file.is_empty() && self.art_file_video_id == self.video_id {
            return &self.art_file;
        }
        &self.art_url
    }

    fn can_previous(&self) -> bool {
        self.index > 0 || self.live_position() > PREVIOUS_RESTART_THRESHOLD
    }

    /// `NO_TRACK` has a reserved meaning on the track list, so an idle player
    /// gets its own path, as the Python adapter did.
    fn track_id(&self) -> TrackId {
        if self.video_id.is_empty() {
            return TrackId::try_from(format!("{TRACK_ID_PREFIX}/none")).unwrap_or(TrackId::NO_TRACK);
        }
        // D-Bus path elements take only [A-Za-z0-9_] and cannot start with a digit.
        let mut safe: String = self.video_id.chars().map(|c| if c.is_ascii_alphanumeric() { c } else { '_' }).collect();
        if safe.starts_with(|c: char| c.is_ascii_digit()) {
            safe.insert(0, 'v');
        }
        TrackId::try_from(format!("{TRACK_ID_PREFIX}/{safe}")).unwrap_or(TrackId::NO_TRACK)
    }

    fn metadata(&self) -> Metadata {
        let mut builder = Metadata::builder().trackid(self.track_id());
        if !self.title.is_empty() {
            builder = builder.title(&self.title);
        }
        if !self.artists.is_empty() {
            builder = builder.artist(self.artists.clone());
        }
        if !self.album.is_empty() {
            builder = builder.album(&self.album);
        }
        let art = self.art();
        if !art.is_empty() {
            builder = builder.art_url(art.to_owned());
        }
        if self.duration > 0.0 {
            builder = builder.length(Time::from_micros((self.duration * 1e6) as i64));
        }
        builder.build()
    }
}

/// The server's implementation. Lives on the tokio runtime.
struct Imp {
    shared: Arc<Mutex<Snapshot>>,
    commands: async_channel::Sender<Command>,
}

impl Imp {
    fn read(&self) -> Snapshot {
        self.shared.lock().unwrap().clone()
    }

    fn send(&self, command: Command) -> fdo::Result<()> {
        self.commands.try_send(command).map_err(|_| fdo::Error::Failed("the player is gone".into()))
    }
}

impl RootInterface for Imp {
    async fn raise(&self) -> fdo::Result<()> {
        self.send(Command::Raise)
    }

    async fn quit(&self) -> fdo::Result<()> {
        self.send(Command::Quit)
    }

    async fn can_quit(&self) -> fdo::Result<bool> {
        Ok(true)
    }

    async fn fullscreen(&self) -> fdo::Result<bool> {
        Ok(false)
    }

    async fn set_fullscreen(&self, _fullscreen: bool) -> ZbusResult<()> {
        Ok(())
    }

    async fn can_set_fullscreen(&self) -> fdo::Result<bool> {
        Ok(false)
    }

    async fn can_raise(&self) -> fdo::Result<bool> {
        Ok(true)
    }

    async fn has_track_list(&self) -> fdo::Result<bool> {
        Ok(false)
    }

    async fn identity(&self) -> fdo::Result<String> {
        Ok(IDENTITY.to_owned())
    }

    async fn desktop_entry(&self) -> fdo::Result<String> {
        Ok(DESKTOP_ENTRY.to_owned())
    }

    async fn supported_uri_schemes(&self) -> fdo::Result<Vec<String>> {
        Ok(Vec::new())
    }

    async fn supported_mime_types(&self) -> fdo::Result<Vec<String>> {
        Ok(Vec::new())
    }
}

impl PlayerInterface for Imp {
    async fn next(&self) -> fdo::Result<()> {
        self.send(Command::Next)
    }

    async fn previous(&self) -> fdo::Result<()> {
        self.send(Command::Previous)
    }

    async fn pause(&self) -> fdo::Result<()> {
        self.send(Command::Pause)
    }

    async fn play_pause(&self) -> fdo::Result<()> {
        self.send(Command::PlayPause)
    }

    async fn stop(&self) -> fdo::Result<()> {
        self.send(Command::Stop)
    }

    async fn play(&self) -> fdo::Result<()> {
        self.send(Command::Play)
    }

    async fn seek(&self, offset: Time) -> fdo::Result<()> {
        self.send(Command::SeekBy(offset.as_micros()))
    }

    async fn set_position(&self, track_id: TrackId, position: Time) -> fdo::Result<()> {
        // The spec says to ignore a position meant for a track that moved on.
        if track_id != self.read().track_id() {
            return Ok(());
        }
        self.send(Command::SetPosition(position.as_micros() as f64 / 1e6))
    }

    async fn open_uri(&self, _uri: String) -> fdo::Result<()> {
        Err(fdo::Error::NotSupported("opening URIs is not supported".into()))
    }

    async fn playback_status(&self) -> fdo::Result<MprisStatus> {
        Ok(self.read().playback_status())
    }

    async fn loop_status(&self) -> fdo::Result<LoopStatus> {
        Ok(match self.read().repeat {
            RepeatMode::Track => LoopStatus::Track,
            RepeatMode::All => LoopStatus::Playlist,
            RepeatMode::Off => LoopStatus::None,
        })
    }

    async fn set_loop_status(&self, loop_status: LoopStatus) -> ZbusResult<()> {
        let mode = match loop_status {
            LoopStatus::Track => RepeatMode::Track,
            LoopStatus::Playlist => RepeatMode::All,
            LoopStatus::None => RepeatMode::Off,
        };
        let _ = self.send(Command::SetLoop(mode));
        Ok(())
    }

    async fn rate(&self) -> fdo::Result<PlaybackRate> {
        Ok(1.0)
    }

    async fn set_rate(&self, _rate: PlaybackRate) -> ZbusResult<()> {
        Ok(())
    }

    async fn shuffle(&self) -> fdo::Result<bool> {
        Ok(self.read().shuffle)
    }

    async fn set_shuffle(&self, shuffle: bool) -> ZbusResult<()> {
        let _ = self.send(Command::SetShuffle(shuffle));
        Ok(())
    }

    async fn metadata(&self) -> fdo::Result<Metadata> {
        Ok(self.read().metadata())
    }

    async fn volume(&self) -> fdo::Result<Volume> {
        Ok(self.read().volume)
    }

    async fn set_volume(&self, volume: Volume) -> ZbusResult<()> {
        let _ = self.send(Command::SetVolume(volume.clamp(0.0, 1.0)));
        Ok(())
    }

    async fn position(&self) -> fdo::Result<Time> {
        Ok(Time::from_micros((self.read().live_position() * 1e6) as i64))
    }

    async fn minimum_rate(&self) -> fdo::Result<PlaybackRate> {
        Ok(1.0)
    }

    async fn maximum_rate(&self) -> fdo::Result<PlaybackRate> {
        Ok(1.0)
    }

    async fn can_go_next(&self) -> fdo::Result<bool> {
        Ok(self.read().can_next)
    }

    async fn can_go_previous(&self) -> fdo::Result<bool> {
        Ok(self.read().can_previous())
    }

    async fn can_play(&self) -> fdo::Result<bool> {
        Ok(true)
    }

    async fn can_pause(&self) -> fdo::Result<bool> {
        Ok(true)
    }

    async fn can_seek(&self) -> fdo::Result<bool> {
        Ok(self.read().duration > 0.0)
    }

    async fn can_control(&self) -> fdo::Result<bool> {
        Ok(true)
    }
}

/// The GTK-thread half: mirrors `PlayerState` into the snapshot and applies
/// what the shell sends back.
pub struct Mpris {
    shared: Arc<Mutex<Snapshot>>,
    server: RefCell<Option<Arc<Server<Imp>>>>,
    /// Property changes waiting for the next idle, so a track change sends one signal.
    pending: RefCell<Vec<Property>>,
    flush_queued: Cell<bool>,
    net: NetHandle,
    paths: Paths,
    player: Rc<Player>,
    /// Track whose art file is written or being written.
    art_video_id: RefCell<Option<String>>,
}

impl Mpris {
    /// Publish the player on D-Bus. Failure is logged and the app carries on.
    pub fn start(ctx: &Rc<App>) -> Rc<Self> {
        let (tx, rx) = async_channel::unbounded::<Command>();
        let shared = Arc::new(Mutex::new(Snapshot::default()));
        let this = Rc::new(Self {
            shared: shared.clone(),
            server: RefCell::new(None),
            pending: RefCell::new(Vec::new()),
            flush_queued: Cell::new(false),
            net: ctx.net.clone(),
            paths: ctx.paths.clone(),
            player: ctx.player.clone(),
            art_video_id: RefCell::new(None),
        });
        this.refresh_all();

        let imp = Imp { shared, commands: tx };
        let handle = ctx.net.spawn(async move { Server::new(BUS_SUFFIX, imp).await });
        let weak = Rc::downgrade(&this);
        glib::spawn_future_local(async move {
            match handle.await {
                Ok(Ok(server)) => {
                    if let Some(this) = weak.upgrade() {
                        tracing::info!(bus = %server.bus_name(), "mpris published");
                        this.server.replace(Some(Arc::new(server)));
                    }
                }
                Ok(Err(err)) => tracing::warn!(%err, "mpris server failed to start"),
                Err(_) => {}
            }
        });

        this.pump_commands(ctx, rx);
        this.watch_state(ctx.player.state());
        this
    }

    /// Apply what the shell sends. Runs on the GTK thread, so it calls the
    /// controller directly; seeks and volume reach GStreamer through it.
    fn pump_commands(self: &Rc<Self>, ctx: &Rc<App>, rx: async_channel::Receiver<Command>) {
        let weak_ctx = Rc::downgrade(ctx);
        let player = self.player.clone();
        let weak = Rc::downgrade(self);
        glib::spawn_future_local(async move {
            while let Ok(command) = rx.recv().await {
                tracing::debug!(?command, "mpris command");
                match command {
                    // Play on a stopped player starts the staged track, which
                    // the shell expects; the controller's plain play() only
                    // resumes a pipeline that already holds a stream.
                    Command::Play => match player.state().status() {
                        PlaybackStatus::Stopped => player.toggle_play(),
                        _ => player.play(),
                    },
                    Command::Pause => player.pause(),
                    Command::PlayPause => player.toggle_play(),
                    Command::Next => player.next(),
                    Command::Previous => player.previous(),
                    Command::Stop => player.stop(),
                    Command::SeekBy(micros) => {
                        let state = player.state();
                        let target = state.position() + micros as f64 / 1e6;
                        let duration = state.duration();
                        let target = if duration > 0.0 { target.clamp(0.0, duration) } else { target.max(0.0) };
                        player.seek(target);
                    }
                    Command::SetPosition(seconds) => player.seek(seconds),
                    Command::SetVolume(volume) => player.set_volume(volume),
                    Command::SetShuffle(shuffle) => player.set_shuffle(shuffle),
                    Command::SetLoop(mode) => player.set_repeat(mode),
                    Command::Raise => {
                        if let Some(window) = weak_ctx.upgrade().and_then(|c| c.window.borrow().clone()) {
                            window.window().set_visible(true);
                            window.present();
                        }
                    }
                    Command::Quit => {
                        player.stop();
                        if let Some(app) = gio::Application::default() {
                            app.quit();
                        }
                    }
                }
                if let Some(this) = weak.upgrade() {
                    this.refresh_all();
                }
            }
            tracing::debug!("mpris command pump ended");
        });
    }

    /// Every property the shell cares about, mirrored as the state changes.
    fn watch_state(self: &Rc<Self>, state: &PlayerState) {
        let connect = |name: &str, f: fn(&Rc<Mpris>)| {
            let weak = Rc::downgrade(self);
            state.connect_notify_local(Some(name), move |_, _| {
                if let Some(this) = weak.upgrade() {
                    f(&this);
                }
            });
        };
        connect("status", |this| {
            this.refresh_status();
            this.queue(Property::PlaybackStatus(this.shared.lock().unwrap().playback_status()));
        });
        for name in ["title", "artist", "thumbnail-url", "video-id"] {
            connect(name, |this| this.refresh_metadata());
        }
        connect("duration", |this| {
            this.refresh_metadata();
            this.queue(Property::CanSeek(this.shared.lock().unwrap().duration > 0.0));
        });
        connect("position", |this| this.refresh_position());
        connect("volume", |this| {
            let volume = this.player.state().volume();
            this.shared.lock().unwrap().volume = volume;
            this.queue(Property::Volume(volume));
        });
        connect("shuffle", |this| {
            let shuffle = this.player.state().shuffle();
            this.shared.lock().unwrap().shuffle = shuffle;
            this.queue(Property::Shuffle(shuffle));
        });
        connect("repeat", |this| {
            let repeat = this.player.state().repeat();
            this.shared.lock().unwrap().repeat = repeat;
            this.queue(Property::LoopStatus(match repeat {
                RepeatMode::Track => LoopStatus::Track,
                RepeatMode::All => LoopStatus::Playlist,
                RepeatMode::Off => LoopStatus::None,
            }));
        });
        for name in ["current-index", "queue-length"] {
            connect(name, |this| this.refresh_queue_bounds());
        }

        // A seek moves the position in a way clients cannot predict.
        let weak = Rc::downgrade(self);
        state.connect_seeked(move |position| {
            let Some(this) = weak.upgrade() else { return };
            this.refresh_position();
            let Some(server) = this.server.borrow().clone() else { return };
            this.net.spawn(async move {
                let signal = Signal::Seeked { position: Time::from_micros((position * 1e6) as i64) };
                if let Err(err) = server.emit(signal).await {
                    tracing::warn!(%err, "mpris seeked signal failed");
                }
            });
        });
    }

    fn refresh_all(self: &Rc<Self>) {
        self.refresh_status();
        self.refresh_metadata();
        self.refresh_position();
        self.refresh_queue_bounds();
        let state = self.player.state();
        let mut snapshot = self.shared.lock().unwrap();
        snapshot.volume = state.volume();
        snapshot.shuffle = state.shuffle();
        snapshot.repeat = state.repeat();
    }

    fn refresh_status(&self) {
        let status = self.player.state().status();
        let mut snapshot = self.shared.lock().unwrap();
        snapshot.status = status;
        // Freeze the drift baseline whenever playback stops advancing.
        snapshot.position_at = (status == PlaybackStatus::Playing).then(Instant::now);
    }

    fn refresh_position(&self) {
        let state = self.player.state();
        let mut snapshot = self.shared.lock().unwrap();
        snapshot.position = state.position();
        snapshot.position_at = (state.status() == PlaybackStatus::Playing).then(Instant::now);
    }

    fn refresh_queue_bounds(self: &Rc<Self>) {
        // The queue decides. The snapshot still answers CanGoPrevious from the
        // live position, because it crosses the restart threshold between ticks.
        let can_next = self.player.bounds().can_next;
        let can_previous = {
            let mut snapshot = self.shared.lock().unwrap();
            snapshot.can_next = can_next;
            snapshot.index = self.player.state().current_index();
            snapshot.can_previous()
        };
        self.queue(Property::CanGoNext(can_next));
        self.queue(Property::CanGoPrevious(can_previous));
    }

    /// Title, artists, album and art for the playing track. The album and the
    /// artist list come off the queue entry, which keeps more than the bar shows.
    fn refresh_metadata(self: &Rc<Self>) {
        let state = self.player.state();
        let track = self.player.current_track();
        let artists: Vec<String> = match track.as_ref().map(|t| &t.artists).filter(|a| !a.is_empty()) {
            Some(list) => list.iter().map(|a| a.name.clone()).filter(|n| !n.is_empty()).collect(),
            None => match state.artist() {
                artist if artist.is_empty() => Vec::new(),
                artist => vec![artist],
            },
        };
        let metadata = {
            let mut snapshot = self.shared.lock().unwrap();
            snapshot.video_id = state.video_id();
            snapshot.title = state.title();
            snapshot.artists = artists;
            snapshot.album = track.as_ref().and_then(|t| t.album.as_ref()).map(|a| a.name.clone()).unwrap_or_default();
            snapshot.art_url = state.thumbnail_url();
            snapshot.duration = state.duration();
            snapshot.metadata()
        };
        self.queue(Property::Metadata(metadata));
        self.sync_art(state.video_id(), state.thumbnail_url());
    }

    /// Port of _sync_mpris_art: shells want a local file, and the address the
    /// app carries is often a dead ytimg quality, so the art is downloaded,
    /// squared off and written to the cache. The remote address stays in the
    /// metadata until the file lands.
    fn sync_art(self: &Rc<Self>, video_id: String, thumbnail: String) {
        if video_id.is_empty() || thumbnail.is_empty() {
            return;
        }
        if self.art_video_id.borrow().as_deref() == Some(video_id.as_str()) {
            return;
        }
        self.art_video_id.replace(Some(video_id.clone()));
        let http = self.net.client().http().clone();
        let auth = self.net.client().media_auth();
        let dir = self.paths.cache_dir.join("mpris");
        let wanted = video_id.clone();
        let handle = self.net.spawn(async move { write_art_file(http, auth, dir, wanted, thumbnail).await });
        let weak = Rc::downgrade(self);
        glib::spawn_future_local(async move {
            let Ok(Some(path)) = handle.await else { return };
            let Some(this) = weak.upgrade() else { return };
            // The track may have moved on while the art was downloading.
            if this.player.state().video_id() != video_id {
                tracing::debug!(video_id, "mpris art arrived for a track that moved on");
                return;
            }
            let metadata = {
                let mut snapshot = this.shared.lock().unwrap();
                snapshot.art_file = format!("file://{}", path.display());
                snapshot.art_file_video_id = video_id;
                snapshot.metadata()
            };
            this.queue(Property::Metadata(metadata));
        });
    }

    /// Hold a change until the next idle: a track change touches four
    /// properties and the shell only needs one signal.
    fn queue(self: &Rc<Self>, property: Property) {
        self.pending.borrow_mut().push(property);
        if self.flush_queued.replace(true) {
            return;
        }
        let weak = Rc::downgrade(self);
        glib::idle_add_local_once(move || {
            if let Some(this) = weak.upgrade() {
                this.flush();
            }
        });
    }

    fn flush(&self) {
        self.flush_queued.set(false);
        let mut properties = std::mem::take(&mut *self.pending.borrow_mut());
        if properties.is_empty() {
            return;
        }
        // Keep the last of each kind: four metadata rewrites are one signal.
        let mut seen = Vec::new();
        properties.reverse();
        properties.retain(|p| {
            let kind = std::mem::discriminant(p);
            if seen.contains(&kind) {
                return false;
            }
            seen.push(kind);
            true
        });
        properties.reverse();
        let Some(server) = self.server.borrow().clone() else { return };
        self.net.spawn(async move {
            if let Err(err) = server.properties_changed(properties).await {
                tracing::warn!(%err, "mpris properties_changed failed");
            }
        });
    }

    /// Release the bus name and drop the server, which ends the command pump.
    pub fn shutdown(&self) {
        let Some(server) = self.server.borrow_mut().take() else { return };
        self.net.spawn(async move {
            if let Err(err) = server.release_bus_name().await {
                tracing::warn!(%err, "mpris bus name release failed");
            }
            drop(server);
        });
        tracing::info!("mpris shut down");
    }
}

/// Fetch the art, centre-crop it to a square, upscale anything small and
/// write it as JPEG under the cache. Returns the file it wrote.
async fn write_art_file(http: reqwest::Client, auth: Option<HttpAuth>, dir: PathBuf, video_id: String, thumbnail: String) -> Option<PathBuf> {
    let bytes = fetch_cover_bytes(&http, auth.as_ref(), &thumbnail).await?;
    // A unique name per track: clients cache art by address.
    let safe: String = video_id.chars().map(|c| if c.is_ascii_alphanumeric() { c } else { '_' }).collect();
    let path = dir.join(format!("mpris_art_{safe}.jpg"));
    let result = tokio::task::spawn_blocking(move || save_square_jpeg(&dir, &path, &bytes).map(|()| path)).await;
    match result {
        Ok(Ok(path)) => Some(path),
        Ok(Err(err)) => {
            tracing::warn!(%err, video_id, "mpris art could not be written");
            None
        }
        Err(_) => None,
    }
}

/// Blocking half: the pixbuf work and the file write. Older art goes first,
/// so the cache holds one cover rather than one per track ever played.
fn save_square_jpeg(dir: &Path, path: &Path, bytes: &[u8]) -> anyhow::Result<()> {
    use gtk::gdk_pixbuf::{InterpType, PixbufLoader};
    use gtk::prelude::PixbufLoaderExt;

    std::fs::create_dir_all(dir)?;
    if let Ok(entries) = std::fs::read_dir(dir) {
        for entry in entries.flatten() {
            let name = entry.file_name();
            let name = name.to_string_lossy();
            if name.starts_with("mpris_art_") && name.ends_with(".jpg") {
                let _ = std::fs::remove_file(entry.path());
            }
        }
    }
    let loader = PixbufLoader::new();
    loader.write(bytes)?;
    loader.close()?;
    let pixbuf = loader.pixbuf().ok_or_else(|| anyhow::anyhow!("art did not decode"))?;
    // Video thumbnails are 16:9 and MPRIS wants a square.
    let (width, height) = (pixbuf.width(), pixbuf.height());
    let size = width.min(height);
    let mut pixbuf = pixbuf.new_subpixbuf((width - size) / 2, (height - size) / 2, size, size);
    if size < MIN_ART_SIZE {
        pixbuf = pixbuf.scale_simple(MIN_ART_SIZE, MIN_ART_SIZE, InterpType::Bilinear).ok_or_else(|| anyhow::anyhow!("art did not scale"))?;
    }
    pixbuf.savev(path, "jpeg", &[("quality", "90")])?;
    Ok(())
}

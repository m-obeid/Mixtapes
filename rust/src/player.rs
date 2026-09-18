//! Playback controller. Owns the queue and drives the audio thread.
//!
//! This is the Rust home of what Player (player.py) did on the GTK thread:
//! queue mutation, repeat and shuffle, the load generation counter, and the
//! reaction to audio events. It never blocks. Stream resolution runs on the
//! tokio runtime and comes back through `glib::spawn_future_local`.

use std::cell::{Cell, RefCell};
use std::rc::{Rc, Weak};
use std::sync::Arc;

use gio::prelude::*;
use tokio::task::AbortHandle;

use crate::audio::{AudioCommand, AudioEvent, AudioEvents, AudioHandle, AudioTelemetry};
use crate::model::{LikeStatus, PlaybackStatus, RepeatMode, StreamInfo, Track, VideoId};
use crate::downloads::Downloads;
use crate::net::NetHandle;
use crate::paths::Paths;
use crate::net::ytmusic::AuthState;
use crate::queue::{Bounds, Queue, Step};
use crate::state::{PlayerState, QueueEntry};
use gtk::glib;

const STREAM_RETRY_MAX: u8 = 2;

/// `history_mode`, the pref both apps read: when a play is recorded.
const HISTORY_IMMEDIATE: &str = "immediate";
const HISTORY_AFTER_30S: &str = "after_30s";
const HISTORY_NEVER: &str = "never";
/// The video type of a song, as opposed to a music video.
const AUDIO_VIDEO_TYPE: &str = "MUSIC_VIDEO_TYPE_ATV";
/// How long "after_30s" waits. YT Music counts a play at about this point.
const HISTORY_THRESHOLD_SECS: f64 = 30.0;

/// The track handed to playbin ahead of time, so the switch has no gap.
#[derive(Clone)]
struct Armed {
    index: usize,
    generation: u64,
    video_id: VideoId,
}

/// Port of _summarize_yt_dlp_error: the part of a resolver or pipeline error
/// a person would put in a toast. The first sentence after "ERROR:", capped.
fn summarize_error(message: &str) -> String {
    let message = message.split_once("ERROR:").map_or(message, |(_, rest)| rest).trim();
    let sentence = [". ", "; "].iter().find_map(|sep| message.split_once(sep)).map_or(message, |(first, _)| first);
    let trimmed = sentence.trim_matches([' ', '.']);
    if trimmed.is_empty() { "Could not load this track".to_owned() } else { trimmed.chars().take(140).collect() }
}

/// A play as the scrobbler sees it: a fresh one, or the same one under better metadata.
#[derive(Clone, Debug)]
pub enum PlayEvent {
    Started(Track),
    Refined(Track),
}

type PlayListener = Box<dyn Fn(&PlayEvent)>;

pub struct Player {
    me: Weak<Player>,
    state: PlayerState,
    queue: RefCell<Queue>,
    audio: AudioHandle,
    audio_events: RefCell<Option<AudioEvents>>,
    net: NetHandle,
    /// What is already on disk, checked before a stream is resolved.
    downloads: Arc<Downloads>,
    /// The stream behind what is playing, for the Stream Info panel.
    loaded: RefCell<Option<StreamInfo>>,
    /// Allocator for load generations. Monotonic, never reused.
    counter: Cell<u64>,
    /// Generation of the stream the pipeline should be playing right now.
    current: Cell<u64>,
    /// The track pre-armed for a gapless switch, if any.
    armed_next: RefCell<Option<Armed>>,
    inflight: RefCell<Option<AbortHandle>>,
    retries: Cell<u8>,
    /// Spectrum frames keyed by stream time, what _viz_queue held: released
    /// once the sink has reached them, so the bars follow the audible sound.
    viz_queue: RefCell<std::collections::VecDeque<(i64, Vec<f32>)>>,
    /// Load generation the queued frames belong to. A gapless switch restarts
    /// stream time, so frames from the old stream would sit in the queue's
    /// front with times the new play-head never reaches, and the drain would
    /// hand back nothing until the length cap evicted them.
    viz_generation: Cell<u64>,
    /// The last position tick and its arrival time, interpolated between ticks.
    position_mark: Cell<Option<(std::time::Instant, f64)>>,
    /// A radio extension is in flight, like _is_fetching_infinite.
    infinite_fetching: Cell<bool>,
    /// Counter behind the queue stamps play_then_radio hands out.
    stamp: Cell<u64>,
    /// When a play is written to the account's history.
    history_mode: RefCell<String>,
    /// The video this session has already recorded, so it records once.
    history_recorded: RefCell<Option<String>>,
    /// Videos already looked up for an audio twin, so the lookup is paid once.
    swap_checked: RefCell<std::collections::HashSet<String>>,
    /// Told when a play starts or its metadata is corrected. See `on_play`.
    play_listeners: RefCell<Vec<PlayListener>>,
    /// Set while the audio-version swap writes its metadata, so the source id survives it.
    swapping: Cell<bool>,
}

impl Player {
    pub fn new(net: NetHandle, downloads: Arc<Downloads>, audio: AudioHandle, events: AudioEvents, paths: &Paths) -> Rc<Self> {
        Rc::new_cyclic(|me| Self {
            me: me.clone(),
            downloads,
            loaded: RefCell::new(None),
            state: PlayerState::new(),
            queue: RefCell::new(Queue::default()),
            audio,
            audio_events: RefCell::new(Some(events)),
            net,
            counter: Cell::new(0),
            current: Cell::new(0),
            armed_next: RefCell::new(None),
            inflight: RefCell::new(None),
            retries: Cell::new(0),
            viz_queue: RefCell::new(std::collections::VecDeque::new()),
            viz_generation: Cell::new(0),
            position_mark: Cell::new(None),
            infinite_fetching: Cell::new(false),
            stamp: Cell::new(0),
            history_mode: RefCell::new(paths.read_prefs().get("history_mode").and_then(|v| v.as_str()).unwrap_or(HISTORY_IMMEDIATE).to_owned()),
            history_recorded: RefCell::new(None),
            swap_checked: RefCell::new(std::collections::HashSet::new()),
            play_listeners: RefCell::new(Vec::new()),
            swapping: Cell::new(false),
        })
    }

    /// Follow plays for the app's lifetime. A metadata re-emit mid-load never
    /// arrives as Started, so a listening clock is not restarted by one.
    pub fn on_play(&self, listener: impl Fn(&PlayEvent) + 'static) {
        self.play_listeners.borrow_mut().push(Box::new(listener));
    }

    fn emit_play(&self, event: PlayEvent) {
        for listener in self.play_listeners.borrow().iter() {
            listener(&event);
        }
    }

    /// Port of _precache_next: resolve the three tracks either side of the
    /// playing one, so Next, Previous and a short jump start from the stream
    /// cache. One after another on the runtime, and only what is not on disk.
    fn precache_neighbours(&self) {
        const REACH: usize = 3;
        let (tracks, current) = {
            let queue = self.queue.borrow();
            (queue.tracks().to_vec(), queue.current())
        };
        let Some(current) = current else { return };
        let wanted: Vec<VideoId> = (1..=REACH)
            .flat_map(|step| [current.checked_add(step), current.checked_sub(step)])
            .flatten()
            .filter_map(|i| tracks.get(i))
            .filter(|t| !t.video_id.0.is_empty() && !t.is_upload() && !self.downloads.is_downloaded(&t.video_id.0))
            .map(|t| t.video_id.clone())
            .collect();
        if wanted.is_empty() {
            return;
        }
        let resolver = self.net.resolver().clone();
        let auth = self.net.client().media_auth();
        self.net.spawn(async move {
            for video_id in wanted {
                if let Err(err) = resolver.resolve(video_id.clone(), auth.clone()).await {
                    tracing::debug!(%video_id, %err, "neighbour not pre-resolved");
                }
            }
        });
    }

    /// A track queued from a search or a shelf often has no album, which
    /// leaves Discord's art caption and the scrobble's album blank. The watch
    /// panel knows it, so it is asked once per play, off the hot path.
    fn backfill_album(&self, track: &Track) {
        if track.album.is_some() || track.video_id.0.is_empty() || track.is_upload() || track.video_id.0.starts_with("demo:") {
            return;
        }
        let api = self.net.client().api();
        let video_id = track.video_id.clone();
        let wanted = video_id.0.clone();
        let handle = self.net.spawn(async move { crate::net::playlists::get_watch_playlist(&*api, Some(&wanted), None, 1, false).await });
        let weak = self.weak_self();
        glib::spawn_future_local(async move {
            let album = handle.await.ok().and_then(Result::ok).and_then(|w| w.tracks.into_iter().map(|t| t.track).find(|t| t.video_id == video_id)).and_then(|t| t.album);
            if let (Some(player), Some(album)) = (weak.upgrade(), album) {
                // Only the album: the row's own title and artists stay as the listener saw them.
                player.refresh_track_metadata(&Track { video_id, album: Some(album), ..Track::default() });
            }
        });
    }

    /// Port of _apply_metadata: fresh title, artists, album and art for a
    /// track, written over every copy in the queue and over what is playing.
    pub fn refresh_track_metadata(&self, fresh: &Track) {
        if !self.queue.borrow_mut().refresh_metadata(fresh) {
            return;
        }
        self.sync_queue_model();
        let current = self.queue.borrow().current_track().cloned();
        if let Some(current) = current.filter(|t| t.video_id == fresh.video_id) {
            self.apply_track_metadata(Some(&current));
            self.emit_play(PlayEvent::Refined(current));
        }
    }

    /// Change when plays are written to the account's history, live.
    pub fn set_history_mode(&self, mode: &str) {
        self.history_mode.replace(mode.to_owned());
    }

    pub fn state(&self) -> &PlayerState {
        &self.state
    }

    pub fn net(&self) -> &NetHandle {
        &self.net
    }

    pub fn current_track(&self) -> Option<Track> {
        self.queue.borrow().current_track().cloned()
    }

    pub fn queue_tracks(&self) -> Vec<Track> {
        self.queue.borrow().tracks().to_vec()
    }

    pub fn repeat_mode(&self) -> RepeatMode {
        self.queue.borrow().repeat()
    }

    /// Whether Next and Previous have anywhere to go, for the transport bar
    /// and the system controls. The queue decides; nobody re-derives it.
    pub fn bounds(&self) -> Bounds {
        self.queue.borrow().bounds(self.state.position())
    }

    /// Port of pull_visualizer_bands: the spectrum frame the sink is playing
    /// right now, or None while paused or stopped so the bars fall. The most
    /// recent entry whose stream time the play-head has reached wins;
    /// entries more than a second behind are dropped.
    pub fn pull_visualizer_bands(&self) -> Option<Vec<f32>> {
        if self.state.status() != PlaybackStatus::Playing {
            return None;
        }
        let mut queue = self.viz_queue.borrow_mut();
        if queue.is_empty() {
            return None;
        }

        let Some((at, position)) = self.position_mark.get() else {
            return queue.back().map(|(_, b)| b.clone());
        };
        let pos_ns = ((position + at.elapsed().as_secs_f64()) * 1e9) as i64;
        let mut latest = None;
        for (st, bands) in queue.iter() {
            if *st < 0 || *st <= pos_ns {
                latest = Some(bands.clone());
            } else {
                break;
            }
        }
        let stale = pos_ns - 1_000_000_000;
        while queue.front().is_some_and(|(st, _)| *st >= 0 && *st < stale) {
            queue.pop_front();
        }
        latest
    }

    fn clear_visualizer_queue(&self) {
        self.viz_queue.borrow_mut().clear();
        self.position_mark.set(None);
    }

    /// Rate a track. Applies locally at once, reverts if the server rejects it.
    pub fn set_like_status(&self, video_id: VideoId, status: LikeStatus) {
        let client = self.net.client().clone();
        let previous = client
            .known_like_status(video_id.as_str())
            .or_else(|| {
                self.queue
                    .borrow()
                    .tracks()
                    .iter()
                    .find(|t| t.video_id == video_id)
                    .map(|t| t.like_status)
            })
            .unwrap_or_default();
        self.apply_like_locally(&video_id, status);
        let vid = video_id.clone();
        let handle = self
            .net
            .spawn(async move { client.rate_song(vid.as_str(), status).await });
        let weak = self.weak_self();
        glib::spawn_future_local(async move {
            let Ok(Err(err)) = handle.await else { return };
            tracing::warn!(%err, %video_id, "rating failed");
            if let Some(player) = weak.upgrade() {
                player.apply_like_locally(&video_id, previous);
                player.state.emit_notice("Couldn't update rating");
            }
        });
    }

    fn apply_like_locally(&self, video_id: &VideoId, status: LikeStatus) {
        self.net
            .client()
            .set_known_like_status(video_id.as_str(), status);
        self.queue.borrow_mut().set_like_status(video_id, status);
        if self.state.video_id() == video_id.as_str() {
            self.state.set_like_status(status.as_str().to_owned());
        }
    }

    /// Attach the event pumps to the GTK main context. Call once, from startup.
    pub fn start(self: &Rc<Self>) {
        let Some(events) = self.audio_events.borrow_mut().take() else {
            return;
        };

        let weak = Rc::downgrade(self);
        let control = events.control;
        glib::spawn_future_local(async move {
            while let Ok(event) = control.recv().await {
                let Some(player) = weak.upgrade() else { break };
                player.on_audio_event(event);
            }
        });

        let weak = Rc::downgrade(self);
        let telemetry = events.telemetry;
        glib::spawn_future_local(async move {
            while let Ok(item) = telemetry.recv().await {
                let Some(player) = weak.upgrade() else { break };
                player.on_telemetry(item);
            }
        });

        let weak = Rc::downgrade(self);
        let mut auth = self.net.client().subscribe_auth();
        glib::spawn_future_local(async move {
            loop {
                let snapshot = auth.borrow_and_update().clone();
                let Some(player) = weak.upgrade() else { break };
                player.apply_auth(&snapshot);
                if auth.changed().await.is_err() {
                    break;
                }
            }
        });

        // Deferred validation of the saved session, same as check_auth() in window.py.
        let client = self.net.client().clone();
        self.net.spawn(async move {
            if let Err(err) = client.validate().await {
                tracing::warn!(%err, "session validation failed");
            }
        });
    }

    pub fn shutdown(&self) {
        self.abort_inflight();
        self.audio.send(AudioCommand::Stop);
        self.audio.shutdown();
    }

    // -- queue API (public surface for the UI) ----------------------------

    /// Load a queue and select `start_index` without starting playback.
    /// Used for the demo queue and, later, for restoring the last session.
    pub fn stage_tracks(&self, tracks: Vec<Track>, start_index: usize) {
        self.abort_inflight();
        self.disarm_gapless();
        self.audio.send(AudioCommand::Stop);
        let track = {
            let mut q = self.queue.borrow_mut();
            q.stage(tracks, start_index);
            q.current_track().cloned()
        };
        self.state.set_shuffle(false);
        self.state.set_status(PlaybackStatus::Stopped);
        self.state.set_position(0.0);
        self.state.set_duration(
            track
                .as_ref()
                .and_then(|t| t.duration_seconds)
                .map(f64::from)
                .unwrap_or(0.0),
        );
        self.apply_track_metadata(track.as_ref());
        self.sync_queue_model();
    }

    pub fn play_tracks(
        &self,
        tracks: Vec<Track>,
        start_index: usize,
        shuffle: bool,
        source_id: Option<String>,
        infinite: bool,
    ) {
        let step = self.queue.borrow_mut().replace(tracks, start_index, shuffle, source_id, infinite);
        self.state.set_shuffle(shuffle);
        self.sync_queue_model();
        self.apply(step);
    }

    pub fn add_to_queue(&self, tracks: Vec<Track>, play_next: bool) {
        let step = self.queue.borrow_mut().insert(tracks, play_next);
        self.sync_queue_model();
        self.apply(step);
    }

    pub fn remove_from_queue(&self, index: usize) {
        let step = self.queue.borrow_mut().remove(index);
        self.sync_queue_model();
        self.apply(step);
    }

    pub fn move_queue_item(&self, old_index: usize, new_index: usize) -> bool {
        if !self.queue.borrow_mut().move_item(old_index, new_index) {
            return false;
        }
        self.sync_queue_model();
        true
    }

    pub fn clear_queue(&self) {
        self.abort_inflight();
        // A fresh generation makes any in-flight resolution stale.
        self.current.set(self.alloc_generation());
        self.queue.borrow_mut().clear();
        self.audio.send(AudioCommand::Stop);
        self.state.set_status(PlaybackStatus::Stopped);
        self.state.set_shuffle(false);
        self.apply_track_metadata(None);
        self.sync_queue_model();
    }

    pub fn play_queue_index(&self, index: usize) {
        let step = self.queue.borrow_mut().jump(index);
        self.apply(step);
    }

    pub fn next(&self) {
        let step = self.queue.borrow_mut().advance();
        self.apply(step);
    }

    pub fn previous(&self) {
        let step = self.queue.borrow_mut().back(self.state.position());
        self.apply(step);
    }

    pub fn toggle_shuffle(&self) {
        let shuffled = self.queue.borrow_mut().toggle_shuffle();
        self.state.set_shuffle(shuffled);
        self.sync_queue_model();
    }

    /// Shuffle on or off, for callers that know which they want.
    pub fn set_shuffle(&self, on: bool) {
        if self.queue.borrow().shuffle() != on {
            self.toggle_shuffle();
        }
    }

    pub fn set_repeat(&self, mode: RepeatMode) {
        self.queue.borrow_mut().set_repeat(mode);
        self.state.set_repeat(mode);
        // Repeat changes what follows the current track, so what is armed may be wrong.
        self.resync_gapless();
    }

    /// Carry out what the queue decided.
    fn apply(&self, step: Step) {
        match step {
            Step::Load(_) => {
                self.load_current();
                self.maybe_extend_infinite();
            }
            Step::Restart => self.seek(0.0),
            Step::Stop => {
                self.stop();
                self.mark_current(None);
            }
            // A deduped batch can leave a radio dry before the halfway trigger fires.
            Step::Extend => self.force_radio_extend(),
            Step::Stay => {}
        }
    }

    // -- transport API ----------------------------------------------------

    pub fn play(&self) {
        if self.queue.borrow().current().is_none() {
            return;
        }
        self.audio.send(AudioCommand::Play);
    }

    pub fn pause(&self) {
        self.audio.send(AudioCommand::Pause);
    }

    pub fn toggle_play(&self) {
        match self.state.status() {
            PlaybackStatus::Playing => self.pause(),
            PlaybackStatus::Paused => self.play(),
            PlaybackStatus::Stopped if self.queue.borrow().current().is_some() => self.load_current(),
            _ => {}
        }
    }

    pub fn stop(&self) {
        self.abort_inflight();
        self.disarm_gapless();
        self.audio.send(AudioCommand::Stop);
        self.state.set_status(PlaybackStatus::Stopped);
        self.state.set_position(0.0);
    }

    pub fn seek(&self, seconds: f64) {
        // Queued spectrum frames belong to the old position on either side of the seek.
        self.clear_visualizer_queue();
        self.audio.send(AudioCommand::Seek { seconds });
        let target = seconds.max(0.0);
        self.state.set_position(target);
        self.state.emit_seeked(target);
    }

    pub fn set_volume(&self, volume: f64) {
        self.audio.send(AudioCommand::SetVolume(volume));
    }

    pub fn set_mute(&self, muted: bool) {
        self.audio.send(AudioCommand::SetMute(muted));
    }

    // -- loading ----------------------------------------------------------

    fn alloc_generation(&self) -> u64 {
        let g = self.counter.get() + 1;
        self.counter.set(g);
        g
    }

    fn abort_inflight(&self) {
        if let Some(handle) = self.inflight.borrow_mut().take() {
            handle.abort();
        }
    }

    fn disarm_gapless(&self) {
        if self.armed_next.borrow_mut().take().is_some() {
            self.audio.send(AudioCommand::DisarmNext);
        }
    }

    /// Keep the armed track in step with the queue after an edit.
    ///
    /// Only an edit that changes what follows the current track makes the
    /// armed URI wrong. Disarming on every edit meant a radio extension, which
    /// appends to the queue, cancelled gapless for the rest of the track.
    fn resync_gapless(&self) {
        let armed = self.armed_next.borrow().as_ref().map(|a| a.video_id.clone());
        let wanted = {
            let q = self.queue.borrow();
            q.armable_next().and_then(|i| q.track_at(i)).map(|t| t.video_id.clone())
        };
        if armed == wanted {
            return;
        }
        self.disarm_gapless();
        if wanted.is_some() && self.state.status() == PlaybackStatus::Playing {
            self.arm_gapless();
        }
    }

    /// Start playing the queue's current index under a new generation.
    fn load_current(&self) {
        // A new stream restarts stream time; queued frames would mislead the drain.
        self.clear_visualizer_queue();
        let (index, track) = {
            let q = self.queue.borrow();
            match q.current().zip(q.current_track().cloned()) {
                Some(pair) => pair,
                None => {
                    drop(q);
                    self.stop();
                    return;
                }
            }
        };
        self.abort_inflight();
        self.disarm_gapless();
        let generation = self.alloc_generation();
        self.current.set(generation);
        self.retries.set(0);
        // Resolution can take seconds for an uncached stream, and the pipeline
        // would keep playing the old track throughout. Tear it down now, as
        // Player.set_queue and play_queue_index did. The Stopped event this
        // raises carries the old generation, so the Loading state set below
        // survives it. Gapless never comes through here: the pipeline swaps
        // to the armed URI on its own and reports StreamStarted.
        self.audio.send(AudioCommand::Stop);

        self.mark_current(Some(index));
        self.state.set_status(PlaybackStatus::Loading);
        self.state.set_position(0.0);
        self.state
            .set_duration(track.duration_seconds.map(f64::from).unwrap_or(0.0));
        self.apply_track_metadata(Some(&track));
        self.emit_play(PlayEvent::Started(track.clone()));
        self.backfill_album(&track);
        tracing::debug!(generation, index, video_id = %track.video_id, "loading");

        self.spawn_resolve(track, generation, false);
    }

    /// Resolve `track` on the runtime; on success either load it or arm it for gapless.
    fn spawn_resolve(&self, track: Track, generation: u64, arm_only: bool) {
        // A downloaded file needs no resolve and plays offline.
        if let Some(uri) = self.downloads.local_path(track.video_id.as_str()).and_then(|p| glib::filename_to_uri(&p, None).ok()) {
            tracing::debug!(video_id = %track.video_id, "playing the downloaded file");
            if !arm_only {
                *self.loaded.borrow_mut() = Some(StreamInfo { uri: uri.to_string(), is_local: true, ..StreamInfo::default() });
            }
            match arm_only {
                true if self.armed_next.borrow().as_ref().is_some_and(|a| a.generation == generation) => self.audio.send(AudioCommand::ArmNext { uri: uri.to_string(), generation }),
                true => {}
                false => self.audio.send(AudioCommand::Load { uri: uri.to_string(), generation, auth: None }),
            }
            return;
        }
        let auth = self.net.client().media_auth();
        let resolver = self.net.resolver().clone();
        let video_id = track.video_id.clone();
        let resolve_auth = auth.clone();
        // Port of the swap _fetch_and_play does before yt-dlp runs: a music
        // video's audio twin is the cleaner album master, and its id is what
        // the stream cache should be keyed on, so this has to happen first.
        let swap = !arm_only && self.wants_audio_version(&track);
        let api = self.net.client().api();
        let handle = self.net.spawn(async move {
            let twin = match swap {
                true => match crate::net::playlists::find_audio_version(&api, video_id.as_str()).await {
                    Ok(twin) => twin,
                    Err(err) => {
                        tracing::warn!(%err, video_id = %video_id, "audio-version lookup failed");
                        None
                    }
                },
                false => None,
            };
            let resolve_id = twin.as_ref().map(|t| t.video_id.clone()).unwrap_or(video_id);
            (twin, resolver.resolve(resolve_id, resolve_auth).await)
        });
        if !arm_only {
            *self.inflight.borrow_mut() = Some(handle.abort_handle());
        }

        let weak = self.weak_self();
        glib::spawn_future_local(async move {
            let outcome = handle.await;
            let Some(player) = weak.upgrade() else { return };
            let Ok((twin, result)) = outcome else { return };
            if arm_only {
                if let Ok(info) = result {
                    if player.armed_next.borrow().as_ref().is_some_and(|a| a.generation == generation) {
                        player.audio.send(AudioCommand::ArmNext {
                            uri: info.uri,
                            generation,
                        });
                    }
                }
                return;
            }
            if player.current.get() != generation {
                tracing::debug!(generation, "stale resolution dropped");
                return;
            }
            player.inflight.borrow_mut().take();
            // Everything below reads the track that is actually going to play.
            let track = match twin {
                Some(twin) => player.apply_audio_version(&track, twin),
                None => track,
            };
            match result {
                Ok(info) => {
                    *player.loaded.borrow_mut() = Some(info.clone());
                    if !info.from_cache {
                        player.refine_metadata(
                            &track,
                            info.title.as_deref(),
                            info.uploader.as_deref(),
                            info.thumbnail.as_deref(),
                        );
                    }
                    player.audio.send(AudioCommand::Load {
                        uri: info.uri,
                        generation,
                        auth,
                    });
                }
                Err(err) => {
                    tracing::warn!(video_id = %track.video_id, %err, "resolution failed");
                    player.state.emit_track_error(
                        track.video_id.as_str(),
                        &track.title,
                        &summarize_error(&err.to_string()),
                    );
                    player.advance_after_failure();
                }
            }
        });
    }

    /// What is playing and how, for the Stream Info panel in the expanded
    /// player. The pipeline half is queried on the audio thread; `full` keeps
    /// the whole signed URI, which is what the Copy button wants.
    pub async fn stream_debug(&self, full: bool) -> String {
        let mut lines = Vec::new();
        let track = self.current_track();
        lines.push(format!("Track:      {}", track.as_ref().map(|t| t.video_id.0.clone()).unwrap_or_else(|| "none".to_owned())));
        if let Some(track) = &track {
            lines.push(format!("Title:      {} - {}", track.artist, track.title));
        }
        let loaded = self.loaded.borrow().clone();
        match &loaded {
            Some(info) if info.is_local => lines.push("Source:     local file".to_owned()),
            Some(info) if info.from_cache => lines.push("Source:     cached stream url".to_owned()),
            Some(_) => lines.push("Source:     fresh resolution".to_owned()),
            None => lines.push("Source:     nothing loaded".to_owned()),
        }
        if let Some(info) = &loaded {
            let format = [info.format_id.as_deref(), info.protocol.as_deref(), info.ext.as_deref(), info.acodec.as_deref()].into_iter().flatten().collect::<Vec<_>>().join(" - ");
            if !format.is_empty() {
                lines.push(format!("Format:     {format}"));
            }
            // Signed urls run to several hundred characters, too wide for the panel.
            let uri = match full || info.uri.len() <= 96 {
                true => info.uri.clone(),
                false => format!("{}…", &info.uri[..96]),
            };
            lines.push(format!("URI:        {uri}"));
        }
        let (reply, answers) = async_channel::bounded(1);
        self.audio.send(AudioCommand::Describe { reply });
        match answers.recv().await {
            Ok(text) => lines.push(text),
            Err(_) => lines.push("State:      the audio thread did not answer".to_owned()),
        }
        lines.join("\n")
    }

    fn advance_after_failure(&self) {
        let step = self.queue.borrow_mut().failed_current();
        self.apply(step);
    }

    /// Pre-resolve the following track and hand its URI to playbin for a gapless switch.
    fn arm_gapless(&self) {
        let next = {
            let q = self.queue.borrow();
            q.armable_next().and_then(|i| q.track_at(i).map(|t| (i, t.clone())))
        };
        let Some((index, track)) = next else { return };
        if track.is_upload() {
            return;
        }
        let generation = self.alloc_generation();
        self.armed_next.replace(Some(Armed { index, generation, video_id: track.video_id.clone() }));
        self.spawn_resolve(track, generation, true);
    }

    // -- event application ------------------------------------------------

    fn on_audio_event(&self, event: AudioEvent) {
        match event {
            AudioEvent::VolumeChanged { volume, muted } => {
                if (self.state.volume() - volume).abs() > 1e-3 {
                    self.state.set_volume(volume);
                }
                if self.state.muted() != muted {
                    self.state.set_muted(muted);
                }
            }
            AudioEvent::StreamStarted { generation } => {
                if let Some(armed) = self.armed_next.borrow_mut().take() {
                    if armed.generation == generation {
                        let index = armed.index;
                        self.current.set(generation);
                        self.retries.set(0);
                        let track = self.queue.borrow_mut().adopt(index).cloned();
                        self.mark_current(Some(index));
                        self.clear_visualizer_queue();
                        self.state.set_position(0.0);
                        self.state.set_duration(
                            track
                                .as_ref()
                                .and_then(|t| t.duration_seconds)
                                .map(f64::from)
                                .unwrap_or(0.0),
                        );
                        self.apply_track_metadata(track.as_ref());
                        if let Some(track) = track {
                            self.emit_play(PlayEvent::Started(track));
                        }
                    }
                }
            }
            AudioEvent::StateChanged { generation, status } => {
                if generation != self.current.get()
                    && !self.armed_next.borrow().as_ref().is_some_and(|a| a.generation == generation)
                {
                    return;
                }
                tracing::debug!(generation, ?status, "audio state");
                if status == PlaybackStatus::Playing
                    && self.state.status() != PlaybackStatus::Playing
                {
                    self.retries.set(0);
                    if self.armed_next.borrow().is_none() {
                        self.arm_gapless();
                    }
                    self.precache_neighbours();
                }
                self.state.set_status(status);
                self.sync_paused_flag();
            }
            AudioEvent::Prerolled { .. } => {}
            AudioEvent::EndOfStream { generation } => {
                if generation != self.current.get() {
                    return;
                }
                let step = self.queue.borrow_mut().finished();
                self.apply(step);
            }
            AudioEvent::Error {
                generation,
                message,
                debug: debug_info,
            } => {
                if generation != self.current.get() {
                    return;
                }
                tracing::warn!(%message, ?debug_info, "stream error");
                let track = self.queue.borrow().current_track().cloned();
                let Some(track) = track else { return };
                if self.retries.get() < STREAM_RETRY_MAX {
                    // googlevideo hosts rotate; a fresh resolution usually lands on a healthy one.
                    self.retries.set(self.retries.get() + 1);
                    let resolver = self.net.resolver().clone();
                    let vid = track.video_id.clone();
                    self.net
                        .spawn(async move { resolver.invalidate(&vid).await });
                    let retries = self.retries.get();
                    self.load_current();
                    self.retries.set(retries);
                } else {
                    self.state
                        .emit_track_error(track.video_id.as_str(), &track.title, &summarize_error(&message));
                    self.advance_after_failure();
                }
            }
        }
    }

    fn on_telemetry(&self, item: AudioTelemetry) {
        match item {
            AudioTelemetry::Position {
                generation,
                position,
                duration,
            } => {
                if generation != self.current.get()
                    || self.state.status() == PlaybackStatus::Loading
                {
                    return;
                }
                self.state.set_position(position);
                self.position_mark
                    .set(Some((std::time::Instant::now(), position)));
                if self.history_mode.borrow().as_str() == HISTORY_AFTER_30S
                    && position >= HISTORY_THRESHOLD_SECS
                    && self.state.status() == PlaybackStatus::Playing
                {
                    self.record_play(&self.state.video_id());
                }
                if let Some(d) = duration {
                    if (self.state.duration() - d).abs() > 0.1 {
                        self.state.set_duration(d);
                    }
                }
            }
            AudioTelemetry::Spectrum {
                generation,
                stream_time,
                bands,
            } => {
                if generation != self.current.get() {
                    return;
                }
                let mut queue = self.viz_queue.borrow_mut();
                if self.viz_generation.replace(generation) != generation {
                    queue.clear();
                }
                queue.push_back((stream_time, bands));
                // About three seconds at the element's 30 Hz tick.
                while queue.len() > 90 {
                    queue.pop_front();
                }
            }
        }
    }

    fn apply_auth(&self, auth: &AuthState) {
        match auth {
            AuthState::Authenticated(info) => {
                self.state.set_authenticated(true);
                self.state.set_account_name(info.name.clone());
                self.state
                    .set_account_handle(info.handle.clone().unwrap_or_default());
                self.state
                    .set_account_photo_url(info.photo_url.clone().unwrap_or_default());
            }
            other => {
                tracing::debug!(?other, "auth state");
                self.state.set_authenticated(false);
                self.state.set_account_name(String::new());
                self.state.set_account_handle(String::new());
                self.state.set_account_photo_url(String::new());
            }
        }
    }

    // -- state mirroring --------------------------------------------------

    fn apply_track_metadata(&self, track: Option<&Track>) {
        // A different track ends whatever swap the last one went through. The
        // swap itself sets the source id first and keeps the new id, so it survives this.
        let incoming = track.map(|t| t.video_id.0.as_str()).unwrap_or_default();
        if incoming != self.state.video_id() && !self.swapping.get() {
            self.state.set_source_video_id(String::new());
        }
        // A new track is a fresh gate, whichever way it started playing.
        if track.map(|t| t.video_id.0.as_str()) != self.history_recorded.borrow().as_deref() {
            self.history_recorded.replace(None);
        }
        if let Some(t) = track.filter(|_| self.history_mode.borrow().as_str() == HISTORY_IMMEDIATE) {
            self.record_play(&t.video_id.0);
        }
        match track {
            Some(t) => {
                self.state.set_title(t.title.clone());
                self.state.set_artist(t.artist.clone());
                self.state
                    .set_thumbnail_url(t.thumb.clone().unwrap_or_default());
                self.state.set_video_id(t.video_id.0.clone());
                self.state
                    .set_like_status(t.like_status.as_str().to_owned());
            }
            None => {
                self.state.set_title(String::new());
                self.state.set_artist(String::new());
                self.state.set_thumbnail_url(String::new());
                self.state.set_video_id(String::new());
                self.state.set_like_status("INDIFFERENT".to_owned());
                self.state.set_position(0.0);
                self.state.set_duration(0.0);
            }
        }
    }

    /// Whether this track is worth a lookup for its audio twin.
    ///
    /// Uploads have no counterpart, and a track that says it is already the
    /// audio version needs no lookup. Anything else is asked about once per
    /// session: the answer costs a request, and it does not change.
    fn wants_audio_version(&self, track: &Track) -> bool {
        if track.video_id.0.is_empty() || track.entity_id.is_some() {
            return false;
        }
        if track.video_type.as_deref() == Some(AUDIO_VIDEO_TYPE) {
            return false;
        }
        self.swap_checked.borrow_mut().insert(track.video_id.0.clone())
    }

    /// Put the audio twin in the playing slot, keeping what the queue entry
    /// already knew. Port of the swap block in _fetch_and_play: the id, the
    /// type, and the three display fields move over, the rest stays.
    fn apply_audio_version(&self, previous: &Track, twin: Track) -> Track {
        let mut swapped = previous.clone();
        swapped.video_id = twin.video_id.clone();
        swapped.video_type = Some(AUDIO_VIDEO_TYPE.to_owned());
        if !twin.title.is_empty() {
            swapped.title = twin.title;
        }
        if !twin.artist.is_empty() {
            swapped.artist = twin.artist;
            swapped.artists = twin.artists;
        }
        if twin.thumb.is_some() {
            swapped.thumb = twin.thumb;
        }
        tracing::info!(from = %previous.video_id, to = %swapped.video_id, title = %swapped.title, "playing the audio version");

        if !self.queue.borrow_mut().swap_current(&previous.video_id, swapped.clone()) {
            return swapped;
        }
        // This is the same play under a new id: the history entry the load
        // already recorded stands, so mark it before the metadata goes out.
        self.history_recorded.replace(Some(swapped.video_id.0.clone()));
        self.sync_queue_model();
        // Rows on the page the listener came from still hold the video's id.
        self.state.set_source_video_id(previous.video_id.0.clone());
        self.swapping.set(true);
        self.apply_track_metadata(Some(&swapped));
        self.swapping.set(false);
        self.emit_play(PlayEvent::Refined(swapped.clone()));
        swapped
    }

    /// Fill placeholder metadata from the resolver, like _fetch_and_play did.    /// Fill placeholder metadata from the resolver, like _fetch_and_play did.
    fn refine_metadata(
        &self,
        track: &Track,
        title: Option<&str>,
        artist: Option<&str>,
        thumb: Option<&str>,
    ) {
        let mut changed = track.clone();
        if changed.title.is_empty() || changed.title == "Loading..." {
            if let Some(t) = title {
                changed.title = t.to_owned();
            }
        }
        if changed.artist.is_empty() || changed.artist == "Unknown" {
            if let Some(a) = artist {
                changed.artist = a.to_owned();
            }
        }
        if changed.thumb.is_none() {
            changed.thumb = thumb.map(str::to_owned);
        }
        if changed == *track {
            return;
        }
        self.queue.borrow_mut().refine_current(&changed);
        self.apply_track_metadata(Some(&changed));
        self.emit_play(PlayEvent::Refined(changed));
    }

    /// Mirror the current index into the store and flip the `playing` flag on the affected rows.
    fn mark_current(&self, index: Option<usize>) {
        let value = index.map(|i| i as i32).unwrap_or(-1);
        if self.state.current_index() != value {
            self.state.set_current_index(value);
        }
        let paused = self.state.status() != PlaybackStatus::Playing;
        let model = self.state.queue_model();
        for i in 0..model.n_items() {
            if let Some(entry) = model.item(i).and_downcast::<QueueEntry>() {
                let playing = index == Some(i as usize);
                if entry.playing() != playing {
                    entry.set_playing(playing);
                }
                if entry.paused() != paused {
                    entry.set_paused(paused);
                }
            }
        }
    }

    /// Keep the playing row's pause indicator in step with the transport state.
    fn sync_paused_flag(&self) {
        let paused = self.state.status() != PlaybackStatus::Playing;
        let model = self.state.queue_model();
        for i in 0..model.n_items() {
            if let Some(entry) = model.item(i).and_downcast::<QueueEntry>() {
                if entry.paused() != paused {
                    entry.set_paused(paused);
                }
            }
        }
    }

    /// Rebuild the ListStore after a structural change.
    fn sync_queue_model(&self) {
        let paused = self.state.status() != PlaybackStatus::Playing;
        let (items, current) = {
            let q = self.queue.borrow();
            let items: Vec<QueueEntry> = q
                .tracks()
                .iter()
                .enumerate()
                .map(|(i, t)| QueueEntry::new(i as u32, t, Some(i) == q.current(), paused))
                .collect();
            (items, q.current())
        };
        let model = self.state.queue_model();
        model.splice(0, model.n_items(), &items);
        self.state.set_queue_length(items.len() as u32);
        self.mark_current(current);
        self.resync_gapless();
        self.state.emit_queue_changed();
    }

    fn weak_self(&self) -> Weak<Self> {
        self.me.clone()
    }
}

// -- playlist page support ------------------------------------------------

impl Player {
    /// The playlist or album the queue came from, what a page compares itself to.
    pub fn queue_source_id(&self) -> Option<String> {
        self.queue.borrow().source_id().map(str::to_owned)
    }

    pub fn queue_is_infinite(&self) -> bool {
        self.queue.borrow().is_infinite()
    }

    /// Port of add_history_item_async: tell the account this played, once per
    /// track, and put it at the top of the shared cache so the history page
    /// shows it before YouTube's own roll-up catches up.
    ///
    /// `history_mode` in the prefs both apps share decides when this runs:
    /// "immediate" as the track loads, "after_30s" once it has played that
    /// long, "never" not at all.
    fn record_play(&self, video_id: &str) {
        if video_id.is_empty() || self.history_mode.borrow().as_str() == HISTORY_NEVER {
            return;
        }
        if self.history_recorded.borrow().as_deref() == Some(video_id) {
            return;
        }
        if !matches!(self.net.client().auth_state(), crate::net::ytmusic::AuthState::Authenticated(_)) {
            return;
        }
        self.history_recorded.replace(Some(video_id.to_owned()));

        let api = self.net.client().api();
        let http = self.net.client().http().clone();
        let auth = self.net.client().media_auth();
        let downloads = self.downloads.clone();
        let video_id = video_id.to_owned();
        self.net.spawn(async move {
            match crate::net::history::record_play(&api, &http, auth.as_ref(), &video_id).await {
                Ok(track) => {
                    tracing::debug!(video_id = %track.video_id, "play recorded");
                    crate::net::history::prepend_cached(downloads.store(), &track);
                }
                Err(err) => tracing::warn!(%err, video_id, "recording the play failed"),
            }
        });
    }

    /// Port of Player.extend_queue: append at the end. Under shuffle the new
    /// tracks mix into the upcoming part, never into history or the current song.
    pub fn extend_queue(&self, tracks: Vec<Track>) {
        if tracks.is_empty() {
            return;
        }
        self.queue.borrow_mut().append(tracks);
        self.sync_queue_model();
    }

    /// Port of Player.play_then_radio: play a section, then keep going with a
    /// radio seeded from its last track.
    ///
    /// The queue is stamped with an id of its own so the reply can tell it is
    /// still the queue that asked; the real radio playlist replaces the stamp
    /// once the tracks land, and the infinite extender takes it from there.
    pub fn play_then_radio(self: &Rc<Self>, tracks: Vec<Track>, start_index: usize, seed: &str) {
        if tracks.is_empty() || seed.is_empty() {
            self.play_tracks(tracks, start_index, false, None, false);
            return;
        }
        let stamp = format!("home-radio:{seed}:{}", self.next_stamp());
        self.play_tracks(tracks, start_index, false, Some(stamp.clone()), false);

        let api = self.net.client().api();
        let seed = seed.to_owned();
        let handle = self.net.spawn(async move { crate::net::playlists::radio_tracks(&api, Some(&seed), None).await });
        let weak = Rc::downgrade(self);
        glib::spawn_future_local(async move {
            let watch = match handle.await {
                Ok(Ok(watch)) => watch,
                Ok(Err(err)) => {
                    tracing::warn!(%err, "section radio failed");
                    return;
                }
                Err(_) => return,
            };
            let Some(player) = weak.upgrade() else { return };
            // The listener has moved on; their queue is not ours to extend.
            if player.queue.borrow().source_id() != Some(stamp.as_str()) {
                return;
            }
            let existing: std::collections::HashSet<String> = player.queue.borrow().tracks().iter().map(|t| t.video_id.0.clone()).collect();
            let fresh: Vec<Track> = watch.tracks.into_iter().map(|t| t.track).filter(|t| !t.video_id.0.is_empty() && !existing.contains(&t.video_id.0)).collect();
            if !fresh.is_empty() {
                player.extend_queue(fresh);
            }
            if let Some(playlist_id) = watch.playlist_id {
                player.queue.borrow_mut().adopt_source(playlist_id, true);
            }
        });
    }

    /// A number that is not the last one, so two sections played in a row
    /// cannot share a stamp.
    fn next_stamp(&self) -> u64 {
        let next = self.stamp.get().wrapping_add(1);
        self.stamp.set(next);
        next
    }

    /// Port of Player.start_radio: fetch a mix for a song or playlist on the
    /// runtime and play it as an infinite queue sourced from the radio id.
    pub fn start_radio(self: &Rc<Self>, video_id: Option<String>, playlist_id: Option<String>) {
        let api = self.net.client().api();
        let handle = self.net.spawn(async move {
            crate::net::playlists::radio_tracks(&api, video_id.as_deref(), playlist_id.as_deref())
                .await
        });
        let weak = Rc::downgrade(self);
        glib::spawn_future_local(async move {
            match handle.await {
                Ok(Ok(watch)) => {
                    let tracks: Vec<Track> = watch.tracks.into_iter().map(|t| t.track).collect();
                    if tracks.is_empty() {
                        tracing::warn!("radio: no tracks returned");
                        return;
                    }
                    if let Some(player) = weak.upgrade() {
                        player.play_tracks(tracks, 0, false, watch.playlist_id, true);
                    }
                }
                Ok(Err(err)) => tracing::warn!(%err, "radio failed"),
                Err(_) => {}
            }
        });
    }
}

// -- infinite radio -------------------------------------------------------

impl Player {
    /// Port of _maybe_extend_infinite: fetch more radio tracks when fewer
    /// than fifteen remain or the play-head is past halfway.
    pub fn maybe_extend_infinite(&self) {
        let (infinite, has_source, current, len) = {
            let q = self.queue.borrow();
            (q.is_infinite(), q.source_id().is_some(), q.current(), q.len())
        };
        if !infinite || !has_source || self.infinite_fetching.get() {
            return;
        }
        let Some(current) = current else { return };
        if len == 0 || current >= len {
            return;
        }
        let remaining = len - 1 - current;
        if remaining <= 15 || current >= len / 2 {
            self.start_infinite_fetch();
        }
    }

    /// Port of _start_infinite_fetch: seed the watch playlist from the queue's
    /// tail, retry once from the playing track when the batch was all duplicates.
    fn start_infinite_fetch(&self) {
        self.infinite_fetching.set(true);
        let (last_vid, source, existing) = {
            let q = self.queue.borrow();
            (
                q.tracks().last().map(|t| t.video_id.0.clone()),
                q.source_id().map(str::to_owned),
                q.tracks().iter().map(|t| t.video_id.0.clone()).collect::<std::collections::HashSet<_>>(),
            )
        };
        // Queue-identity stamps with a colon are not playlist ids.
        let playlist_id = source.filter(|s| !s.contains(':'));
        let current_vid = Some(self.state.video_id()).filter(|v| !v.is_empty());
        let api = self.net.client().api();
        let handle = self.net.spawn(async move {
            let mut fresh = fetch_new_radio_tracks(
                &api,
                last_vid.as_deref(),
                playlist_id.as_deref(),
                &existing,
            )
            .await;
            if fresh.is_empty() {
                if let Some(cv) = current_vid.filter(|c| Some(c) != last_vid.as_ref()) {
                    fresh = fetch_new_radio_tracks(&api, Some(&cv), None, &existing).await;
                }
            }
            fresh
        });
        let weak = self.me.clone();
        glib::spawn_future_local(async move {
            let fresh = handle.await.unwrap_or_default();
            let Some(player) = weak.upgrade() else { return };
            player.infinite_fetching.set(false);
            if !fresh.is_empty() {
                player.extend_queue(fresh);
            }
        });
    }

    /// Port of _force_radio_extend: the queue ran dry on a radio. Seed from
    /// the playing track and accept repeats over silence.
    fn force_radio_extend(&self) {
        if self.infinite_fetching.get() {
            return;
        }
        self.infinite_fetching.set(true);
        let (last_vid, existing) = {
            let q = self.queue.borrow();
            (
                q.tracks().last().map(|t| t.video_id.0.clone()),
                q.tracks().iter().map(|t| t.video_id.0.clone()).collect::<std::collections::HashSet<_>>(),
            )
        };
        let seed = Some(self.state.video_id())
            .filter(|v| !v.is_empty())
            .or(last_vid);
        let Some(seed) = seed else {
            self.infinite_fetching.set(false);
            self.queue.borrow_mut().clear_current();
            self.apply(Step::Stop);
            return;
        };
        let api = self.net.client().api();
        let seed_c = seed.clone();
        let handle = self.net.spawn(async move {
            let all: Vec<Track> = crate::net::playlists::radio_tracks(&api, Some(&seed_c), None)
                .await
                .map(|w| w.tracks.into_iter().map(|t| t.track).collect())
                .unwrap_or_default();
            let mut fresh: Vec<Track> = all
                .iter()
                .filter(|t| !t.video_id.0.is_empty() && !existing.contains(&t.video_id.0))
                .cloned()
                .collect();
            if fresh.is_empty() {
                fresh = all
                    .into_iter()
                    .filter(|t| !t.video_id.0.is_empty() && t.video_id.0 != seed_c)
                    .collect();
            }
            fresh
        });
        let weak = self.me.clone();
        glib::spawn_future_local(async move {
            let fresh = handle.await.unwrap_or_default();
            let Some(player) = weak.upgrade() else { return };
            player.infinite_fetching.set(false);
            if fresh.is_empty() {
                player.queue.borrow_mut().clear_current();
                player.apply(Step::Stop);
                return;
            }
            let start = player.queue.borrow().len();
            player.extend_queue(fresh);
            let step = player.queue.borrow_mut().jump(start);
            player.apply(step);
        });
    }
}

/// Radio tracks for a seed minus the ids already queued.
async fn fetch_new_radio_tracks(
    api: &ytmusicapi::YTMusicClient,
    video_id: Option<&str>,
    playlist_id: Option<&str>,
    existing: &std::collections::HashSet<String>,
) -> Vec<Track> {
    match crate::net::playlists::radio_tracks(api, video_id, playlist_id).await {
        Ok(watch) => watch
            .tracks
            .into_iter()
            .map(|t| t.track)
            .filter(|t| !t.video_id.0.is_empty() && !existing.contains(&t.video_id.0))
            .collect(),
        Err(err) => {
            tracing::warn!(%err, "radio extension failed");
            Vec::new()
        }
    }
}

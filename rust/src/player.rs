//! Playback controller. Owns the queue and drives the audio thread.
//!
//! This is the Rust home of what Player (player.py) did on the GTK thread:
//! queue mutation, repeat and shuffle, the load generation counter, and the
//! reaction to audio events. It never blocks. Stream resolution runs on the
//! tokio runtime and comes back through `glib::spawn_future_local`.

use std::cell::{Cell, RefCell};
use std::rc::{Rc, Weak};

use gio::prelude::*;
use tokio::task::AbortHandle;

use crate::audio::{AudioCommand, AudioEvent, AudioEvents, AudioHandle, AudioTelemetry};
use crate::model::{LikeStatus, PlaybackStatus, RepeatMode, Track, VideoId};
use crate::net::NetHandle;
use crate::net::ytmusic::AuthState;
use crate::state::{PlayerState, QueueEntry};
use gtk::glib;

const STREAM_RETRY_MAX: u8 = 2;
const PREVIOUS_RESTART_THRESHOLD: f64 = 3.0;

/// Queue as plain data. Mutated only through `Player`, mirrored into the ListStore afterwards.
#[derive(Default)]
struct Queue {
    tracks: Vec<Track>,
    /// Unshuffled order, for turning shuffle back off.
    original: Vec<Track>,
    current: Option<usize>,
    shuffle: bool,
    repeat: RepeatMode,
    source_id: Option<String>,
    infinite: bool,
}

impl Queue {
    fn current_track(&self) -> Option<&Track> {
        self.current.and_then(|i| self.tracks.get(i))
    }

    /// Index that follows `current` under the repeat rules, or None at the end.
    fn next_index(&self) -> Option<usize> {
        let cur = self.current?;
        match self.repeat {
            RepeatMode::Track => Some(cur),
            _ if cur + 1 < self.tracks.len() => Some(cur + 1),
            RepeatMode::All if !self.tracks.is_empty() => Some(0),
            _ => None,
        }
    }
}

pub struct Player {
    me: Weak<Player>,
    state: PlayerState,
    queue: RefCell<Queue>,
    audio: AudioHandle,
    audio_events: RefCell<Option<AudioEvents>>,
    net: NetHandle,
    /// Allocator for load generations. Monotonic, never reused.
    counter: Cell<u64>,
    /// Generation of the stream the pipeline should be playing right now.
    current: Cell<u64>,
    /// (queue index, generation) pre-armed for gapless.
    armed_next: Cell<Option<(usize, u64)>>,
    inflight: RefCell<Option<AbortHandle>>,
    retries: Cell<u8>,
    /// Spectrum frames keyed by stream time, what _viz_queue held: released
    /// once the sink has reached them, so the bars follow the audible sound.
    viz_queue: RefCell<std::collections::VecDeque<(i64, Vec<f32>)>>,
    /// The last position tick and its arrival time, interpolated between ticks.
    position_mark: Cell<Option<(std::time::Instant, f64)>>,
    /// A radio extension is in flight, like _is_fetching_infinite.
    infinite_fetching: Cell<bool>,
}

impl Player {
    pub fn new(net: NetHandle, audio: AudioHandle, events: AudioEvents) -> Rc<Self> {
        Rc::new_cyclic(|me| Self {
            me: me.clone(),
            state: PlayerState::new(),
            queue: RefCell::new(Queue::default()),
            audio,
            audio_events: RefCell::new(Some(events)),
            net,
            counter: Cell::new(0),
            current: Cell::new(0),
            armed_next: Cell::new(None),
            inflight: RefCell::new(None),
            retries: Cell::new(0),
            viz_queue: RefCell::new(std::collections::VecDeque::new()),
            position_mark: Cell::new(None),
            infinite_fetching: Cell::new(false),
        })
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
        self.queue.borrow().tracks.clone()
    }

    pub fn repeat_mode(&self) -> RepeatMode {
        self.queue.borrow().repeat
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
                    .tracks
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
        {
            let mut guard = self.queue.borrow_mut();
            let q = &mut *guard;
            for t in q.tracks.iter_mut().chain(q.original.iter_mut()) {
                if t.video_id == *video_id {
                    t.like_status = status;
                }
            }
        }
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
            q.original = tracks.clone();
            q.tracks = tracks;
            q.shuffle = false;
            q.current = (start_index < q.tracks.len()).then_some(start_index);
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
        {
            let mut q = self.queue.borrow_mut();
            q.original = tracks.clone();
            q.tracks = tracks;
            q.shuffle = shuffle;
            q.source_id = source_id;
            q.infinite = infinite;
            if shuffle {
                // Play the chosen track first, shuffle everything behind it.
                let mut rest = q.tracks.clone();
                let first = if start_index < rest.len() {
                    Some(rest.remove(start_index))
                } else {
                    None
                };
                shuffle_in_place(&mut rest);
                if let Some(first) = first {
                    rest.insert(0, first);
                }
                q.tracks = rest;
                q.current = if q.tracks.is_empty() { None } else { Some(0) };
            } else {
                q.current = (start_index < q.tracks.len()).then_some(start_index);
            }
        }
        self.state.set_shuffle(shuffle);
        self.sync_queue_model();
        self.load_current();
    }

    pub fn add_to_queue(&self, tracks: Vec<Track>, play_next: bool) {
        if tracks.is_empty() {
            return;
        }
        let start_now = {
            let mut q = self.queue.borrow_mut();
            let pos = match (play_next, q.current) {
                (true, Some(cur)) => cur + 1,
                _ => q.tracks.len(),
            };
            let opos = pos.min(q.original.len());
            for (i, t) in tracks.into_iter().enumerate() {
                q.tracks.insert(pos + i, t.clone());
                let at = (opos + i).min(q.original.len());
                q.original.insert(at, t);
            }
            if q.current.is_none() {
                q.current = Some(0);
                true
            } else {
                false
            }
        };
        self.sync_queue_model();
        if start_now {
            self.load_current();
        }
    }

    pub fn remove_from_queue(&self, index: usize) {
        let reload = {
            let mut q = self.queue.borrow_mut();
            if index >= q.tracks.len() {
                return;
            }
            let removed = q.tracks.remove(index);
            if let Some(pos) = q.original.iter().position(|t| *t == removed) {
                q.original.remove(pos);
            }
            match q.current {
                Some(cur) if index < cur => {
                    q.current = Some(cur - 1);
                    false
                }
                Some(cur) if index == cur => {
                    if cur < q.tracks.len() {
                        true
                    } else {
                        q.current = None;
                        true
                    }
                }
                _ => false,
            }
        };
        self.sync_queue_model();
        if reload {
            self.load_current();
        }
    }

    pub fn move_queue_item(&self, old_index: usize, new_index: usize) -> bool {
        {
            let mut q = self.queue.borrow_mut();
            if old_index >= q.tracks.len() || new_index >= q.tracks.len() || old_index == new_index
            {
                return false;
            }
            let item = q.tracks.remove(old_index);
            let insert_at = if old_index < new_index {
                new_index - 1
            } else {
                new_index
            };
            q.tracks.insert(insert_at, item);
            q.current = q.current.map(|cur| {
                if cur == old_index {
                    insert_at
                } else if old_index < cur && cur <= insert_at {
                    cur - 1
                } else if insert_at <= cur && cur < old_index {
                    cur + 1
                } else {
                    cur
                }
            });
        }
        self.sync_queue_model();
        true
    }

    pub fn clear_queue(&self) {
        self.abort_inflight();
        // A fresh generation makes any in-flight resolution stale.
        self.current.set(self.alloc_generation());
        let repeat = self.queue.borrow().repeat;
        *self.queue.borrow_mut() = Queue {
            repeat,
            ..Queue::default()
        };
        self.audio.send(AudioCommand::Stop);
        self.state.set_status(PlaybackStatus::Stopped);
        self.state.set_shuffle(false);
        self.apply_track_metadata(None);
        self.sync_queue_model();
    }

    pub fn play_queue_index(&self, index: usize) {
        {
            let mut q = self.queue.borrow_mut();
            if index >= q.tracks.len() {
                return;
            }
            q.current = Some(index);
        }
        self.load_current();
        self.maybe_extend_infinite();
    }

    pub fn next(&self) {
        let target = {
            let q = self.queue.borrow();
            match q.current {
                Some(cur) if cur + 1 < q.tracks.len() => Some(cur + 1),
                Some(_) if q.repeat == RepeatMode::All && !q.tracks.is_empty() => Some(0),
                _ => None,
            }
        };
        match target {
            Some(i) => {
                self.queue.borrow_mut().current = Some(i);
                self.load_current();
                self.maybe_extend_infinite();
            }
            None => {
                let ran_out_on_radio = {
                    let q = self.queue.borrow();
                    q.infinite && q.source_id.is_some() && !q.tracks.is_empty()
                };
                if ran_out_on_radio {
                    // The halfway trigger normally hides this; a deduped batch can leave the queue dry.
                    self.force_radio_extend();
                    return;
                }
                self.queue.borrow_mut().current = None;
                self.stop();
                self.mark_current(None);
            }
        }
    }

    pub fn previous(&self) {
        if self.state.position() > PREVIOUS_RESTART_THRESHOLD {
            self.seek(0.0);
            return;
        }
        let target = {
            let q = self.queue.borrow();
            match q.current {
                Some(0) => Some(0),
                Some(cur) => Some(cur - 1),
                None => None,
            }
        };
        if let Some(i) = target {
            self.queue.borrow_mut().current = Some(i);
            self.load_current();
        }
    }

    pub fn toggle_shuffle(&self) {
        let now_shuffled = {
            let mut q = self.queue.borrow_mut();
            let playing = q.current_track().cloned();
            if q.shuffle {
                q.shuffle = false;
                q.tracks = q.original.clone();
                q.current = playing
                    .as_ref()
                    .and_then(|p| q.tracks.iter().position(|t| t == p))
                    .or(if q.tracks.is_empty() { None } else { Some(0) });
            } else {
                q.shuffle = true;
                let mut rest: Vec<Track> = q
                    .tracks
                    .iter()
                    .filter(|t| Some(*t) != playing.as_ref())
                    .cloned()
                    .collect();
                shuffle_in_place(&mut rest);
                if let Some(p) = playing {
                    rest.insert(0, p);
                    q.current = Some(0);
                } else {
                    q.current = None;
                }
                q.tracks = rest;
            }
            q.shuffle
        };
        self.state.set_shuffle(now_shuffled);
        self.sync_queue_model();
    }

    pub fn set_repeat(&self, mode: RepeatMode) {
        self.queue.borrow_mut().repeat = mode;
        self.state.set_repeat(mode);
        self.disarm_gapless();
    }

    // -- transport API ----------------------------------------------------

    pub fn play(&self) {
        if self.queue.borrow().current.is_none() {
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
            PlaybackStatus::Stopped if self.queue.borrow().current.is_some() => self.load_current(),
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
        self.state.set_position(seconds.max(0.0));
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
        if self.armed_next.take().is_some() {
            self.audio.send(AudioCommand::DisarmNext);
        }
    }

    /// Start playing the queue's current index under a new generation.
    fn load_current(&self) {
        // A new stream restarts stream time; queued frames would mislead the drain.
        self.clear_visualizer_queue();
        let (index, track) = {
            let q = self.queue.borrow();
            match q
                .current
                .and_then(|i| q.tracks.get(i).map(|t| (i, t.clone())))
            {
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

        self.mark_current(Some(index));
        self.state.set_status(PlaybackStatus::Loading);
        self.state.set_position(0.0);
        self.state
            .set_duration(track.duration_seconds.map(f64::from).unwrap_or(0.0));
        self.apply_track_metadata(Some(&track));
        tracing::debug!(generation, index, video_id = %track.video_id, "loading");

        self.spawn_resolve(track, generation, false);
    }

    /// Resolve `track` on the runtime; on success either load it or arm it for gapless.
    fn spawn_resolve(&self, track: Track, generation: u64, arm_only: bool) {
        let auth = self.net.client().media_auth();
        let resolver = self.net.resolver().clone();
        let video_id = track.video_id.clone();
        let resolve_auth = auth.clone();
        let handle = self
            .net
            .spawn(async move { resolver.resolve(video_id, resolve_auth).await });
        if !arm_only {
            *self.inflight.borrow_mut() = Some(handle.abort_handle());
        }

        let weak = self.weak_self();
        glib::spawn_future_local(async move {
            let outcome = handle.await;
            let Some(player) = weak.upgrade() else { return };
            let Ok(result) = outcome else { return };
            if arm_only {
                if let Ok(info) = result {
                    if player
                        .armed_next
                        .get()
                        .is_some_and(|(_, g)| g == generation)
                    {
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
            match result {
                Ok(info) => {
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
                        &err.to_string(),
                    );
                    player.advance_after_failure();
                }
            }
        });
    }

    fn advance_after_failure(&self) {
        let has_next = {
            let q = self.queue.borrow();
            q.current.is_some_and(|cur| cur + 1 < q.tracks.len())
        };
        if has_next {
            self.next();
        } else {
            self.queue.borrow_mut().current = None;
            self.stop();
            self.mark_current(None);
        }
    }

    /// Pre-resolve the following track and hand its URI to playbin for a gapless switch.
    fn arm_gapless(&self) {
        let next = {
            let q = self.queue.borrow();
            q.next_index()
                .and_then(|i| q.tracks.get(i).map(|t| (i, t.clone())))
        };
        let Some((index, track)) = next else { return };
        if track.is_upload() {
            return;
        }
        let generation = self.alloc_generation();
        self.armed_next.set(Some((index, generation)));
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
                if let Some((index, armed)) = self.armed_next.take() {
                    if armed == generation {
                        self.current.set(generation);
                        self.retries.set(0);
                        let track = {
                            let mut q = self.queue.borrow_mut();
                            q.current = Some(index);
                            q.tracks.get(index).cloned()
                        };
                        self.mark_current(Some(index));
                        self.state.set_position(0.0);
                        self.state.set_duration(
                            track
                                .as_ref()
                                .and_then(|t| t.duration_seconds)
                                .map(f64::from)
                                .unwrap_or(0.0),
                        );
                        self.apply_track_metadata(track.as_ref());
                    }
                }
            }
            AudioEvent::StateChanged { generation, status } => {
                if generation != self.current.get()
                    && !self.armed_next.get().is_some_and(|(_, g)| g == generation)
                {
                    return;
                }
                tracing::debug!(generation, ?status, "audio state");
                if status == PlaybackStatus::Playing
                    && self.state.status() != PlaybackStatus::Playing
                {
                    self.retries.set(0);
                    if self.armed_next.get().is_none() {
                        self.arm_gapless();
                    }
                }
                self.state.set_status(status);
                self.sync_paused_flag();
            }
            AudioEvent::Prerolled { .. } => {}
            AudioEvent::EndOfStream { generation } => {
                if generation != self.current.get() {
                    return;
                }
                let repeat_track = self.queue.borrow().repeat == RepeatMode::Track;
                if repeat_track {
                    self.load_current()
                } else {
                    self.next()
                }
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
                        .emit_track_error(track.video_id.as_str(), &track.title, &message);
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

    /// Fill placeholder metadata from the resolver, like _fetch_and_play did.
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
        {
            let mut q = self.queue.borrow_mut();
            if let Some(slot) = q.current.and_then(|i| q.tracks.get_mut(i)) {
                if slot.video_id == changed.video_id {
                    *slot = changed.clone();
                }
            }
        }
        self.apply_track_metadata(Some(&changed));
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
                .tracks
                .iter()
                .enumerate()
                .map(|(i, t)| QueueEntry::new(i as u32, t, Some(i) == q.current, paused))
                .collect();
            (items, q.current)
        };
        let model = self.state.queue_model();
        model.splice(0, model.n_items(), &items);
        self.state.set_queue_length(items.len() as u32);
        self.mark_current(current);
        self.disarm_gapless();
        self.state.emit_queue_changed();
    }

    fn weak_self(&self) -> Weak<Self> {
        self.me.clone()
    }
}

/// Fisher-Yates with a tiny xorshift source. Good enough for a play queue, no rand dependency.
fn shuffle_in_place(items: &mut [Track]) {
    let mut seed = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_nanos() as u64)
        .unwrap_or(0x9E37_79B9_7F4A_7C15)
        | 1;
    for i in (1..items.len()).rev() {
        seed ^= seed << 13;
        seed ^= seed >> 7;
        seed ^= seed << 17;
        let j = (seed % (i as u64 + 1)) as usize;
        items.swap(i, j);
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::model::VideoId;

    fn track(id: &str) -> Track {
        Track {
            video_id: VideoId(id.into()),
            title: id.into(),
            ..Track::default()
        }
    }

    #[test]
    fn next_index_follows_repeat_rules() {
        let mut q = Queue {
            tracks: vec![track("a"), track("b")],
            current: Some(1),
            ..Queue::default()
        };
        assert_eq!(q.next_index(), None);
        q.repeat = RepeatMode::All;
        assert_eq!(q.next_index(), Some(0));
        q.repeat = RepeatMode::Track;
        assert_eq!(q.next_index(), Some(1));
    }

    #[test]
    fn shuffle_keeps_every_track() {
        let mut items: Vec<Track> = (0..20).map(|i| track(&i.to_string())).collect();
        let before = items.clone();
        shuffle_in_place(&mut items);
        assert_eq!(items.len(), before.len());
        for t in &before {
            assert!(items.contains(t));
        }
    }
}

// -- playlist page support ------------------------------------------------

impl Player {
    /// The playlist or album the queue came from, what a page compares itself to.
    pub fn queue_source_id(&self) -> Option<String> {
        self.queue.borrow().source_id.clone()
    }

    pub fn queue_is_infinite(&self) -> bool {
        self.queue.borrow().infinite
    }

    /// Port of Player.extend_queue: append at the end. Under shuffle the new
    /// tracks mix into the upcoming part, never into history or the current song.
    pub fn extend_queue(&self, tracks: Vec<Track>) {
        if tracks.is_empty() {
            return;
        }
        {
            let mut q = self.queue.borrow_mut();
            q.original.extend(tracks.iter().cloned());
            if q.shuffle {
                match q.current {
                    Some(cur) if cur < q.tracks.len() => {
                        let mut upcoming = q.tracks.split_off(cur + 1);
                        upcoming.extend(tracks);
                        shuffle_in_place(&mut upcoming);
                        q.tracks.extend(upcoming);
                    }
                    _ => {
                        q.tracks.extend(tracks);
                        shuffle_in_place(&mut q.tracks);
                        if q.current.is_none() && !q.tracks.is_empty() {
                            q.current = Some(0);
                        }
                    }
                }
            } else {
                q.tracks.extend(tracks);
            }
        }
        self.sync_queue_model();
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
            (q.infinite, q.source_id.is_some(), q.current, q.tracks.len())
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
                q.tracks.last().map(|t| t.video_id.0.clone()),
                q.source_id.clone(),
                q.tracks
                    .iter()
                    .map(|t| t.video_id.0.clone())
                    .collect::<std::collections::HashSet<_>>(),
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
                q.tracks.last().map(|t| t.video_id.0.clone()),
                q.tracks
                    .iter()
                    .map(|t| t.video_id.0.clone())
                    .collect::<std::collections::HashSet<_>>(),
            )
        };
        let seed = Some(self.state.video_id())
            .filter(|v| !v.is_empty())
            .or(last_vid);
        let Some(seed) = seed else {
            self.infinite_fetching.set(false);
            self.queue.borrow_mut().current = None;
            self.stop();
            self.mark_current(None);
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
                player.queue.borrow_mut().current = None;
                player.stop();
                player.mark_current(None);
                return;
            }
            let start = player.queue.borrow().tracks.len();
            player.extend_queue(fresh);
            player.queue.borrow_mut().current = Some(start);
            player.load_current();
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

//! Audio engine: one dedicated thread that owns the playbin pipeline.
//!
//! The thread runs its own GLib main context. The GStreamer bus watch, the
//! 100 ms position ticker and the command receiver all live on that context,
//! so blocking calls such as `set_state(Null)` stall this loop and never the
//! GTK loop. Two callbacks run on GStreamer streaming threads (about-to-finish
//! and source-setup); they touch only `Arc<Mutex<_>>` slots and stay short.
//!
//! Commands flow in over an unbounded channel. Events flow out over two
//! channels: `control` (unbounded, lossless) and `telemetry` (bounded,
//! newest-wins). Every event is stamped with the load generation it belongs
//! to so the controller can drop anything stale.

use std::cell::Cell;
use std::rc::Rc;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex};
use std::thread::JoinHandle;
use std::time::Duration;

use async_channel::{Receiver, Sender};
use gst::prelude::*;
use gst_audio::prelude::*;

use crate::model::{HttpAuth, PlaybackStatus};

const TELEMETRY_CAPACITY: usize = 64;
const POSITION_TICK: Duration = Duration::from_millis(100);
const SPECTRUM_BANDS: u32 = 128;
const SPECTRUM_THRESHOLD_DB: i32 = -80;
const SPECTRUM_INTERVAL_NS: u64 = 33_000_000;

#[derive(Debug)]
pub enum AudioCommand {
    /// Tear down the current stream and start `uri`. `auth` rides along for souphttpsrc.
    Load { uri: String, generation: u64, auth: Option<HttpAuth> },
    /// Pre-set the next URI for gapless playback under a fresh generation.
    ArmNext { uri: String, generation: u64 },
    /// Forget any armed URI. Sent on every queue mutation.
    DisarmNext,
    Play,
    Pause,
    Stop,
    Seek { seconds: f64 },
    SetVolume(f64),
    SetMute(bool),
    Shutdown,
}

#[derive(Debug, Clone)]
pub enum AudioEvent {
    StateChanged { generation: u64, status: PlaybackStatus },
    /// The pipeline switched to an armed URI. `generation` is the armed one.
    StreamStarted { generation: u64 },
    /// Preroll finished; duration is now queryable.
    Prerolled { generation: u64 },
    EndOfStream { generation: u64 },
    Error { generation: u64, message: String, debug: Option<String> },
    /// External or internal volume change, in cubic (user-facing) scale.
    VolumeChanged { volume: f64, muted: bool },
}

#[derive(Debug, Clone)]
pub enum AudioTelemetry {
    Position { generation: u64, position: f64, duration: Option<f64> },
    /// `stream_time` is the position the frame belongs to, in ns, or -1 when the element did not tag it.
    Spectrum { generation: u64, stream_time: i64, bands: Vec<f32> },
}

/// Receivers handed to the controller exactly once.
pub struct AudioEvents {
    pub control: Receiver<AudioEvent>,
    pub telemetry: Receiver<AudioTelemetry>,
}

/// Cheap handle held by the controller on the GTK thread.
pub struct AudioHandle {
    cmd: Sender<AudioCommand>,
    thread: Mutex<Option<JoinHandle<()>>>,
}

impl AudioHandle {
    pub fn send(&self, cmd: AudioCommand) {
        if self.cmd.send_blocking(cmd).is_err() {
            tracing::warn!("audio thread is gone; command dropped");
        }
    }

    /// Ask the thread to stop and wait for it.
    pub fn shutdown(&self) {
        self.send(AudioCommand::Shutdown);
        if let Some(handle) = self.thread.lock().unwrap().take() {
            let _ = handle.join();
        }
    }
}

/// Spawn the audio thread. Call after `gst::init()`.
pub fn spawn() -> anyhow::Result<(AudioHandle, AudioEvents)> {
    let (cmd_tx, cmd_rx) = async_channel::unbounded::<AudioCommand>();
    let (control_tx, control_rx) = async_channel::unbounded::<AudioEvent>();
    let (telemetry_tx, telemetry_rx) = async_channel::bounded::<AudioTelemetry>(TELEMETRY_CAPACITY);
    let (ready_tx, ready_rx) = std::sync::mpsc::channel::<anyhow::Result<()>>();

    let thread = std::thread::Builder::new()
        .name("mixtapes-audio".into())
        .spawn(move || run_loop(cmd_rx, control_tx, telemetry_tx, ready_tx))?;

    ready_rx.recv().unwrap_or_else(|_| Err(anyhow::anyhow!("audio thread died during setup")))?;

    Ok((
        AudioHandle { cmd: cmd_tx, thread: Mutex::new(Some(thread)) },
        AudioEvents { control: control_rx, telemetry: telemetry_rx },
    ))
}

/// State shared with GStreamer streaming-thread callbacks.
struct Shared {
    generation: AtomicU64,
    armed_next: Mutex<Option<(String, u64)>>,
    pending_gapless: Mutex<Option<u64>>,
    http_auth: Mutex<Option<HttpAuth>>,
}

struct Engine {
    playbin: gst::Element,
    shared: Arc<Shared>,
    control: Sender<AudioEvent>,
    telemetry: Sender<AudioTelemetry>,
    loading: Cell<bool>,
    last_status: Cell<PlaybackStatus>,
    main_loop: glib::MainLoop,
}

fn run_loop(
    cmd_rx: Receiver<AudioCommand>,
    control: Sender<AudioEvent>,
    telemetry: Sender<AudioTelemetry>,
    ready: std::sync::mpsc::Sender<anyhow::Result<()>>,
) {
    let ctx = glib::MainContext::new();
    let main_loop = glib::MainLoop::new(Some(&ctx), false);

    let setup = ctx.with_thread_default(|| -> anyhow::Result<(Rc<Engine>, gst::bus::BusWatchGuard)> {
        let engine = Rc::new(Engine::build(control, telemetry, main_loop.clone())?);

        let bus = engine.playbin.bus().expect("playbin has a bus");
        let watch = {
            let engine = engine.clone();
            bus.add_watch_local(move |_, msg| {
                engine.on_message(msg);
                glib::ControlFlow::Continue
            })?
        };

        // timeout_add_local targets the global default context, which is the GTK
        // thread. Build the source by hand and attach it to this thread's context.
        {
            let engine = glib::thread_guard::ThreadGuard::new(engine.clone());
            let ticker = glib::timeout_source_new(POSITION_TICK, Some("mixtapes-position-tick"), glib::Priority::DEFAULT, move || {
                engine.get_ref().tick();
                glib::ControlFlow::Continue
            });
            ticker.attach(Some(&ctx));
        }

        {
            let engine = engine.clone();
            ctx.spawn_local(async move {
                while let Ok(cmd) = cmd_rx.recv().await {
                    if engine.handle(cmd).is_break() {
                        break;
                    }
                }
                engine.main_loop.quit();
            });
        }

        Ok((engine, watch))
    });

    let (engine, _watch) = match setup {
        Ok(Ok(pair)) => {
            let _ = ready.send(Ok(()));
            pair
        }
        Ok(Err(err)) => {
            let _ = ready.send(Err(err));
            return;
        }
        Err(err) => {
            let _ = ready.send(Err(anyhow::anyhow!("could not acquire audio main context: {err}")));
            return;
        }
    };

    engine.emit(AudioEvent::VolumeChanged { volume: cubic_volume(&engine.playbin), muted: engine.playbin.property::<bool>("mute") });
    main_loop.run();
    let _ = engine.playbin.set_state(gst::State::Null);
    tracing::info!("audio thread exited");
}

impl Engine {
    fn build(control: Sender<AudioEvent>, telemetry: Sender<AudioTelemetry>, main_loop: glib::MainLoop) -> anyhow::Result<Self> {
        let playbin = gst::ElementFactory::make("playbin").name("player").build()?;

        // Audio only: clear the video flag so no window ever pops up.
        let flags = playbin.property_value("flags");
        if let Some(class) = glib::FlagsClass::with_type(flags.type_()) {
            if let Some(value) = class.builder_with_value(flags).and_then(|b| b.unset_by_nick("video").unset_by_nick("text").build()) {
                playbin.set_property_from_value("flags", &value);
            }
        }

        // Passthrough spectrum analyzer feeding the visualizer.
        match gst::ElementFactory::make("spectrum")
            .name("visualizer-spectrum")
            .property("post-messages", true)
            .property("message-magnitude", true)
            .property("message-phase", false)
            .property("interval", SPECTRUM_INTERVAL_NS)
            .property("bands", SPECTRUM_BANDS)
            .property("threshold", SPECTRUM_THRESHOLD_DB)
            .property("multi-channel", false)
            .build()
        {
            Ok(spectrum) => playbin.set_property("audio-filter", &spectrum),
            Err(err) => tracing::warn!(%err, "spectrum element unavailable; visualizer stays inert"),
        }

        let shared = Arc::new(Shared {
            generation: AtomicU64::new(0),
            armed_next: Mutex::new(None),
            pending_gapless: Mutex::new(None),
            http_auth: Mutex::new(None),
        });

        // Streaming thread: push cookies and UA onto every HTTP request the source makes.
        {
            let shared = shared.clone();
            playbin.connect("source-setup", false, move |values| {
                if let Ok(source) = values[1].get::<gst::Element>() {
                    apply_http_auth(&source, shared.http_auth.lock().unwrap().as_ref());
                }
                None
            });
        }

        // Streaming thread: hand playbin the armed URI so the switch is gapless.
        {
            let shared = shared.clone();
            let control = control.clone();
            playbin.connect("about-to-finish", false, move |values| {
                let Ok(playbin) = values[0].get::<gst::Element>() else { return None };
                let armed = shared.armed_next.lock().unwrap().take();
                if let Some((uri, generation)) = armed {
                    playbin.set_property("uri", &uri);
                    *shared.pending_gapless.lock().unwrap() = Some(generation);
                    tracing::debug!(generation, "gapless uri handed to playbin");
                    let _ = control.send_blocking(AudioEvent::StateChanged { generation, status: PlaybackStatus::Loading });
                }
                None
            });
        }

        // Volume changes from the system mixer arrive here on arbitrary threads.
        {
            let control = control.clone();
            let notify = move |obj: &gst::Element, _: &glib::ParamSpec| {
                let _ = control.send_blocking(AudioEvent::VolumeChanged { volume: cubic_volume(obj), muted: obj.property::<bool>("mute") });
            };
            playbin.connect_notify(Some("volume"), notify.clone());
            playbin.connect_notify(Some("mute"), notify);
        }

        Ok(Self {
            playbin,
            shared,
            control,
            telemetry,
            loading: Cell::new(false),
            last_status: Cell::new(PlaybackStatus::Stopped),
            main_loop,
        })
    }

    fn generation(&self) -> u64 {
        self.shared.generation.load(Ordering::Acquire)
    }

    fn emit(&self, event: AudioEvent) {
        let _ = self.control.send_blocking(event);
    }

    fn set_status(&self, status: PlaybackStatus) {
        if self.last_status.replace(status) != status {
            self.emit(AudioEvent::StateChanged { generation: self.generation(), status });
        }
    }

    fn handle(&self, cmd: AudioCommand) -> glib::ControlFlow {
        match cmd {
            AudioCommand::Load { uri, generation, auth } => {
                self.shared.generation.store(generation, Ordering::Release);
                *self.shared.armed_next.lock().unwrap() = None;
                *self.shared.pending_gapless.lock().unwrap() = None;
                *self.shared.http_auth.lock().unwrap() = auth;
                self.loading.set(true);
                self.last_status.set(PlaybackStatus::Loading);
                self.emit(AudioEvent::StateChanged { generation, status: PlaybackStatus::Loading });
                // Null flushes the bus, so no message from the old stream survives this point.
                let _ = self.playbin.set_state(gst::State::Null);
                self.playbin.set_property("uri", &uri);
                if let Err(err) = self.playbin.set_state(gst::State::Playing) {
                    self.loading.set(false);
                    self.emit(AudioEvent::Error { generation, message: format!("could not start playback: {err}"), debug: None });
                    self.set_status(PlaybackStatus::Stopped);
                }
            }
            AudioCommand::ArmNext { uri, generation } => {
                *self.shared.armed_next.lock().unwrap() = Some((uri, generation));
            }
            AudioCommand::DisarmNext => {
                *self.shared.armed_next.lock().unwrap() = None;
            }
            AudioCommand::Play => {
                let _ = self.playbin.set_state(gst::State::Playing);
            }
            AudioCommand::Pause => {
                let _ = self.playbin.set_state(gst::State::Paused);
            }
            AudioCommand::Stop => {
                *self.shared.armed_next.lock().unwrap() = None;
                *self.shared.pending_gapless.lock().unwrap() = None;
                self.loading.set(false);
                let _ = self.playbin.set_state(gst::State::Null);
                self.set_status(PlaybackStatus::Stopped);
            }
            AudioCommand::Seek { seconds } => {
                let target = gst::ClockTime::from_seconds_f64(seconds.max(0.0));
                // Accurate first, like Player.seek. A key-unit seek lands on the
                // previous keyframe or cluster, which is the jump back the user sees.
                if self.playbin.seek_simple(gst::SeekFlags::FLUSH | gst::SeekFlags::ACCURATE, target).is_err() {
                    if let Err(err) = self.playbin.seek_simple(gst::SeekFlags::FLUSH | gst::SeekFlags::KEY_UNIT, target) {
                        tracing::warn!(%err, seconds, "seek rejected by pipeline");
                    }
                }
            }
            AudioCommand::SetVolume(v) => {
                if let Some(sv) = self.playbin.dynamic_cast_ref::<gst_audio::StreamVolume>() {
                    sv.set_volume(gst_audio::StreamVolumeFormat::Cubic, v.clamp(0.0, 1.0));
                }
            }
            AudioCommand::SetMute(m) => {
                self.playbin.set_property("mute", m);
            }
            AudioCommand::Shutdown => return glib::ControlFlow::Break,
        }
        glib::ControlFlow::Continue
    }

    fn on_message(&self, msg: &gst::Message) {
        use gst::MessageView;
        let from_playbin = msg.src().map(|s| s == self.playbin.upcast_ref::<gst::Object>()).unwrap_or(false);
        match msg.view() {
            MessageView::StreamStart(_) => {
                let pending = self.shared.pending_gapless.lock().unwrap().take();
                if let Some(generation) = pending {
                    self.shared.generation.store(generation, Ordering::Release);
                    self.loading.set(false);
                    self.last_status.set(PlaybackStatus::Playing);
                    self.emit(AudioEvent::StreamStarted { generation });
                    self.emit(AudioEvent::StateChanged { generation, status: PlaybackStatus::Playing });
                }
            }
            MessageView::AsyncDone(_) => {
                self.emit(AudioEvent::Prerolled { generation: self.generation() });
            }
            MessageView::Eos(_) => {
                if self.loading.get() {
                    tracing::debug!("EOS during load ignored");
                    return;
                }
                self.emit(AudioEvent::EndOfStream { generation: self.generation() });
            }
            MessageView::Error(err) => {
                let generation = self.generation();
                let message = err.error().to_string();
                let debug = err.debug().map(|d| d.to_string());
                tracing::warn!(generation, %message, "pipeline error");
                self.loading.set(false);
                let _ = self.playbin.set_state(gst::State::Null);
                self.emit(AudioEvent::Error { generation, message, debug });
                self.set_status(PlaybackStatus::Stopped);
            }
            MessageView::StateChanged(s) if from_playbin => match s.current() {
                gst::State::Playing => {
                    self.loading.set(false);
                    self.set_status(PlaybackStatus::Playing);
                }
                // Preroll passes through Paused; only report it once loading is over.
                gst::State::Paused if !self.loading.get() => self.set_status(PlaybackStatus::Paused),
                gst::State::Null | gst::State::Ready if !self.loading.get() => self.set_status(PlaybackStatus::Stopped),
                _ => {}
            },
            MessageView::Element(e) => {
                if let Some(s) = e.structure().filter(|s| s.name().as_str() == "spectrum") {
                    if let Ok(list) = s.get::<gst::List>("magnitude") {
                        let bands: Vec<f32> = list.iter().filter_map(|v| v.get::<f32>().ok()).collect();
                        let stream_time = s.get::<gst::ClockTime>("stream-time").ok().map(|t| t.nseconds() as i64).unwrap_or(-1);
                        let _ = self.telemetry.force_send(AudioTelemetry::Spectrum { generation: self.generation(), stream_time, bands });
                    }
                }
            }
            _ => {}
        }
    }

    /// 100 ms ticker: publish position and duration while a stream is up.
    fn tick(&self) {
        if self.loading.get() {
            return;
        }
        let (_, state, _) = self.playbin.state(gst::ClockTime::ZERO);
        if !matches!(state, gst::State::Playing | gst::State::Paused) {
            return;
        }
        let Some(position) = self.playbin.query_position::<gst::ClockTime>() else { return };
        // Some upload streams report a zero duration; treat that as unknown.
        let duration = self.playbin.query_duration::<gst::ClockTime>().filter(|d| !d.is_zero()).map(|d| d.seconds_f64());
        let _ = self.telemetry.force_send(AudioTelemetry::Position { generation: self.generation(), position: position.seconds_f64(), duration });
    }
}

fn cubic_volume(playbin: &gst::Element) -> f64 {
    playbin
        .dynamic_cast_ref::<gst_audio::StreamVolume>()
        .map(|sv| sv.volume(gst_audio::StreamVolumeFormat::Cubic))
        .unwrap_or_else(|| playbin.property::<f64>("volume"))
}

/// Only HTTP sources carry these properties; filesrc has none of them.
fn apply_http_auth(source: &gst::Element, auth: Option<&HttpAuth>) {
    let Some(auth) = auth else { return };
    let factory = source.factory().map(|f| f.name().to_string()).unwrap_or_default();
    if factory != "souphttpsrc" && factory != "curlhttpsrc" {
        return;
    }
    if source.find_property("user-agent").is_some() {
        source.set_property("user-agent", &auth.user_agent);
    }
    if source.find_property("extra-headers").is_some() {
        let mut headers = gst::Structure::builder("extra-headers").field("Cookie", auth.cookie.as_str());
        if let Some(authorization) = &auth.authorization {
            headers = headers.field("Authorization", authorization.as_str());
        }
        source.set_property("extra-headers", headers.build());
    }
}

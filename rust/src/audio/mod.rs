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
    /// Report what the pipeline is doing, for the Stream Info panel. The
    /// answer goes back on `reply` because only this thread may ask playbin.
    Describe { reply: async_channel::Sender<String> },
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
    /// The sink playbin plays into, held so its remembered device can be cleared.
    audio_sink: Option<gst::Element>,
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

/// Fragmented m4a, the only format upload-locker tracks come in, cannot seek
/// while qtdemux is fed in push mode ("ignoring seek in push mode"). With the
/// download flag queue2 keeps the stream in a temp file and the demuxer pulls
/// from it, which makes every position reachable. Opus in WebM seeks fine
/// without it, so those streams stay off the disk.
fn set_download_buffering(playbin: &gst::Element, uri: &str) {
    let wanted = uri.starts_with("http") && uri.contains("mime=audio%2Fmp4");
    let flags = playbin.property_value("flags");
    let Some(class) = glib::FlagsClass::with_type(flags.type_()) else { return };
    let Some(builder) = class.builder_with_value(flags) else { return };
    let builder = if wanted { builder.set_by_nick("download") } else { builder.unset_by_nick("download") };
    if let Some(value) = builder.build() {
        playbin.set_property_from_value("flags", &value);
    }
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

        // Our own sink, so `clear_remembered_device` has something to reach.
        // What playbin would have built on its own is this same element.
        let audio_sink = match gst::ElementFactory::make("autoaudiosink").name("audio-sink").build() {
            Ok(sink) => {
                playbin.set_property("audio-sink", &sink);
                Some(sink)
            }
            Err(err) => {
                tracing::warn!(%err, "no autoaudiosink; playbin picks its own and the output device cannot be reset");
                None
            }
        };

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
            playbin.connect("about-to-finish", false, move |values| {
                let Ok(playbin) = values[0].get::<gst::Element>() else { return None };
                let armed = shared.armed_next.lock().unwrap().take();
                if let Some((uri, generation)) = armed {
                    // The pipeline plays the tail of the current stream for about
                    // a second after this fires. Reporting Loading here froze the
                    // slider and stopped the visualizer for that whole stretch,
                    // with audio still running. The switch reports itself through
                    // StreamStart instead.
                    set_download_buffering(&playbin, &uri);
                    playbin.set_property("uri", &uri);
                    *shared.pending_gapless.lock().unwrap() = Some(generation);
                    tracing::debug!(generation, "gapless uri handed to playbin");
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
            audio_sink,
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

    /// Live pipeline state, queried at call time.
    ///
    /// The seeking query is the telling one: when its end is below the
    /// position, or the source cannot do byte ranges, that is why a seek is
    /// refused even with seekable true.
    fn describe(&self) -> String {
        let mut lines = Vec::new();
        let (_, state, _) = self.playbin.state(gst::ClockTime::ZERO);
        lines.push(format!("State:      {state:?}"));
        let source: Option<gst::Element> = self.playbin.property("source");
        let factory = source.and_then(|s| s.factory().map(|f| f.name().to_string()));
        lines.push(format!("Src elem:   {}", factory.unwrap_or_else(|| "unknown".to_owned())));
        let position = self.playbin.query_position::<gst::ClockTime>();
        let duration = self.playbin.query_duration::<gst::ClockTime>();
        lines.push(format!("Position:   {}", clock(position)));
        lines.push(format!("Duration:   {}", clock(duration)));
        let mut seeking = gst::query::Seeking::new(gst::Format::Time);
        if self.playbin.query(&mut seeking) {
            let (seekable, start, end) = seeking.result();
            lines.push(format!("Seekable:   {seekable}"));
            let span = |value: gst::GenericFormattedValue| match value {
                gst::GenericFormattedValue::Time(t) => clock(t),
                _ => "unknown".to_owned(),
            };
            lines.push(format!("Seek range: {} to {}", span(start), span(end)));
        } else {
            lines.push("Seekable:   query failed".to_owned());
        }
        lines.join("\n")
    }

    /// Forget which output device the sink last played to, so the next stream
    /// asks for the default again.
    ///
    /// `pulsesink` fills its own `device` property in with the sink it landed
    /// on, and from then on connects there explicitly. PipeWire never moves a
    /// stream that names its device, so plugging in headphones moved every
    /// other app across and left this one on the speakers until it was
    /// restarted. A stream that asks for the default is moved with the rest.
    fn clear_remembered_device(&self) {
        if let Some(sink) = &self.audio_sink {
            clear_device(sink);
        }
    }

    fn handle(&self, cmd: AudioCommand) -> glib::ControlFlow {
        match cmd {
            AudioCommand::Load { uri, generation, auth } => {
                tracing::debug!(generation, "audio: load");
                self.shared.generation.store(generation, Ordering::Release);
                *self.shared.armed_next.lock().unwrap() = None;
                *self.shared.pending_gapless.lock().unwrap() = None;
                *self.shared.http_auth.lock().unwrap() = auth;
                self.loading.set(true);
                self.last_status.set(PlaybackStatus::Loading);
                self.emit(AudioEvent::StateChanged { generation, status: PlaybackStatus::Loading });
                // Null flushes the bus, so no message from the old stream survives this point.
                let _ = self.playbin.set_state(gst::State::Null);
                self.clear_remembered_device();
                set_download_buffering(&self.playbin, &uri);
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
                tracing::debug!(generation = self.generation(), "audio: stop");
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
            AudioCommand::Describe { reply } => {
                let _ = reply.send_blocking(self.describe());
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

/// Reset every string `device` property under `sink`, so the next stream asks
/// for the default output rather than the one it last used.
///
/// Only string properties: an integer `device` on some sinks names a card,
/// which is not ours to reset.
fn clear_device(sink: &gst::Element) {
    let Some(bin) = sink.dynamic_cast_ref::<gst::Bin>() else { return };
    let mut iter = bin.iterate_recurse();
    while let Ok(Some(element)) = iter.next() {
        let Some(spec) = element.find_property("device") else { continue };
        if spec.value_type() != glib::Type::STRING || !spec.flags().contains(glib::ParamFlags::WRITABLE) {
            continue;
        }
        if let Some(device) = element.property::<Option<String>>("device") {
            tracing::debug!(element = %element.name(), device, "clearing the sink's remembered device");
            element.set_property("device", None::<String>);
        }
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

/// A pipeline time as minutes and seconds, or unknown.
fn clock(time: Option<gst::ClockTime>) -> String {
    match time {
        Some(t) => {
            let seconds = t.seconds_f64();
            format!("{}:{:02} ({seconds:.1}s)", seconds as u64 / 60, seconds as u64 % 60)
        }
        None => "unknown".to_owned(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A sink that has played once carries the device it landed on and
    /// connects there explicitly from then on, which is what kept playback on
    /// the speakers after headphones were plugged in.
    #[test]
    fn clearing_the_sink_forgets_the_device_it_last_used() {
        if gst::init().is_err() {
            println!("no gstreamer");
            return;
        }
        let Ok(sink) = gst::ElementFactory::make("autoaudiosink").build() else {
            println!("no autoaudiosink");
            return;
        };
        // READY is where autoaudiosink picks and builds the real sink.
        if sink.set_state(gst::State::Ready).is_err() {
            println!("no audio output available");
            let _ = sink.set_state(gst::State::Null);
            return;
        }
        let bin = sink.dynamic_cast_ref::<gst::Bin>().expect("autoaudiosink is a bin");
        let mut iter = bin.iterate_recurse();
        let mut pinned = Vec::new();
        while let Ok(Some(element)) = iter.next() {
            let Some(spec) = element.find_property("device") else { continue };
            if spec.value_type() == glib::Type::STRING && spec.flags().contains(glib::ParamFlags::WRITABLE) {
                element.set_property("device", "some-device-it-played-to");
                pinned.push(element);
            }
        }
        if pinned.is_empty() {
            println!("this sink names no device");
            let _ = sink.set_state(gst::State::Null);
            return;
        }

        clear_device(&sink);

        for element in &pinned {
            assert_eq!(element.property::<Option<String>>("device"), None, "{} still names a device", element.name());
        }
        let _ = sink.set_state(gst::State::Null);
    }
}

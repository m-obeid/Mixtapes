//! Discord Rich Presence over the raw IPC protocol. Port of player/discord_rpc.py.
//!
//! All socket I/O is pinned to one worker thread. The GTK thread builds the
//! activity payload from `PlayerState` and the shared prefs, then hands it over.

use std::io::{Read, Write};
use std::path::PathBuf;
use std::sync::mpsc::{Receiver, RecvTimeoutError, Sender, channel};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

use serde_json::{Map, Value, json};

use crate::model::PlaybackStatus;

const DISCORD_APP_ID: &str = "1492500060087255231";
/// Reconnect delays in seconds while the IPC socket is unreachable. The tail
/// is short so opening Discord later lights presence up within half a minute.
const RECONNECT_BACKOFF: [u64; 5] = [3, 5, 10, 15, 30];
/// Below Discord's limit of about five updates per 20 seconds.
const MIN_UPDATE_INTERVAL: Duration = Duration::from_millis(400);
const IO_TIMEOUT: Duration = Duration::from_secs(5);
const MIXTAPES_LOGO: &str = "https://raw.githubusercontent.com/m-obeid/Mixtapes/main/screenshots/omori-mixtape.png";

const OP_HANDSHAKE: u32 = 0;
const OP_FRAME: u32 = 1;

/// Keys of the `discord_rpc_status_display` pref, in the order the settings row lists them.
pub const STATUS_DISPLAY_KEYS: [&str; 3] = ["app_name", "artist", "song_title"];
pub const STATUS_DISPLAY_DEFAULT: &str = "artist";

fn status_display_type(key: &str) -> u8 {
    match key {
        "app_name" => 0,
        "song_title" => 2,
        _ => 1,
    }
}

/// What the activity is built from, read off the player on the GTK thread.
#[derive(Clone, Debug, Default)]
pub struct Snapshot {
    pub status: PlaybackStatus,
    pub has_track: bool,
    pub title: String,
    pub artist: String,
    pub album: String,
    pub thumb: String,
    pub position: f64,
    pub duration: f64,
}

/// The four Discord prefs, read from prefs.json at build time like Python did.
#[derive(Clone, Debug)]
pub struct Options {
    pub status_display: String,
    pub small_icon: bool,
    pub hide_on_pause: bool,
}

impl Options {
    pub fn read(prefs: &Map<String, Value>) -> Self {
        Self {
            status_display: prefs.get("discord_rpc_status_display").and_then(Value::as_str).unwrap_or(STATUS_DISPLAY_DEFAULT).to_owned(),
            small_icon: prefs.get("discord_rpc_small_icon_enabled").and_then(Value::as_bool).unwrap_or(true),
            hide_on_pause: prefs.get("discord_rpc_hide_pause_enabled").and_then(Value::as_bool).unwrap_or(false),
        }
    }
}

pub fn enabled_pref(prefs: &Map<String, Value>) -> bool {
    prefs.get("discord_rpc_enabled").and_then(Value::as_bool).unwrap_or(true)
}

/// Discord wants 2 to 128 characters.
fn field(text: &str) -> String {
    let mut out: String = text.chars().take(128).collect();
    if out.chars().count() < 2 {
        out.push(' ');
    }
    out
}

/// Port of _build_activity. None clears the presence.
pub fn build_activity(snap: &Snapshot, options: &Options, now_ms: i64) -> Option<Value> {
    let playing = snap.status == PlaybackStatus::Playing;
    let paused = snap.status == PlaybackStatus::Paused;
    if snap.status == PlaybackStatus::Stopped || (options.hide_on_pause && paused) || !snap.has_track {
        return None;
    }
    // Translators: stands in for a missing track title in the Discord status.
    let title = if snap.title.is_empty() { tr!("Unknown") } else { snap.title.clone() };
    let artist = if snap.artist.is_empty() { tr!("Unknown artist") } else { snap.artist.clone() };
    let (title, artist) = (title.as_str(), artist.as_str());

    let mut assets = if snap.thumb.starts_with("http") {
        let text = [snap.album.as_str(), title].into_iter().find(|t| !t.is_empty()).unwrap_or("Mixtapes");
        json!({"large_image": snap.thumb, "large_text": text.chars().take(128).collect::<String>()})
    } else {
        json!({"large_image": MIXTAPES_LOGO, "large_text": "Mixtapes"})
    };
    if options.small_icon {
        // The icon mirrors the transport button, like the Python app.
        assets["small_image"] = json!(if playing { "pause" } else { "play" });
        assets["small_text"] = json!(if playing { tr!("Playing") } else { tr!("Paused") });
    }

    let mut activity = json!({
        "details": field(title),
        "state": field(artist),
        "type": 2,
        "status_display_type": status_display_type(&options.status_display),
        "assets": assets,
    });
    if playing {
        let start = now_ms - (snap.position.max(0.0) * 1000.0) as i64;
        let mut timestamps = json!({"start": start});
        if snap.duration > 0.0 {
            timestamps["end"] = json!(start + (snap.duration * 1000.0) as i64);
        }
        activity["timestamps"] = timestamps;
    }
    Some(activity)
}

pub fn now_ms() -> i64 {
    SystemTime::now().duration_since(UNIX_EPOCH).map(|d| d.as_millis() as i64).unwrap_or(0)
}

/// The IPC connection: a Unix socket, or on Windows the named pipe Discord
/// listens on, which opens like a file. The frames are the same on both.
#[cfg(unix)]
type IpcStream = std::os::unix::net::UnixStream;
#[cfg(windows)]
type IpcStream = std::fs::File;

#[cfg(unix)]
fn open_ipc(path: &std::path::Path) -> std::io::Result<IpcStream> {
    let stream = IpcStream::connect(path)?;
    stream.set_read_timeout(Some(IO_TIMEOUT))?;
    stream.set_write_timeout(Some(IO_TIMEOUT))?;
    Ok(stream)
}

/// A pipe has no timeouts to set, so `wait_for_reply` bounds each read instead.
#[cfg(windows)]
fn open_ipc(path: &std::path::Path) -> std::io::Result<IpcStream> {
    std::fs::OpenOptions::new().read(true).write(true).open(path)
}

/// Wait until a whole frame header sits in the pipe, or give up after
/// IO_TIMEOUT. A blocking read on a pipe never times out, and one reply that
/// never came used to stall the worker for the rest of the session.
#[cfg(windows)]
fn wait_for_reply(stream: &IpcStream) -> std::io::Result<()> {
    use std::os::windows::io::AsRawHandle;
    use windows::Win32::Foundation::HANDLE;
    use windows::Win32::System::Pipes::PeekNamedPipe;
    let deadline = Instant::now() + IO_TIMEOUT;
    loop {
        let mut available = 0u32;
        // SAFETY: the handle belongs to `stream`, which outlives the call; only the count is written.
        unsafe { PeekNamedPipe(HANDLE(stream.as_raw_handle()), None, 0, None, Some(&mut available), None) }.map_err(std::io::Error::other)?;
        if available >= 8 {
            return Ok(());
        }
        if Instant::now() >= deadline {
            return Err(std::io::Error::new(std::io::ErrorKind::TimedOut, "discord did not answer"));
        }
        std::thread::sleep(Duration::from_millis(20));
    }
}

/// The socket's own read timeout covers this on Unix.
#[cfg(unix)]
fn wait_for_reply(_stream: &IpcStream) -> std::io::Result<()> {
    Ok(())
}

#[cfg(windows)]
fn candidate_ipc_paths() -> Vec<PathBuf> {
    (0..10).map(|i| PathBuf::from(format!(r"\\.\pipe\discord-ipc-{i}"))).collect()
}

/// Every path a Discord client exposes, Flatpak and Snap variants included.
#[cfg(unix)]
fn candidate_ipc_paths() -> Vec<PathBuf> {
    let mut bases: Vec<PathBuf> = Vec::new();
    if let Some(xdg) = std::env::var_os("XDG_RUNTIME_DIR").map(PathBuf::from) {
        bases.push(xdg.clone());
        bases.push(xdg.join("app/com.discordapp.Discord"));
        bases.push(xdg.join("app/dev.vencord.Vesktop"));
        bases.push(xdg.join("snap.discord"));
    }
    bases.push(std::env::var_os("TMPDIR").map(PathBuf::from).unwrap_or_else(|| PathBuf::from("/tmp")));
    bases.push(PathBuf::from("/tmp"));
    let mut seen = std::collections::HashSet::new();
    bases.retain(|b| !b.as_os_str().is_empty() && seen.insert(b.clone()));
    bases.iter().flat_map(|base| (0..10).map(move |i| base.join(format!("discord-ipc-{i}")))).collect()
}

enum Op {
    Connect,
    /// The newest activity, None to clear the presence.
    Update(Option<Value>),
    Stop,
}

struct Worker {
    tx: Sender<Op>,
}

pub struct Discord {
    worker: Mutex<Option<Worker>>,
    status: Arc<Mutex<String>>,
}

impl Discord {
    pub fn new(enabled: bool) -> Self {
        let this = Self {
            worker: Mutex::new(None),
            status: Arc::new(Mutex::new(tr_noop!("Disabled").to_owned())),
        };
        if enabled {
            this.set_enabled(true);
        }
        this
    }

    /// "Connected", "Disconnected" or "Disabled", for the settings row, in the app's language.
    pub fn status(&self) -> String {
        self.status.lock().map(|s| crate::i18n::gettext(&s)).unwrap_or_default()
    }

    pub fn is_enabled(&self) -> bool {
        self.worker.lock().map(|w| w.is_some()).unwrap_or(false)
    }

    pub fn set_enabled(&self, enabled: bool) {
        let Ok(mut worker) = self.worker.lock() else { return };
        match (enabled, worker.is_some()) {
            (true, false) => {
                let (tx, rx) = channel();
                set_status(&self.status, tr_noop!("Disconnected"));
                let status = self.status.clone();
                let spawned = std::thread::Builder::new().name("discord-rpc".into()).spawn(move || Connection::new(status).run(rx));
                match spawned {
                    Ok(_) => {
                        let _ = tx.send(Op::Connect);
                        *worker = Some(Worker { tx });
                    }
                    Err(err) => tracing::warn!(%err, "discord worker did not start"),
                }
            }
            (false, true) => {
                if let Some(w) = worker.take() {
                    let _ = w.tx.send(Op::Stop);
                }
                set_status(&self.status, tr_noop!("Disabled"));
            }
            _ => {}
        }
    }

    /// Publish a new activity. Dropped when presence is switched off.
    pub fn update(&self, activity: Option<Value>) {
        if let Ok(worker) = self.worker.lock() {
            if let Some(w) = worker.as_ref() {
                let _ = w.tx.send(Op::Update(activity));
            }
        }
    }

    pub fn stop(&self) {
        self.set_enabled(false);
    }
}

fn set_status(status: &Arc<Mutex<String>>, text: &str) {
    if let Ok(mut s) = status.lock() {
        *s = text.to_owned();
    }
}

struct Connection {
    stream: Option<IpcStream>,
    status: Arc<Mutex<String>>,
    attempt: usize,
    reconnect_at: Option<Instant>,
    /// The last activity asked for, sent again after a reconnect.
    latest: Option<Value>,
    last_update: Option<Instant>,
}

impl Connection {
    fn new(status: Arc<Mutex<String>>) -> Self {
        Self { stream: None, status, attempt: 0, reconnect_at: None, latest: None, last_update: None }
    }

    fn run(mut self, rx: Receiver<Op>) {
        loop {
            let wait = self.reconnect_at.map(|at| at.saturating_duration_since(Instant::now())).unwrap_or(Duration::from_secs(1));
            let first = match rx.recv_timeout(wait) {
                Ok(op) => Some(op),
                Err(RecvTimeoutError::Timeout) => None,
                Err(RecvTimeoutError::Disconnected) => Some(Op::Stop),
            };
            // Coalesce: keep only the newest update, and let a stop win.
            let (mut stop, mut connect, mut update) = (false, false, false);
            let mut absorb = |op: Op, latest: &mut Option<Value>| match op {
                Op::Stop => stop = true,
                Op::Connect => connect = true,
                Op::Update(activity) => {
                    *latest = activity;
                    update = true;
                }
            };
            if let Some(op) = first {
                absorb(op, &mut self.latest);
            }
            while let Ok(op) = rx.try_recv() {
                absorb(op, &mut self.latest);
            }
            if stop {
                return self.shutdown();
            }
            if self.reconnect_at.is_some_and(|at| Instant::now() >= at) {
                self.reconnect_at = None;
                connect = true;
            }
            if (connect || update) && self.stream.is_none() {
                // Connecting sends the latest activity itself.
                self.connect();
                continue;
            }
            if update {
                // During rapid skips this lets more events pile up, so only the newest goes out.
                if let Some(rest) = self.last_update.and_then(|at| MIN_UPDATE_INTERVAL.checked_sub(at.elapsed())) {
                    std::thread::sleep(rest);
                    while let Ok(op) = rx.try_recv() {
                        match op {
                            Op::Stop => return self.shutdown(),
                            Op::Update(activity) => self.latest = activity,
                            Op::Connect => {}
                        }
                    }
                }
                self.send_activity();
            }
        }
    }

    fn connect(&mut self) {
        if self.stream.is_some() || self.reconnect_at.is_some() {
            return;
        }
        for path in candidate_ipc_paths() {
            // A named pipe does not show up as an existing path; opening it is the test.
            if cfg!(unix) && !path.exists() {
                continue;
            }
            match self.handshake(&path) {
                Ok(stream) => {
                    tracing::info!(?path, "discord connected");
                    self.stream = Some(stream);
                    self.attempt = 0;
                    set_status(&self.status, tr_noop!("Connected"));
                    self.send_activity();
                    return;
                }
                Err(err) => tracing::debug!(?path, %err, "discord connect attempt failed"),
            }
        }
        tracing::debug!("no discord ipc endpoint reachable");
        set_status(&self.status, tr_noop!("Disconnected"));
        self.schedule_reconnect();
    }

    /// Discord answers a good handshake with a READY event. Anything else,
    /// usually a CLOSE frame carrying a code and message, is a failure.
    fn handshake(&self, path: &std::path::Path) -> std::io::Result<IpcStream> {
        let mut stream = open_ipc(path)?;
        send_frame(&mut stream, OP_HANDSHAKE, &json!({"v": 1, "client_id": DISCORD_APP_ID}))?;
        wait_for_reply(&stream)?;
        let (op, reply) = recv_frame(&mut stream)?;
        let evt = reply.as_ref().and_then(|r| r.get("evt")).and_then(Value::as_str);
        if op != OP_FRAME || evt != Some("READY") {
            let message = reply.as_ref().and_then(|r| r.get("message").or_else(|| r.pointer("/data/message"))).and_then(Value::as_str).unwrap_or("no READY event");
            return Err(std::io::Error::other(format!("handshake refused (op {op}): {message}")));
        }
        Ok(stream)
    }

    fn schedule_reconnect(&mut self) {
        if self.reconnect_at.is_some() {
            return;
        }
        let delay = RECONNECT_BACKOFF[self.attempt.min(RECONNECT_BACKOFF.len() - 1)];
        self.attempt += 1;
        self.reconnect_at = Some(Instant::now() + Duration::from_secs(delay));
    }

    fn send_activity(&mut self) {
        let Some(stream) = self.stream.as_mut() else { return };
        let frame = json!({
            "cmd": "SET_ACTIVITY",
            "args": {"pid": std::process::id(), "activity": self.latest},
            "nonce": glib::uuid_string_random().as_str(),
        });
        let sent = send_frame(stream, OP_FRAME, &frame).and_then(|()| wait_for_reply(stream)).and_then(|()| recv_frame(stream));
        self.last_update = Some(Instant::now());
        match sent {
            // Discord refuses a bad activity with an ERROR event, not a closed pipe.
            Ok((_, Some(reply))) if reply.get("evt").and_then(Value::as_str) == Some("ERROR") => {
                let message = reply.pointer("/data/message").and_then(Value::as_str).unwrap_or("no message");
                tracing::warn!(message, "discord rejected the activity");
            }
            Ok(reply) => tracing::debug!(?reply, "discord activity set"),
            Err(err) => {
                tracing::info!(%err, "discord update failed, reconnecting");
                self.teardown();
                self.schedule_reconnect();
            }
        }
    }

    fn shutdown(mut self) {
        if let Some(stream) = self.stream.as_mut() {
            let frame = json!({
                "cmd": "SET_ACTIVITY",
                "args": {"pid": std::process::id(), "activity": null},
                "nonce": glib::uuid_string_random().as_str(),
            });
            let _ = send_frame(stream, OP_FRAME, &frame);
        }
        self.stream = None;
    }

    fn teardown(&mut self) {
        self.stream = None;
        set_status(&self.status, tr_noop!("Disconnected"));
    }
}

/// A frame is two little-endian u32s, opcode and length, then the JSON body.
fn encode_frame(op: u32, payload: &Value) -> Vec<u8> {
    let body = payload.to_string().into_bytes();
    let mut frame = Vec::with_capacity(8 + body.len());
    frame.extend_from_slice(&op.to_le_bytes());
    frame.extend_from_slice(&(body.len() as u32).to_le_bytes());
    frame.extend_from_slice(&body);
    frame
}

fn send_frame(stream: &mut impl Write, op: u32, payload: &Value) -> std::io::Result<()> {
    stream.write_all(&encode_frame(op, payload))
}

fn recv_frame(stream: &mut impl Read) -> std::io::Result<(u32, Option<Value>)> {
    let mut header = [0u8; 8];
    stream.read_exact(&mut header)?;
    let op = u32::from_le_bytes([header[0], header[1], header[2], header[3]]);
    let length = u32::from_le_bytes([header[4], header[5], header[6], header[7]]) as usize;
    // A presence reply is a few hundred bytes. Anything huge is not Discord.
    if length > 1 << 20 {
        return Err(std::io::Error::other("oversized discord frame"));
    }
    let mut body = vec![0u8; length];
    stream.read_exact(&mut body)?;
    Ok((op, serde_json::from_slice(&body).ok()))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn options() -> Options {
        Options { status_display: "artist".into(), small_icon: true, hide_on_pause: false }
    }

    fn playing() -> Snapshot {
        Snapshot {
            status: PlaybackStatus::Playing,
            has_track: true,
            title: "Song".into(),
            artist: "Artist".into(),
            album: "Album".into(),
            thumb: "https://img/x.jpg".into(),
            position: 10.0,
            duration: 200.0,
        }
    }

    #[test]
    fn a_playing_track_carries_timestamps_and_art() {
        let activity = build_activity(&playing(), &options(), 1_000_000).unwrap();
        assert_eq!(activity["details"], "Song");
        assert_eq!(activity["state"], "Artist");
        assert_eq!(activity["type"], 2);
        assert_eq!(activity["status_display_type"], 1);
        assert_eq!(activity["assets"]["large_image"], "https://img/x.jpg");
        assert_eq!(activity["assets"]["large_text"], "Album");
        assert_eq!(activity["assets"]["small_image"], "pause");
        assert_eq!(activity["timestamps"]["start"], 990_000);
        assert_eq!(activity["timestamps"]["end"], 1_190_000);
    }

    #[test]
    fn paused_drops_the_clock_and_can_hide() {
        let mut snap = playing();
        snap.status = PlaybackStatus::Paused;
        let activity = build_activity(&snap, &options(), 0).unwrap();
        assert!(activity.get("timestamps").is_none());
        assert_eq!(activity["assets"]["small_text"], "Paused");
        let hide = Options { hide_on_pause: true, ..options() };
        assert!(build_activity(&snap, &hide, 0).is_none());
    }

    #[test]
    fn stopped_or_empty_clears_the_presence() {
        let mut snap = playing();
        snap.status = PlaybackStatus::Stopped;
        assert!(build_activity(&snap, &options(), 0).is_none());
        let mut snap = playing();
        snap.has_track = false;
        assert!(build_activity(&snap, &options(), 0).is_none());
    }

    #[test]
    fn short_fields_are_padded_and_local_art_falls_back_to_the_logo() {
        let mut snap = playing();
        snap.title = "X".into();
        snap.thumb = "/home/me/cover.jpg".into();
        let plain = Options { small_icon: false, status_display: "song_title".into(), ..options() };
        let activity = build_activity(&snap, &plain, 0).unwrap();
        assert_eq!(activity["details"], "X ");
        assert_eq!(activity["assets"]["large_image"], MIXTAPES_LOGO);
        assert!(activity["assets"].get("small_image").is_none());
        assert_eq!(activity["status_display_type"], 2);
    }

    #[test]
    fn frames_round_trip() {
        let payload = json!({"cmd": "SET_ACTIVITY"});
        let bytes = encode_frame(OP_FRAME, &payload);
        assert_eq!(&bytes[..4], &1u32.to_le_bytes());
        let (op, body) = recv_frame(&mut bytes.as_slice()).unwrap();
        assert_eq!(op, OP_FRAME);
        assert_eq!(body, Some(payload));
    }
}

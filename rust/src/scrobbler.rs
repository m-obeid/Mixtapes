//! Scrobbling to Last.fm and ListenBrainz. Port of player/scrobbler.py.
//!
//! The GTK thread feeds the listening clock and reads state for the
//! preferences rows. One tokio task owns every network call. Scrobbles that
//! fail are kept on disk and retried, so a dropped connection loses no plays.
//! Files are shared with the Python app: scrobbler.json and scrobble_queue.json.

use std::collections::BTreeMap;
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

use md5::{Digest, Md5};
use serde::{Deserialize, Serialize};
use serde_json::{Map, Value, json};
use tokio::sync::mpsc::{UnboundedReceiver, UnboundedSender, unbounded_channel};

use crate::paths::Paths;

// Committed on purpose, like the Python app: builds run on the user's machine
// and a key inside a shipped binary is extractable anyway. Env vars override.
const EMBEDDED_LASTFM_API_KEY: &str = "1aa73ecd8d085e53977fc8e781afa2fa";
const EMBEDDED_LASTFM_API_SECRET: &str = "51523d91a6e58babdb0b06b130f5ec45";

const LASTFM_API_ROOT: &str = "https://ws.audioscrobbler.com/2.0/";
const LASTFM_AUTH_URL: &str = "https://www.last.fm/api/auth/";
const LISTENBRAINZ_API_ROOT: &str = "https://api.listenbrainz.org";
const USER_AGENT: &str = "Mixtapes (https://pocoguy.com/#!/mixtapes)";

/// Last.fm never takes a track under 30 seconds.
const MIN_TRACK_LENGTH: f64 = 30.0;
/// Submit once half the track or 4 minutes has played, whichever is first.
const SCROBBLE_CAP_SECONDS: f64 = 240.0;
/// Streams that never report a length fall back to a flat threshold.
const UNKNOWN_DURATION_THRESHOLD: f64 = 120.0;
/// Cap on the offline backlog per service.
const MAX_PENDING: usize = 500;
/// Give up on an entry that keeps being refused.
const MAX_ATTEMPTS: u32 = 10;
/// Retry the backlog on this cadence while the worker is idle.
const RETRY_INTERVAL: Duration = Duration::from_secs(300);
/// Last.fm's per-request scrobble limit.
const BATCH_SIZE: usize = 50;
/// Refresh "now playing" no more often than this.
const NOW_PLAYING_INTERVAL: Duration = Duration::from_secs(30);
const NETWORK_TIMEOUT: Duration = Duration::from_secs(15);
const IDLE_WAKE: Duration = Duration::from_secs(30);

#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub enum Service {
    LastFm,
    ListenBrainz,
}

pub const SERVICES: [Service; 2] = [Service::LastFm, Service::ListenBrainz];

impl Service {
    /// Key in scrobbler.json and scrobble_queue.json.
    pub fn key(self) -> &'static str {
        match self {
            Service::LastFm => "lastfm",
            Service::ListenBrainz => "listenbrainz",
        }
    }

    pub fn label(self) -> &'static str {
        match self {
            Service::LastFm => "Last.fm",
            Service::ListenBrainz => "ListenBrainz",
        }
    }
}

#[derive(Debug, thiserror::Error)]
pub enum ScrobbleError {
    /// Worth retrying: network down, rate limit, 5xx.
    #[error("{0}")]
    Transient(String),
    /// Retrying will not fix it.
    #[error("{0}")]
    Permanent(String),
    /// The stored credential was rejected. The user has to reconnect.
    #[error("{0}")]
    Auth(String),
}

use ScrobbleError::{Auth, Permanent, Transient};

/// One listen, as it sits in scrobble_queue.json.
#[derive(Clone, Debug, Default, Serialize, Deserialize, PartialEq)]
pub struct Entry {
    #[serde(default)]
    pub artist: String,
    #[serde(default)]
    pub track: String,
    #[serde(default)]
    pub album: String,
    #[serde(default)]
    pub video_id: String,
    #[serde(default)]
    pub timestamp: i64,
    #[serde(default)]
    pub duration: u64,
    #[serde(default)]
    pub attempts: u32,
}

/// The play in flight and its listening clock.
#[derive(Clone, Debug)]
struct Current {
    video_id: String,
    track: String,
    artist: String,
    album: String,
    duration: f64,
    /// Filled when playback reaches Playing, so it reflects the listen and not the load.
    timestamp: Option<i64>,
    elapsed: Duration,
    playing_since: Option<Instant>,
    scrobbled: bool,
    now_playing_at: Option<Instant>,
}

impl Current {
    fn entry(&self) -> Entry {
        Entry {
            artist: self.artist.clone(),
            track: self.track.clone(),
            album: self.album.clone(),
            video_id: self.video_id.clone(),
            timestamp: self.timestamp.unwrap_or_else(unix_now),
            duration: if self.duration > 0.0 { self.duration as u64 } else { 0 },
            attempts: 0,
        }
    }
}

/// What a progress tick asks the worker to send.
#[derive(Debug, Default, PartialEq)]
struct Due {
    now_playing: Option<Entry>,
    scrobble: Option<Entry>,
}

fn threshold(duration: f64) -> f64 {
    if duration <= 0.0 { UNKNOWN_DURATION_THRESHOLD } else { (duration / 2.0).min(SCROBBLE_CAP_SECONDS) }
}

/// Advance the clock for one position tick. Pure apart from the wall clock stamp.
fn tick(cur: &mut Current, duration: f64, is_playing: bool, now: Instant) -> Due {
    let mut due = Due::default();
    if is_playing {
        if cur.playing_since.is_none() {
            cur.playing_since = Some(now);
        }
        if cur.timestamp.is_none() {
            cur.timestamp = Some(unix_now());
        }
        // Last.fm expires a now-playing update on its own, so refresh it while the track runs.
        if cur.now_playing_at.is_none_or(|sent| now.duration_since(sent) > NOW_PLAYING_INTERVAL) {
            cur.now_playing_at = Some(now);
            due.now_playing = Some(cur.entry());
        }
    } else if let Some(since) = cur.playing_since.take() {
        cur.elapsed += now.duration_since(since);
    }

    if !cur.scrobbled {
        // GStreamer often learns the real length a few ticks in.
        if duration > cur.duration {
            cur.duration = duration;
        }
        let mut elapsed = cur.elapsed;
        if let Some(since) = cur.playing_since {
            elapsed += now.duration_since(since);
        }
        if elapsed.as_secs_f64() >= threshold(cur.duration) {
            cur.scrobbled = true;
            due.scrobble = Some(cur.entry());
        }
    }
    due
}

enum Op {
    NowPlaying(Entry),
    Scrobble(Entry),
    Flush,
    Stop,
}

type Pending = BTreeMap<String, Vec<Entry>>;

struct Inner {
    creds: Map<String, Value>,
    pending: Pending,
    cur: Option<Current>,
    enabled: bool,
    now_playing_enabled: bool,
    stopping: bool,
    /// Demo runs play throwaway tracks, which must never reach a real profile.
    muted: bool,
    last_error: String,
}

pub struct Scrobbler {
    inner: Mutex<Inner>,
    tx: UnboundedSender<Op>,
    http: reqwest::Client,
    paths: Paths,
    api_key: String,
    api_secret: String,
}

impl Scrobbler {
    /// Load credentials and the backlog, and start the worker on `rt`.
    pub fn start(paths: &Paths, rt: &tokio::runtime::Handle) -> Arc<Self> {
        let (tx, rx) = unbounded_channel();
        let prefs = paths.read_prefs();
        let flag = |key: &str| prefs.get(key).and_then(Value::as_bool).unwrap_or(true);
        let http = reqwest::Client::builder().user_agent(USER_AGENT).timeout(NETWORK_TIMEOUT).pool_max_idle_per_host(2).build().unwrap_or_default();
        let this = Arc::new(Self {
            inner: Mutex::new(Inner {
                creds: read_object(&creds_path(paths)),
                pending: load_pending(paths),
                cur: None,
                enabled: flag("scrobble_enabled"),
                now_playing_enabled: flag("scrobble_now_playing"),
                stopping: false,
                muted: false,
                last_error: String::new(),
            }),
            tx,
            http,
            paths: paths.clone(),
            api_key: env_or("MIXTAPES_LASTFM_API_KEY", EMBEDDED_LASTFM_API_KEY),
            api_secret: env_or("MIXTAPES_LASTFM_API_SECRET", EMBEDDED_LASTFM_API_SECRET),
        });
        rt.spawn(this.clone().run(rx));
        if this.pending_count() > 0 {
            let _ = this.tx.send(Op::Flush);
        }
        this
    }

    fn lock(&self) -> std::sync::MutexGuard<'_, Inner> {
        self.inner.lock().unwrap_or_else(|e| e.into_inner())
    }

    // -- state the preferences rows read ----------------------------------

    /// False when the build ships without Last.fm API credentials.
    pub fn lastfm_configured(&self) -> bool {
        !self.api_key.is_empty() && !self.api_secret.is_empty()
    }

    pub fn is_connected(&self, service: Service) -> bool {
        !self.credential(service).is_empty()
    }

    pub fn username(&self, service: Service) -> String {
        self.lock().creds.get(service.key()).and_then(|e| e.get("username")).and_then(Value::as_str).unwrap_or_default().to_owned()
    }

    pub fn pending_count(&self) -> usize {
        self.lock().pending.values().map(Vec::len).sum()
    }

    pub fn last_error(&self) -> String {
        self.lock().last_error.clone()
    }

    pub fn enabled(&self) -> bool {
        self.lock().enabled
    }

    pub fn set_enabled(&self, enabled: bool) {
        self.lock().enabled = enabled;
        if enabled {
            let _ = self.tx.send(Op::Flush);
        }
    }

    pub fn set_now_playing_enabled(&self, enabled: bool) {
        self.lock().now_playing_enabled = enabled;
    }

    /// Keep everything working except the submissions. Not saved anywhere.
    pub fn set_muted(&self, muted: bool) {
        self.lock().muted = muted;
    }

    pub fn stop(&self) {
        self.lock().stopping = true;
        let _ = self.tx.send(Op::Stop);
    }

    // -- player events ------------------------------------------------------

    /// A new play began. Resets the listening clock.
    pub fn on_track_started(&self, video_id: &str, title: &str, artist: &str, album: &str, duration: f64) {
        self.lock().cur = Some(Current {
            video_id: video_id.to_owned(),
            track: title.trim().to_owned(),
            artist: artist.trim().to_owned(),
            album: album.trim().to_owned(),
            duration,
            timestamp: None,
            elapsed: Duration::ZERO,
            playing_since: None,
            scrobbled: false,
            now_playing_at: None,
        });
    }

    /// Correct the in-flight track's metadata without touching the clock.
    /// The audio-version swap lands after playback starts.
    pub fn refine_current_track(&self, video_id: &str, title: &str, artist: &str) {
        let mut inner = self.lock();
        let Some(cur) = inner.cur.as_mut().filter(|c| !c.scrobbled) else { return };
        if !title.trim().is_empty() {
            cur.track = title.trim().to_owned();
        }
        if !artist.trim().is_empty() {
            cur.artist = artist.trim().to_owned();
        }
        if !video_id.is_empty() {
            cur.video_id = video_id.to_owned();
        }
    }

    /// Only ever stops the clock. Starting it is the progress tick's job.
    pub fn on_state_changed(&self, playing: bool) {
        if playing {
            return;
        }
        let mut inner = self.lock();
        if let Some(cur) = inner.cur.as_mut() {
            if let Some(since) = cur.playing_since.take() {
                cur.elapsed += since.elapsed();
            }
        }
    }

    /// Drives the listening clock. Called on every position tick.
    pub fn on_progress(&self, duration: f64, is_playing: bool) {
        let due = {
            let mut inner = self.lock();
            let Some(cur) = inner.cur.as_mut() else { return };
            tick(cur, duration, is_playing, Instant::now())
        };
        if let Some(entry) = due.now_playing {
            if self.lock().now_playing_enabled && self.playable(&entry) {
                let _ = self.tx.send(Op::NowPlaying(entry));
            }
        }
        if let Some(entry) = due.scrobble {
            if self.playable(&entry) {
                tracing::info!(artist = %entry.artist, track = %entry.track, "scrobble queued");
                let _ = self.tx.send(Op::Scrobble(entry));
            }
        }
    }

    fn playable(&self, entry: &Entry) -> bool {
        {
            let inner = self.lock();
            if !inner.enabled || inner.stopping || inner.muted {
                return false;
            }
        }
        if entry.track.is_empty() || entry.artist.is_empty() {
            return false;
        }
        // A missing duration is unknown, not short.
        if entry.duration > 0 && (entry.duration as f64) < MIN_TRACK_LENGTH {
            return false;
        }
        SERVICES.iter().any(|s| self.is_connected(*s))
    }

    // -- worker ---------------------------------------------------------------

    async fn run(self: Arc<Self>, mut rx: UnboundedReceiver<Op>) {
        let mut last_flush = Instant::now();
        loop {
            let op = match tokio::time::timeout(IDLE_WAKE, rx.recv()).await {
                Ok(Some(op)) => op,
                Ok(None) => break,
                Err(_) => {
                    if self.pending_count() > 0 && last_flush.elapsed() > RETRY_INTERVAL {
                        last_flush = Instant::now();
                        self.flush().await;
                    }
                    continue;
                }
            };
            match op {
                Op::Stop => break,
                Op::NowPlaying(entry) => self.do_now_playing(&entry).await,
                Op::Scrobble(entry) => {
                    self.append_pending(entry);
                    last_flush = Instant::now();
                    self.flush().await;
                }
                Op::Flush => {
                    last_flush = Instant::now();
                    self.flush().await;
                }
            }
        }
    }

    async fn do_now_playing(&self, entry: &Entry) {
        for service in SERVICES {
            if !self.is_connected(service) {
                continue;
            }
            let sent = match service {
                Service::LastFm => self.lastfm_now_playing(entry).await,
                Service::ListenBrainz => self.listenbrainz_now_playing(entry).await,
            };
            match sent {
                Ok(()) => {}
                Err(Auth(message)) => self.handle_auth_error(service, &message),
                // Cosmetic and it expires anyway, so a failure is logged and dropped.
                Err(err) => tracing::warn!(service = service.key(), %err, "now-playing failed"),
            }
        }
    }

    fn append_pending(&self, entry: Entry) {
        let snapshot = {
            let mut inner = self.lock();
            for service in SERVICES {
                let connected = !credential_of(&inner.creds, service, self.lastfm_configured()).is_empty();
                if !connected {
                    continue;
                }
                let bucket = inner.pending.entry(service.key().to_owned()).or_default();
                bucket.push(entry.clone());
                if bucket.len() > MAX_PENDING {
                    let excess = bucket.len() - MAX_PENDING;
                    bucket.drain(..excess);
                }
            }
            inner.pending.clone()
        };
        save_pending(&self.paths, &snapshot);
    }

    async fn flush(&self) {
        for service in SERVICES {
            if !self.is_connected(service) {
                continue;
            }
            loop {
                let batch: Vec<Entry> = {
                    let inner = self.lock();
                    if inner.stopping {
                        return;
                    }
                    inner.pending.get(service.key()).map(|b| b.iter().take(BATCH_SIZE).cloned().collect()).unwrap_or_default()
                };
                if batch.is_empty() {
                    break;
                }
                let sent = match service {
                    Service::LastFm => self.lastfm_scrobble(&batch).await,
                    Service::ListenBrainz => self.listenbrainz_scrobble(&batch).await,
                };
                match sent {
                    Ok(()) => {
                        self.drop_sent(service, batch.len());
                        tracing::info!(service = service.key(), count = batch.len(), "listens accepted");
                    }
                    Err(Auth(message)) => {
                        self.handle_auth_error(service, &message);
                        break;
                    }
                    Err(Transient(message)) => {
                        tracing::warn!(service = service.key(), %message, "retry later");
                        self.lock().last_error = format!("{}: {message}", service.label());
                        break;
                    }
                    Err(Permanent(message)) => {
                        tracing::warn!(service = service.key(), %message, "batch rejected");
                        self.lock().last_error = format!("{}: {message}", service.label());
                        self.penalize(service, batch.len());
                        break;
                    }
                }
            }
        }
    }

    /// Count a failed attempt and drop entries that keep being refused, so
    /// one bad play cannot wedge the backlog forever.
    fn penalize(&self, service: Service, count: usize) {
        let snapshot = {
            let mut inner = self.lock();
            let bucket = inner.pending.entry(service.key().to_owned()).or_default();
            for item in bucket.iter_mut().take(count) {
                item.attempts += 1;
            }
            let before = bucket.len();
            bucket.retain(|i| i.attempts < MAX_ATTEMPTS);
            let dropped = before - bucket.len();
            if dropped > 0 {
                tracing::warn!(service = service.key(), dropped, "unsendable listens dropped");
            }
            inner.pending.clone()
        };
        save_pending(&self.paths, &snapshot);
    }

    fn drop_sent(&self, service: Service, count: usize) {
        let snapshot = {
            let mut inner = self.lock();
            if let Some(bucket) = inner.pending.get_mut(service.key()) {
                bucket.drain(..count.min(bucket.len()));
            }
            inner.pending.clone()
        };
        save_pending(&self.paths, &snapshot);
    }

    fn handle_auth_error(&self, service: Service, message: &str) {
        tracing::warn!(service = service.key(), %message, "credentials rejected");
        self.disconnect(service);
        // Set after disconnect, which clears it, so the row can say why the service dropped out.
        self.lock().last_error = format!("{}: {message}", service.label());
    }

    fn credential(&self, service: Service) -> String {
        credential_of(&self.lock().creds, service, self.lastfm_configured())
    }

    // -- Last.fm ----------------------------------------------------------------

    async fn lastfm_call(&self, method: &str, params: BTreeMap<String, String>, post: bool, session_key: Option<&str>) -> Result<Value, ScrobbleError> {
        let mut payload = params;
        payload.insert("method".into(), method.to_owned());
        payload.insert("api_key".into(), self.api_key.clone());
        if let Some(sk) = session_key.filter(|s| !s.is_empty()) {
            payload.insert("sk".into(), sk.to_owned());
        }
        payload.insert("api_sig".into(), sign(&payload, &self.api_secret));
        payload.insert("format".into(), "json".into());

        let request = if post { self.http.post(LASTFM_API_ROOT).form(&payload) } else { self.http.get(LASTFM_API_ROOT).query(&payload) };
        let response = request.send().await.map_err(|e| Transient(e.to_string()))?;
        let status = response.status().as_u16();
        if status == 429 || status >= 500 {
            return Err(Transient(format!("HTTP {status}")));
        }
        let data: Value = response.json().await.map_err(|_| Transient(format!("unreadable response (HTTP {status})")))?;
        classify_lastfm(data)
    }

    async fn lastfm_now_playing(&self, entry: &Entry) -> Result<(), ScrobbleError> {
        let mut params = BTreeMap::new();
        params.insert("artist".to_owned(), entry.artist.clone());
        params.insert("track".to_owned(), entry.track.clone());
        if !entry.album.is_empty() {
            params.insert("album".to_owned(), entry.album.clone());
        }
        if entry.duration > 0 {
            params.insert("duration".to_owned(), entry.duration.to_string());
        }
        let sk = self.credential(Service::LastFm);
        let data = self.lastfm_call("track.updateNowPlaying", params, true, Some(&sk)).await?;
        self.report_ignored(&data);
        Ok(())
    }

    async fn lastfm_scrobble(&self, batch: &[Entry]) -> Result<(), ScrobbleError> {
        let sk = self.credential(Service::LastFm);
        let data = self.lastfm_call("track.scrobble", lastfm_batch_params(batch), true, Some(&sk)).await?;
        self.report_ignored(&data);
        Ok(())
    }

    /// Log whatever Last.fm accepted then dropped. Without this a discarded
    /// scrobble looks the same as a successful one.
    fn report_ignored(&self, data: &Value) {
        for (artist, track, reason) in ignored_listens(data) {
            tracing::warn!(%artist, %track, %reason, "lastfm discarded a listen");
            self.lock().last_error = format!("Last.fm: {reason}");
        }
    }

    /// Start the desktop auth flow. Returns (token, url) for the caller to open in a browser.
    pub async fn lastfm_request_token(&self) -> Result<(String, String), ScrobbleError> {
        if !self.lastfm_configured() {
            return Err(Permanent("This build has no Last.fm API credentials".into()));
        }
        let data = self.lastfm_call("auth.getToken", BTreeMap::new(), false, None).await?;
        let token = data.get("token").and_then(Value::as_str).unwrap_or_default().to_owned();
        if token.is_empty() {
            return Err(Permanent("Last.fm returned no token".into()));
        }
        let url = reqwest::Url::parse_with_params(LASTFM_AUTH_URL, [("api_key", self.api_key.as_str()), ("token", token.as_str())])
            .map(String::from)
            .map_err(|e| Permanent(e.to_string()))?;
        Ok((token, url))
    }

    /// Trade an authorized token for a session key. Fails with Auth while the
    /// user has not pressed Allow yet, so the caller polls this.
    pub async fn lastfm_finish_auth(&self, token: &str) -> Result<String, ScrobbleError> {
        let mut params = BTreeMap::new();
        params.insert("token".to_owned(), token.to_owned());
        let data = self.lastfm_call("auth.getSession", params, false, None).await?;
        let session = data.get("session").cloned().unwrap_or_default();
        let key = session.get("key").and_then(Value::as_str).unwrap_or_default();
        if key.is_empty() {
            return Err(Permanent("Last.fm returned no session".into()));
        }
        let name = session.get("name").and_then(Value::as_str).unwrap_or_default().to_owned();
        self.store_credentials(Service::LastFm, json!({"session_key": key, "username": name}));
        Ok(name)
    }

    // -- ListenBrainz -------------------------------------------------------------

    async fn listenbrainz_post(&self, body: Value) -> Result<(), ScrobbleError> {
        let token = self.credential(Service::ListenBrainz);
        let response = self
            .http
            .post(format!("{LISTENBRAINZ_API_ROOT}/1/submit-listens"))
            .header("Authorization", format!("Token {token}"))
            .json(&body)
            .send()
            .await
            .map_err(|e| Transient(e.to_string()))?;
        let status = response.status().as_u16();
        match status {
            200 => Ok(()),
            401 | 403 => Err(Auth("user token rejected".into())),
            429 => Err(Transient(format!("HTTP {status}"))),
            s if s >= 500 => Err(Transient(format!("HTTP {status}"))),
            _ => {
                let text: String = response.text().await.unwrap_or_default().chars().take(200).collect();
                Err(Permanent(format!("HTTP {status}: {text}")))
            }
        }
    }

    async fn listenbrainz_now_playing(&self, entry: &Entry) -> Result<(), ScrobbleError> {
        self.listenbrainz_post(json!({
            "listen_type": "playing_now",
            "payload": [{"track_metadata": listenbrainz_metadata(entry)}],
        }))
        .await
    }

    async fn listenbrainz_scrobble(&self, batch: &[Entry]) -> Result<(), ScrobbleError> {
        self.listenbrainz_post(listenbrainz_body(batch)).await
    }

    /// Validate a user token and store it. Returns the ListenBrainz username.
    pub async fn listenbrainz_connect(&self, token: &str) -> Result<String, ScrobbleError> {
        let token = token.trim();
        if token.is_empty() {
            return Err(Permanent("Enter your ListenBrainz user token".into()));
        }
        let response = self
            .http
            .get(format!("{LISTENBRAINZ_API_ROOT}/1/validate-token"))
            .header("Authorization", format!("Token {token}"))
            .send()
            .await
            .map_err(|e| Transient(e.to_string()))?;
        let status = response.status().as_u16();
        if status == 401 || status == 403 {
            return Err(Auth("that token is not valid".into()));
        }
        if status == 429 || status >= 500 {
            return Err(Transient(format!("HTTP {status}")));
        }
        let data: Value = response.json().await.map_err(|e| Transient(e.to_string()))?;
        if !data.get("valid").and_then(Value::as_bool).unwrap_or(false) {
            let message = data.get("message").and_then(Value::as_str).unwrap_or("that token is not valid");
            return Err(Auth(message.to_owned()));
        }
        let name = data.get("user_name").and_then(Value::as_str).unwrap_or_default().to_owned();
        self.store_credentials(Service::ListenBrainz, json!({"token": token, "username": name}));
        Ok(name)
    }

    // -- credential storage ---------------------------------------------------------

    fn store_credentials(&self, service: Service, entry: Value) {
        let snapshot = {
            let mut inner = self.lock();
            inner.creds.insert(service.key().to_owned(), entry);
            inner.last_error.clear();
            inner.creds.clone()
        };
        write_json(&creds_path(&self.paths), &Value::Object(snapshot), true);
        let _ = self.tx.send(Op::Flush);
    }

    pub fn disconnect(&self, service: Service) {
        let (creds, pending) = {
            let mut inner = self.lock();
            inner.last_error.clear();
            inner.creds.remove(service.key());
            inner.pending.insert(service.key().to_owned(), Vec::new());
            (inner.creds.clone(), inner.pending.clone())
        };
        write_json(&creds_path(&self.paths), &Value::Object(creds), true);
        save_pending(&self.paths, &pending);
    }
}

// -- pure helpers ---------------------------------------------------------------------

fn env_or(name: &str, fallback: &str) -> String {
    std::env::var(name).ok().filter(|v| !v.is_empty()).unwrap_or_else(|| fallback.to_owned())
}

fn unix_now() -> i64 {
    SystemTime::now().duration_since(UNIX_EPOCH).map(|d| d.as_secs() as i64).unwrap_or(0)
}

fn credential_of(creds: &Map<String, Value>, service: Service, lastfm_configured: bool) -> String {
    let field = match service {
        Service::LastFm if !lastfm_configured => return String::new(),
        Service::LastFm => "session_key",
        Service::ListenBrainz => "token",
    };
    creds.get(service.key()).and_then(|e| e.get(field)).and_then(Value::as_str).unwrap_or_default().to_owned()
}

/// Last.fm api_sig: md5 of every key and value in key order, with the shared secret appended.
fn sign(params: &BTreeMap<String, String>, secret: &str) -> String {
    let mut raw = String::new();
    for (k, v) in params {
        if k != "format" && k != "callback" {
            raw.push_str(k);
            raw.push_str(v);
        }
    }
    raw.push_str(secret);
    Md5::digest(raw.as_bytes()).iter().map(|b| format!("{b:02x}")).collect()
}

fn lastfm_batch_params(batch: &[Entry]) -> BTreeMap<String, String> {
    let mut params = BTreeMap::new();
    for (i, item) in batch.iter().enumerate() {
        params.insert(format!("artist[{i}]"), item.artist.clone());
        params.insert(format!("track[{i}]"), item.track.clone());
        params.insert(format!("timestamp[{i}]"), item.timestamp.to_string());
        if !item.album.is_empty() {
            params.insert(format!("album[{i}]"), item.album.clone());
        }
        if item.duration > 0 {
            params.insert(format!("duration[{i}]"), item.duration.to_string());
        }
    }
    params
}

/// Sort a Last.fm answer into success or the kind of failure its error code means.
fn classify_lastfm(data: Value) -> Result<Value, ScrobbleError> {
    let code = match data.get("error") {
        Some(Value::Number(n)) => n.as_i64().unwrap_or(0),
        Some(Value::String(s)) => s.parse().unwrap_or(0),
        _ => 0,
    };
    if code == 0 {
        return Ok(data);
    }
    let message = data.get("message").and_then(Value::as_str).unwrap_or("unknown error").to_owned();
    match code {
        // All four mean "this credential is no longer good".
        4 | 9 | 14 | 15 => Err(Auth(message)),
        // 8 operation failed, 11 service offline, 16 unavailable, 29 rate limited.
        8 | 11 | 16 | 29 => Err(Transient(message)),
        _ => Err(Permanent(format!("{message} (code {code})"))),
    }
}

fn ignore_reason(code: &str) -> Option<&'static str> {
    Some(match code {
        "1" => "artist name ignored",
        "2" => "track name ignored",
        "3" => "timestamp too old",
        "4" => "timestamp too far in the future",
        "5" => "daily scrobble limit reached",
        _ => return None,
    })
}

/// (artist, track, reason) for every listen Last.fm took with HTTP 200 and then discarded.
fn ignored_listens(data: &Value) -> Vec<(String, String, String)> {
    let Some(body) = data.get("scrobbles").or_else(|| data.get("nowplaying")).filter(|b| !b.is_null()) else {
        return Vec::new();
    };
    let entries = body.get("scrobble").unwrap_or(body);
    let items: Vec<&Value> = match entries {
        Value::Array(list) => list.iter().collect(),
        other => vec![other],
    };
    let text = |item: &Value, key: &str| item.get(key).and_then(|v| v.get("#text")).and_then(Value::as_str).unwrap_or("?").to_owned();
    items
        .into_iter()
        .filter(|item| item.is_object())
        .filter_map(|item| {
            let message = item.get("ignoredMessage")?;
            let code = match message.get("code") {
                Some(Value::String(s)) => s.clone(),
                Some(Value::Number(n)) => n.to_string(),
                _ => "0".to_owned(),
            };
            if code == "0" || code.is_empty() {
                return None;
            }
            let reason = ignore_reason(&code).map(str::to_owned).unwrap_or_else(|| message.get("#text").and_then(Value::as_str).filter(|t| !t.is_empty()).unwrap_or("unknown reason").to_owned());
            Some((text(item, "artist"), text(item, "track"), reason))
        })
        .collect()
}

fn listenbrainz_metadata(entry: &Entry) -> Value {
    let mut info = json!({
        "media_player": "Mixtapes",
        "submission_client": "Mixtapes",
        "music_service": "music.youtube.com",
    });
    if entry.duration > 0 {
        info["duration_ms"] = json!(entry.duration * 1000);
    }
    if !entry.video_id.is_empty() {
        info["origin_url"] = json!(format!("https://music.youtube.com/watch?v={}", entry.video_id));
    }
    let mut metadata = json!({
        "artist_name": entry.artist,
        "track_name": entry.track,
        "additional_info": info,
    });
    if !entry.album.is_empty() {
        metadata["release_name"] = json!(entry.album);
    }
    metadata
}

fn listenbrainz_body(batch: &[Entry]) -> Value {
    json!({
        "listen_type": if batch.len() == 1 { "single" } else { "import" },
        "payload": batch.iter().map(|item| json!({
            "listened_at": item.timestamp,
            "track_metadata": listenbrainz_metadata(item),
        })).collect::<Vec<_>>(),
    })
}

// -- files ------------------------------------------------------------------------------

// Kept out of prefs.json on purpose: users paste that file into bug reports.
fn creds_path(paths: &Paths) -> std::path::PathBuf {
    paths.data_dir.join("scrobbler.json")
}

fn pending_path(paths: &Paths) -> std::path::PathBuf {
    paths.data_dir.join("scrobble_queue.json")
}

fn read_object(path: &std::path::Path) -> Map<String, Value> {
    std::fs::read_to_string(path).ok().and_then(|t| serde_json::from_str::<Value>(&t).ok()).and_then(|v| v.as_object().cloned()).unwrap_or_default()
}

fn load_pending(paths: &Paths) -> Pending {
    let data = read_object(&pending_path(paths));
    SERVICES
        .iter()
        .map(|s| {
            let list = data.get(s.key()).cloned().and_then(|v| serde_json::from_value::<Vec<Entry>>(v).ok()).unwrap_or_default();
            (s.key().to_owned(), list)
        })
        .collect()
}

fn save_pending(paths: &Paths, pending: &Pending) {
    match serde_json::to_value(pending) {
        Ok(value) => write_json(&pending_path(paths), &value, false),
        Err(err) => tracing::warn!(%err, "could not encode the scrobble queue"),
    }
}

/// Write through a temp file so a crash never leaves half a file, 0600 for credentials.
fn write_json(path: &std::path::Path, data: &Value, private: bool) {
    let write = || -> std::io::Result<()> {
        if let Some(dir) = path.parent() {
            std::fs::create_dir_all(dir)?;
        }
        let tmp = path.with_extension("json.tmp");
        std::fs::write(&tmp, serde_json::to_vec_pretty(data).map_err(std::io::Error::other)?)?;
        #[cfg(unix)]
        if private {
            use std::os::unix::fs::PermissionsExt;
            let _ = std::fs::set_permissions(&tmp, std::fs::Permissions::from_mode(0o600));
        }
        #[cfg(not(unix))]
        let _ = private;
        std::fs::rename(&tmp, path)
    };
    if let Err(err) = write() {
        tracing::warn!(?path, %err, "scrobbler file write failed");
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn current(duration: f64) -> Current {
        Current {
            video_id: "vid".into(),
            track: "Song".into(),
            artist: "Artist".into(),
            album: String::new(),
            duration,
            timestamp: None,
            elapsed: Duration::ZERO,
            playing_since: None,
            scrobbled: false,
            now_playing_at: None,
        }
    }

    #[test]
    fn signature_matches_the_lastfm_recipe() {
        let mut params = BTreeMap::new();
        params.insert("method".to_owned(), "auth.getSession".to_owned());
        params.insert("api_key".to_owned(), "key".to_owned());
        params.insert("token".to_owned(), "tok".to_owned());
        params.insert("format".to_owned(), "json".to_owned());
        // md5("api_keykeymethodauth.getSessiontokentoksecret")
        assert_eq!(sign(&params, "secret"), format!("{:x}", Md5::digest(b"api_keykeymethodauth.getSessiontokentoksecret")));
    }

    #[test]
    fn threshold_is_half_the_track_capped_at_four_minutes() {
        assert_eq!(threshold(200.0), 100.0);
        assert_eq!(threshold(1000.0), 240.0);
        assert_eq!(threshold(0.0), 120.0);
    }

    #[test]
    fn a_play_scrobbles_once_after_half_the_track() {
        let start = Instant::now();
        let mut cur = current(100.0);
        let first = tick(&mut cur, 100.0, true, start);
        assert!(first.now_playing.is_some(), "the first playing tick announces the track");
        assert!(first.scrobble.is_none());
        assert!(tick(&mut cur, 100.0, true, start + Duration::from_secs(49)).scrobble.is_none());
        let due = tick(&mut cur, 100.0, true, start + Duration::from_secs(50));
        assert_eq!(due.scrobble.as_ref().map(|e| e.track.as_str()), Some("Song"));
        assert!(tick(&mut cur, 100.0, true, start + Duration::from_secs(60)).scrobble.is_none(), "once per play");
    }

    #[test]
    fn paused_time_does_not_count() {
        let start = Instant::now();
        let mut cur = current(100.0);
        tick(&mut cur, 100.0, true, start);
        tick(&mut cur, 100.0, false, start + Duration::from_secs(30));
        // Ten minutes paused, then playing again: 30 s listened so far.
        let resumed = start + Duration::from_secs(630);
        assert!(tick(&mut cur, 100.0, true, resumed).scrobble.is_none());
        assert!(tick(&mut cur, 100.0, true, resumed + Duration::from_secs(19)).scrobble.is_none());
        assert!(tick(&mut cur, 100.0, true, resumed + Duration::from_secs(20)).scrobble.is_some());
    }

    #[test]
    fn now_playing_refreshes_every_thirty_seconds() {
        let start = Instant::now();
        let mut cur = current(600.0);
        assert!(tick(&mut cur, 600.0, true, start).now_playing.is_some());
        assert!(tick(&mut cur, 600.0, true, start + Duration::from_secs(30)).now_playing.is_none());
        assert!(tick(&mut cur, 600.0, true, start + Duration::from_secs(31)).now_playing.is_some());
    }

    #[test]
    fn a_late_duration_raises_the_threshold() {
        let start = Instant::now();
        let mut cur = current(0.0);
        tick(&mut cur, 0.0, true, start);
        // The pipeline reports 400 s, so half is 200 s and not the flat 120 s.
        assert!(tick(&mut cur, 400.0, true, start + Duration::from_secs(150)).scrobble.is_none());
        assert!(tick(&mut cur, 400.0, true, start + Duration::from_secs(200)).scrobble.is_some());
    }

    #[test]
    fn lastfm_errors_sort_by_code() {
        assert!(matches!(classify_lastfm(json!({"error": 9, "message": "bad session"})), Err(Auth(_))));
        assert!(matches!(classify_lastfm(json!({"error": 29, "message": "slow down"})), Err(Transient(_))));
        assert!(matches!(classify_lastfm(json!({"error": 6, "message": "bad params"})), Err(Permanent(_))));
        assert!(classify_lastfm(json!({"scrobbles": {}})).is_ok());
    }

    #[test]
    fn discarded_listens_are_reported_with_their_reason() {
        let one = json!({"scrobbles": {"scrobble": {
            "artist": {"#text": "A"}, "track": {"#text": "T"}, "ignoredMessage": {"code": "3", "#text": ""}
        }}});
        assert_eq!(ignored_listens(&one), vec![("A".to_owned(), "T".to_owned(), "timestamp too old".to_owned())]);
        let many = json!({"scrobbles": {"scrobble": [
            {"artist": {"#text": "A"}, "track": {"#text": "T"}, "ignoredMessage": {"code": "0", "#text": ""}},
            {"artist": {"#text": "B"}, "track": {"#text": "U"}, "ignoredMessage": {"code": "1", "#text": ""}},
        ]}});
        assert_eq!(ignored_listens(&many).len(), 1);
        let now_playing = json!({"nowplaying": {"artist": {"#text": "A"}, "track": {"#text": "T"}, "ignoredMessage": {"code": "0", "#text": ""}}});
        assert!(ignored_listens(&now_playing).is_empty());
    }

    #[test]
    fn listenbrainz_body_picks_single_or_import() {
        let entry = Entry { artist: "A".into(), track: "T".into(), album: "L".into(), video_id: "v".into(), timestamp: 10, duration: 200, attempts: 0 };
        let single = listenbrainz_body(std::slice::from_ref(&entry));
        assert_eq!(single["listen_type"], "single");
        assert_eq!(single["payload"][0]["listened_at"], 10);
        let meta = &single["payload"][0]["track_metadata"];
        assert_eq!(meta["release_name"], "L");
        assert_eq!(meta["additional_info"]["duration_ms"], 200_000);
        assert_eq!(meta["additional_info"]["origin_url"], "https://music.youtube.com/watch?v=v");
        assert_eq!(listenbrainz_body(&[entry.clone(), entry])["listen_type"], "import");
    }

    #[test]
    fn the_queue_file_round_trips_in_the_python_shape() {
        let dir = tempfile::tempdir().unwrap();
        let paths = Paths::for_tests(dir.path());
        std::fs::write(
            pending_path(&paths),
            r#"{"lastfm": [{"artist": "A", "track": "T", "album": "", "video_id": "v", "timestamp": 5, "duration": 0, "attempts": 2}], "listenbrainz": []}"#,
        )
        .unwrap();
        let pending = load_pending(&paths);
        assert_eq!(pending["lastfm"][0].attempts, 2);
        assert!(pending["listenbrainz"].is_empty());
        save_pending(&paths, &pending);
        assert_eq!(load_pending(&paths), pending);
    }

    #[test]
    fn lastfm_needs_a_session_key_and_app_credentials() {
        let creds = json!({"lastfm": {"session_key": "sk"}, "listenbrainz": {"token": "tk"}});
        let creds = creds.as_object().unwrap();
        assert_eq!(credential_of(creds, Service::LastFm, true), "sk");
        assert_eq!(credential_of(creds, Service::LastFm, false), "");
        assert_eq!(credential_of(creds, Service::ListenBrainz, false), "tk");
    }
}

//! Session and endpoint layer on top of the `ytmusicapi` crate.
//!
//! The crate owns the InnerTube transport: browser-cookie auth with the
//! SAPISIDHASH header, the WEB_REMIX context, and `send_request`. This module
//! owns what the app needs around it: the auth state machine published on a
//! watch channel, the headers_auth.json file the Python app wrote, header
//! normalization, the media auth snapshot for GStreamer and yt-dlp, and the
//! ratings cache. Endpoint parsers the crate lacks live in sibling modules.

use std::collections::{BTreeMap, HashMap};
use std::path::PathBuf;
use std::sync::{Arc, RwLock};

use serde::{Deserialize, Serialize};
use serde_json::{Value, json};
use tokio::sync::watch;
use ytmusicapi::{BrowserAuth, YTMusicClient};

use crate::model::{HttpAuth, LikeStatus};
use crate::paths::Paths;

const LOGIN_USER_AGENT: &str = "Mozilla/5.0 (Windows NT 10.0; Win64; x64) AppleWebKit/537.36 (KHTML, like Gecko) Chrome/115.0.0.0 Safari/537.36";

#[derive(Debug, thiserror::Error)]
pub enum NetError {
    #[error("network: {0}")]
    Transport(#[from] reqwest::Error),
    #[error("{0}")]
    Api(#[from] ytmusicapi::Error),
    #[error("HTTP {status}: {message}")]
    Http { status: u16, message: String },
    #[error("not signed in")]
    Unauthenticated,
    #[error("invalid auth input: {0}")]
    InvalidAuth(String),
    #[error("{0}")]
    Message(String),
    #[error(transparent)]
    Io(#[from] std::io::Error),
    #[error(transparent)]
    Json(#[from] serde_json::Error),
}

impl NetError {
    /// HTTP status carried by an API server error, if any.
    pub fn status(&self) -> Option<u16> {
        match self {
            NetError::Api(ytmusicapi::Error::Server { status, .. }) => Some(*status),
            NetError::Http { status, .. } => Some(*status),
            _ => None,
        }
    }
}

#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct AccountInfo {
    pub name: String,
    pub handle: Option<String>,
    pub photo_url: Option<String>,
}

/// Authentication state machine.
/// Anonymous: no saved headers. Unverified: headers loaded, no round trip yet.
/// Authenticated: server confirmed the session. Invalid: server rejected it.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum AuthState {
    Anonymous,
    Unverified,
    Authenticated(AccountInfo),
    Invalid(String),
}

impl AuthState {
    pub fn is_authenticated(&self) -> bool {
        matches!(self, AuthState::Authenticated(_))
    }

    /// True whenever we have headers worth sending, verified or not.
    pub fn has_session(&self) -> bool {
        matches!(self, AuthState::Authenticated(_) | AuthState::Unverified)
    }
}

#[derive(Default)]
struct Session {
    /// Normalized browser headers, Title-Case keys. Empty when anonymous.
    headers: BTreeMap<String, String>,
    browser_auth: Option<BrowserAuth>,
}

pub struct YtMusic {
    /// App-side HTTP client for cover art and other plain fetches.
    http: reqwest::Client,
    auth_file: PathBuf,
    /// The crate client, rebuilt on login and logout since it is immutable.
    api: RwLock<Arc<YTMusicClient>>,
    session: RwLock<Session>,
    auth: watch::Sender<AuthState>,
    /// Ratings seen or set this session, keyed by video id. Mirrors MusicClient._known_likes.
    known_likes: RwLock<HashMap<String, LikeStatus>>,
}

impl YtMusic {
    pub fn new(paths: &Paths) -> anyhow::Result<Arc<Self>> {
        let http = reqwest::Client::builder().timeout(std::time::Duration::from_secs(30)).gzip(true).build()?;
        let (auth, _) = watch::channel(AuthState::Anonymous);
        let anonymous = YTMusicClient::builder().build()?;
        let client = Arc::new(Self {
            http,
            auth_file: paths.auth_file.clone(),
            api: RwLock::new(Arc::new(anonymous)),
            session: RwLock::new(Session::default()),
            auth,
            known_likes: RwLock::new(HashMap::new()),
        });
        client.load_saved_session();
        Ok(client)
    }

    /// Shared HTTP client for non-InnerTube fetches such as cover art.
    pub fn http(&self) -> &reqwest::Client {
        &self.http
    }

    /// The `ytmusicapi` client for the current session.
    pub fn api(&self) -> Arc<YTMusicClient> {
        self.api.read().unwrap().clone()
    }

    // -- auth state ------------------------------------------------------

    pub fn auth_state(&self) -> AuthState {
        self.auth.borrow().clone()
    }

    pub fn subscribe_auth(&self) -> watch::Receiver<AuthState> {
        self.auth.subscribe()
    }

    pub fn is_authenticated(&self) -> bool {
        self.auth.borrow().is_authenticated()
    }

    fn publish(&self, state: AuthState) {
        self.auth.send_replace(state);
    }

    /// Cookie, UA and authorization snapshot for GStreamer and yt-dlp. None when anonymous.
    pub fn media_auth(&self) -> Option<HttpAuth> {
        if !self.auth.borrow().has_session() {
            return None;
        }
        let session = self.session.read().unwrap();
        let cookie = session.headers.get("Cookie")?.clone();
        let user_agent = session.headers.get("User-Agent").cloned().unwrap_or_else(|| LOGIN_USER_AGENT.to_owned());
        let authorization = session.browser_auth.as_ref().and_then(|a| a.get_authorization().ok());
        Some(HttpAuth { cookie, user_agent, authorization })
    }

    /// Load headers_auth.json the way MusicClient.try_login(skip_validation=True) did.
    fn load_saved_session(&self) {
        let Ok(text) = std::fs::read_to_string(&self.auth_file) else {
            self.publish(AuthState::Anonymous);
            return;
        };
        match serde_json::from_str::<BTreeMap<String, String>>(&text) {
            Ok(raw) => match self.install_headers(normalize_headers(raw)) {
                Ok(()) => {
                    tracing::info!(path = %self.auth_file.display(), "saved session loaded, validation deferred");
                    self.publish(AuthState::Unverified);
                }
                Err(err) => {
                    tracing::warn!(%err, "saved session unusable");
                    self.publish(AuthState::Anonymous);
                }
            },
            Err(err) => {
                tracing::warn!(%err, "headers_auth.json unreadable");
                self.publish(AuthState::Anonymous);
            }
        }
    }

    /// Build a signed-in crate client from normalized headers and remember them.
    fn install_headers(&self, headers: BTreeMap<String, String>) -> Result<(), NetError> {
        let browser_auth = BrowserAuth::from_json(&serde_json::to_string(&headers)?).map_err(|e| NetError::InvalidAuth(e.to_string()))?;
        browser_auth.sapisid().map_err(|e| NetError::InvalidAuth(e.to_string()))?;
        let client = YTMusicClient::builder().with_browser_auth(browser_auth.clone()).build()?;
        *self.api.write().unwrap() = Arc::new(client);
        let mut session = self.session.write().unwrap();
        session.headers = headers;
        session.browser_auth = Some(browser_auth);
        Ok(())
    }

    /// Confirm the loaded session with the server. Offline keeps Unverified.
    pub async fn validate(&self) -> Result<AuthState, NetError> {
        if !self.auth.borrow().has_session() {
            return Ok(self.auth_state());
        }
        match self.account_info().await {
            Ok(Some(info)) => {
                tracing::info!(name = %info.name, "session validated");
                self.publish(AuthState::Authenticated(info));
            }
            Ok(None) => {
                tracing::warn!("session rejected: account menu has no active account");
                self.publish(AuthState::Invalid("session expired".into()));
            }
            Err(err) if matches!(err.status(), Some(401) | Some(403)) => {
                self.publish(AuthState::Invalid(err.to_string()));
            }
            Err(err) => {
                tracing::warn!(%err, "validation skipped, keeping cached session");
                return Err(err);
            }
        }
        Ok(self.auth_state())
    }

    /// Accept a browser.json path, a JSON object string, or a raw "Key: value" header block.
    pub async fn login(&self, input: &str) -> Result<AccountInfo, NetError> {
        let raw = parse_auth_input(input)?;
        let mut headers = normalize_headers(raw);
        headers.entry("User-Agent".into()).or_insert_with(|| LOGIN_USER_AGENT.to_owned());
        headers.insert("Accept-Language".into(), "en-US,en;q=0.9".into());
        headers.entry("Content-Type".into()).or_insert_with(|| "application/json; charset=UTF-8".into());

        self.install_headers(headers.clone())?;
        self.publish(AuthState::Unverified);
        match self.account_info().await? {
            Some(info) => {
                if let Some(parent) = self.auth_file.parent() {
                    tokio::fs::create_dir_all(parent).await?;
                }
                tokio::fs::write(&self.auth_file, serde_json::to_vec(&headers)?).await?;
                self.publish(AuthState::Authenticated(info.clone()));
                Ok(info)
            }
            None => {
                self.clear_session();
                Err(NetError::InvalidAuth("server did not accept these headers".into()))
            }
        }
    }

    pub async fn logout(&self) {
        let _ = tokio::fs::remove_file(&self.auth_file).await;
        self.clear_session();
    }

    fn clear_session(&self) {
        if let Ok(anonymous) = YTMusicClient::builder().build() {
            *self.api.write().unwrap() = Arc::new(anonymous);
        }
        *self.session.write().unwrap() = Session::default();
        self.known_likes.write().unwrap().clear();
        self.publish(AuthState::Anonymous);
    }

    // -- ratings ---------------------------------------------------------

    pub fn known_like_status(&self, video_id: &str) -> Option<LikeStatus> {
        self.known_likes.read().unwrap().get(video_id).copied()
    }

    pub fn set_known_like_status(&self, video_id: &str, status: LikeStatus) {
        self.known_likes.write().unwrap().insert(video_id.to_owned(), status);
    }

    /// Thumbs up, thumbs down, or clear, through the crate's rate_song.
    pub async fn rate_song(&self, video_id: &str, status: LikeStatus) -> Result<(), NetError> {
        if !self.auth.borrow().has_session() {
            return Err(NetError::Unauthenticated);
        }
        let rating = match status {
            LikeStatus::Like => ytmusicapi::LikeStatus::Like,
            LikeStatus::Dislike => ytmusicapi::LikeStatus::Dislike,
            LikeStatus::Indifferent => ytmusicapi::LikeStatus::Indifferent,
        };
        self.api().rate_song(video_id, rating).await?;
        Ok(())
    }

    // -- transport -------------------------------------------------------

    /// POST to an InnerTube endpoint through the crate, context merged in.
    pub async fn post(&self, endpoint: &str, body: Value) -> Result<Value, NetError> {
        Ok(self.api().send_request(endpoint, body).await?)
    }

    /// The signed-in account, or None when the server treats us as anonymous.
    pub async fn account_info(&self) -> Result<Option<AccountInfo>, NetError> {
        let resp = self.post("account/account_menu", json!({})).await?;
        let header = resp.pointer("/actions/0/openPopupAction/popup/multiPageMenuRenderer/header/activeAccountHeaderRenderer");
        let Some(header) = header else { return Ok(None) };
        let name = header.pointer("/accountName/runs/0/text").and_then(Value::as_str).unwrap_or_default().to_owned();
        if name.is_empty() {
            return Ok(None);
        }
        let handle = header.pointer("/channelHandle/runs/0/text").and_then(Value::as_str).map(str::to_owned);
        let photo_url = header
            .pointer("/accountPhoto/thumbnails")
            .and_then(Value::as_array)
            .and_then(|t| t.last())
            .and_then(|t| t.get("url"))
            .and_then(Value::as_str)
            .map(str::to_owned);
        Ok(Some(AccountInfo { name, handle, photo_url }))
    }
}

// -- helpers -------------------------------------------------------------

/// Port of MusicClient._normalize_headers: Title-Case the known keys and drop OAuth material.
pub fn normalize_headers(raw: BTreeMap<String, String>) -> BTreeMap<String, String> {
    const DROP: &[&str] = &["oauth_credentials", "client_id", "client_secret", "access_token", "refresh_token", "token_type", "expires_at", "expires_in"];
    let mut out = BTreeMap::new();
    for (k, v) in raw {
        let lk = k.to_ascii_lowercase().replace('-', "_");
        match lk.as_str() {
            "cookie" => out.insert("Cookie".to_owned(), v),
            "user_agent" => out.insert("User-Agent".to_owned(), v),
            "accept_language" => out.insert("Accept-Language".to_owned(), v),
            "content_type" => out.insert("Content-Type".to_owned(), v),
            "x_goog_authuser" => out.insert("X-Goog-AuthUser".to_owned(), v),
            "x_goog_visitor_id" => out.insert("X-Goog-Visitor-Id".to_owned(), v),
            "authorization" if v.to_ascii_lowercase().starts_with("bearer") => None,
            "authorization" => out.insert("Authorization".to_owned(), v),
            _ if DROP.contains(&lk.as_str()) => None,
            _ if lk.starts_with("x_") => out.insert(k, v),
            _ => out.insert(title_case(&k), v),
        };
    }
    out.entry("Accept-Language".into()).or_insert_with(|| "en-US,en;q=0.9".into());
    out.entry("Content-Type".into()).or_insert_with(|| "application/json".into());
    out
}

fn title_case(key: &str) -> String {
    key.split('-')
        .map(|part| {
            let mut chars = part.chars();
            match chars.next() {
                Some(first) => first.to_ascii_uppercase().to_string() + &chars.as_str().to_ascii_lowercase(),
                None => String::new(),
            }
        })
        .collect::<Vec<_>>()
        .join("-")
}

fn parse_auth_input(input: &str) -> Result<BTreeMap<String, String>, NetError> {
    let trimmed = input.trim();
    let path = std::path::Path::new(trimmed);
    if path.is_file() {
        let text = std::fs::read_to_string(path)?;
        return Ok(serde_json::from_str(&text)?);
    }
    if trimmed.starts_with('{') {
        return Ok(serde_json::from_str(trimmed)?);
    }
    // Raw request headers copied from the browser's network panel.
    let mut map = BTreeMap::new();
    for line in trimmed.lines() {
        if let Some((k, v)) = line.split_once(':') {
            let k = k.trim_start_matches(':').trim();
            if !k.is_empty() {
                map.insert(k.to_owned(), v.trim().to_owned());
            }
        }
    }
    if map.keys().any(|k| k.eq_ignore_ascii_case("cookie")) {
        Ok(map)
    } else {
        Err(NetError::InvalidAuth("no Cookie header found".into()))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn header_normalization_drops_oauth() {
        let mut raw = BTreeMap::new();
        raw.insert("cookie".into(), "c".into());
        raw.insert("authorization".into(), "Bearer t".into());
        raw.insert("access_token".into(), "t".into());
        raw.insert("x-goog-authuser".into(), "0".into());
        let out = normalize_headers(raw);
        assert_eq!(out.get("Cookie").map(String::as_str), Some("c"));
        assert!(!out.contains_key("Authorization"));
        assert!(!out.contains_key("access_token"));
        assert_eq!(out.get("X-Goog-AuthUser").map(String::as_str), Some("0"));
    }

    #[test]
    fn crate_auth_accepts_normalized_headers() {
        let mut raw = BTreeMap::new();
        raw.insert("cookie".into(), "a=1; __Secure-3PAPISID=xyz".into());
        raw.insert("x-goog-authuser".into(), "1".into());
        let headers = normalize_headers(raw);
        let auth = BrowserAuth::from_json(&serde_json::to_string(&headers).unwrap()).unwrap();
        assert_eq!(auth.sapisid().unwrap(), "xyz");
        assert_eq!(auth.x_goog_authuser, "1");
    }
}

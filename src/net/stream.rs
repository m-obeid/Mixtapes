//! Video id to playable URI.
//!
//! `StreamResolver` is the seam that lets the port ship before a native
//! InnerTube player endpoint exists. `YtDlpResolver` shells out to the same
//! yt-dlp the Python app embeds, with the same format policy. A native
//! resolver later implements the same trait and nothing above it changes.
//! Both go through `StreamCache`, which is file-compatible with the Python one.

use std::collections::HashMap;
use std::future::Future;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::pin::Pin;
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use serde::{Deserialize, Serialize};
use tokio::process::Command;

use crate::model::{HttpAuth, StreamInfo, VideoId};
use crate::paths::Paths;

pub type BoxFuture<'a, T> = Pin<Box<dyn Future<Output = T> + Send + 'a>>;

#[derive(Debug, thiserror::Error)]
pub enum ResolveError {
    #[error("{0}")]
    Unavailable(String),
    #[error("resolver failed: {0}")]
    Tool(String),
    #[error(transparent)]
    Io(#[from] std::io::Error),
    #[error(transparent)]
    Json(#[from] serde_json::Error),
}

pub trait StreamResolver: Send + Sync {
    fn resolve(&self, video_id: VideoId, auth: Option<HttpAuth>) -> BoxFuture<'_, Result<StreamInfo, ResolveError>>;
    /// Drop a cached URI after it failed mid-play.
    fn invalidate(&self, video_id: &VideoId) -> BoxFuture<'_, ()>;
}

// Same policy as ydl_opts in player.py.
const YTDLP_FORMAT: &str = "bestaudio[acodec=opus]/bestaudio[protocol=https]/bestaudio[protocol=http]/bestaudio/best";
const YTDLP_FORMAT_SORT: &str = "proto:https,acodec:opus";
const YTDLP_PLAYER_CLIENTS: &str = "youtube:player_client=web_music,mweb,tv,web_safari,android_vr,android,ios";

pub struct YtDlpResolver {
    binary: PathBuf,
    tokens: Arc<crate::net::potoken::PoTokens>,
    tmp_dir: PathBuf,
    cache: StreamCache,
}

impl YtDlpResolver {
    pub fn new(paths: &Paths, tokens: Arc<crate::net::potoken::PoTokens>) -> Self {
        let binary = find_executable("yt-dlp").unwrap_or_else(|| PathBuf::from("yt-dlp"));
        Self { binary, tokens, tmp_dir: paths.cache_dir.clone(), cache: StreamCache::new(paths.stream_cache_dir.clone()) }
    }

    async fn run(&self, video_id: &VideoId, auth: Option<&HttpAuth>) -> Result<StreamInfo, ResolveError> {
        let url = format!("https://music.youtube.com/watch?v={video_id}");
        let mut cmd = Command::new(&self.binary);
        cmd.args(["-j", "--no-playlist", "--no-warnings", "-f", YTDLP_FORMAT, "-S", YTDLP_FORMAT_SORT])
            .args(["--extractor-args", YTDLP_PLAYER_CLIENTS])
            .args(["--js-runtimes", "node"]);
        // YouTube serves the web_music client's formats only against a PO
        // token bound to the video. Uploaded songs come from no other client,
        // so without this they look unavailable, and ordinary songs lose their
        // seekable Opus formats.
        if let Some(token) = self.tokens.for_video(video_id.as_str()).await {
            cmd.arg("--extractor-args").arg(crate::net::potoken::extractor_arg(&token));
        }
        let cookie_file = match auth {
            Some(auth) => {
                let path = write_netscape_cookies(&self.tmp_dir, &auth.cookie).await?;
                cmd.arg("--cookies").arg(&path).arg("--user-agent").arg(&auth.user_agent);
                if let Some(a) = &auth.authorization {
                    cmd.arg("--add-headers").arg(format!("Authorization:{a}"));
                }
                Some(path)
            }
            None => None,
        };
        cmd.arg(&url).kill_on_drop(true).stdin(std::process::Stdio::null());

        let output = cmd.output().await;
        if let Some(path) = cookie_file {
            let _ = tokio::fs::remove_file(path).await;
        }
        let output = output?;
        if !output.status.success() {
            let stderr = String::from_utf8_lossy(&output.stderr);
            let line = stderr.lines().rev().find(|l| l.contains("ERROR")).unwrap_or("yt-dlp failed").trim();
            return Err(ResolveError::Unavailable(line.to_owned()));
        }

        #[derive(Deserialize)]
        struct Info {
            url: String,
            format_id: Option<String>,
            protocol: Option<String>,
            ext: Option<String>,
            acodec: Option<String>,
            title: Option<String>,
            uploader: Option<String>,
            thumbnail: Option<String>,
        }
        let info: Info = serde_json::from_slice(&output.stdout)?;
        Ok(StreamInfo {
            uri: info.url,
            format_id: info.format_id,
            protocol: info.protocol,
            ext: info.ext,
            acodec: info.acodec,
            title: info.title,
            uploader: info.uploader,
            thumbnail: info.thumbnail,
            is_local: false,
            from_cache: false,
        })
    }
}

impl StreamResolver for YtDlpResolver {
    fn resolve(&self, video_id: VideoId, auth: Option<HttpAuth>) -> BoxFuture<'_, Result<StreamInfo, ResolveError>> {
        Box::pin(async move {
            if let Some(uri) = self.cache.get(&video_id).await {
                return Ok(StreamInfo { uri, from_cache: true, ..StreamInfo::default() });
            }
            let info = self.run(&video_id, auth.as_ref()).await?;
            self.cache.put(&video_id, &info.uri).await;
            Ok(info)
        })
    }

    fn invalidate(&self, video_id: &VideoId) -> BoxFuture<'_, ()> {
        let video_id = video_id.clone();
        Box::pin(async move { self.cache.invalidate(&video_id).await })
    }
}

/// Serves fixed URIs for `demo:` ids and delegates everything else.
pub struct DemoResolver {
    inner: Arc<dyn StreamResolver>,
    uris: HashMap<String, String>,
}

impl DemoResolver {
    pub fn new(inner: Arc<dyn StreamResolver>, uris: HashMap<String, String>) -> Self {
        Self { inner, uris }
    }
}

impl StreamResolver for DemoResolver {
    fn resolve(&self, video_id: VideoId, auth: Option<HttpAuth>) -> BoxFuture<'_, Result<StreamInfo, ResolveError>> {
        if let Some(uri) = self.uris.get(video_id.as_str()).cloned() {
            let is_local = uri.starts_with("file://");
            return Box::pin(async move { Ok(StreamInfo { uri, is_local, ..StreamInfo::default() }) });
        }
        self.inner.resolve(video_id, auth)
    }

    fn invalidate(&self, video_id: &VideoId) -> BoxFuture<'_, ()> {
        self.inner.invalidate(video_id)
    }
}

/// Disk cache of resolved URIs. Layout matches player/cache.py: `<id>.json` with url + timestamp.
pub struct StreamCache {
    dir: PathBuf,
    puts: std::sync::atomic::AtomicUsize,
}

#[derive(Serialize, Deserialize)]
struct CacheEntry {
    url: String,
    timestamp: f64,
}

impl StreamCache {
    const MAX_ENTRIES: usize = 500;
    const EVICT_EVERY: usize = 20;
    const TTL: Duration = Duration::from_secs(5 * 3600);

    pub fn new(dir: PathBuf) -> Self {
        Self { dir, puts: std::sync::atomic::AtomicUsize::new(0) }
    }

    fn path_for(&self, video_id: &VideoId) -> PathBuf {
        self.dir.join(format!("{video_id}.json"))
    }

    pub async fn get(&self, video_id: &VideoId) -> Option<String> {
        let path = self.path_for(video_id);
        let bytes = tokio::fs::read(&path).await.ok()?;
        let entry: CacheEntry = serde_json::from_slice(&bytes).ok()?;
        let age = now_secs() - entry.timestamp;
        if age > Self::TTL.as_secs_f64() {
            let _ = tokio::fs::remove_file(&path).await;
            return None;
        }
        // Touch for LRU eviction.
        let _ = tokio::task::spawn_blocking(move || std::fs::File::open(&path).and_then(|f| f.set_modified(SystemTime::now()))).await;
        Some(entry.url)
    }

    pub async fn put(&self, video_id: &VideoId, url: &str) {
        let entry = CacheEntry { url: url.to_owned(), timestamp: now_secs() };
        if let Ok(bytes) = serde_json::to_vec(&entry) {
            if let Err(err) = tokio::fs::write(self.path_for(video_id), bytes).await {
                tracing::warn!(%err, %video_id, "stream cache write failed");
            }
        }
        // A scan of 500 files per resolved track is waste. Every so often holds the cap.
        if self.puts.fetch_add(1, std::sync::atomic::Ordering::Relaxed) % Self::EVICT_EVERY == 0 {
            let dir = self.dir.clone();
            let _ = tokio::task::spawn_blocking(move || evict_old(&dir, Self::MAX_ENTRIES)).await;
        }
    }

    pub async fn invalidate(&self, video_id: &VideoId) {
        let _ = tokio::fs::remove_file(self.path_for(video_id)).await;
    }
}

fn evict_old(dir: &Path, keep: usize) {
    let Ok(read) = std::fs::read_dir(dir) else { return };
    let mut entries: Vec<(SystemTime, PathBuf)> = read
        .filter_map(|e| e.ok())
        .filter(|e| e.path().extension().is_some_and(|x| x == "json"))
        .filter_map(|e| Some((e.metadata().ok()?.modified().ok()?, e.path())))
        .collect();
    if entries.len() <= keep {
        return;
    }
    entries.sort();
    let excess = entries.len() - keep;
    for (_, path) in entries.into_iter().take(excess) {
        let _ = std::fs::remove_file(path);
    }
}

fn now_secs() -> f64 {
    SystemTime::now().duration_since(UNIX_EPOCH).map(|d| d.as_secs_f64()).unwrap_or(0.0)
}

/// Netscape cookie jar for yt-dlp, created 0600 and removed after the run.
pub async fn write_netscape_cookies(dir: &Path, cookie_header: &str) -> std::io::Result<PathBuf> {
    let nanos = SystemTime::now().duration_since(UNIX_EPOCH).map(|d| d.as_nanos()).unwrap_or(0);
    let path = dir.join(format!("ytdlp-cookies-{}-{nanos}.txt", std::process::id()));
    let expiry = now_secs() as u64 + 365 * 24 * 3600;
    let mut body = String::from("# Netscape HTTP Cookie File\n");
    for part in cookie_header.split(';') {
        if let Some((k, v)) = part.trim().split_once('=') {
            body.push_str(&format!(".youtube.com\tTRUE\t/\tTRUE\t{expiry}\t{k}\t{v}\n"));
        }
    }
    let write_path = path.clone();
    tokio::task::spawn_blocking(move || {
        use std::io::Write;
        let mut opts = std::fs::OpenOptions::new();
        opts.write(true).create_new(true);
        #[cfg(unix)]
        {
            use std::os::unix::fs::OpenOptionsExt;
            opts.mode(0o600);
        }
        opts.open(&write_path)?.write_all(body.as_bytes())
    })
    .await
    .map_err(|e| std::io::Error::other(e.to_string()))??;
    Ok(path)
}

/// PATH lookup plus the install spots the Python app checked.
pub fn find_executable(name: &str) -> Option<PathBuf> {
    let mut candidates: Vec<PathBuf> = std::env::var_os("PATH")
        .map(|p| std::env::split_paths(&p).map(|d| d.join(name)).collect())
        .unwrap_or_default();
    if let Some(home) = std::env::var_os("HOME") {
        candidates.push(PathBuf::from(home).join(".cargo/bin").join(name));
    }
    candidates.push(PathBuf::from("/usr/lib/mixtapes/bin").join(name));
    candidates.push(PathBuf::from("/usr/local/bin").join(name));
    candidates.push(PathBuf::from("/usr/bin").join(name));
    candidates.into_iter().find(|p| p.is_file())
}

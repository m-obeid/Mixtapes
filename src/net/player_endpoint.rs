//! Native stream resolution through InnerTube's `player` endpoint.
//!
//! yt-dlp took about six seconds per cold play: a Python start, seven player
//! clients in turn, player.js and a node runtime for the signature. The
//! VISIONOS client answers one request in about 0.2 s with direct URLs that
//! need no signature, no PO token and no cookies, and serve open-ended ranges,
//! so GStreamer can seek them. Measured 2026-09-18: ANDROID_VR URLs stop after
//! the first 100 KB without a token that botguard could not satisfy, VISIONOS
//! URLs serve the whole file.
//!
//! Anything this client will not serve (uploads, age-gated or private videos)
//! falls through to the wrapped resolver, which is yt-dlp with the session.

use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use regex::Regex;
use serde_json::{Value, json};

use crate::model::{HttpAuth, StreamInfo, VideoId};
use crate::net::stream::{BoxFuture, ResolveError, StreamCache, StreamResolver};
use crate::paths::Paths;

const PLAYER_URL: &str = "https://www.youtube.com/youtubei/v1/player?prettyPrint=false";
const CLIENT_NAME: &str = "VISIONOS";
const CLIENT_ID: &str = "101";
const CLIENT_VERSION: &str = "0.1";
const CLIENT_USER_AGENT: &str = "Mozilla/5.0 (Macintosh; Intel Mac OS X 10_15_7) AppleWebKit/605.1.15 (KHTML, like Gecko) Version/18.0 Safari/605.1.15";
const LANDING_URL: &str = "https://music.youtube.com/";
const LANDING_USER_AGENT: &str = "Mozilla/5.0 (X11; Linux x86_64; rv:128.0) Gecko/20100101 Firefox/128.0";
const REQUEST_TIMEOUT: Duration = Duration::from_secs(8);
/// A visitor id keeps working for days. Refetched well before that.
const VISITOR_TTL: Duration = Duration::from_secs(6 * 3600);
/// Past the token-less preview every client gets, so a URL that would die mid-song fails here.
const PROBE_OFFSET: u64 = 200_000;

/// An audio format as the player response lists it.
#[derive(Clone, Debug, PartialEq)]
struct Format {
    itag: u32,
    url: String,
    mime: String,
    bitrate: u64,
    content_length: u64,
}

impl Format {
    fn is_opus(&self) -> bool {
        self.mime.contains("opus")
    }
}

/// Audio formats that carry a plain URL. A `signatureCipher` entry needs player.js and is left out.
fn audio_formats(response: &Value) -> Vec<Format> {
    let Some(list) = response.pointer("/streamingData/adaptiveFormats").and_then(Value::as_array) else { return Vec::new() };
    list.iter()
        .filter_map(|f| {
            let mime = f.get("mimeType")?.as_str()?;
            if !mime.starts_with("audio/") {
                return None;
            }
            // A dubbed or DRC track is not the master.
            if f.get("isDrc").and_then(Value::as_bool).unwrap_or(false) || f.pointer("/audioTrack/audioIsDefault").and_then(Value::as_bool) == Some(false) {
                return None;
            }
            Some(Format {
                itag: f.get("itag")?.as_u64()? as u32,
                url: f.get("url")?.as_str()?.to_owned(),
                mime: mime.to_owned(),
                bitrate: f.get("bitrate").and_then(Value::as_u64).unwrap_or(0),
                content_length: f.get("contentLength").and_then(Value::as_str).and_then(|s| s.parse().ok()).unwrap_or(0),
            })
        })
        .collect()
}

/// Same policy as the yt-dlp format string: opus first, then the highest bitrate.
fn pick_format(formats: &[Format]) -> Option<&Format> {
    formats.iter().max_by_key(|f| (f.is_opus(), f.bitrate))
}

/// Why the player refused, in the words it used. None when it is playable.
fn refusal(response: &Value) -> Option<String> {
    let status = response.pointer("/playabilityStatus/status").and_then(Value::as_str).unwrap_or("missing");
    if status == "OK" {
        return None;
    }
    let reason = response.pointer("/playabilityStatus/reason").and_then(Value::as_str).unwrap_or_default();
    Some(format!("{status} {reason}").trim().to_owned())
}

fn player_body(video_id: &str, visitor: &str) -> Value {
    json!({
        "context": { "client": {
            "clientName": CLIENT_NAME,
            "clientVersion": CLIENT_VERSION,
            "deviceMake": "Apple",
            "deviceModel": "RealityDevice14,1",
            "osName": "visionOS",
            "osVersion": "1.3.21O771",
            "hl": "en",
            "gl": "US",
            "visitorData": visitor,
        }},
        "videoId": video_id,
        "contentCheckOk": true,
        "racyCheckOk": true,
    })
}

fn visitor_from_landing(html: &str) -> Option<String> {
    static PATTERN: std::sync::LazyLock<Regex> = std::sync::LazyLock::new(|| Regex::new(r#""VISITOR_DATA":"([^"]+)""#).expect("static regex"));
    PATTERN.captures(html).map(|c| c[1].to_owned())
}

pub struct PlayerEndpointResolver {
    http: reqwest::Client,
    fallback: Arc<dyn StreamResolver>,
    cache: StreamCache,
    visitor: Mutex<Option<(String, Instant)>>,
}

impl PlayerEndpointResolver {
    pub fn new(paths: &Paths, fallback: Arc<dyn StreamResolver>) -> Self {
        let http = reqwest::Client::builder().timeout(REQUEST_TIMEOUT).gzip(true).build().unwrap_or_default();
        Self { http, fallback, cache: StreamCache::new(paths.stream_cache_dir.clone()), visitor: Mutex::new(None) }
    }

    /// Fetch the visitor id ahead of the first play.
    pub async fn warm(&self) {
        if let Err(err) = self.visitor_data().await {
            tracing::debug!(%err, "visitor data not warmed");
        }
    }

    /// Without a visitor id the endpoint answers "Sign in to confirm you're not a bot".
    async fn visitor_data(&self) -> Result<String, String> {
        if let Some((visitor, at)) = self.visitor.lock().unwrap().clone() {
            if at.elapsed() < VISITOR_TTL {
                return Ok(visitor);
            }
        }
        let html = self.http.get(LANDING_URL).header("User-Agent", LANDING_USER_AGENT).header("Cookie", "SOCS=CAI").send().await.map_err(|e| e.to_string())?.text().await.map_err(|e| e.to_string())?;
        let visitor = visitor_from_landing(&html).ok_or("no visitor data on the landing page")?;
        *self.visitor.lock().unwrap() = Some((visitor.clone(), Instant::now()));
        Ok(visitor)
    }

    async fn resolve_native(&self, video_id: &VideoId) -> Result<StreamInfo, String> {
        let visitor = self.visitor_data().await?;
        let response: Value = self
            .http
            .post(PLAYER_URL)
            .header("User-Agent", CLIENT_USER_AGENT)
            .header("X-YouTube-Client-Name", CLIENT_ID)
            .header("X-YouTube-Client-Version", CLIENT_VERSION)
            .header("X-Goog-Visitor-Id", &visitor)
            .json(&player_body(video_id.as_str(), &visitor))
            .send()
            .await
            .map_err(|e| e.to_string())?
            .json()
            .await
            .map_err(|e| e.to_string())?;
        if let Some(reason) = refusal(&response) {
            // A stale visitor id reads as a bot. The next resolve fetches a new one.
            if reason.contains("bot") {
                *self.visitor.lock().unwrap() = None;
            }
            return Err(reason);
        }
        // A response for another video means the id was redirected, which the queue did not ask for.
        if response.pointer("/videoDetails/videoId").and_then(Value::as_str).is_some_and(|id| id != video_id.as_str()) {
            return Err("answered for a different video".to_owned());
        }
        let formats = audio_formats(&response);
        let format = pick_format(&formats).ok_or("no direct audio url")?;
        self.probe(format).await?;
        Ok(StreamInfo {
            uri: format.url.clone(),
            format_id: Some(format.itag.to_string()),
            protocol: Some("https".to_owned()),
            ext: Some(if format.is_opus() { "webm" } else { "m4a" }.to_owned()),
            acodec: Some(if format.is_opus() { "opus" } else { "mp4a" }.to_owned()),
            title: response.pointer("/videoDetails/title").and_then(Value::as_str).map(str::to_owned),
            uploader: response.pointer("/videoDetails/author").and_then(Value::as_str).map(str::to_owned),
            thumbnail: None,
            is_local: false,
            from_cache: false,
        })
    }

    /// Two bytes from the middle of the file. A URL that only serves its preview answers 403 here.
    async fn probe(&self, format: &Format) -> Result<(), String> {
        let offset = if format.content_length > PROBE_OFFSET + 2 { PROBE_OFFSET } else { 0 };
        let status = self.http.get(&format.url).header("Range", format!("bytes={offset}-{}", offset + 1)).send().await.map_err(|e| e.to_string())?.status();
        if status.is_success() { Ok(()) } else { Err(format!("stream probe answered {status}")) }
    }
}

impl StreamResolver for PlayerEndpointResolver {
    fn resolve(&self, video_id: VideoId, auth: Option<HttpAuth>) -> BoxFuture<'_, Result<StreamInfo, ResolveError>> {
        Box::pin(async move {
            if let Some(uri) = self.cache.get(&video_id).await {
                return Ok(StreamInfo { uri, from_cache: true, ..StreamInfo::default() });
            }
            let started = Instant::now();
            match self.resolve_native(&video_id).await {
                Ok(info) => {
                    tracing::debug!(%video_id, itag = ?info.format_id, took_ms = started.elapsed().as_millis() as u64, "resolved through the player endpoint");
                    self.cache.put(&video_id, &info.uri).await;
                    Ok(info)
                }
                Err(reason) => {
                    tracing::debug!(%video_id, %reason, "player endpoint declined, falling back to yt-dlp");
                    self.fallback.resolve(video_id, auth).await
                }
            }
        })
    }

    fn invalidate(&self, video_id: &VideoId) -> BoxFuture<'_, ()> {
        self.fallback.invalidate(video_id)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn response() -> Value {
        json!({
            "playabilityStatus": {"status": "OK"},
            "videoDetails": {"videoId": "abc", "title": "Song", "author": "Artist"},
            "streamingData": {"adaptiveFormats": [
                {"itag": 137, "mimeType": "video/mp4; codecs=\"avc1\"", "url": "https://v", "bitrate": 4_000_000},
                {"itag": 140, "mimeType": "audio/mp4; codecs=\"mp4a.40.2\"", "url": "https://m4a", "bitrate": 130_916, "contentLength": "3000000"},
                {"itag": 251, "mimeType": "audio/webm; codecs=\"opus\"", "url": "https://opus", "bitrate": 143_381, "contentLength": "3350747"},
                {"itag": 250, "mimeType": "audio/webm; codecs=\"opus\"", "url": "https://opus-low", "bitrate": 73_393},
                {"itag": 774, "mimeType": "audio/webm; codecs=\"opus\"", "signatureCipher": "s=...", "bitrate": 260_000},
                {"itag": 251, "mimeType": "audio/webm; codecs=\"opus\"", "url": "https://drc", "bitrate": 150_000, "isDrc": true},
            ]}
        })
    }

    #[test]
    fn the_best_opus_with_a_plain_url_wins() {
        let formats = audio_formats(&response());
        assert_eq!(formats.iter().map(|f| f.itag).collect::<Vec<_>>(), [140, 251, 250], "video, ciphered and DRC entries are left out");
        let best = pick_format(&formats).unwrap();
        assert_eq!((best.itag, best.url.as_str(), best.content_length), (251, "https://opus", 3_350_747));
    }

    #[test]
    fn m4a_is_the_fallback_when_there_is_no_opus() {
        let formats: Vec<Format> = audio_formats(&response()).into_iter().filter(|f| !f.is_opus()).collect();
        assert_eq!(pick_format(&formats).unwrap().itag, 140);
        assert!(pick_format(&[]).is_none());
    }

    #[test]
    fn a_refusal_carries_the_reason() {
        assert_eq!(refusal(&response()), None);
        let blocked = json!({"playabilityStatus": {"status": "LOGIN_REQUIRED", "reason": "Sign in to confirm you're not a bot"}});
        assert_eq!(refusal(&blocked).as_deref(), Some("LOGIN_REQUIRED Sign in to confirm you're not a bot"));
        assert_eq!(refusal(&json!({})).as_deref(), Some("missing"));
    }

    #[test]
    fn visitor_data_comes_off_the_landing_page() {
        assert_eq!(visitor_from_landing(r#"ytcfg.set({"VISITOR_DATA":"Cgt4eXo%3D","X":1})"#).as_deref(), Some("Cgt4eXo%3D"));
        assert_eq!(visitor_from_landing("<html></html>"), None);
    }

    /// One cold resolve against the live service, timed.
    #[tokio::test]
    #[ignore]
    async fn live_resolves_quickly_and_seekably() {
        struct Never;
        impl StreamResolver for Never {
            fn resolve(&self, _: VideoId, _: Option<HttpAuth>) -> BoxFuture<'_, Result<StreamInfo, ResolveError>> {
                Box::pin(async { Err(ResolveError::Unavailable("fallback was reached".into())) })
            }
            fn invalidate(&self, _: &VideoId) -> BoxFuture<'_, ()> {
                Box::pin(async {})
            }
        }
        let dir = tempfile::tempdir().unwrap();
        let resolver = PlayerEndpointResolver::new(&Paths::for_tests(dir.path()), Arc::new(Never));
        for id in ["J7p4bzqLvCw", "CuklIb9d3fI"] {
            let started = Instant::now();
            let info = resolver.resolve(VideoId(id.to_owned()), None).await.unwrap();
            println!("{id}: itag {:?} {:?} in {:?}", info.format_id, info.title, started.elapsed());
            assert!(started.elapsed() < Duration::from_secs(3));
        }
    }
}

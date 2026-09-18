//! Apple Music lyrics through the Paxsenix proxy (lyrics.paxsenix.org), which serves Apple's syllable-level lyrics as JSON plus the original TTML, keyed by Apple's catalog song id.
//!
//! Finding that id is the hard part, and there are two ways:
//!
//! 1. `itunes.apple.com/search`: public, documented, no auth, and it returns the same catalog ids the lyrics endpoint accepts.
//! 2. `amp-api.music.apple.com`: needs a developer JWT scraped out of the Apple Music web app's minified bundle.
//!
//! The public one goes first. The scrape is a 3 MB download and a regex over obfuscated JavaScript that Apple can change at any time. It stays as the fallback because its search ranks better for some queries.

use std::sync::{LazyLock, Mutex};
use std::time::Duration;

use regex::Regex;
use serde_json::Value;

use super::{HttpError, Request, get_json, get_text, label_or_unknown, seconds_at, text_at, title_and_artist, variants_or_title};
use crate::lyrics::lrc::strip_leading_watermarks;
use crate::lyrics::matching::{Candidate, gate, match_detail, rank_matches};
use crate::lyrics::model::{Align, LyricLine, LyricPart, LyricsMatch, LyricsResult, RANK_WORD};
use crate::lyrics::script::space_between;
use crate::lyrics::ttml::ttml_to_lines;

const SOURCE: &str = "Apple Music";
const ITUNES_SEARCH: &str = "https://itunes.apple.com/search";
const AMP_SEARCH: &str = "https://amp-api.music.apple.com/v1/catalog/us/search";
const PAXSENIX_LYRICS: &str = "https://lyrics.paxsenix.org/apple-music/lyrics";
const WEB_APP: &str = "https://beta.music.apple.com";
const APP_USER_AGENT: &str = "Mixtapes/1.0";
const BROWSER_USER_AGENT: &str = "Mozilla/5.0";
const TIMEOUT: Duration = Duration::from_secs(6);
const BUNDLE_TIMEOUT: Duration = Duration::from_secs(10);
const LYRICS_TIMEOUT: Duration = Duration::from_secs(8);
const SEARCH_LIMIT: &str = "8";
/// How many gated candidates get a lyrics request before giving up.
const MAX_LYRIC_FETCHES: usize = 5;

static BUNDLE_RE: LazyLock<Regex> = LazyLock::new(|| Regex::new(r#"/assets/index[~\-][^/"' ]+\.js"#).unwrap());
// Matched on the JWT's own shape rather than a fixed prefix: the header's first key is not always "alg", so the old "eyJh" prefix missed every token.
static JWT_RE: LazyLock<Regex> = LazyLock::new(|| Regex::new(r"eyJ[A-Za-z0-9_-]{10,}\.[A-Za-z0-9_-]{10,}\.[A-Za-z0-9_-]{10,}").unwrap());

/// One catalog song, from either search.
#[derive(Clone, Debug, PartialEq)]
pub struct Song {
    id: String,
    name: String,
    artist: String,
    duration: u32,
}

impl Candidate for Song {
    fn name(&self) -> &str {
        &self.name
    }
    fn artist(&self) -> &str {
        &self.artist
    }
    fn duration(&self) -> u32 {
        self.duration
    }
}

/// A catalog id as text. iTunes sends a number, amp-api a string.
fn id_text(value: Option<&Value>) -> String {
    match value {
        Some(Value::String(s)) => s.clone(),
        Some(Value::Number(n)) if n.as_f64() != Some(0.0) => n.to_string(),
        _ => String::new(),
    }
}

/// The songs of an `itunes.apple.com/search` response.
pub fn parse_itunes(data: &Value) -> Vec<Song> {
    let rows = data.get("results").and_then(Value::as_array).map(Vec::as_slice).unwrap_or_default();
    rows.iter()
        .map(|row| Song { id: id_text(row.get("trackId")), name: text_at(row, "trackName").to_owned(), artist: text_at(row, "artistName").to_owned(), duration: seconds_at(row, "trackTimeMillis") / 1000 })
        .filter(|song| !song.id.is_empty())
        .collect()
}

/// The songs of an amp-api catalog search response.
pub fn parse_amp(data: &Value) -> Vec<Song> {
    let rows = data.pointer("/results/songs/data").and_then(Value::as_array).map(Vec::as_slice).unwrap_or_default();
    rows.iter()
        .map(|row| {
            let attributes = row.get("attributes").unwrap_or(&Value::Null);
            Song { id: id_text(row.get("id")), name: text_at(attributes, "name").to_owned(), artist: text_at(attributes, "artistName").to_owned(), duration: seconds_at(attributes, "durationInMillis") / 1000 }
        })
        .collect()
}

/// Gate the candidates down to this recording, best first.
///
/// The gate runs before the richest-lyrics walk: that walk keeps whichever candidate has the best lyric type, so one unrelated song with word-level timing would beat the correct song's line-level timing.
pub fn shortlist(candidates: Vec<Song>, request: &Request) -> Vec<Song> {
    let mut gated = gate(candidates, request.title, request.artist, request.duration);
    let title = request.title.trim().to_lowercase();
    gated.sort_by_key(|song| {
        let mut score = 0i32;
        if request.duration != 0 && song.duration != 0 {
            score += match request.duration.abs_diff(song.duration) {
                0..=2 => 100,
                3..=5 => 50,
                _ => 10,
            };
        }
        let name = song.name.to_lowercase();
        if !title.is_empty() && title == name {
            score += 80;
        } else if !title.is_empty() && (name.contains(&title) || title.contains(&name)) {
            score += 40;
        }
        -score
    });
    gated
}

/// The address of the web app's main bundle, from its landing page.
pub fn find_bundle(html: &str) -> Option<&str> {
    BUNDLE_RE.find(html).map(|m| m.as_str())
}

/// Every distinct JWT in the bundle, in the order they appear. Not all of them authorize the catalog API, so the caller tries each.
pub fn find_tokens(js: &str) -> Vec<&str> {
    let mut out: Vec<&str> = Vec::new();
    for found in JWT_RE.find_iter(js).map(|m| m.as_str()) {
        if !out.contains(&found) {
            out.push(found);
        }
    }
    out
}

/// A millisecond stamp as seconds.
fn milliseconds(value: Option<&Value>) -> Option<f64> {
    value.and_then(Value::as_f64).map(|ms| ms / 1000.0)
}

/// `[{text, timestamp, endtime}]` as parts.
///
/// `space_after` comes from trailing whitespace on the raw word when the payload carries any. This endpoint's words carry none at all, so the fallback decides per boundary from the scripts on either side.
fn words_to_parts(words: &Value) -> Vec<LyricPart> {
    let words = words.as_array().map(Vec::as_slice).unwrap_or_default();
    let mut parts: Vec<LyricPart> = Vec::new();
    for word in words.iter().filter(|w| w.is_object()) {
        let raw = text_at(word, "text");
        if raw.trim().is_empty() {
            continue;
        }
        parts.push(LyricPart { start: milliseconds(word.get("timestamp")), end: milliseconds(word.get("endtime")), text: raw.trim().to_owned(), space_after: raw != raw.trim_end() });
    }
    if !parts.iter().any(|p| p.space_after) {
        for i in 0..parts.len() {
            parts[i].space_after = i + 1 < parts.len() && space_between(&parts[i].text, &parts[i + 1].text);
        }
    }
    parts
}

fn join_parts(parts: &[LyricPart]) -> String {
    let mut out = String::new();
    for part in parts {
        out.push_str(&part.text);
        if part.space_after {
            out.push(' ');
        }
    }
    out.trim().to_owned()
}

/// A Paxsenix `/apple-music/lyrics` response as a result.
///
/// The response carries Apple's TTML beside the flattened JSON, and the TTML is strictly richer: transliterations, translations, background vocals as nested spans, and real spacing between words. The JSON is the fallback for responses without it:
///
/// - `type`: "Syllable" (word-level), "Line" or "None" (plain)
/// - `content`: `[{timestamp, endtime, oppositeTurn, text: [{text, timestamp, endtime}], backgroundText: [...]}]`
pub fn paxsenix_to_lines(data: &Value) -> Option<LyricsResult> {
    if !data.is_object() {
        return None;
    }
    if let Some(ttml) = data.get("ttmlContent").and_then(Value::as_str).filter(|t| !t.trim().is_empty()) {
        let lines = strip_leading_watermarks(ttml_to_lines(ttml));
        if !lines.is_empty() {
            return Some(LyricsResult::from_lines(lines, SOURCE));
        }
    }

    let kind = text_at(data, "type");
    let content = data.get("content").and_then(Value::as_array)?;
    let mut lines = Vec::new();
    for entry in content.iter().filter(|e| e.is_object()) {
        let parts = words_to_parts(entry.get("text").unwrap_or(&Value::Null));
        let text = join_parts(&parts);
        if text.is_empty() {
            continue;
        }
        let mut line = LyricLine::new(milliseconds(entry.get("timestamp")), text);
        line.end = milliseconds(entry.get("endtime"));
        if entry.get("oppositeTurn").is_some_and(truthy) {
            line.align = Some(Align::End);
        }
        // Word timing only exists for the "Syllable" type.
        if kind == "Syllable" && parts.iter().any(|p| p.start.is_some()) {
            line.parts = parts;
        }
        // `background` is a flag on the lead line, and the words sit in `backgroundText` beside it.
        let bg_parts = words_to_parts(entry.get("backgroundText").unwrap_or(&Value::Null));
        let bg_text = join_parts(&bg_parts);
        if !bg_text.is_empty() {
            line.bg_text = Some(bg_text);
            if bg_parts.iter().any(|p| p.start.is_some()) {
                line.bg = bg_parts;
            }
        }
        let side = |key: &str| Some(text_at(entry, key).trim()).filter(|v| !v.is_empty() && *v != line.text).map(str::to_owned);
        line.translation = side("translation");
        line.romanization = side("romanization").or_else(|| side("transliteration"));
        lines.push(line);
    }

    let mut lines = strip_leading_watermarks(lines);
    if lines.is_empty() {
        return None;
    }
    if kind == "None" {
        // Apple's untimed tracks rank as plain, so they cannot preempt a line-synced source.
        for line in &mut lines {
            line.start = None;
        }
    }
    Some(LyricsResult::from_lines(lines, SOURCE))
}

fn truthy(value: &Value) -> bool {
    !matches!(value, Value::Null | Value::Bool(false)) && value.as_f64() != Some(0.0) && value.as_str() != Some("")
}

/// Why the authenticated search had nothing.
enum AmpError {
    /// The token was refused. It is forgotten, and a fresh scrape may work.
    Unauthorized,
    Failed,
}

/// The Apple Music provider. Holds the scraped JWT for the life of the process.
pub struct AppleMusic {
    http: reqwest::Client,
    token: Mutex<Option<String>>,
}

impl AppleMusic {
    pub fn new(http: reqwest::Client) -> Self {
        Self { http, token: Mutex::new(None) }
    }

    pub async fn fetch(&self, request: Request<'_>) -> Option<LyricsResult> {
        let candidates = self.search_itunes(request.title, request.artist).await;
        if let Some(result) = self.richest(candidates.clone(), &request).await {
            return Some(result);
        }

        // Fall back to the authenticated catalog search.
        let token = self.token(false).await?;
        let songs = match self.search_amp(&token, request.title, request.artist).await {
            Ok(songs) => songs,
            Err(_) => {
                // The token might have expired, so try once with a fresh one.
                let token = self.token(true).await?;
                self.search_amp(&token, request.title, request.artist).await.ok()?
            }
        };
        // Ids the iTunes pass already tried and rejected are not fetched again.
        let fresh: Vec<Song> = songs.into_iter().filter(|s| !candidates.iter().any(|c| c.id == s.id)).collect();
        self.richest(fresh, &request).await
    }

    /// The richest lyrics among the candidates that pass the gate.
    async fn richest(&self, candidates: Vec<Song>, request: &Request<'_>) -> Option<LyricsResult> {
        let mut best: Option<LyricsResult> = None;
        for song in shortlist(candidates, request).into_iter().take(MAX_LYRIC_FETCHES) {
            let Some(result) = self.lyrics(&song.id).await.as_ref().and_then(paxsenix_to_lines) else { continue };
            if result.rank() > best.as_ref().map_or(0, LyricsResult::rank) {
                best = Some(result);
            }
            if best.as_ref().is_some_and(|b| b.rank() >= RANK_WORD) {
                break;
            }
        }
        best
    }

    /// Both catalog searches, not only the public one. They do not agree: iTunes Search lacks songs amp-api has at the exact duration, so browsing iTunes alone would hide the match the chain itself found.
    pub async fn matches(&self, request: Request<'_>, limit: usize) -> Vec<LyricsMatch> {
        let mut songs: Vec<Song> = Vec::new();
        let collect = |found: Vec<Song>, songs: &mut Vec<Song>| {
            for song in found {
                if !song.id.is_empty() && !songs.iter().any(|seen| seen.id == song.id) {
                    songs.push(song);
                }
            }
        };
        for variant in variants_or_title(request.title) {
            collect(self.search_itunes(&variant, request.artist).await, &mut songs);
            if songs.len() >= limit * 2 {
                break;
            }
        }
        if let Some(token) = self.token(false).await {
            for variant in variants_or_title(request.title) {
                collect(self.search_amp(&token, &variant, request.artist).await.unwrap_or_default(), &mut songs);
                if songs.len() >= limit * 3 {
                    break;
                }
            }
        }

        let mut out = Vec::new();
        for song in rank_matches(songs, request.title, request.artist, request.duration) {
            let Some(result) = self.lyrics(&song.id).await.as_ref().and_then(paxsenix_to_lines) else { continue };
            out.push(LyricsMatch { label: label_or_unknown(&song.name), detail: match_detail(&song.artist, song.duration), result, source: None });
            if out.len() >= limit {
                break;
            }
        }
        out
    }

    async fn search_itunes(&self, title: &str, artist: &str) -> Vec<Song> {
        let term = title_and_artist(title, artist);
        if term.is_empty() {
            return Vec::new();
        }
        match get_json(&self.http, ITUNES_SEARCH, &[("term", term.as_str()), ("entity", "song"), ("limit", SEARCH_LIMIT)], &[("User-Agent", APP_USER_AGENT)], TIMEOUT).await {
            Ok(data) => parse_itunes(&data),
            Err(err) => {
                tracing::debug!(%err, "iTunes search failed");
                Vec::new()
            }
        }
    }

    async fn amp_get(&self, token: &str, term: &str, limit: &str) -> Result<Value, HttpError> {
        let bearer = format!("Bearer {token}");
        let headers = [("Authorization", bearer.as_str()), ("Origin", "https://music.apple.com"), ("Referer", "https://music.apple.com/"), ("User-Agent", BROWSER_USER_AGENT)];
        get_json(&self.http, AMP_SEARCH, &[("term", term), ("types", "songs"), ("limit", limit), ("l", "en-US"), ("platform", "web")], &headers, TIMEOUT).await
    }

    async fn search_amp(&self, token: &str, title: &str, artist: &str) -> Result<Vec<Song>, AmpError> {
        match self.amp_get(token, &title_and_artist(title, artist), SEARCH_LIMIT).await {
            Ok(data) => Ok(parse_amp(&data)),
            Err(HttpError::Status(401)) => {
                *self.token.lock().unwrap() = None;
                Err(AmpError::Unauthorized)
            }
            Err(err) => {
                tracing::debug!(%err, "Apple Music search failed");
                Err(AmpError::Failed)
            }
        }
    }

    /// A JWT that authorizes the catalog search, scraped from the web app and kept for the life of the process.
    async fn token(&self, force_new: bool) -> Option<String> {
        if !force_new && let Some(token) = self.token.lock().unwrap().clone() {
            return Some(token);
        }
        let browser = [("User-Agent", BROWSER_USER_AGENT)];
        let scraped = async {
            let html = get_text(&self.http, WEB_APP, &[], &browser, TIMEOUT).await?;
            let Some(bundle) = find_bundle(&html) else {
                return Err(HttpError::Other("could not locate the Apple Music bundle".into()));
            };
            get_text(&self.http, &format!("{WEB_APP}{bundle}"), &[], &browser, BUNDLE_TIMEOUT).await
        };
        let js = match scraped.await {
            Ok(js) => js,
            Err(err) => {
                tracing::debug!(%err, "Apple Music token fetch failed");
                return None;
            }
        };
        let candidates = find_tokens(&js);
        for token in &candidates {
            // A cheap search says whether this one authorizes the catalog API.
            if self.amp_get(token, "test", "1").await.is_ok() {
                *self.token.lock().unwrap() = Some((*token).to_owned());
                return Some((*token).to_owned());
            }
        }
        tracing::debug!(tried = candidates.len(), "no Apple Music token authorized");
        None
    }

    async fn lyrics(&self, song_id: &str) -> Option<Value> {
        if song_id.is_empty() {
            return None;
        }
        match get_json(&self.http, PAXSENIX_LYRICS, &[("id", song_id)], &[("User-Agent", APP_USER_AGENT)], LYRICS_TIMEOUT).await {
            Ok(data) => Some(data),
            // Paxsenix answers 404 for a song Apple has no lyrics for. That is a miss, not an outage.
            Err(err) if err.to_string().contains("404") => {
                tracing::debug!(song_id, "Apple Music has no lyrics for this song");
                None
            }
            Err(err) => {
                tracing::debug!(%err, "Paxsenix lyric fetch failed");
                None
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn request<'a>(title: &'a str, artist: &'a str, duration: u32) -> Request<'a> {
        Request { video_id: "", title, artist, duration, strict: true }
    }

    #[test]
    fn both_catalogs_normalize_to_the_same_song() {
        let itunes = parse_itunes(&json!({"results": [{"trackId": 1440858689, "trackName": "Lemon", "artistName": "Kenshi Yonezu", "trackTimeMillis": 255_800}, {"trackName": "no id"}]}));
        assert_eq!(itunes, [Song { id: "1440858689".into(), name: "Lemon".into(), artist: "Kenshi Yonezu".into(), duration: 255 }]);
        let amp = parse_amp(&json!({"results": {"songs": {"data": [{"id": "1440858689", "attributes": {"name": "Lemon", "artistName": "Kenshi Yonezu", "durationInMillis": 255_800}}]}}}));
        assert_eq!(amp, itunes);
        assert!(parse_itunes(&json!({})).is_empty());
        assert!(parse_amp(&json!({"results": {}})).is_empty());
    }

    #[test]
    fn the_shortlist_is_gated_then_ordered() {
        let songs = vec![
            Song { id: "1".into(), name: "Popular".into(), artist: "Ariana Grande".into(), duration: 214 },
            Song { id: "2".into(), name: "Popular (feat. Playboi Carti)".into(), artist: "The Weeknd, Madonna".into(), duration: 218 },
            Song { id: "3".into(), name: "Popular".into(), artist: "The Weeknd".into(), duration: 215 },
            Song { id: "4".into(), name: "Popular (Live)".into(), artist: "The Weeknd".into(), duration: 215 },
        ];
        let ids: Vec<String> = shortlist(songs, &request("Popular", "The Weeknd", 215)).into_iter().map(|s| s.id).collect();
        assert_eq!(ids, ["3", "2"]);
        assert!(shortlist(Vec::new(), &request("Popular", "The Weeknd", 215)).is_empty());
    }

    #[test]
    fn the_bundle_and_its_tokens_are_found() {
        assert_eq!(find_bundle(r#"<script type="module" src="/assets/index~8a6f5b2c.js"></script>"#), Some("/assets/index~8a6f5b2c.js"));
        assert_eq!(find_bundle(r#"<script src="/assets/index-legacy-abc.js">"#), Some("/assets/index-legacy-abc.js"));
        assert_eq!(find_bundle("<html></html>"), None);
        let js = r#"a="eyJ0eXAiOiJKV1QiLCJhbGciOiJFUzI1NiJ9.eyJpc3MiOiJBTVBXZWJQbGF5In0.c2lnbmF0dXJlX2J5dGVz";b="eyJhbGciOiJFUzI1NiJ9.eyJpc3MiOiJvdGhlciJ9.b3RoZXJfc2lnbmF0dXJl";c="eyJ0eXAiOiJKV1QiLCJhbGciOiJFUzI1NiJ9.eyJpc3MiOiJBTVBXZWJQbGF5In0.c2lnbmF0dXJlX2J5dGVz";d="eyJshort.a.b""#;
        let tokens = find_tokens(js);
        assert_eq!(tokens.len(), 2, "duplicates collapse and short look-alikes are skipped");
        assert!(tokens[0].starts_with("eyJ0"));
    }

    #[test]
    fn the_ttml_is_preferred_over_the_flattened_json() {
        let ttml = r#"<tt xmlns="http://www.w3.org/ns/ttml" xmlns:ttm="http://www.w3.org/ns/ttml#metadata"><body><div><p begin="0.5" end="1">(Purchase your tracks today)</p><p begin="1" end="2"><span begin="1" end="1.5">千本</span><span begin="1.5" end="2">桜</span></p></div></body></tt>"#;
        let result = paxsenix_to_lines(&json!({"type": "Syllable", "ttmlContent": ttml, "content": [{"timestamp": 1000, "text": [{"text": "ignored", "timestamp": 1000, "endtime": 2000}]}]})).unwrap();
        assert_eq!(result.source, "Apple Music");
        assert_eq!(result.lines.len(), 1, "the watermark goes");
        assert_eq!(result.lines[0].text, "千本桜");
        assert_eq!(result.rank(), 3);
    }

    #[test]
    fn syllable_json_is_the_fallback() {
        let data = json!({"type": "Syllable", "ttmlContent": "", "content": [
            {"timestamp": 1000, "endtime": 3000, "oppositeTurn": true, "background": true,
             "text": [{"text": "Hello", "timestamp": 1000, "endtime": 1500}, {"text": "千", "timestamp": 1500, "endtime": 2000}, {"text": "本", "timestamp": 2000, "endtime": 3000}, {"text": "  "}],
             "backgroundText": [{"text": "ooh", "timestamp": 2500, "endtime": 3000}],
             "transliteration": " hello sen bon ", "translation": "Hello 千本"},
            {"timestamp": 4000, "text": []}
        ]});
        let result = paxsenix_to_lines(&data).unwrap();
        assert_eq!(result.lines.len(), 1);
        let line = &result.lines[0];
        assert_eq!(line.text, "Hello 千本", "a space between words, none between syllables");
        assert_eq!((line.start, line.end), (Some(1.0), Some(3.0)));
        assert!(line.opposite_voice());
        assert_eq!(line.parts.len(), 3);
        assert!(line.parts[0].space_after && !line.parts[1].space_after && !line.parts[2].space_after);
        assert_eq!(line.bg_text.as_deref(), Some("ooh"));
        assert_eq!(line.bg[0].start, Some(2.5));
        assert_eq!(line.romanization.as_deref(), Some("hello sen bon"));
        assert_eq!(line.translation, None, "a translation equal to the line is dropped");
        assert!(result.synced);
    }

    #[test]
    fn trailing_whitespace_on_the_words_is_trusted_when_present() {
        let data = json!({"type": "Syllable", "content": [{"timestamp": 0, "text": [{"text": "to ", "timestamp": 0, "endtime": 100}, {"text": "geth", "timestamp": 100, "endtime": 200}, {"text": "er", "timestamp": 200, "endtime": 300}]}]});
        assert_eq!(paxsenix_to_lines(&data).unwrap().lines[0].text, "to gether");
    }

    #[test]
    fn line_and_untimed_types_carry_no_parts() {
        let content = json!([{"timestamp": 1000, "text": [{"text": "one", "timestamp": 1000, "endtime": 2000}, {"text": "two", "timestamp": 2000, "endtime": 3000}]}]);
        let line = paxsenix_to_lines(&json!({"type": "Line", "content": content})).unwrap();
        assert_eq!(line.rank(), 2);
        assert!(line.lines[0].parts.is_empty());
        let untimed = paxsenix_to_lines(&json!({"type": "None", "content": content})).unwrap();
        assert_eq!(untimed.rank(), 1);
        assert_eq!(untimed.lines[0].start, None);
        assert!(!untimed.synced);
    }

    #[test]
    fn junk_is_nothing() {
        assert!(paxsenix_to_lines(&json!([])).is_none());
        assert!(paxsenix_to_lines(&json!({"type": "Line", "content": []})).is_none());
        assert!(paxsenix_to_lines(&json!({"error": "not found"})).is_none());
        assert!(paxsenix_to_lines(&json!({"type": "Line", "content": [{"timestamp": 0, "text": [{"text": "Lyrics provided by X"}]}]})).is_none());
    }
}

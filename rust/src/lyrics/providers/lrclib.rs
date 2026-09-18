//! LRCLIB (https://lrclib.net), line-synced LRC.
//!
//! Tries the exact `/get` first (title, artist and duration within 2 s), then `/get` without the duration, then `/search`, so a (Remastered) suffix or a duration a few seconds off does not lose the lyrics.

use std::time::Duration;

use serde_json::Value;

use super::{Cooldowns, HttpError, Provider, Request, get_json, label_or_unknown, seconds_at, text_at, variants_or_title};
use crate::lyrics::lrc::{parse_lrc_text, plain_lines, strip_leading_credits};
use crate::lyrics::matching::{Candidate, artist_matches, match_detail, rank_matches};
use crate::lyrics::model::{LyricsMatch, LyricsResult};

const SOURCE: &str = "LRCLIB";
const API: &str = "https://lrclib.net/api/";
const USER_AGENT: &str = "Mixtapes (https://github.com/m-obeid/Mixtapes)";
const BROWSER_USER_AGENT: &str = "Mixtapes/1.0";
/// Enough for a hit. Capping low keeps the worst case at a few seconds when the chain walks several title variants and the API is sluggish.
const TIMEOUT: Duration = Duration::from_secs(3);
const BROWSER_TIMEOUT: Duration = Duration::from_secs(6);
const RATE_LIMIT_BACKOFF: Duration = Duration::from_secs(120);
const TIMEOUT_BACKOFF: Duration = Duration::from_secs(60);

/// One `/search` row.
struct Hit(Value);

impl Candidate for Hit {
    fn name(&self) -> &str {
        text_at(&self.0, "trackName")
    }
    fn artist(&self) -> &str {
        text_at(&self.0, "artistName")
    }
    fn duration(&self) -> u32 {
        seconds_at(&self.0, "duration")
    }
}

/// One GET against the API. Once a 429 or a timeout has tripped the cooldown, the remaining probes and any later title variants are skipped rather than fired at a server that said to back off.
async fn hit(http: &reqwest::Client, cooldowns: &Cooldowns, path: &str, query: &[(&str, &str)]) -> Option<Value> {
    if !cooldowns.ready(Provider::Lrclib) {
        return None;
    }
    match get_json(http, &format!("{API}{path}"), query, &[("User-Agent", USER_AGENT)], TIMEOUT).await {
        Ok(data) => Some(data),
        Err(HttpError::Status(429)) => {
            cooldowns.trip(Provider::Lrclib, RATE_LIMIT_BACKOFF, "HTTP 429");
            None
        }
        Err(HttpError::Status(404)) => None,
        Err(HttpError::Timeout) => {
            cooldowns.trip(Provider::Lrclib, TIMEOUT_BACKOFF, "timeout");
            None
        }
        Err(err) => {
            tracing::debug!(%err, path, "LRCLIB fetch failed");
            None
        }
    }
}

/// The search row to try: the artist has to match, then the closest duration wins.
///
/// Without the artist filter, an unfilled exact request followed by /search returns some random album's "Intro" for any track called "Intro".
pub fn pick_search_hit(results: &Value, artist: &str, duration: u32) -> Option<Value> {
    let mut rows: Vec<&Value> = results.as_array()?.iter().filter(|r| artist.is_empty() || artist_matches(artist, text_at(r, "artistName"))).collect();
    if duration != 0 {
        rows.sort_by(|a, b| distance(a, duration).total_cmp(&distance(b, duration)));
    }
    rows.first().map(|r| (*r).clone())
}

fn distance(row: &Value, duration: u32) -> f64 {
    (f64::from(duration) - row.get("duration").and_then(Value::as_f64).unwrap_or(0.0)).abs()
}

/// A `/get` answer or a `/search` row as a result: synced lyrics when it has them, plain otherwise.
pub fn result_from(candidate: &Value) -> Option<LyricsResult> {
    let synced = text_at(candidate, "syncedLyrics");
    if !synced.trim().is_empty() {
        let lines = parse_lrc_text(synced);
        if !lines.is_empty() {
            return Some(LyricsResult::from_lines(lines, SOURCE));
        }
    }
    let lines = plain_lines(text_at(candidate, "plainLyrics"));
    (!lines.is_empty()).then(|| LyricsResult::from_lines(lines, SOURCE))
}

pub async fn fetch(http: &reqwest::Client, cooldowns: &Cooldowns, request: Request<'_>) -> Option<LyricsResult> {
    let mut base = vec![("track_name", request.title)];
    if !request.artist.is_empty() {
        base.push(("artist_name", request.artist));
    }
    let mut candidates: Vec<Value> = Vec::new();
    if request.duration > 0 {
        let seconds = request.duration.to_string();
        let mut exact = base.clone();
        exact.push(("duration", seconds.as_str()));
        candidates.extend(hit(http, cooldowns, "get", &exact).await.filter(Value::is_object));
    }
    candidates.extend(hit(http, cooldowns, "get", &base).await.filter(Value::is_object));
    if candidates.is_empty() {
        let results = hit(http, cooldowns, "search", &base).await?;
        candidates.extend(pick_search_hit(&results, request.artist, request.duration));
    }
    candidates.iter().find_map(result_from)
}

/// A search row as a match browser entry. Credits go here, since nothing else strips them for LRCLIB.
fn match_from(hit: &Hit) -> Option<LyricsMatch> {
    let synced = text_at(&hit.0, "syncedLyrics");
    let lines = if synced.is_empty() { plain_lines(text_at(&hit.0, "plainLyrics")) } else { strip_leading_credits(parse_lrc_text(synced)) };
    if lines.is_empty() {
        return None;
    }
    Some(LyricsMatch { label: label_or_unknown(hit.name()), detail: match_detail(hit.artist(), hit.duration()), result: LyricsResult::from_lines(lines, SOURCE), source: None })
}

pub async fn matches(http: &reqwest::Client, request: Request<'_>, limit: usize) -> Vec<LyricsMatch> {
    let mut hits: Vec<Hit> = Vec::new();
    for variant in variants_or_title(request.title) {
        let mut query = vec![("track_name", variant.as_str())];
        if !request.artist.is_empty() {
            query.push(("artist_name", request.artist));
        }
        let Ok(data) = get_json(http, &format!("{API}search"), &query, &[("User-Agent", BROWSER_USER_AGENT)], BROWSER_TIMEOUT).await else { continue };
        for row in data.as_array().map(Vec::as_slice).unwrap_or_default() {
            if !hits.iter().any(|seen| seen.0.get("id") == row.get("id")) {
                hits.push(Hit(row.clone()));
            }
        }
        if hits.len() >= limit * 2 {
            break;
        }
    }
    rank_matches(hits, request.title, request.artist, request.duration).iter().filter_map(match_from).take(limit).collect()
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn synced_lyrics_win_over_plain() {
        let result = result_from(&json!({"syncedLyrics": "[00:01.00] one\n[00:02.00] two", "plainLyrics": "one\ntwo"})).unwrap();
        assert!(result.synced);
        assert_eq!(result.source, "LRCLIB");
        assert_eq!(result.lines[1].start, Some(2.0));
    }

    #[test]
    fn plain_lyrics_are_the_fallback() {
        let result = result_from(&json!({"syncedLyrics": null, "plainLyrics": "one\n\ntwo\n"})).unwrap();
        assert!(!result.synced);
        assert_eq!(result.lines.len(), 2);
        assert_eq!(result.rank(), 1);
        // Synced text with no stamps in it falls through to the plain text as well.
        assert!(!result_from(&json!({"syncedLyrics": "no stamps", "plainLyrics": "one"})).unwrap().synced);
    }

    #[test]
    fn an_instrumental_has_nothing() {
        assert!(result_from(&json!({"instrumental": true, "syncedLyrics": null, "plainLyrics": null})).is_none());
        assert!(result_from(&json!({"syncedLyrics": "  ", "plainLyrics": " \n "})).is_none());
    }

    #[test]
    fn the_search_hit_needs_the_artist_and_prefers_the_duration() {
        let results = json!([
            {"id": 1, "trackName": "Intro", "artistName": "Random Band", "duration": 60.0},
            {"id": 2, "trackName": "Intro", "artistName": "The xx", "duration": 140.0},
            {"id": 3, "trackName": "Intro", "artistName": "The xx", "duration": 128.0}
        ]);
        assert_eq!(pick_search_hit(&results, "The xx", 127).unwrap()["id"], 3);
        assert_eq!(pick_search_hit(&results, "The xx", 0).unwrap()["id"], 2, "no duration keeps the server's order");
        assert_eq!(pick_search_hit(&results, "", 61).unwrap()["id"], 1);
        assert!(pick_search_hit(&results, "Nobody", 127).is_none());
        assert!(pick_search_hit(&json!({"error": "x"}), "The xx", 127).is_none());
    }

    #[test]
    fn a_match_row_strips_credits_and_describes_itself() {
        let hit = Hit(json!({"id": 1, "trackName": "Lemon", "artistName": "Kenshi Yonezu", "duration": 255.0, "syncedLyrics": "[00:00.00] Lyricist: X\n[00:05.00] 夢ならば"}));
        let found = match_from(&hit).unwrap();
        assert_eq!(found.label, "Lemon");
        assert_eq!(found.detail, "Kenshi Yonezu · 4:15");
        assert_eq!(found.result.lines.len(), 1);
        assert!(match_from(&Hit(json!({"id": 2, "trackName": "", "plainLyrics": ""}))).is_none());
        assert_eq!(match_from(&Hit(json!({"id": 3, "plainLyrics": "la"}))).unwrap().label, "Unknown");
    }
}

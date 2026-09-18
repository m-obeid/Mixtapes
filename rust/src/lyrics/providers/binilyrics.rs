//! BiniLyrics (lyrics-api.binimum.org), the open TTML database behind the BetterLyrics extension, with good word-level coverage for Western pop and Japanese tracks.
//!
//! The endpoint takes a free-text query and returns matches with their timing type and a `lyricsUrl` pointing at the TTML.

use std::time::Duration;

use serde_json::Value;

use super::{HttpError, Request, get_text, seconds_at, text_at, title_and_artist};
use crate::lyrics::matching::{Candidate, artist_matches, gate, is_generic_title};
use crate::lyrics::model::LyricsResult;
use crate::lyrics::ttml::ttml_to_lines;

const SOURCE: &str = "BiniLyrics";
const URL: &str = "https://lyrics-api.binimum.org/getLyrics";
const USER_AGENT: &str = "BetterLyrics/1.0";
const TIMEOUT: Duration = Duration::from_secs(6);

/// One search row.
#[derive(Clone, Debug, PartialEq)]
pub struct Hit {
    name: String,
    artist: String,
    duration: u32,
    word_level: bool,
    lyrics_url: String,
}

impl Candidate for Hit {
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

pub fn parse_search(data: &Value) -> Vec<Hit> {
    let rows = data.get("results").and_then(Value::as_array).map(Vec::as_slice).unwrap_or_default();
    rows.iter()
        .map(|row| Hit {
            name: [text_at(row, "track_name"), text_at(row, "name")].into_iter().find(|n| !n.is_empty()).unwrap_or_default().to_owned(),
            artist: text_at(row, "artist_name").to_owned(),
            duration: seconds_at(row, "duration"),
            word_level: matches!(text_at(row, "timing_type"), "word" | "syllable"),
            lyrics_url: text_at(row, "lyricsUrl").to_owned(),
        })
        .collect()
}

/// The row to fetch. None when nothing is plausibly this recording.
pub fn pick(mut hits: Vec<Hit>, request: &Request) -> Option<Hit> {
    // Hard-filter to artist matches before scoring, so "Intro" from one album cannot claim to be the lyrics of an unrelated track of the same name.
    if !request.artist.is_empty() {
        if hits.iter().any(|h| artist_matches(request.artist, &h.artist)) {
            hits.retain(|h| artist_matches(request.artist, &h.artist));
        } else if is_generic_title(request.title) {
            return None;
        }
    }
    // The same recording gate as Apple Music: the search is fuzzy and the score below rewards word-level timing, so an unrelated hit that has it would win outright.
    let mut hits = gate(hits, request.title, request.artist, request.duration);
    hits.sort_by_key(|hit| {
        let mut score = if hit.word_level { 100i64 } else { 0 };
        if request.duration != 0 && hit.duration != 0 {
            score -= i64::from(request.duration.abs_diff(hit.duration).min(30));
        }
        -score
    });
    hits.into_iter().next()
}

/// The fetched TTML as a result. When the search promised word-level timing and the document has none, this answers None so a line-synced provider gets its turn.
pub fn result_from(ttml: &str, promised_word_level: bool) -> Option<LyricsResult> {
    let lines = ttml_to_lines(ttml);
    if lines.is_empty() {
        return None;
    }
    let result = LyricsResult::from_lines(lines, SOURCE);
    if promised_word_level && !result.is_word_level() {
        return None;
    }
    Some(result)
}

pub async fn fetch(http: &reqwest::Client, request: Request<'_>) -> Option<LyricsResult> {
    let headers = [("User-Agent", USER_AGENT)];
    // The backend does a fuzzy lookup, so "title artist" is enough.
    let query = title_and_artist(request.title, request.artist);
    let body = match get_text(http, URL, &[("q", query.as_str())], &headers, TIMEOUT).await {
        Ok(body) => body,
        Err(HttpError::Status(404)) => return None,
        Err(err) => {
            tracing::debug!(%err, "BiniLyrics search failed");
            return None;
        }
    };
    let best = pick(parse_search(&serde_json::from_str(&body).ok()?), &request)?;
    if best.lyrics_url.is_empty() {
        return None;
    }
    match get_text(http, &best.lyrics_url, &[], &headers, TIMEOUT).await {
        Ok(ttml) => result_from(&ttml, best.word_level),
        Err(err) => {
            tracing::debug!(%err, "BiniLyrics TTML fetch failed");
            None
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

    fn rows() -> Value {
        json!({"results": [
            {"track_name": "Blinding Lights", "artist_name": "The Weeknd", "duration": 200, "timing_type": "line", "lyricsUrl": "https://x/line.ttml"},
            {"track_name": "Blinding Lights", "artist_name": "The Weeknd", "duration": 202.4, "timing_type": "syllable", "lyricsUrl": "https://x/word.ttml"},
            {"name": "Blinding Lights (Remix)", "artist_name": "The Weeknd", "duration": 200, "timing_type": "word", "lyricsUrl": "https://x/remix.ttml"},
            {"track_name": "Unrelated", "artist_name": "Someone", "duration": 200, "timing_type": "word", "lyricsUrl": "https://x/other.ttml"}
        ]})
    }

    #[test]
    fn word_level_timing_outweighs_a_closer_duration() {
        let best = pick(parse_search(&rows()), &request("Blinding Lights", "The Weeknd", 200)).unwrap();
        assert_eq!(best.lyrics_url, "https://x/word.ttml");
        assert!(best.word_level);
    }

    #[test]
    fn nothing_plausible_is_nothing() {
        assert!(pick(parse_search(&rows()), &request("Blinding Lights", "The Weeknd", 300)).is_none());
        assert!(pick(parse_search(&rows()), &request("Intro", "Nobody", 200)).is_none());
        assert!(pick(Vec::new(), &request("Blinding Lights", "The Weeknd", 200)).is_none());
        assert!(parse_search(&json!({"results": null})).is_empty());
    }

    #[test]
    fn a_broken_word_level_promise_lets_the_next_provider_try() {
        let line_only = r#"<tt><body><div><p begin="1" end="2">just a line</p></div></body></tt>"#;
        assert!(result_from(line_only, true).is_none());
        let kept = result_from(line_only, false).unwrap();
        assert!(kept.synced);
        assert_eq!(kept.source, "BiniLyrics");
        let unsynced = result_from("<tt><body><div><p>no timing</p></div></body></tt>", false).unwrap();
        assert!(!unsynced.synced);
        assert!(result_from("<tt/>", false).is_none());
    }
}

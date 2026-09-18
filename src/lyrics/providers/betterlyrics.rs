//! BetterLyrics (lyrics-api.boidu.dev), Apple Music's TTML behind a fuzzy title and artist lookup.

use std::time::Duration;

use serde_json::Value;

use super::{HttpError, Request, get_text};
use crate::lyrics::matching::is_generic_title;
use crate::lyrics::model::{LyricLine, LyricsResult};
use crate::lyrics::ttml::ttml_to_lines;

const SOURCE: &str = "BetterLyrics";
const URL: &str = "https://lyrics-api.boidu.dev/getLyrics";
/// The endpoint answers 403 to generic user agents. The BetterLyrics browser extension identifies itself this way and the API mirrors that as a soft gate.
const USER_AGENT: &str = "BetterLyrics/1.0";
const TIMEOUT: Duration = Duration::from_secs(6);

/// A response body as a result.
///
/// Newer responses are `{"ttml": "<tt ...>"}` with word-level spans. The older list of `{words, startTimeMs}` rows is kept in case the deployment ever rolls back.
pub fn parse(data: &Value) -> Option<LyricsResult> {
    if let Some(ttml) = data.get("ttml").and_then(Value::as_str) {
        let lines = ttml_to_lines(ttml);
        return (!lines.is_empty()).then(|| LyricsResult::from_lines(lines, SOURCE));
    }
    let rows = if data.is_object() { data.get("lyrics")? } else { data }.as_array()?;
    let lines: Vec<LyricLine> = rows
        .iter()
        .filter(|row| row.is_object())
        .filter_map(|row| {
            let text = ["words", "text"].iter().filter_map(|key| row.get(*key)).map(scalar_text).find(|t| !t.is_empty())?;
            Some(LyricLine::new(milliseconds(row.get("startTimeMs")), text))
        })
        .collect();
    (!lines.is_empty()).then(|| LyricsResult::from_lines(lines, SOURCE))
}

/// str() of a JSON scalar, trimmed. Empty for null, false and zero, which Python read as missing.
fn scalar_text(value: &Value) -> String {
    match value {
        Value::String(s) => s.trim().to_owned(),
        Value::Number(n) if n.as_f64() != Some(0.0) => n.to_string(),
        _ => String::new(),
    }
}

/// A millisecond stamp as seconds. The old shape sent it as a string.
fn milliseconds(value: Option<&Value>) -> Option<f64> {
    match value? {
        Value::Number(n) => n.as_f64(),
        Value::String(s) => s.trim().parse::<f64>().ok(),
        _ => None,
    }
    .map(|ms| ms / 1000.0)
}

pub async fn fetch(http: &reqwest::Client, request: Request<'_>) -> Option<LyricsResult> {
    // The response is the lyric body alone, with no artist to verify against. For a generic title the server's fuzzy match would hand back some other track's lyrics, and Apple Music covers the same catalog with an artist check.
    if is_generic_title(request.title) && !request.artist.is_empty() {
        return None;
    }
    let seconds = request.duration.to_string();
    let mut query = vec![("s", request.title), ("a", request.artist)];
    if request.duration > 0 {
        query.push(("d", seconds.as_str()));
    }
    let body = match get_text(http, URL, &query, &[("User-Agent", USER_AGENT)], TIMEOUT).await {
        Ok(body) => body,
        // 401 and 404 mean no lyrics for this track: common and not actionable.
        Err(HttpError::Status(401 | 404)) => return None,
        Err(err) => {
            tracing::debug!(%err, "BetterLyrics fetch failed");
            return None;
        }
    };
    parse(&serde_json::from_str(&body).ok()?)
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn the_ttml_wrapper_is_word_level() {
        let ttml = r#"<tt xmlns="http://www.w3.org/ns/ttml"><body><div><p begin="1.0" end="2.0"><span begin="1.0" end="1.5">Hey</span> <span begin="1.5" end="2.0">you</span></p></div></body></tt>"#;
        let result = parse(&json!({"ttml": ttml})).unwrap();
        assert_eq!(result.source, "BetterLyrics");
        assert_eq!(result.rank(), 3);
        assert_eq!(result.lines[0].text, "Hey you");
    }

    #[test]
    fn unusable_ttml_is_no_result_rather_than_a_fallback() {
        assert!(parse(&json!({"ttml": "<tt><body/></tt>", "lyrics": [{"words": "la", "startTimeMs": 1}]})).is_none());
    }

    #[test]
    fn the_old_row_shape_still_reads() {
        let result = parse(&json!({"lyrics": [{"words": "one", "startTimeMs": "1500"}, {"text": " two ", "startTimeMs": 2500}, {"words": ""}, "junk"]})).unwrap();
        assert!(result.synced);
        assert_eq!(result.lines.len(), 2);
        assert_eq!(result.lines[0].start, Some(1.5));
        assert_eq!(result.lines[1].text, "two");
        // A bare list, and a row with no stamp.
        let bare = parse(&json!([{"words": "one", "startTimeMs": 1000}, {"words": "two"}])).unwrap();
        assert!(!bare.synced);
    }

    #[test]
    fn anything_else_is_nothing() {
        assert!(parse(&json!({"error": "not found"})).is_none());
        assert!(parse(&json!({"lyrics": []})).is_none());
        assert!(parse(&json!("text")).is_none());
    }
}

//! YouTube's own subtitles. Many official music videos carry the lyrics as
//! captions written by hand, line by line. Auto-generated tracks are left out:
//! speech recognition mangles singing.
//!
//! The iOS client is the one that lists caption tracks without a session. A song
//! version has none of its own, so its music video is asked when one is known.

use std::time::Duration;

use serde_json::{Value, json};

use super::{Request, get_json};
use crate::lyrics::model::{LyricLine, LyricsResult};

const SOURCE: &str = "YouTube Captions";
const PLAYER_URL: &str = "https://www.youtube.com/youtubei/v1/player?prettyPrint=false";
const USER_AGENT: &str = "com.google.ios.youtube/20.10.4 (iPhone16,2; U; CPU iOS 18_3_2 like Mac OS X)";
const TIMEOUT: Duration = Duration::from_secs(5);

/// The address of the hand-written track to use, the first one YouTube lists.
fn track_url(player: &Value) -> Option<String> {
    let tracks = player.pointer("/captions/playerCaptionsTracklistRenderer/captionTracks")?.as_array()?;
    let track = tracks.iter().find(|t| t.get("kind").and_then(Value::as_str) != Some("asr"))?;
    let url = track.get("baseUrl")?.as_str()?;
    // The address usually asks for XML already. json3 replaces that.
    let base = url.split('&').filter(|part| !part.starts_with("fmt=")).collect::<Vec<_>>().join("&");
    Some(format!("{base}&fmt=json3"))
}

/// One caption's text, without the music notes, the bracketed cues and the line break a caption wraps at.
fn clean(text: &str) -> String {
    let text: String = text.chars().filter(|c| !matches!(c, '\u{266a}' | '\u{266b}')).collect();
    let text = text.split_whitespace().collect::<Vec<_>>().join(" ");
    if text.starts_with('[') && text.ends_with(']') || text.starts_with('(') && text.ends_with(')') && !text.contains(' ') { String::new() } else { text }
}

/// A json3 caption file as a result.
pub fn parse(data: &Value) -> Option<LyricsResult> {
    let events = data.get("events")?.as_array()?;
    let lines: Vec<LyricLine> = events
        .iter()
        .filter_map(|event| {
            let start = event.get("tStartMs")?.as_f64()? / 1000.0;
            let text: String = event.get("segs")?.as_array()?.iter().filter_map(|seg| seg.get("utf8")?.as_str()).collect();
            let text = clean(&text);
            if text.is_empty() {
                return None;
            }
            let mut line = LyricLine::new(Some(start), text);
            line.end = event.get("dDurationMs").and_then(Value::as_f64).map(|ms| start + ms / 1000.0);
            Some(line)
        })
        .collect();
    (!lines.is_empty()).then(|| LyricsResult::from_lines(lines, SOURCE))
}

async fn captions_of(http: &reqwest::Client, video_id: &str) -> Option<LyricsResult> {
    let body = json!({
        "context": { "client": { "clientName": "IOS", "clientVersion": "20.10.4", "deviceModel": "iPhone16,2", "osName": "iPhone", "osVersion": "18.3", "hl": "en", "gl": "US" } },
        "videoId": video_id,
        "contentCheckOk": true,
        "racyCheckOk": true,
    });
    let answer = http.post(PLAYER_URL).header("User-Agent", USER_AGENT).timeout(TIMEOUT).json(&body).send().await.ok()?;
    let player: Value = answer.json().await.ok()?;
    let url = track_url(&player)?;
    match get_json(http, &url, &[], &[("User-Agent", "Mozilla/5.0")], TIMEOUT).await {
        Ok(data) => parse(&data),
        Err(err) => {
            tracing::debug!(%err, "caption fetch failed");
            None
        }
    }
}

pub async fn fetch(http: &reqwest::Client, request: Request<'_>) -> Option<LyricsResult> {
    if request.video_id.is_empty() {
        return None;
    }
    if let Some(found) = captions_of(http, request.video_id).await {
        return Some(found);
    }
    for other in crate::net::twins::other_ids(request.video_id) {
        if let Some(found) = captions_of(http, &other).await {
            return Some(found);
        }
    }
    None
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Hits the network. `cargo test -- --ignored live_captions --nocapture`
    #[tokio::test]
    #[ignore]
    async fn live_captions() {
        let http = reqwest::Client::new();
        for id in ["4NRXx6U8ABQ", "kJQP7kiw5Fk"] {
            let request = Request { video_id: id, title: "", artist: "", duration: 0, strict: true };
            let found = fetch(&http, request).await.expect("captions");
            println!("{id}: {} lines, first {:?}", found.lines.len(), found.lines.first().map(|l| (&l.text, l.start)));
            assert!(found.lines.len() > 10);
        }
    }

    #[test]
    fn auto_generated_tracks_are_passed_over() {
        let player = json!({"captions": {"playerCaptionsTracklistRenderer": {"captionTracks": [
            {"kind": "asr", "baseUrl": "https://y/asr?a=1"},
            {"baseUrl": "https://y/en?a=1&fmt=srv3&b=2"},
        ]}}});
        assert_eq!(track_url(&player).unwrap(), "https://y/en?a=1&b=2&fmt=json3");
        let only_auto = json!({"captions": {"playerCaptionsTracklistRenderer": {"captionTracks": [{"kind": "asr", "baseUrl": "x"}]}}});
        assert!(track_url(&only_auto).is_none());
    }

    #[test]
    fn captions_become_timed_lines_without_notes_or_cues() {
        let data = json!({"events": [
            {"tStartMs": 1360, "dDurationMs": 1680, "segs": [{"utf8": "[\u{266a}\u{266a}\u{266a}]"}]},
            {"tStartMs": 22640, "dDurationMs": 4320, "segs": [{"utf8": "\u{266a} You know the rules\nand so do I \u{266a}"}]},
            {"tStartMs": 30000},
        ]});
        let result = parse(&data).unwrap();
        assert_eq!(result.lines.len(), 1);
        assert_eq!(result.lines[0].text, "You know the rules and so do I");
        assert_eq!(result.lines[0].start, Some(22.64));
        assert_eq!(result.rank(), 2);
    }
}

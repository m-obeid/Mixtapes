//! YouTube's own subtitles. Many official music videos carry the lyrics as
//! captions written by hand, line by line. Auto-generated tracks are left out:
//! speech recognition mangles singing.
//!
//! The iOS client is the one that lists caption tracks without a session. A song
//! version has none of its own, so its music video is asked when one is known.

use std::time::Duration;

use serde_json::{Value, json};

use super::{Request, get_json};
use crate::lyrics::lrc::{Secondary, attach_secondary};
use crate::lyrics::model::{LyricLine, LyricsResult};

const SOURCE: &str = "YouTube Captions";
const PLAYER_URL: &str = "https://www.youtube.com/youtubei/v1/player?prettyPrint=false";
const USER_AGENT: &str = "com.google.ios.youtube/20.10.4 (iPhone16,2; U; CPU iOS 18_3_2 like Mac OS X)";
const TIMEOUT: Duration = Duration::from_secs(5);

/// A caption track YouTube lists: its language ("ko", "pt-BR"), its address, and whether speech recognition wrote it.
struct Track<'a> {
    lang: &'a str,
    url: &'a str,
    auto: bool,
}

fn tracks(player: &Value) -> Vec<Track<'_>> {
    let Some(list) = player.pointer("/captions/playerCaptionsTracklistRenderer/captionTracks").and_then(Value::as_array) else { return Vec::new() };
    list.iter()
        .filter_map(|t| {
            Some(Track {
                lang: t.get("languageCode").and_then(Value::as_str).unwrap_or_default(),
                url: t.get("baseUrl")?.as_str()?,
                auto: t.get("kind").and_then(Value::as_str) == Some("asr"),
            })
        })
        .collect()
}

/// "pt-BR" and "pt" name the same language here.
fn same_language(a: &str, b: &str) -> bool {
    let primary = |code: &str| code.split(['-', '_']).next().unwrap_or_default().to_ascii_lowercase();
    !a.is_empty() && primary(a) == primary(b)
}

/// The hand-written track holding the words as sung, and one in the listener's language to show under them.
///
/// YouTube lists tracks by language name, so the first one is often a translation ("Chinese" ahead of "Korean").
/// The auto-generated track tells the sung language: speech recognition runs on the audio's own language.
/// Without one, the listener's language goes first.
fn pick<'a>(player: &'a Value, viewer: &str) -> Option<(&'a str, Option<&'a str>)> {
    let all = tracks(player);
    let manual: Vec<&Track> = all.iter().filter(|t| !t.auto).collect();
    let sung = all.iter().find(|t| t.auto).map(|t| t.lang);
    let in_viewer = manual.iter().find(|t| same_language(t.lang, viewer)).copied();
    let main = sung.and_then(|lang| manual.iter().find(|t| same_language(t.lang, lang)).copied()).or(in_viewer).or(manual.first().copied())?;
    let under = in_viewer.filter(|t| !same_language(t.lang, main.lang)).map(|t| t.url);
    Some((main.url, under))
}

/// A track's address asking for json3. The address usually asks for XML already.
fn json3(url: &str) -> String {
    let base = url.split('&').filter(|part| !part.starts_with("fmt=")).collect::<Vec<_>>().join("&");
    format!("{base}&fmt=json3")
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
    let viewer = crate::i18n::youtube_language().unwrap_or_else(|| "en".to_owned());
    let (main, under) = pick(&player, &viewer)?;
    let mut result = parse(&caption_file(http, main).await?)?;
    if let Some(under) = under
        && let Some(translation) = caption_file(http, under).await.as_ref().and_then(parse)
    {
        attach_secondary(&mut result.lines, translation.lines, Secondary::Translation);
    }
    Some(result)
}

async fn caption_file(http: &reqwest::Client, url: &str) -> Option<Value> {
    match get_json(http, &json3(url), &[], &[("User-Agent", "Mozilla/5.0")], TIMEOUT).await {
        Ok(data) => Some(data),
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
        for id in ["4NRXx6U8ABQ", "kJQP7kiw5Fk", "M9G4guYg-i8"] {
            let request = Request { video_id: id, title: "", artist: "", duration: 0, strict: true };
            let found = fetch(&http, request).await.expect("captions");
            println!("{id}: {} lines, first {:?}", found.lines.len(), found.lines.first().map(|l| (&l.text, &l.translation, l.start)));
            assert!(found.lines.len() > 10);
        }
    }

    fn track(lang: &str, auto: bool) -> Value {
        let mut t = json!({"languageCode": lang, "baseUrl": format!("https://y/{lang}{}?a=1&fmt=srv3", if auto { "-asr" } else { "" })});
        if auto {
            t["kind"] = json!("asr");
        }
        t
    }

    fn player(tracks: &[Value]) -> Value {
        json!({"captions": {"playerCaptionsTracklistRenderer": {"captionTracks": tracks}}})
    }

    #[test]
    fn auto_generated_tracks_are_passed_over() {
        let both = player(&[track("en", true), track("en", false)]);
        assert_eq!(pick(&both, "en").unwrap(), ("https://y/en?a=1&fmt=srv3", None));
        assert!(pick(&player(&[track("en", true)]), "en").is_none());
        assert_eq!(json3("https://y/en?a=1&fmt=srv3&b=2"), "https://y/en?a=1&b=2&fmt=json3");
    }

    /// TXT's "Angel Bite" MV (#188) in YouTube's order: Chinese first, speech recognition in Korean.
    fn angel_bite() -> Value {
        player(&[track("zh", false), track("en", false), track("ja", false), track("ko", false), track("ko", true), track("es", false)])
    }

    #[test]
    fn the_sung_language_wins_over_the_first_listed() {
        assert_eq!(pick(&angel_bite(), "en").unwrap(), ("https://y/ko?a=1&fmt=srv3", Some("https://y/en?a=1&fmt=srv3")));
        assert_eq!(pick(&angel_bite(), "pt-BR").unwrap(), ("https://y/ko?a=1&fmt=srv3", None));
        assert_eq!(pick(&angel_bite(), "ko").unwrap(), ("https://y/ko?a=1&fmt=srv3", None));
    }

    #[test]
    fn without_a_sung_language_the_listeners_goes_first() {
        let tracks = player(&[track("zh", false), track("pt-BR", false), track("en", false)]);
        assert_eq!(pick(&tracks, "pt").unwrap(), ("https://y/pt-BR?a=1&fmt=srv3", None));
        assert_eq!(pick(&tracks, "de").unwrap(), ("https://y/zh?a=1&fmt=srv3", None));
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

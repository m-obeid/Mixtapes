//! Unison (https://unison.boidu.dev), the Better Lyrics community's own lyrics,
//! looked up by video id first, then by title and artist.
//!
//! Free for open source use with credit: the Lyrics settings page carries it.

use std::time::Duration;

use serde_json::Value;

use super::{Cooldowns, HttpError, Provider, Request, get_json, text_at};
use crate::lyrics::lrc::{parse_lrc_text, plain_lines};
use crate::lyrics::matching::artist_matches;
use crate::lyrics::model::LyricsResult;
use crate::lyrics::ttml::ttml_to_lines;

const SOURCE: &str = "Unison";
const URL: &str = "https://unison.boidu.dev/lyrics";
const USER_AGENT: &str = "Mixtapes (https://github.com/m-obeid/Mixtapes)";
const TIMEOUT: Duration = Duration::from_secs(5);
/// The API allows 120 reads a minute per address.
const RATE_LIMIT_BACKOFF: Duration = Duration::from_secs(60);

/// The `data` of a response as a result. Hidden is what the community voted down.
pub fn parse(data: &Value) -> Option<LyricsResult> {
    if data.get("hidden").and_then(Value::as_bool) == Some(true) {
        return None;
    }
    let body = text_at(data, "lyrics");
    let lines = match text_at(data, "format") {
        "ttml" => ttml_to_lines(body),
        "lrc" => parse_lrc_text(body),
        "plain" => plain_lines(body),
        _ => return None,
    };
    (!lines.is_empty()).then(|| LyricsResult::from_lines(lines, SOURCE))
}

async fn lookup(http: &reqwest::Client, cooldowns: &Cooldowns, query: &[(&str, &str)]) -> Option<Value> {
    if !cooldowns.ready(Provider::Unison) {
        return None;
    }
    match get_json(http, URL, query, &[("User-Agent", USER_AGENT)], TIMEOUT).await {
        Ok(body) => body.get("data").cloned(),
        Err(HttpError::Status(404)) => None,
        Err(HttpError::Status(429)) => {
            cooldowns.trip(Provider::Unison, RATE_LIMIT_BACKOFF, "HTTP 429");
            None
        }
        Err(err) => {
            tracing::debug!(%err, "Unison fetch failed");
            None
        }
    }
}

pub async fn fetch(http: &reqwest::Client, cooldowns: &Cooldowns, request: Request<'_>) -> Option<LyricsResult> {
    // Entries hang off one upload, so a song and its music video can each find the other's.
    let ids = (!request.video_id.is_empty()).then(|| request.video_id.to_owned()).into_iter().chain(crate::net::twins::other_ids(request.video_id));
    for id in ids {
        if let Some(found) = lookup(http, cooldowns, &[("v", &id)]).await.as_ref().and_then(parse) {
            return Some(found);
        }
    }
    if request.artist.is_empty() {
        return None;
    }
    // A name lookup can land on another song, so its artist has to agree.
    let data = lookup(http, cooldowns, &[("song", request.title), ("artist", request.artist)]).await?;
    artist_matches(request.artist, text_at(&data, "artist")).then(|| parse(&data)).flatten()
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    /// Hits the network. `cargo test -- --ignored live_unison --nocapture`
    #[tokio::test]
    #[ignore]
    async fn live_unison() {
        let http = reqwest::Client::new();
        let cooldowns = Cooldowns::default();
        let by_id = Request { video_id: "cxKs2b5lRsA", title: "", artist: "", duration: 0, strict: true };
        let found = fetch(&http, &cooldowns, by_id).await.expect("by video id");
        println!("{} lines at rank {}", found.lines.len(), found.rank());
        let by_name = Request { video_id: "", title: "Flashing Lights", artist: "Kanye West", duration: 0, strict: true };
        println!("by name: {:?}", fetch(&http, &cooldowns, by_name).await.map(|r| r.lines.len()));
    }

    #[test]
    fn every_format_reads() {
        let ttml = r#"<tt xmlns="http://www.w3.org/ns/ttml"><body><div><p begin="1.0" end="2.0"><span begin="1.0" end="1.5">Hey</span> <span begin="1.5" end="2.0">you</span></p></div></body></tt>"#;
        let word = parse(&json!({"format": "ttml", "lyrics": ttml})).unwrap();
        assert_eq!((word.source.as_str(), word.rank()), ("Unison", 3));
        assert_eq!(parse(&json!({"format": "lrc", "lyrics": "[00:01.00]one"})).unwrap().rank(), 2);
        assert_eq!(parse(&json!({"format": "plain", "lyrics": "one\ntwo"})).unwrap().rank(), 1);
    }

    #[test]
    fn hidden_and_unknown_answers_are_nothing() {
        assert!(parse(&json!({"format": "lrc", "lyrics": "[00:01.00]one", "hidden": true})).is_none());
        assert!(parse(&json!({"format": "midi", "lyrics": "x"})).is_none());
    }
}

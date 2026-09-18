//! NetEase Music (music.163.com), by far the broadest source for Japanese, Vocaloid, K-pop and other Asian tracks, and the one that ships romanization and translation tracks beside the lyric.
//!
//! Uses the unencrypted `cloudsearch/pc` search and the `song/lyric` endpoint. Both return plain JSON with no auth.

use std::time::Duration;

use serde_json::Value;

use super::{Request, get_json, label_or_unknown, text_at, title_and_artist, variants_or_title};
use crate::lyrics::lrc::{Secondary, attach_secondary_lrc, parse_lrc_text, strip_leading_credits};
use crate::lyrics::matching::{Candidate, artist_matches, gate, is_generic_title, match_detail, rank_matches};
use crate::lyrics::model::{LyricsMatch, LyricsResult};

const SOURCE: &str = "NetEase";
const SEARCH_URL: &str = "https://music.163.com/api/cloudsearch/pc";
const LYRIC_URL: &str = "https://music.163.com/api/song/lyric";
const HEADERS: [(&str, &str); 2] = [("User-Agent", "Mozilla/5.0"), ("Referer", "https://music.163.com/")];
const TIMEOUT: Duration = Duration::from_secs(5);
const SEARCH_LIMIT: &str = "8";

/// One search hit.
#[derive(Clone, Debug, PartialEq)]
pub struct Song {
    id: i64,
    name: String,
    artists: Vec<String>,
    /// The artist names joined by spaces, which is what gets matched against.
    credited: String,
    duration: u32,
}

impl Candidate for Song {
    fn name(&self) -> &str {
        &self.name
    }
    fn artist(&self) -> &str {
        &self.credited
    }
    fn duration(&self) -> u32 {
        self.duration
    }
}

/// The songs of a `cloudsearch/pc` response. NetEase reports duration in milliseconds as `dt`.
pub fn parse_search(data: &Value) -> Vec<Song> {
    let songs = data.pointer("/result/songs").and_then(Value::as_array).map(Vec::as_slice).unwrap_or_default();
    songs
        .iter()
        .map(|song| {
            let artists: Vec<String> = song.get("ar").and_then(Value::as_array).map(|ar| ar.iter().map(|a| text_at(a, "name").to_owned()).collect()).unwrap_or_default();
            Song {
                id: song.get("id").and_then(Value::as_i64).unwrap_or(0),
                name: text_at(song, "name").to_owned(),
                credited: artists.join(" "),
                artists,
                duration: (song.get("dt").and_then(Value::as_f64).unwrap_or(0.0).max(0.0) / 1000.0) as u32,
            }
        })
        .collect()
}

/// Narrow the hits to this recording and put the best first. None when nothing survives.
pub fn pick(mut songs: Vec<Song>, request: &Request) -> Option<Song> {
    // Hard-filter to artist matches first. For a generic title like "Intro" the closest duration is a coin toss across everyone with an Intro, so an artist mismatch has to disqualify the hit, not just dock its score.
    if !request.artist.is_empty() {
        if songs.iter().any(|s| artist_matches(request.artist, &s.credited)) {
            songs.retain(|s| artist_matches(request.artist, &s.credited));
        } else if is_generic_title(request.title) {
            return None;
        }
    }

    // Drop anything that is not plausibly this recording. Scoring only ranks, it never rejects, so the closest duration used to win even when it was a different song entirely.
    if request.strict {
        songs = gate(songs, request.title, request.artist, request.duration);
    }

    // Closest duration wins, with a small bonus for a title match.
    let title = request.title.trim().to_lowercase();
    songs.sort_by_key(|song| {
        let mut score = 0i64;
        if request.duration != 0 && song.duration != 0 {
            score -= i64::from(request.duration.abs_diff(song.duration).min(30));
        }
        let name = song.name.to_lowercase();
        if !title.is_empty() && (name.contains(&title) || title.contains(&name)) {
            score += 5;
        }
        -score
    });
    songs.into_iter().next()
}

/// A `song/lyric` response as a result. `romalrc` and `tlyric` are optional per track, and both are stamped against the main lyric's clock.
pub fn parse_lyric(data: &Value) -> Option<LyricsResult> {
    let lyric_of = |key: &str| data.pointer(&format!("/{key}/lyric")).and_then(Value::as_str).unwrap_or_default();
    let lrc = lyric_of("lrc").trim();
    if lrc.is_empty() {
        return None;
    }
    // NetEase pads the start with credit lines stamped [00:00.000], lyricist first.
    let mut lines = strip_leading_credits(parse_lrc_text(lrc));
    if lines.is_empty() {
        return None;
    }
    attach_secondary_lrc(&mut lines, lyric_of("romalrc"), Secondary::Romanization);
    attach_secondary_lrc(&mut lines, lyric_of("tlyric"), Secondary::Translation);
    Some(LyricsResult::from_lines(lines, SOURCE))
}

async fn search(http: &reqwest::Client, query: &str) -> Vec<Song> {
    let query = query.trim();
    if query.is_empty() {
        return Vec::new();
    }
    match get_json(http, SEARCH_URL, &[("s", query), ("type", "1"), ("limit", SEARCH_LIMIT), ("offset", "0")], &HEADERS, TIMEOUT).await {
        Ok(data) => parse_search(&data),
        Err(err) => {
            tracing::debug!(%err, "NetEase search failed");
            Vec::new()
        }
    }
}

/// `tv=-1` asks for the translation and `rv=-1` for the romanization.
async fn lyric(http: &reqwest::Client, song_id: i64) -> Option<LyricsResult> {
    if song_id == 0 {
        return None;
    }
    let id = song_id.to_string();
    match get_json(http, LYRIC_URL, &[("id", id.as_str()), ("lv", "1"), ("kv", "1"), ("tv", "-1"), ("rv", "-1")], &HEADERS, TIMEOUT).await {
        Ok(data) => parse_lyric(&data),
        Err(err) => {
            tracing::debug!(%err, "NetEase lyric fetch failed");
            None
        }
    }
}

pub async fn fetch(http: &reqwest::Client, request: Request<'_>) -> Option<LyricsResult> {
    let songs = search(http, &title_and_artist(request.title, request.artist)).await;
    let best = pick(songs, &request)?;
    lyric(http, best.id).await
}

/// Every NetEase song worth offering for this track, with its lyrics.
pub async fn matches(http: &reqwest::Client, request: Request<'_>, limit: usize) -> Vec<LyricsMatch> {
    let mut songs: Vec<Song> = Vec::new();
    for variant in variants_or_title(request.title) {
        for query in [title_and_artist(&variant, request.artist), variant.clone()] {
            for song in search(http, &query).await {
                if !songs.iter().any(|seen| seen.id == song.id) {
                    songs.push(song);
                }
            }
        }
        if songs.len() >= limit * 2 {
            break;
        }
    }
    let mut out = Vec::new();
    for song in rank_matches(songs, request.title, request.artist, request.duration) {
        let Some(result) = lyric(http, song.id).await else { continue };
        out.push(LyricsMatch { label: label_or_unknown(&song.name), detail: match_detail(&song.artists.join(" & "), song.duration), result, source: None });
        if out.len() >= limit {
            break;
        }
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn request<'a>(title: &'a str, artist: &'a str, duration: u32, strict: bool) -> Request<'a> {
        Request { video_id: "", title, artist, duration, strict }
    }

    fn search_response() -> Value {
        json!({"result": {"songs": [
            {"id": 1, "name": "千本桜 (Live)", "ar": [{"name": "初音ミク"}], "dt": 245000},
            {"id": 2, "name": "Sakura Biyori and Time Machine", "ar": [{"name": "Ado"}, {"name": "初音ミク"}], "dt": 300000},
            {"id": 3, "name": "千本桜", "ar": [{"name": "黒うさP"}, {"name": "初音ミク"}], "dt": 244500},
            {"id": 4, "name": "千本桜", "ar": [{"name": "Someone Else"}], "dt": 244000}
        ]}})
    }

    #[test]
    fn search_hits_are_read_with_their_credits() {
        let songs = parse_search(&search_response());
        assert_eq!(songs.len(), 4);
        assert_eq!(songs[2].credited, "黒うさP 初音ミク");
        assert_eq!(songs[2].duration, 244);
        assert!(parse_search(&json!({"result": {}})).is_empty());
        assert!(parse_search(&json!([])).is_empty());
    }

    #[test]
    fn the_pick_is_gated_then_scored() {
        let songs = parse_search(&search_response());
        let best = pick(songs.clone(), &request("千本桜", "初音ミク", 245, true)).unwrap();
        assert_eq!(best.id, 3, "the live take and the other song are gated out, the other artist is narrowed out");
        // The romanization pass skips the gate, so the closest duration with a title bonus wins.
        let loose = pick(songs, &request("千本桜", "初音ミク", 245, false)).unwrap();
        assert_eq!(loose.id, 1);
    }

    #[test]
    fn a_generic_title_needs_its_artist() {
        let songs = parse_search(&json!({"result": {"songs": [{"id": 9, "name": "Intro", "ar": [{"name": "The xx"}], "dt": 128000}]}}));
        assert!(pick(songs.clone(), &request("Intro", "Someone Unrelated", 128, true)).is_none());
        assert_eq!(pick(songs.clone(), &request("Intro", "The xx", 128, true)).unwrap().id, 9);
        assert!(pick(Vec::new(), &request("Intro", "The xx", 128, true)).is_none());
        // A specific title still gets through on the title and duration.
        let named = parse_search(&json!({"result": {"songs": [{"id": 5, "name": "Gruppa krovi", "ar": [{"name": "Кино"}], "dt": 285000}]}}));
        assert_eq!(pick(named, &request("Gruppa krovi", "Kino", 285, true)).unwrap().id, 5);
    }

    #[test]
    fn the_lyric_gets_its_second_lines_and_loses_its_credits() {
        let data = json!({
            "lrc": {"lyric": "[00:00.000] 作词 : 黒うさP\n[00:00.500] 作曲 : 黒うさP\n[00:10.000]千本桜 夜ニ紛レ\n[00:15.000]君ノ声モ届カナイヨ"},
            "romalrc": {"lyric": "[00:10.000]senbonzakura yoru ni magire"},
            "tlyric": {"lyric": "[00:10.000]千本樱 融入夜色\n[00:15.000]你的声音也无法传达"}
        });
        let result = parse_lyric(&data).unwrap();
        assert_eq!(result.source, "NetEase");
        assert!(result.synced);
        assert_eq!(result.lines.len(), 2);
        assert_eq!(result.lines[0].romanization.as_deref(), Some("senbonzakura yoru ni magire"));
        assert_eq!(result.lines[1].romanization, None);
        assert_eq!(result.lines[1].translation.as_deref(), Some("你的声音也无法传达"));
    }

    #[test]
    fn no_lyric_is_no_result() {
        assert!(parse_lyric(&json!({"lrc": {"lyric": "  "}})).is_none());
        assert!(parse_lyric(&json!({"nolyric": true})).is_none());
        assert!(parse_lyric(&json!({"lrc": {"lyric": "[00:00.000] 作词 : X"}})).is_none());
        assert!(parse_lyric(&json!({"lrc": {"lyric": "unsynced words only"}})).is_none());
    }
}

//! QQ Music (y.qq.com), strong on Japanese anime and game songs and Chinese releases NetEase misses.
//!
//! Uses the plain web search and the `nobase64` lyric endpoint, which answer JSON with no account.
//! Word-level QRC is encrypted, so this stays line-synced.

use std::time::Duration;

use serde_json::Value;

use super::{Request, get_json, label_or_unknown, text_at, title_and_artist, variants_or_title};
use crate::lyrics::lrc::{Secondary, attach_secondary_lrc, parse_lrc_text};
use crate::lyrics::matching::{Candidate, artist_matches, gate, is_generic_title, match_detail, rank_matches};
use crate::lyrics::model::{LyricLine, LyricsMatch, LyricsResult};

const SOURCE: &str = "QQ Music";
const SEARCH_URL: &str = "https://c.y.qq.com/soso/fcgi-bin/client_search_cp";
const LYRIC_URL: &str = "https://c.y.qq.com/lyric/fcgi-bin/fcg_query_lyric_new.fcg";
const HEADERS: [(&str, &str); 2] = [("User-Agent", "Mozilla/5.0"), ("Referer", "https://y.qq.com/")];
const TIMEOUT: Duration = Duration::from_secs(5);
const SEARCH_LIMIT: &str = "8";
/// What QQ Music answers for a song with no words.
const INSTRUMENTAL: &str = "\u{7eaf}\u{97f3}\u{4e50}";

/// One search hit.
#[derive(Clone, Debug, PartialEq)]
pub struct Song {
    mid: String,
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

pub fn parse_search(data: &Value) -> Vec<Song> {
    let songs = data.pointer("/data/song/list").and_then(Value::as_array).map(Vec::as_slice).unwrap_or_default();
    songs
        .iter()
        .map(|song| {
            let artists: Vec<String> = song.get("singer").and_then(Value::as_array).map(|list| list.iter().map(|a| text_at(a, "name").to_owned()).collect()).unwrap_or_default();
            Song {
                mid: text_at(song, "mid").to_owned(),
                name: text_at(song, "name").to_owned(),
                credited: artists.join(" "),
                artists,
                duration: song.get("interval").and_then(Value::as_u64).unwrap_or(0) as u32,
            }
        })
        .filter(|song| !song.mid.is_empty())
        .collect()
}

/// Narrow the hits to this recording, closest length first. None when nothing survives.
pub fn pick(mut songs: Vec<Song>, request: &Request) -> Option<Song> {
    if !request.artist.is_empty() {
        if songs.iter().any(|s| artist_matches(request.artist, &s.credited)) {
            songs.retain(|s| artist_matches(request.artist, &s.credited));
        } else if is_generic_title(request.title) {
            return None;
        }
    }
    if request.strict {
        songs = gate(songs, request.title, request.artist, request.duration);
    }
    songs.sort_by_key(|song| if request.duration == 0 || song.duration == 0 { 0 } else { request.duration.abs_diff(song.duration) });
    songs.into_iter().next()
}

/// QQ Music opens with a line naming the song, then the writers as `詞∶Name`. They sit before the first sung line.
fn without_header(lines: Vec<LyricLine>) -> Vec<LyricLine> {
    let is_credit = |line: &LyricLine| {
        let text = line.text.trim();
        text.split([':', '：', '∶']).next().is_some_and(|head| head.len() < text.len() && head.chars().count() <= 4)
    };
    let skip_title = usize::from(lines.first().is_some_and(|l| l.start == Some(0.0)));
    let first = lines.iter().skip(skip_title).position(|l| !is_credit(l)).map_or(lines.len(), |at| at + skip_title);
    lines.into_iter().skip(first).collect()
}

/// A lyric response as a result. `trans` is stamped against the main lyric's clock.
pub fn parse_lyric(data: &Value) -> Option<LyricsResult> {
    let lrc = text_at(data, "lyric");
    if lrc.contains(INSTRUMENTAL) {
        return None;
    }
    let mut lines = without_header(parse_lrc_text(lrc));
    if lines.is_empty() {
        return None;
    }
    attach_secondary_lrc(&mut lines, text_at(data, "trans"), Secondary::Translation);
    Some(LyricsResult::from_lines(lines, SOURCE))
}

async fn search(http: &reqwest::Client, query: &str) -> Vec<Song> {
    let query = query.trim();
    if query.is_empty() {
        return Vec::new();
    }
    let params = [("w", query), ("format", "json"), ("p", "1"), ("n", SEARCH_LIMIT), ("cr", "1"), ("new_json", "1"), ("inCharset", "utf8"), ("outCharset", "utf-8")];
    match get_json(http, SEARCH_URL, &params, &HEADERS, TIMEOUT).await {
        Ok(data) => parse_search(&data),
        Err(err) => {
            tracing::debug!(%err, "QQ Music search failed");
            Vec::new()
        }
    }
}

async fn lyric(http: &reqwest::Client, mid: &str) -> Option<LyricsResult> {
    match get_json(http, LYRIC_URL, &[("songmid", mid), ("format", "json"), ("nobase64", "1"), ("g_tk", "5381")], &HEADERS, TIMEOUT).await {
        Ok(data) => parse_lyric(&data),
        Err(err) => {
            tracing::debug!(%err, "QQ Music lyric fetch failed");
            None
        }
    }
}

pub async fn fetch(http: &reqwest::Client, request: Request<'_>) -> Option<LyricsResult> {
    let songs = search(http, &title_and_artist(request.title, request.artist)).await;
    let best = pick(songs, &request)?;
    lyric(http, &best.mid).await
}

/// Every QQ Music song worth offering for this track, with its lyrics.
pub async fn matches(http: &reqwest::Client, request: Request<'_>, limit: usize) -> Vec<LyricsMatch> {
    let mut songs: Vec<Song> = Vec::new();
    for variant in variants_or_title(request.title) {
        for query in [title_and_artist(&variant, request.artist), variant.clone()] {
            for song in search(http, &query).await {
                if !songs.iter().any(|seen| seen.mid == song.mid) {
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
        let Some(result) = lyric(http, &song.mid).await else { continue };
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

    fn request<'a>(title: &'a str, artist: &'a str, duration: u32) -> Request<'a> {
        Request { video_id: "", title, artist, duration, strict: true }
    }

    fn song(mid: &str, name: &str, duration: u32) -> Song {
        Song { mid: mid.to_owned(), name: name.to_owned(), artists: vec!["nao".to_owned()], credited: "nao".to_owned(), duration }
    }

    /// Hits the network. `cargo test -- --ignored live_qq --nocapture`
    #[tokio::test]
    #[ignore]
    async fn live_qq() {
        let http = reqwest::Client::new();
        let found = fetch(&http, request("流星のビヴロスト", "nao", 240)).await.expect("the song");
        println!("{} lines, first {:?}", found.lines.len(), found.lines.first().map(|l| (&l.text, l.start)));
        assert!(found.lines.len() > 10);
        assert!(found.synced);
    }

    #[test]
    fn the_right_length_beats_the_remix_and_the_short_version() {
        let songs = vec![song("short", "流星のビヴロスト", 93), song("remix", "流星のビヴロスト", 351), song("full", "流星のビヴロスト", 240)];
        assert_eq!(pick(songs, &request("流星のビヴロスト", "nao", 241)).unwrap().mid, "full");
    }

    #[test]
    fn the_header_lines_go_and_the_song_stays() {
        let lrc = "[ti:Song]\n[offset:0]\n[00:00.00]Song - nao (なお)\n[00:04.59]詞∶Someone\n[00:06.37]曲∶Someone\n[00:07.74]世界に生まれたその意味は\n[00:12.00]Time: a sung line";
        let result = parse_lyric(&json!({"lyric": lrc})).unwrap();
        assert_eq!(result.lines[0].text, "世界に生まれたその意味は");
        assert_eq!(result.lines.len(), 2);
        assert!(result.synced);
    }

    #[test]
    fn a_translation_joins_its_line() {
        let result = parse_lyric(&json!({"lyric": "[00:01.00]愛してる", "trans": "[00:01.00]我爱你"})).unwrap();
        assert_eq!(result.lines[0].translation.as_deref(), Some("我爱你"));
    }

    #[test]
    fn instrumentals_and_empty_answers_are_nothing() {
        assert!(parse_lyric(&json!({"lyric": "[00:00.00]此歌曲为没有填词的纯音乐，请您欣赏"})).is_none());
        assert!(parse_lyric(&json!({"lyric": ""})).is_none());
    }
}

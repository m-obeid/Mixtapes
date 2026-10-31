//! QQ Music (y.qq.com), strong on Japanese anime and game songs and Chinese releases NetEase misses.
//!
//! Uses the plain web search and two lyric endpoints, none of which need an account: the
//! signed app API for word-level QRC, and the `nobase64` one for LRC and any translation.

use std::sync::LazyLock;
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use regex::Regex;
use serde_json::{Value, json};
use sha1::{Digest, Sha1};

use super::{Request, get_json, label_or_unknown, text_at, title_and_artist, variants_or_title};
use crate::lyrics::lrc::{Secondary, attach_secondary_lrc, parse_lrc_text};
use crate::lyrics::matching::{Candidate, artist_matches, gate, is_generic_title, match_detail, rank_matches};
use super::qrc::decode_hex;
use crate::lyrics::model::{LyricLine, LyricPart, LyricsMatch, LyricsResult};

const SOURCE: &str = "QQ Music";
const SEARCH_URL: &str = "https://c.y.qq.com/soso/fcgi-bin/client_search_cp";
const LYRIC_URL: &str = "https://c.y.qq.com/lyric/fcgi-bin/fcg_query_lyric_new.fcg";
const HEADERS: [(&str, &str); 2] = [("User-Agent", "Mozilla/5.0"), ("Referer", "https://y.qq.com/")];
const TIMEOUT: Duration = Duration::from_secs(5);
const SEARCH_LIMIT: &str = "8";
const API_URL: &str = "https://u.y.qq.com/cgi-bin/musics.fcg";
const API_USER_AGENT: &str = "Mozilla/5.0 (Windows NT 10.0; Win64; x64) AppleWebKit/537.36";
const CLIENT_VERSION: u32 = 13020508;
const SIGN_SCRAMBLE: [u8; 20] = [89, 39, 179, 150, 218, 82, 58, 252, 177, 52, 186, 123, 120, 64, 242, 133, 143, 161, 121, 179];

/// `[start,duration]` opening a timed line, in milliseconds.
static LINE_RE: LazyLock<Regex> = LazyLock::new(|| Regex::new(r"^\[(\d+),(\d+)\](.*)$").expect("static regex"));
/// A word and the `(start,duration)` following it.
static WORD_RE: LazyLock<Regex> = LazyLock::new(|| Regex::new(r"(.*?)\((\d+),(\d+)\)").expect("static regex"));

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
    let skip_title = usize::from(lines.first().is_some_and(|l| l.start == Some(0.0) || l.text.contains(" - ")));
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

/// The body of a decoded QRC document. It sits in an attribute, so a line break in it
/// is real, and an XML parser would flatten it to a space.
fn lyric_content(document: &str) -> Option<String> {
    let start = document.find("LyricContent=\"")? + "LyricContent=\"".len();
    // The attribute ends at its closing tag: a quote sung in a line arrives unescaped.
    let end = start + document[start..].rfind("\"/>")?;
    Some(document[start..end].replace("&quot;", "\"").replace("&apos;", "'").replace("&lt;", "<").replace("&gt;", ">").replace("&amp;", "&"))
}

/// QRC lines as lyric lines with word timing. A plain LRC body has none of these lines.
pub fn parse_qrc(content: &str) -> Vec<LyricLine> {
    let seconds = |text: &str| text.parse::<f64>().unwrap_or(0.0) / 1000.0;
    let lines = content.lines().filter_map(|raw| {
        let timed = LINE_RE.captures(raw.trim())?;
        let start = seconds(&timed[1]);
        let mut parts: Vec<LyricPart> = Vec::new();
        for word in WORD_RE.captures_iter(&timed[3]) {
            let text = &word[1];
            // Songs in Latin script time the gap too: a token of bare space, or a space leading the next word.
            if text.starts_with(char::is_whitespace) {
                if let Some(last) = parts.last_mut() {
                    last.space_after = true;
                }
            }
            if !text.trim().is_empty() {
                let at = seconds(&word[2]);
                parts.push(LyricPart { start: Some(at), end: Some(at + seconds(&word[3])), text: text.trim().to_owned(), space_after: text.ends_with(char::is_whitespace) });
            }
        }
        if parts.is_empty() {
            return None;
        }
        let text: String = parts.iter().map(|p| if p.space_after { format!("{} ", p.text) } else { p.text.clone() }).collect();
        let mut line = LyricLine::new(Some(start), text.trim());
        line.end = Some(start + seconds(&timed[2]));
        line.parts = parts;
        Some(line)
    });
    without_header(lines.collect())
}

/// The signature the app API checks, computed from the request body.
fn sign(body: &str) -> String {
    let hash: String = Sha1::digest(body.as_bytes()).iter().map(|byte| format!("{byte:02X}")).collect();
    let pick = |positions: &[usize]| positions.iter().map(|at| &hash[*at..=*at]).collect::<String>();
    let mixed: Vec<u8> = (0..20).map(|at| SIGN_SCRAMBLE[at] ^ u8::from_str_radix(&hash[at * 2..at * 2 + 2], 16).unwrap_or(0)).collect();
    let encoded: String = glib::base64_encode(&mixed).chars().filter(|c| !matches!(c, '/' | '+' | '=')).collect();
    format!("zzc{}{encoded}{}", pick(&[23, 14, 6, 36, 16, 7, 19]), pick(&[16, 1, 32, 12, 19, 27, 8, 5])).to_lowercase()
}

/// Word-level lyrics through the signed app API. None sends the caller to plain LRC.
async fn word_level(http: &reqwest::Client, mid: &str) -> Option<Vec<LyricLine>> {
    crate::net::offline::check().ok()?;
    let nanos = SystemTime::now().duration_since(UNIX_EPOCH).map_or(0, |d| d.as_nanos());
    let device: String = Sha1::digest(nanos.to_string().as_bytes()).iter().map(|byte| format!("{byte:02X}")).collect();
    let body = json!({
        "comm": { "wid": &device[..32], "cv": CLIENT_VERSION, "v": CLIENT_VERSION, "QIMEI36": "8888888888888888", "ct": "11", "tmeAppID": "qqmusic", "format": "json", "inCharset": "utf-8", "outCharset": "utf-8", "uid": "3931641530" },
        "music.musichallSong.PlayLyricInfo.GetPlayLyricInfo": {
            "module": "music.musichallSong.PlayLyricInfo",
            "method": "GetPlayLyricInfo",
            "param": { "crypt": 1, "ct": 11, "cv": CLIENT_VERSION, "lrc_t": 0, "qrc": 1, "qrc_t": 0, "roma": 0, "roma_t": 0, "trans": 0, "trans_t": 0, "type": 1, "songMid": mid },
        },
    })
    .to_string();
    let url = format!("{API_URL}?sign={}", sign(&body));
    let request = http.post(url).header("User-Agent", API_USER_AGENT).header("Referer", "https://y.qq.com/").header("Origin", "https://y.qq.com").header("Content-Type", "application/json").timeout(TIMEOUT).body(body);
    let answer: Value = request.send().await.ok()?.json().await.ok()?;
    let encrypted = answer.pointer("/music.musichallSong.PlayLyricInfo.GetPlayLyricInfo/data/lyric")?.as_str()?;
    let document = decode_hex(encrypted)?;
    let lines = parse_qrc(&lyric_content(&document).unwrap_or(document));
    (!lines.is_empty()).then_some(lines)
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

/// Word timing when QQ Music has it, line timing otherwise. The translation only comes with the LRC.
async fn lyric(http: &reqwest::Client, mid: &str) -> Option<LyricsResult> {
    let query = [("songmid", mid), ("format", "json"), ("nobase64", "1"), ("g_tk", "5381")];
    let (words, plain) = tokio::join!(word_level(http, mid), get_json(http, LYRIC_URL, &query, &HEADERS, TIMEOUT));
    let plain = plain.inspect_err(|err| tracing::debug!(%err, "QQ Music lyric fetch failed")).ok();
    let Some(mut lines) = words else { return parse_lyric(&plain?) };
    if let Some(data) = &plain {
        attach_secondary_lrc(&mut lines, text_at(data, "trans"), Secondary::Translation);
    }
    Some(LyricsResult::from_lines(lines, SOURCE))
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
        assert_eq!(found.rank(), 3, "word timing");
        println!("last {:?}", found.lines.last().map(|l| (&l.text, l.start)));
        println!("words {:?}", found.lines[0].parts.iter().take(5).map(|p| (&p.text, p.start)).collect::<Vec<_>>());
    }

    #[test]
    fn the_signature_has_the_shape_the_api_wants() {
        let signed = sign("body");
        assert!(signed.starts_with("zzc"));
        assert_eq!(signed, signed.to_lowercase());
        assert_eq!(signed, sign("body"));
        assert_ne!(signed, sign("other"));
    }

    #[test]
    fn qrc_lines_carry_their_word_timing() {
        let content = "[ti:Song]\n[offset:0]\n[5767,324]Song - (5767,109)nao(5876,106)\n[292333,1910]Let (292333,136)it (292469,97)go(292566,202)\n[300000,500]少(300000,100)年(300100,100)";
        let lines = parse_qrc(content);
        assert_eq!(lines.len(), 2, "the title line goes");
        assert_eq!(lines[0].text, "Let it go");
        assert_eq!(lines[0].start, Some(292.333));
        assert!((lines[0].end.unwrap() - 294.243).abs() < 0.001);
        assert_eq!(lines[0].parts[1].start, Some(292.469));
        assert!(lines[0].parts[0].space_after);
        assert_eq!(lines[1].text, "少年");
    }

    #[test]
    fn a_quote_in_a_line_does_not_end_the_lyrics() {
        let document = "<Lyric_1 LyricContent=\"[1,2]a(1,2)\n[3,4]\"(3,0)b(3,4)\n[5,6]c(5,6)\n\"/>\n</LyricInfo>";
        assert_eq!(parse_qrc(&lyric_content(document).unwrap()).len(), 3);
    }

    #[test]
    fn a_timed_gap_is_a_space_between_words() {
        let lines = parse_qrc("[1000,3000]Peeled(1000,200) (1200,0)the(1200,100) (1300,0)paint(1300,100)\n[5000,1000]Let (5000,100) it(5100,100)\n[7000,1000](Hey)(7000,500) you(7500,500)");
        assert_eq!(lines[0].text, "Peeled the paint");
        assert_eq!(lines[0].parts.len(), 3);
        assert_eq!(lines[1].text, "Let it");
        assert_eq!(lines[2].text, "(Hey) you");
    }

    /// Hits the network. `cargo test -- --ignored live_qq_english --nocapture`
    #[tokio::test]
    #[ignore]
    async fn live_qq_english() {
        let found = fetch(&reqwest::Client::new(), request("Clover", "FLAVOR FOLEY", 256)).await.expect("the song");
        for line in found.lines.iter().take(3) {
            println!("{:?}", line.text);
        }
        assert!(found.lines[0].text.contains(' '));
        assert!(found.lines.last().and_then(|l| l.start).unwrap() > 240.0, "the whole song");
    }

    #[test]
    fn the_body_comes_out_of_its_attribute_with_its_line_breaks() {
        let document = "<QrcInfos><Lyric_1 LyricType=\"1\" LyricContent=\"[1,2]a(1,2)\n[3,4]&amp;(3,4)\"/></QrcInfos>";
        assert_eq!(lyric_content(document).unwrap(), "[1,2]a(1,2)\n[3,4]&(3,4)");
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

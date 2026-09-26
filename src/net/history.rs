//! Listening history: the plays YouTube has recorded, the token that removes
//! one, and the ping that records a new one. Port of ytmusicapi's
//! `get_history`, `remove_history_items` and `add_history_item`, plus the
//! disk cache MusicClient keeps beside the download library.
//!
//! The cache is the one-row `history_cache` table in the SQLite file both
//! apps share, written in ytmusicapi's own dict shape so either app can read
//! what the other left.

use std::sync::{Arc, LazyLock};

use regex::Regex;

use serde::{Deserialize, Serialize};
use serde_json::{Value, json};

use super::browse::Browse;
use super::items::{MRLIR, array_at, history_feedback_token, owned_at, parse_playlist_item};
use crate::downloads::store::Store;
use crate::model::{HttpAuth, LikeStatus, Named, Person, Track, VideoId};
use crate::net::ytmusic::NetError;

/// The heading a play with no `played` value is filed under.
const UNDATED: &str = "Recently";
/// What YouTube's playback tracker wants alongside the ping.
const CLIENT_NAME: &str = "WEB_REMIX";
/// Character set of the playback id ytmusicapi makes up per ping.
const CPN_ALPHABET: &[u8] = b"abcdefghijklmnopqrstuvwxyzABCDEFGHIJKLMNOPQRSTUVWXYZ0123456789-_";

/// One play: the track, when YouTube says it happened, and how to forget it.
#[derive(Clone, Debug)]
pub struct HistoryEntry {
    pub track: Track,
    /// The shelf heading this play arrived under: Today, Yesterday, This week.
    pub played: String,
    /// Removes this play when handed to the feedback endpoint. Absent on a
    /// brand account, where YouTube offers no such menu entry.
    pub feedback_token: Option<String>,
}

/// Every play, newest first, grouped the way YouTube grouped them.
pub async fn get_history(api: Arc<dyn Browse>) -> Result<Vec<HistoryEntry>, NetError> {
    let response = api.post("browse", json!({ "browseId": "FEmusic_history" })).await?;
    Ok(parse_history(&response))
}

pub fn parse_history(response: &Value) -> Vec<HistoryEntry> {
    let sections = array_at(response, "/contents/singleColumnBrowseResultsRenderer/tabs/0/tabRenderer/content/sectionListRenderer/contents");
    let mut entries = Vec::new();
    for section in sections {
        let Some(shelf) = section.get("musicShelfRenderer") else { continue };
        let played = owned_at(shelf, "/title/runs/0/text").unwrap_or_else(|| UNDATED.to_owned());
        for row in array_at(shelf, "/contents") {
            let Some(data) = row.get(MRLIR) else { continue };
            let Some(track) = parse_playlist_item(data, false, false) else { continue };
            if track.video_id.0.is_empty() {
                continue;
            }
            entries.push(HistoryEntry { track: without_view_counts(track), played: played.clone(), feedback_token: history_feedback_token(data) });
        }
    }
    entries
}

/// Port of _normalize_durations' artist pass: YouTube files the view count
/// in the same column as the artists, so ytmusicapi hands it back as one of
/// them. Left alone it reads as "Jamie Paige, 7.2M views" everywhere the
/// artist line is shown.
fn without_view_counts(mut track: Track) -> Track {
    track.artists.retain(|artist| !is_view_count(&artist.name));
    track.artist = track.artists.iter().map(|a| a.name.as_str()).filter(|n| !n.is_empty()).collect::<Vec<_>>().join(", ");
    track
}

fn is_view_count(text: &str) -> bool {
    VIEW_COUNT_RE.is_match(text)
}

static VIEW_COUNT_RE: LazyLock<Regex> = LazyLock::new(|| Regex::new(r"^\s*\d+(?:[.,]\d+)?\s*[kKmMbB]?\s*views?\s*$").unwrap());

/// Forget plays. The tokens come off the rows themselves.
pub async fn remove_history_items(api: &dyn Browse, tokens: Vec<String>) -> Result<(), NetError> {
    if tokens.is_empty() {
        return Ok(());
    }
    api.post("feedback", json!({ "feedbackTokens": tokens })).await?;
    Ok(())
}

// -- recording a play -----------------------------------------------------

/// Port of add_history_item: ping the playback tracker the player endpoint
/// hands out for this video, which is what puts the play in the history.
///
/// Answers with the track as YouTube describes it, so the caller can put the
/// play at the top of the cache before the server-side roll-up catches up.
pub async fn record_play(api: &dyn Browse, http: &reqwest::Client, auth: Option<&HttpAuth>, video_id: &str) -> Result<Track, NetError> {
    let song = api.post("player", json!({ "videoId": video_id })).await?;
    let Some(url) = owned_at(&song, "/playbackTracking/videostatsPlaybackUrl/baseUrl") else {
        return Err(NetError::Message(format!("{video_id} has no playback tracker")));
    };

    // The tracker URL already carries a query string, so the three parameters
    // ytmusicapi adds go on the end of it.
    let separator = if url.contains('?') { '&' } else { '?' };
    let url = format!("{url}{separator}ver=2&c={CLIENT_NAME}&cpn={}", cpn());
    let mut request = http.get(&url);
    if let Some(auth) = auth {
        request = request.header("Cookie", &auth.cookie).header("User-Agent", &auth.user_agent);
        if let Some(authorization) = &auth.authorization {
            request = request.header("Authorization", authorization);
        }
    }
    let status = request.send().await?.status();
    if !status.is_success() {
        return Err(NetError::Http { status: status.as_u16(), message: "playback ping refused".into() });
    }
    Ok(track_from_player(&song, video_id))
}

/// A 16-character playback id. YouTube only needs it to be different each time.
fn cpn() -> String {
    let mut seed = glib::monotonic_time() as u64 ^ 0x9e37_79b9_7f4a_7c15;
    (0..16)
        .map(|_| {
            // xorshift: a throwaway id needs no more than this.
            seed ^= seed << 13;
            seed ^= seed >> 7;
            seed ^= seed << 17;
            CPN_ALPHABET[(seed % CPN_ALPHABET.len() as u64) as usize] as char
        })
        .collect()
}

/// What the player endpoint says about the video, as a queue entry.
fn track_from_player(song: &Value, video_id: &str) -> Track {
    let details = song.pointer("/videoDetails").unwrap_or(&Value::Null);
    let author = owned_at(details, "/author").unwrap_or_default();
    let artists = if author.is_empty() { Vec::new() } else { vec![Person { name: author.clone(), id: owned_at(details, "/channelId") }] };
    Track {
        video_id: VideoId(video_id.to_owned()),
        title: owned_at(details, "/title").unwrap_or_default(),
        artist: author,
        artists,
        thumb: array_at(details, "/thumbnail/thumbnails").last().and_then(|t| owned_at(t, "/url")),
        duration_seconds: owned_at(details, "/lengthSeconds").and_then(|s| s.parse().ok()),
        ..Track::default()
    }
}

// -- the shared cache -----------------------------------------------------

/// One cached play. The field names are ytmusicapi's, because the Python app
/// reads and writes this same row.
#[derive(Clone, Debug, Default, Serialize, Deserialize)]
struct CachedPlay {
    #[serde(rename = "videoId")]
    video_id: String,
    #[serde(default)]
    title: String,
    #[serde(default)]
    artists: Vec<Person>,
    #[serde(default)]
    album: Option<Named>,
    #[serde(default)]
    thumbnails: Vec<CachedThumbnail>,
    #[serde(default)]
    duration: Option<String>,
    #[serde(default)]
    duration_seconds: Option<u32>,
    #[serde(default, rename = "likeStatus")]
    like_status: Option<LikeStatus>,
    #[serde(default, rename = "isExplicit")]
    is_explicit: bool,
    #[serde(default)]
    played: Option<String>,
    #[serde(default, rename = "feedbackToken")]
    feedback_token: Option<String>,
}

#[derive(Clone, Debug, Default, Serialize, Deserialize)]
struct CachedThumbnail {
    url: String,
}

impl From<&HistoryEntry> for CachedPlay {
    fn from(entry: &HistoryEntry) -> Self {
        let track = &entry.track;
        Self {
            video_id: track.video_id.0.clone(),
            title: track.title.clone(),
            artists: track.artists.clone(),
            album: track.album.clone(),
            thumbnails: track.thumb.iter().map(|url| CachedThumbnail { url: url.clone() }).collect(),
            duration: track.duration_seconds.map(|d| format!("{}:{:02}", d / 60, d % 60)),
            duration_seconds: track.duration_seconds,
            like_status: Some(track.like_status),
            is_explicit: track.is_explicit,
            played: Some(entry.played.clone()),
            feedback_token: entry.feedback_token.clone(),
        }
    }
}

impl From<CachedPlay> for HistoryEntry {
    fn from(play: CachedPlay) -> Self {
        let artist = play.artists.iter().map(|a| a.name.as_str()).filter(|n| !n.is_empty()).collect::<Vec<_>>().join(", ");
        HistoryEntry {
            track: Track {
                video_id: VideoId(play.video_id),
                title: play.title,
                artist,
                artists: play.artists,
                album: play.album,
                thumb: play.thumbnails.last().map(|t| t.url.clone()),
                duration_seconds: play.duration_seconds.or_else(|| play.duration.as_deref().and_then(parse_clock)),
                like_status: play.like_status.unwrap_or_default(),
                is_explicit: play.is_explicit,
                ..Track::default()
            },
            played: play.played.unwrap_or_else(|| UNDATED.to_owned()),
            feedback_token: play.feedback_token,
        }
    }
}

/// "3:42" or "1:02:03" to seconds, what _normalize_durations filled in.
fn parse_clock(text: &str) -> Option<u32> {
    let mut total = 0u32;
    for part in text.split(':') {
        total = total.checked_mul(60)?.checked_add(part.parse().ok()?)?;
    }
    Some(total)
}

pub fn cached_history(store: &Store) -> Vec<HistoryEntry> {
    let Some(json) = store.history_cache() else { return Vec::new() };
    match serde_json::from_str::<Vec<CachedPlay>>(&json) {
        Ok(plays) => plays.into_iter().map(HistoryEntry::from).collect(),
        Err(err) => {
            tracing::warn!(%err, "history cache unreadable");
            Vec::new()
        }
    }
}

pub fn cache_history(store: &Store, entries: &[HistoryEntry]) {
    let plays: Vec<CachedPlay> = entries.iter().map(CachedPlay::from).collect();
    match serde_json::to_string(&plays) {
        Ok(json) => store.set_history_cache(&json),
        Err(err) => tracing::warn!(%err, "history cache not written"),
    }
}

/// Drop one play from the cache, after it was removed upstream.
pub fn forget_cached(store: &Store, video_id: &str) {
    let mut entries = cached_history(store);
    let before = entries.len();
    entries.retain(|entry| entry.track.video_id.0 != video_id);
    if entries.len() != before {
        cache_history(store, &entries);
    }
}

/// Put a play at the top of the cache, so the page shows it before YouTube's
/// own history catches up. Port of _prepend_to_history_cache.
pub fn prepend_cached(store: &Store, track: &Track) {
    let mut entries = cached_history(store);
    entries.retain(|entry| entry.track.video_id != track.video_id);
    entries.insert(0, HistoryEntry { track: track.clone(), played: "Today".to_owned(), feedback_token: None });
    cache_history(store, &entries);
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Hits the network. `cargo test -- --ignored live_history --nocapture`
    #[tokio::test]
    #[ignore]
    async fn live_history() {
        let paths = crate::paths::Paths::discover();
        let client = crate::net::ytmusic::YtMusic::new(&paths).unwrap();
        let entries = get_history(client.api()).await.expect("history");
        println!("{} plays", entries.len());
        let mut shelf = String::new();
        for entry in entries.iter().take(12) {
            if entry.played != shelf {
                shelf = entry.played.clone();
                println!("-- {shelf}");
            }
            println!("   {:<34} {:<12} {:>6}s like={:?} token={}", entry.track.title.chars().take(32).collect::<String>(), entry.track.artist.chars().take(12).collect::<String>(), entry.track.duration_seconds.unwrap_or(0), entry.track.like_status, entry.feedback_token.is_some());
        }
        assert!(!entries.is_empty(), "the account has plays");
        assert!(entries.iter().all(|e| !e.track.video_id.0.is_empty() && !e.played.is_empty()));

        let handle = client.account_info().await.expect("account").and_then(|a| a.handle).expect("a handle");
        let channel = crate::net::artist::resolve_handle(&client.api(), &handle).await.expect("resolve");
        println!("handle {handle} -> {channel:?}");
        assert!(channel.is_some_and(|id| id.starts_with("UC")));
    }


    fn row(video_id: &str, title: &str, token: Option<&str>) -> Value {
        let mut menu = json!({ "menuRenderer": { "items": [] } });
        if let Some(token) = token {
            menu["menuRenderer"]["items"] = json!([{ "menuServiceItemRenderer": {
                "icon": { "iconType": "REMOVE_FROM_HISTORY" },
                "serviceEndpoint": { "feedbackEndpoint": { "feedbackToken": token } }
            }}]);
        }
        json!({ MRLIR: {
            "flexColumns": [
                { "musicResponsiveListItemFlexColumnRenderer": { "text": { "runs": [{ "text": title, "navigationEndpoint": { "watchEndpoint": { "videoId": video_id } } }] } } },
                { "musicResponsiveListItemFlexColumnRenderer": { "text": { "runs": [{ "text": "An Artist", "navigationEndpoint": { "browseEndpoint": { "browseId": "UC1", "browseEndpointContextSupportedConfigs": { "browseEndpointContextMusicConfig": { "pageType": "MUSIC_PAGE_TYPE_ARTIST" } } } } }] } } }
            ],
            "fixedColumns": [{ "musicResponsiveListItemFixedColumnRenderer": { "text": { "runs": [{ "text": "3:42" }] } } }],
            "overlay": { "musicItemThumbnailOverlayRenderer": { "content": { "musicPlayButtonRenderer": { "playNavigationEndpoint": { "watchEndpoint": { "videoId": video_id } } } } } },
            "menu": menu
        }})
    }

    fn shelf(title: &str, rows: Value) -> Value {
        json!({ "musicShelfRenderer": { "title": { "runs": [{ "text": title }] } , "contents": rows } })
    }

    fn response(shelves: Vec<Value>) -> Value {
        json!({ "contents": { "singleColumnBrowseResultsRenderer": { "tabs": [{ "tabRenderer": { "content": { "sectionListRenderer": { "contents": shelves } } } }] } } })
    }

    #[test]
    fn every_play_keeps_the_heading_it_arrived_under() {
        let parsed = parse_history(&response(vec![
            shelf("Today", json!([row("a", "First", Some("tok-a")), row("b", "Second", None)])),
            shelf("Yesterday", json!([row("c", "Third", Some("tok-c"))])),
        ]));
        assert_eq!(parsed.len(), 3);
        assert_eq!(parsed.iter().map(|e| e.played.as_str()).collect::<Vec<_>>(), ["Today", "Today", "Yesterday"]);
        assert_eq!(parsed[0].feedback_token.as_deref(), Some("tok-a"));
        assert_eq!(parsed[1].feedback_token, None, "a brand account gets no remove entry");
        assert_eq!(parsed[0].track.title, "First");
        assert_eq!(parsed[0].track.artist, "An Artist");
        assert_eq!(parsed[0].track.duration_seconds, Some(222));
    }

    #[test]
    fn a_view_count_is_not_an_artist() {
        let mut row = row("a", "First", None);
        row[MRLIR]["flexColumns"][1]["musicResponsiveListItemFlexColumnRenderer"]["text"]["runs"] = json!([
            { "text": "Jamie Paige", "navigationEndpoint": { "browseEndpoint": { "browseId": "UC1", "browseEndpointContextSupportedConfigs": { "browseEndpointContextMusicConfig": { "pageType": "MUSIC_PAGE_TYPE_ARTIST" } } } } },
            { "text": " \u{2022} " },
            { "text": "7.2M views" }
        ]);
        let parsed = parse_history(&response(vec![shelf("Today", json!([row]))]));
        assert_eq!(parsed[0].track.artist, "Jamie Paige");
        assert_eq!(parsed[0].track.artists.len(), 1);
        assert!(is_view_count("7.2M views") && is_view_count("15 views"));
        assert!(!is_view_count("Jamie Paige") && !is_view_count("Views"));
    }

    #[test]
    fn a_shelf_with_no_heading_is_filed_under_recently() {
        let mut bare = shelf("x", json!([row("a", "First", None)]));
        bare["musicShelfRenderer"]["title"] = json!({});
        assert_eq!(parse_history(&response(vec![bare]))[0].played, "Recently");
    }

    #[test]
    fn a_cached_play_survives_the_round_trip() {
        let entry = HistoryEntry {
            track: Track {
                video_id: VideoId("abc".into()),
                title: "A Song".into(),
                artist: "An Artist".into(),
                artists: vec![Person { name: "An Artist".into(), id: Some("UC1".into()) }],
                album: Some(Named { name: "An Album".into(), id: Some("MPREb_1".into()) }),
                thumb: Some("https://art".into()),
                duration_seconds: Some(222),
                like_status: LikeStatus::Like,
                is_explicit: true,
                ..Track::default()
            },
            played: "Today".into(),
            feedback_token: Some("tok".into()),
        };
        let json = serde_json::to_string(&[CachedPlay::from(&entry)]).unwrap();
        // The Python app reads these keys; keep them spelled its way.
        assert!(json.contains("\"videoId\""), "{json}");
        assert!(json.contains("\"likeStatus\":\"LIKE\""), "{json}");
        assert!(json.contains("\"isExplicit\":true"), "{json}");
        assert!(json.contains("\"duration\":\"3:42\""), "{json}");
        assert!(json.contains("\"thumbnails\":[{\"url\":\"https://art\"}]"), "{json}");

        let back: HistoryEntry = serde_json::from_str::<Vec<CachedPlay>>(&json).unwrap().remove(0).into();
        assert_eq!(back.track, entry.track);
        assert_eq!(back.played, "Today");
        assert_eq!(back.feedback_token.as_deref(), Some("tok"));
    }

    #[test]
    fn a_python_written_row_reads_back() {
        // What the Python app leaves in the table, duration only as a clock.
        let json = r#"[{"videoId":"abc","title":"A Song","artists":[{"name":"An Artist","id":"UC1"}],"album":{"name":"An Album","id":null},"duration":"1:02:03","thumbnails":[{"url":"small"},{"url":"large"}],"played":"Today","likeStatus":"INDIFFERENT"}]"#;
        let entry: HistoryEntry = serde_json::from_str::<Vec<CachedPlay>>(json).unwrap().remove(0).into();
        assert_eq!(entry.track.duration_seconds, Some(3723));
        assert_eq!(entry.track.thumb.as_deref(), Some("large"));
        assert_eq!(entry.track.artist, "An Artist");
    }

    #[test]
    fn a_playback_id_is_sixteen_characters_and_changes() {
        let first = cpn();
        assert_eq!(first.len(), 16);
        assert!(first.chars().all(|c| CPN_ALPHABET.contains(&(c as u8))));
        assert_ne!(first, cpn());
    }
}

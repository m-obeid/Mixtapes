//! InnerTube renderer parsing the ytmusicapi crate does not expose: playlist
//! and album rows, upload rows, watch-panel rows, channel content items and
//! the continuation tokens that page them. Ports of ytmusicapi's parsers,
//! kept row for row so the pages see what the Python app saw.

use regex::Regex;
use serde_json::Value;
use std::sync::LazyLock;

use crate::model::{ItemKind, LikeStatus, MediaItem, Named, Person, Track, VideoId};

pub const MRLIR: &str = "musicResponsiveListItemRenderer";
pub const MTRIR: &str = "musicTwoRowItemRenderer";

static DURATION_RE: LazyLock<Regex> = LazyLock::new(|| Regex::new(r"^(\d+:)*\d+:\d+$").unwrap());
static YEAR_RE: LazyLock<Regex> = LazyLock::new(|| Regex::new(r"^\d{4}$").unwrap());
/// The run YouTube puts between subtitle values.
pub const DOT_SEPARATOR: &str = " \u{2022} ";

static VIEWS_PREFIX_RE: LazyLock<Regex> = LazyLock::new(|| Regex::new(r"^\D*?[\s:\x{ff1a}\x{200e}-\x{200f}\x{202a}-\x{202e}]").unwrap());

// -- json helpers ---------------------------------------------------------

pub fn str_at<'a>(node: &'a Value, pointer: &str) -> Option<&'a str> {
    node.pointer(pointer).and_then(Value::as_str)
}

pub fn owned_at(node: &Value, pointer: &str) -> Option<String> {
    str_at(node, pointer).map(str::to_owned)
}

pub fn array_at<'a>(node: &'a Value, pointer: &str) -> &'a [Value] {
    node.pointer(pointer).and_then(Value::as_array).map(Vec::as_slice).unwrap_or(&[])
}

/// Every value stored under `key` anywhere below `root`, iteratively.
pub fn walk_key<'a>(root: &'a Value, key: &str) -> Vec<&'a Value> {
    let mut out = Vec::new();
    let mut stack = vec![root];
    while let Some(node) = stack.pop() {
        match node {
            Value::Object(map) => {
                if let Some(found) = map.get(key) {
                    out.push(found);
                }
                stack.extend(map.values());
            }
            Value::Array(items) => stack.extend(items.iter()),
            _ => {}
        }
    }
    out
}

/// The sections of a library browse, whichever tab holds them.
///
/// An uploads response comes back with two tabs, Library and Uploads, and the
/// rows are in the second one. Reading the first is why the uploaded library
/// looked empty. Taking the first tab that has sections covers both shapes.
pub fn library_sections(response: &Value) -> &[Value] {
    for column in ["singleColumnBrowseResultsRenderer", "twoColumnBrowseResultsRenderer"] {
        for tab in array_at(response, &format!("/contents/{column}/tabs")) {
            let sections = array_at(tab, "/tabRenderer/content/sectionListRenderer/contents");
            if !sections.is_empty() {
                return sections;
            }
        }
    }
    &[]
}

/// Continuation token of the shelf whose rows use one of `row_keys`.
///
/// A playlist response carries more than one continuation: the row list has
/// its own, and the section list wrapping it has another that only pulls in
/// the trailing related shelf. Match on the list that holds the rows.
pub fn continuation_token_for_rows(response: &Value, row_keys: &[&str]) -> Option<String> {
    let mut stack = vec![response];
    while let Some(node) = stack.pop() {
        match node {
            Value::Object(map) => stack.extend(map.values()),
            Value::Array(items) => {
                let mut token = None;
                let mut has_rows = false;
                for entry in items.iter().filter_map(Value::as_object) {
                    if row_keys.iter().any(|k| entry.contains_key(*k)) {
                        has_rows = true;
                    }
                    if let Some(renderer) = entry.get("continuationItemRenderer") {
                        for command in walk_key(renderer, "continuationCommand") {
                            if let Some(t) = command.get("token").and_then(Value::as_str) {
                                token = Some(t.to_owned());
                                break;
                            }
                        }
                    }
                }
                if let (Some(t), true) = (token, has_rows) {
                    return Some(t);
                }
                stack.extend(items.iter());
            }
            _ => {}
        }
    }
    None
}

/// Token of a `continuationItemRenderer` entry.
pub fn item_continuation_token(entry: &Value) -> Option<String> {
    let renderer = entry.get("continuationItemRenderer")?;
    owned_at(renderer, "/continuationEndpoint/continuationCommand/token").or_else(|| walk_key(renderer, "continuationCommand").into_iter().find_map(|c| owned_at(c, "/token")))
}

/// Legacy `continuations[0].nextContinuationData.continuation` token.
pub fn next_continuation(node: &Value) -> Option<String> {
    owned_at(node, "/continuations/0/nextContinuationData/continuation")
}

pub fn last_thumbnail_url(thumbs: &[Value]) -> Option<String> {
    thumbs.last().and_then(|t| owned_at(t, "/url"))
}

pub fn thumbnails_at(node: &Value, pointer: &str) -> Vec<String> {
    array_at(node, pointer).iter().filter_map(|t| owned_at(t, "/url")).collect()
}

pub fn runs_text(runs: &[Value]) -> String {
    runs.iter().filter_map(|r| r.get("text").and_then(Value::as_str)).collect()
}

/// "3:07" or "1:02:03" to seconds, like ytmusicapi's parse_duration.
pub fn parse_duration(text: &str) -> Option<u32> {
    let text = text.trim();
    if text.is_empty() {
        return None;
    }
    let mut total: u32 = 0;
    for part in text.split(':') {
        if part.is_empty() || !part.chars().all(|c| c.is_ascii_digit()) {
            return None;
        }
        total = total.checked_mul(60)?.checked_add(part.parse().ok()?)?;
    }
    Some(total)
}

/// ytmusicapi's parse_like_status: the endpoint carries the action, the state is its opposite.
pub fn parse_like_status(service: &Value) -> LikeStatus {
    match str_at(service, "/likeEndpoint/status") {
        Some("LIKE") => LikeStatus::Indifferent,
        Some("INDIFFERENT") => LikeStatus::Like,
        _ => LikeStatus::Indifferent,
    }
}

// -- song runs ------------------------------------------------------------

#[derive(Default, Debug, Clone)]
pub struct SongRuns {
    pub artists: Vec<Person>,
    pub album: Option<Named>,
    pub year: Option<String>,
    pub views: Option<String>,
    pub duration: Option<String>,
}

enum RunKind {
    Album(Named),
    Artist(Person),
    Duration(String),
    Year(String),
    Views(String),
}

fn parse_views(text: &str) -> Option<String> {
    let has_latin = text.chars().any(|c| c.is_ascii_alphabetic());
    let (stripped, prefixed) = if has_latin { (text.to_owned(), false) } else { let s = VIEWS_PREFIX_RE.replace(text, "").into_owned(); let p = s != text; (s, p) };
    if !stripped.chars().next().is_some_and(|c| c.is_ascii_digit()) {
        return None;
    }
    if !prefixed && stripped.is_ascii() && !stripped.contains(' ') {
        return None;
    }
    // ytmusicapi keeps the count alone: "1.2M views" is shown as "1.2M".
    Some(stripped.split(' ').next().unwrap_or_default().to_owned())
}

fn parse_song_run(run: &Value) -> RunKind {
    let text = run.get("text").and_then(Value::as_str).unwrap_or_default().to_owned();
    if run.get("navigationEndpoint").is_some() {
        let id = owned_at(run, "/navigationEndpoint/browseEndpoint/browseId");
        if id.as_deref().is_some_and(|i| i.starts_with("MPRE") || i.contains("release_detail")) {
            return RunKind::Album(Named { name: text, id });
        }
        return RunKind::Artist(Person { name: text, id });
    }
    if DURATION_RE.is_match(&text) {
        RunKind::Duration(text)
    } else if YEAR_RE.is_match(&text) {
        RunKind::Year(text)
    } else if let Some(views) = parse_views(&text) {
        RunKind::Views(views)
    } else {
        RunKind::Artist(Person { name: text, id: None })
    }
}

/// Every even run is a value, odd runs are separators.
pub fn parse_song_runs(runs: &[Value]) -> SongRuns {
    let mut out = SongRuns::default();
    for run in runs.iter().step_by(2) {
        match parse_song_run(run) {
            RunKind::Album(a) => out.album = Some(a),
            RunKind::Artist(p) => out.artists.push(p),
            RunKind::Duration(d) => out.duration = Some(d),
            RunKind::Year(y) => out.year = Some(y),
            RunKind::Views(v) => out.views = Some(v),
        }
    }
    out
}

/// ytmusicapi's parse_song_runs with skip_type_spec: a carousel row leads its
/// subtitle with the kind word ("Song \u{2022} Eminem"), which is not an artist.
pub fn parse_song_runs_after_type(runs: &[Value]) -> SongRuns {
    let is_artist = |run: &Value| matches!(parse_song_run(run), RunKind::Artist(_));
    let leads_with_kind = runs.len() > 2 && is_artist(&runs[0]) && str_at(&runs[1], "/text") == Some(DOT_SEPARATOR) && is_artist(&runs[2]);
    parse_song_runs(if leads_with_kind { &runs[2..] } else { runs })
}

/// ytmusicapi's parse_artists_runs: every even run is an artist.
pub fn parse_artists_runs(runs: &[Value]) -> Vec<Person> {
    runs.iter()
        .step_by(2)
        .map(|r| Person { name: r.get("text").and_then(Value::as_str).unwrap_or_default().to_owned(), id: owned_at(r, "/navigationEndpoint/browseEndpoint/browseId") })
        .collect()
}

fn flex_item(data: &Value, index: usize) -> Option<&Value> {
    let col = data.get("flexColumns")?.get(index)?.get("musicResponsiveListItemFlexColumnRenderer")?;
    if col.pointer("/text/runs").is_none() { None } else { Some(col) }
}

fn item_text(data: &Value, index: usize) -> Option<String> {
    flex_item(data, index).and_then(|c| owned_at(c, "/text/runs/0/text"))
}

fn song_artists(data: &Value, index: usize) -> Vec<Person> {
    flex_item(data, index).map(|c| parse_artists_runs(array_at(c, "/text/runs"))).unwrap_or_default()
}

fn song_album(data: &Value, index: usize) -> Option<Named> {
    let col = flex_item(data, index)?;
    Some(Named { name: owned_at(col, "/text/runs/0/text").unwrap_or_default(), id: owned_at(col, "/text/runs/0/navigationEndpoint/browseEndpoint/browseId") })
}

fn fixed_duration(data: &Value) -> Option<String> {
    let col = data.pointer("/fixedColumns/0/musicResponsiveListItemFixedColumnRenderer")?;
    owned_at(col, "/text/simpleText").or_else(|| owned_at(col, "/text/runs/0/text"))
}

fn joined_names(artists: &[Person]) -> String {
    artists.iter().map(|a| a.name.as_str()).collect::<Vec<_>>().join(", ")
}

const VIDEO_TYPE_PATH: &str = "/menu/menuRenderer/items/0/menuNavigationItemRenderer/navigationEndpoint/watchEndpoint/watchEndpointMusicSupportedConfigs/watchEndpointMusicConfig/musicVideoType";
const BADGE_LABEL: &str = "/badges/0/musicInlineBadgeRenderer/accessibilityData/accessibilityData/label";

// -- playlist and album rows ----------------------------------------------

/// Port of ytmusicapi's parse_playlist_item for a `musicResponsiveListItemRenderer`.
pub fn parse_playlist_item(data: &Value, is_album: bool, is_collaborative: bool) -> Option<Track> {
    let mut video_id: Option<String> = None;
    let mut set_video_id: Option<String> = None;
    let mut like = LikeStatus::Indifferent;

    for item in array_at(data, "/menu/menuRenderer/items") {
        if let Some(service) = item.pointer("/menuServiceItemRenderer/serviceEndpoint") {
            if let Some(edit) = service.get("playlistEditEndpoint") {
                set_video_id = owned_at(edit, "/actions/0/setVideoId");
                video_id = owned_at(edit, "/actions/0/removedVideoId");
            }
        }
    }
    if let Some(play) = data.pointer("/overlay/musicItemThumbnailOverlayRenderer/content/musicPlayButtonRenderer") {
        if let Some(v) = owned_at(play, "/playNavigationEndpoint/watchEndpoint/videoId") {
            video_id = Some(v);
            if data.get("menu").is_some() {
                like = LikeStatus::parse(str_at(data, "/menu/menuRenderer/topLevelButtons/0/likeButtonRenderer/likeStatus").unwrap_or("INDIFFERENT"));
            }
        }
    }

    let is_available = data.get("musicItemRendererDisplayPolicy").and_then(Value::as_str) != Some("MUSIC_ITEM_RENDERER_DISPLAY_POLICY_GREY_OUT");
    // Unavailable rows and album rows have preset columns: their meaning
    // cannot be found reliably from navigation endpoints.
    let preset = !is_available || is_album;
    let mut title_index = preset.then_some(0);
    let mut artist_index = preset.then_some(1);
    let mut duration_index: Option<usize> = None;
    let mut album_index = if is_collaborative { Some(3) } else if preset { Some(2) } else { None };
    let mut user_channel_indexes: Vec<usize> = Vec::new();
    let mut unrecognized_index: Option<usize> = None;

    for index in 0..array_at(data, "/flexColumns").len() {
        let Some(col) = flex_item(data, index) else { continue };
        let run = col.pointer("/text/runs/0");
        match run.and_then(|r| r.get("navigationEndpoint")) {
            None => {
                if let Some(text) = run.and_then(|r| r.get("text")).and_then(Value::as_str) {
                    if DURATION_RE.is_match(text) {
                        duration_index = Some(index);
                    } else if unrecognized_index.is_none() {
                        unrecognized_index = Some(index);
                    }
                }
            }
            Some(nav) => {
                if nav.get("watchEndpoint").is_some() {
                    title_index = Some(index);
                } else if let Some(page_type) = str_at(nav, "/browseEndpoint/browseEndpointContextSupportedConfigs/browseEndpointContextMusicConfig/pageType") {
                    match page_type {
                        "MUSIC_PAGE_TYPE_ARTIST" | "MUSIC_PAGE_TYPE_UNKNOWN" => artist_index = Some(index),
                        "MUSIC_PAGE_TYPE_ALBUM" | "MUSIC_PAGE_TYPE_AUDIOBOOK" => album_index = Some(index),
                        "MUSIC_PAGE_TYPE_USER_CHANNEL" => user_channel_indexes.push(index),
                        "MUSIC_PAGE_TYPE_NON_MUSIC_AUDIO_TRACK_PAGE" => title_index = Some(index),
                        _ => {}
                    }
                }
            }
        }
    }
    if artist_index.is_none() {
        artist_index = unrecognized_index.or(user_channel_indexes.last().copied());
    }

    let title = title_index.and_then(|i| item_text(data, i));
    if title.as_deref() == Some("Song deleted") {
        return None;
    }
    let artists = artist_index.map(|i| song_artists(data, i)).unwrap_or_default();
    let album = album_index.and_then(|i| song_album(data, i));
    let duration = duration_index.filter(|i| *i > 0).and_then(|i| item_text(data, i)).or_else(|| fixed_duration(data));
    let thumb = last_thumbnail_url(array_at(data, "/thumbnail/musicThumbnailRenderer/thumbnail/thumbnails"));
    let track_number = if is_album && is_available { str_at(data, "/index/runs/0/text").and_then(|t| t.trim().parse().ok()) } else { None };

    Some(Track {
        video_id: VideoId(video_id.unwrap_or_default()),
        title: title.unwrap_or_default(),
        artist: joined_names(&artists),
        artists,
        album,
        thumb,
        duration_seconds: duration.as_deref().and_then(parse_duration),
        like_status: like,
        video_type: owned_at(data, VIDEO_TYPE_PATH),
        entity_id: None,
        is_explicit: data.pointer(BADGE_LABEL).is_some(),
        set_video_id,
        is_available,
        track_number,
    })
}

pub fn parse_playlist_items<'a>(contents: impl IntoIterator<Item = &'a Value>, is_album: bool, is_collaborative: bool) -> Vec<Track> {
    contents.into_iter().filter_map(|r| r.get(MRLIR)).filter_map(|d| parse_playlist_item(d, is_album, is_collaborative)).collect()
}

/// The token that removes one play from the history, out of the row's menu.
///
/// ytmusicapi reads it in parse_song_menu_data, which the port skips: this is
/// the only entry off that menu any page here uses.
pub fn history_feedback_token(data: &Value) -> Option<String> {
    array_at(data, "/menu/menuRenderer/items").iter().find_map(|item| {
        let menu_item = item.get("menuServiceItemRenderer")?;
        let icon = str_at(menu_item, "/icon/iconType").or_else(|| str_at(menu_item, "/defaultIcon/iconType"))?;
        if icon != "REMOVE_FROM_HISTORY" {
            return None;
        }
        owned_at(menu_item, "/serviceEndpoint/feedbackEndpoint/feedbackToken")
    })
}

/// Port of parse_uploaded_items: upload rows carry the entity id used to delete them.
pub fn parse_uploaded_item(data: &Value) -> Option<Track> {
    data.get("menu")?;
    let items = array_at(data, "/menu/menuRenderer/items");
    let entity_id = items.last().and_then(|i| owned_at(i, "/menuNavigationItemRenderer/navigationEndpoint/confirmDialogEndpoint/content/confirmDialogRenderer/confirmButton/buttonRenderer/command/musicDeletePrivatelyOwnedEntityCommand/entityId"));
    let video_id = items.first().and_then(|i| owned_at(i, "/menuServiceItemRenderer/serviceEndpoint/queueAddEndpoint/queueTarget/videoId"))?;
    let artists = song_artists(data, 1);
    let duration = fixed_duration(data);
    Some(Track {
        video_id: VideoId(video_id),
        title: item_text(data, 0).unwrap_or_default(),
        artist: joined_names(&artists),
        artists,
        album: song_album(data, 2),
        thumb: last_thumbnail_url(array_at(data, "/thumbnail/musicThumbnailRenderer/thumbnail/thumbnails")),
        duration_seconds: duration.as_deref().and_then(parse_duration),
        like_status: LikeStatus::parse(str_at(data, "/menu/menuRenderer/topLevelButtons/0/likeButtonRenderer/likeStatus").unwrap_or("INDIFFERENT")),
        entity_id,
        ..Track::default()
    })
}

pub fn parse_uploaded_items<'a>(contents: impl IntoIterator<Item = &'a Value>) -> Vec<Track> {
    contents.into_iter().filter_map(|r| r.get(MRLIR)).filter_map(parse_uploaded_item).collect()
}

// -- watch panel rows -----------------------------------------------------

#[derive(Debug, Clone)]
pub struct WatchTrack {
    pub track: Track,
    /// The song or video twin YouTube Music pairs with this row.
    pub counterpart: Option<Track>,
}

/// Port of parse_watch_track for a `playlistPanelVideoRenderer`.
pub fn parse_watch_track(data: &Value) -> Option<Track> {
    let video_id = owned_at(data, "/videoId")?;
    let mut like = LikeStatus::Indifferent;
    for item in array_at(data, "/menu/menuRenderer/items") {
        if let Some(service) = item.pointer("/toggleMenuServiceItemRenderer/defaultServiceEndpoint") {
            if service.get("likeEndpoint").is_some() {
                like = parse_like_status(service);
            }
        }
    }
    let runs = parse_song_runs(array_at(data, "/longBylineText/runs"));
    Some(Track {
        video_id: VideoId(video_id),
        title: owned_at(data, "/title/runs/0/text").unwrap_or_default(),
        artist: joined_names(&runs.artists),
        artists: runs.artists,
        album: runs.album,
        thumb: last_thumbnail_url(array_at(data, "/thumbnail/thumbnails")),
        duration_seconds: str_at(data, "/lengthText/runs/0/text").and_then(parse_duration),
        like_status: like,
        video_type: owned_at(data, "/navigationEndpoint/watchEndpoint/watchEndpointMusicSupportedConfigs/watchEndpointMusicConfig/musicVideoType"),
        ..Track::default()
    })
}

pub fn parse_watch_playlist<'a>(results: impl IntoIterator<Item = &'a Value>) -> Vec<WatchTrack> {
    let mut out = Vec::new();
    for result in results {
        let (data, counterpart) = match result.get("playlistPanelVideoWrapperRenderer") {
            Some(wrapper) => (wrapper.pointer("/primaryRenderer/playlistPanelVideoRenderer"), wrapper.pointer("/counterpart/0/counterpartRenderer/playlistPanelVideoRenderer")),
            None => (result.get("playlistPanelVideoRenderer"), None),
        };
        let Some(data) = data else { continue };
        if data.get("unplayableText").is_some() {
            continue;
        }
        if let Some(track) = parse_watch_track(data) {
            out.push(WatchTrack { track, counterpart: counterpart.and_then(parse_watch_track) });
        }
    }
    out
}

// -- browse cards ---------------------------------------------------------
//
// The carousels on Home, Explore and a category page are made of two-row
// cards and list rows. These are ytmusicapi's parsers for them, answering with
// a `MediaItem` so the pages never re-derive what a card is.

/// A two-row card keeps its picture here, a list row under `THUMBNAILS`.
pub const THUMBNAIL_RENDERER: &str = "/thumbnailRenderer/musicThumbnailRenderer/thumbnail/thumbnails";
pub const THUMBNAILS: &str = "/thumbnail/musicThumbnailRenderer/thumbnail/thumbnails";
const SUBTITLE_BADGE_LABEL: &str = "/subtitleBadges/0/musicInlineBadgeRenderer/accessibilityData/accessibilityData/label";
const PLAY_ENDPOINT: &str = "/overlay/musicItemThumbnailOverlayRenderer/content/musicPlayButtonRenderer/playNavigationEndpoint";
const MUSIC_VIDEO_TYPE: &str = "/watchEndpointMusicSupportedConfigs/watchEndpointMusicConfig/musicVideoType";
/// The page a card's title opens, which is what says what the card is.
const CARD_PAGE_TYPE: &str = "/title/runs/0/navigationEndpoint/browseEndpoint/browseEndpointContextSupportedConfigs/browseEndpointContextMusicConfig/pageType";

pub fn is_year(text: &str) -> bool {
    text.len() == 4 && text.chars().all(|c| c.is_ascii_digit())
}

/// Port of ytmusicapi's parse_mixed_content item dispatch: one card of a home
/// or browse carousel, whatever kind it turns out to be.
///
/// `section_title` comes along because a card with no video type is told apart
/// by the shelf it sits in, the way home.py's _detect_kind reads it. Podcast
/// shows and episodes answer None: no page here draws them.
pub fn parse_mixed_item(entry: &Value, section_title: &str) -> Option<MediaItem> {
    if let Some(data) = entry.get(MTRIR) {
        return match str_at(data, CARD_PAGE_TYPE) {
            // No page type means it plays: a song, or a card that opens a mix.
            None => match owned_at(data, "/navigationEndpoint/watchPlaylistEndpoint/playlistId") {
                Some(playlist_id) => Some(MediaItem {
                    kind: ItemKind::Playlist,
                    id: playlist_id,
                    title: owned_at(data, "/title/runs/0/text").unwrap_or_default(),
                    thumb: last_thumbnail_url(array_at(data, THUMBNAIL_RENDERER)),
                    ..MediaItem::default()
                }),
                None => parse_song_card(data, section_title),
            },
            Some("MUSIC_PAGE_TYPE_ALBUM" | "MUSIC_PAGE_TYPE_AUDIOBOOK") => Some(parse_album_card(data)),
            Some("MUSIC_PAGE_TYPE_ARTIST" | "MUSIC_PAGE_TYPE_USER_CHANNEL") => parse_artist_card(data),
            Some("MUSIC_PAGE_TYPE_PLAYLIST") => parse_playlist_card(data),
            Some(_) => None,
        };
    }
    entry.get(MRLIR).and_then(|data| parse_song_row(data, section_title))
}

/// Port of ytmusicapi's parse_album for a `musicTwoRowItemRenderer`.
pub fn parse_album_card(data: &Value) -> MediaItem {
    let artists = array_at(data, "/subtitle/runs")
        .iter()
        .filter(|run| run.get("navigationEndpoint").is_some())
        .map(|run| Person { name: str_at(run, "/text").unwrap_or_default().to_owned(), id: owned_at(run, "/navigationEndpoint/browseEndpoint/browseId") })
        .collect();
    let mut item = MediaItem {
        kind: ItemKind::Album,
        id: owned_at(data, "/title/runs/0/navigationEndpoint/browseEndpoint/browseId").unwrap_or_default(),
        title: owned_at(data, "/title/runs/0/text").unwrap_or_default(),
        artists,
        thumb: last_thumbnail_url(array_at(data, THUMBNAIL_RENDERER)),
        explicit: data.pointer(SUBTITLE_BADGE_LABEL).is_some(),
        playlist_id: owned_at(data, "/thumbnailOverlay/musicItemThumbnailOverlayRenderer/content/musicPlayButtonRenderer/playNavigationEndpoint/watchPlaylistEndpoint/playlistId"),
        ..MediaItem::default()
    };
    // The subtitle leads with either the year on its own or the release type
    // followed by it.
    match str_at(data, "/subtitle/runs/0/text") {
        Some(text) if is_year(text) => item.year = Some(text.to_owned()),
        Some(text) => {
            item.item_type = Some(text.to_owned());
            item.year = str_at(data, "/subtitle/runs/2/text").filter(|y| is_year(y)).map(str::to_owned);
        }
        None => {}
    }
    item
}

/// Port of ytmusicapi's parse_video: artists up to the dot, views after it.
pub fn parse_video_card(data: &Value) -> Option<MediaItem> {
    let runs = array_at(data, "/subtitle/runs");
    let dot = runs.iter().position(|run| str_at(run, "/text") == Some(DOT_SEPARATOR)).unwrap_or(runs.len());
    let video_id = owned_at(data, "/navigationEndpoint/watchEndpoint/videoId")
        .or_else(|| array_at(data, "/menu/menuRenderer/items").iter().find_map(|entry| owned_at(entry, "/menuServiceItemRenderer/serviceEndpoint/queueAddEndpoint/queueTarget/videoId")))?;
    Some(MediaItem {
        kind: ItemKind::Video,
        id: video_id,
        title: owned_at(data, "/title/runs/0/text").unwrap_or_default(),
        artists: parse_artists_runs(&runs[..dot]),
        thumb: last_thumbnail_url(array_at(data, THUMBNAIL_RENDERER)),
        views: runs.last().and_then(|run| str_at(run, "/text")).map(|text| text.split(' ').next().unwrap_or_default().to_owned()),
        ..MediaItem::default()
    })
}

/// Port of ytmusicapi's parse_song: a two-row card that plays.
pub fn parse_song_card(data: &Value, section_title: &str) -> Option<MediaItem> {
    let video_id = owned_at(data, "/navigationEndpoint/watchEndpoint/videoId")?;
    let runs = parse_song_runs_after_type(array_at(data, "/subtitle/runs"));
    let item = MediaItem {
        kind: ItemKind::Song,
        id: video_id,
        title: owned_at(data, "/title/runs/0/text").unwrap_or_default(),
        artists: runs.artists,
        album: runs.album,
        thumb: last_thumbnail_url(array_at(data, THUMBNAIL_RENDERER)),
        year: runs.year,
        views: runs.views,
        duration_seconds: runs.duration.as_deref().and_then(parse_duration),
        explicit: data.pointer(SUBTITLE_BADGE_LABEL).is_some(),
        ..MediaItem::default()
    };
    let video_type = str_at(data, &format!("/navigationEndpoint/watchEndpoint{MUSIC_VIDEO_TYPE}"));
    with_playable_kind(item, video_type, section_title)
}

/// Port of ytmusicapi's parse_playlist: the card of a playlist, with the
/// "Author • N songs" subtitle split back up.
pub fn parse_playlist_card(data: &Value) -> Option<MediaItem> {
    let browse_id = str_at(data, "/title/runs/0/navigationEndpoint/browseEndpoint/browseId")?;
    let runs = array_at(data, "/subtitle/runs");
    let mut item = MediaItem {
        kind: ItemKind::Playlist,
        id: browse_id.trim_start_matches("VL").to_owned(),
        title: owned_at(data, "/title/runs/0/text").unwrap_or_default(),
        thumb: last_thumbnail_url(array_at(data, THUMBNAIL_RENDERER)),
        ..MediaItem::default()
    };
    if !runs.is_empty() {
        item.description = Some(runs_text(runs));
        // Three runs are "Author • N songs"; anything else is a description.
        let third = str_at(data, "/subtitle/runs/2/text").unwrap_or_default();
        if runs.len() == 3 && third.split(' ').next().is_some_and(|n| !n.is_empty() && n.chars().all(|c| c.is_ascii_digit())) {
            item.count = third.split(' ').next().map(str::to_owned);
            item.artists = parse_artists_runs(&runs[..1]);
        }
    }
    Some(item)
}

/// Port of ytmusicapi's parse_related_artist.
pub fn parse_artist_card(data: &Value) -> Option<MediaItem> {
    let browse_id = owned_at(data, "/title/runs/0/navigationEndpoint/browseEndpoint/browseId")?;
    Some(MediaItem {
        kind: ItemKind::Artist,
        id: browse_id,
        title: owned_at(data, "/title/runs/0/text").unwrap_or_default(),
        // "12M subscribers" is shown as the count alone.
        subscribers: str_at(data, "/subtitle/runs/0/text").map(|text| text.split(' ').next().unwrap_or_default().to_owned()),
        thumb: last_thumbnail_url(array_at(data, THUMBNAIL_RENDERER)),
        ..MediaItem::default()
    })
}

/// Port of parse_song_flat: the `musicResponsiveListItemRenderer` a carousel
/// uses instead of a card.
pub fn parse_song_row(data: &Value, section_title: &str) -> Option<MediaItem> {
    let title = owned_at(data, "/flexColumns/0/musicResponsiveListItemFlexColumnRenderer/text/runs/0/text")?;
    let video_id = owned_at(data, "/flexColumns/0/musicResponsiveListItemFlexColumnRenderer/text/runs/0/navigationEndpoint/watchEndpoint/videoId")
        .or_else(|| owned_at(data, &format!("{PLAY_ENDPOINT}/watchEndpoint/videoId")))?;
    let runs = parse_song_runs_after_type(array_at(data, "/flexColumns/1/musicResponsiveListItemFlexColumnRenderer/text/runs"));
    let album_column = "/flexColumns/2/musicResponsiveListItemFlexColumnRenderer/text/runs/0";
    let album = match data.pointer(&format!("{album_column}/navigationEndpoint")) {
        Some(_) => Some(Named { name: owned_at(data, &format!("{album_column}/text")).unwrap_or_default(), id: owned_at(data, &format!("{album_column}/navigationEndpoint/browseEndpoint/browseId")) }),
        None => runs.album,
    };
    let item = MediaItem {
        kind: ItemKind::Song,
        id: video_id,
        title,
        artists: runs.artists,
        album,
        thumb: last_thumbnail_url(array_at(data, THUMBNAILS)),
        views: runs.views,
        duration_seconds: runs.duration.as_deref().and_then(parse_duration),
        explicit: data.pointer(BADGE_LABEL).is_some(),
        ..MediaItem::default()
    };
    let video_type = str_at(data, &format!("{PLAY_ENDPOINT}/watchEndpoint{MUSIC_VIDEO_TYPE}"));
    with_playable_kind(item, video_type, section_title)
}

/// Port of home.py _detect_kind for something that plays: the video type
/// decides, then the shelf it sits in, then the thumbnail address, then the
/// shape of what was parsed. None means a podcast episode, which no page draws.
pub fn with_playable_kind(mut item: MediaItem, video_type: Option<&str>, section_title: &str) -> Option<MediaItem> {
    item.kind = detect_kind(video_type, item.thumb.as_deref(), section_title, &item)?;
    Some(item)
}

/// Shelves whose name says what their untyped rows are.
const SONG_SECTION_KEYS: [&str; 13] = ["song", "track", "favorite", "listen again", "quick pick", "forgotten", "rediscover", "hidden gem", "recap", "your library", "from your library", "mix", "hits"];
const VIDEO_SECTION_KEYS: [&str; 6] = ["music video", "remix", "live performance", "performances", "video for you", "videos for you"];

pub fn detect_kind(video_type: Option<&str>, thumb: Option<&str>, section_title: &str, item: &MediaItem) -> Option<ItemKind> {
    if let Some(video_type) = video_type {
        if video_type == "MUSIC_VIDEO_TYPE_ATV" {
            return Some(ItemKind::Song);
        }
        if video_type.contains("PODCAST") || video_type.contains("EPISODE") {
            return None;
        }
        return Some(ItemKind::Video);
    }
    let low = section_title.to_lowercase();
    if VIDEO_SECTION_KEYS.iter().any(|k| low.contains(k)) {
        return Some(ItemKind::Video);
    }
    if SONG_SECTION_KEYS.iter().any(|k| low.contains(k)) {
        return Some(ItemKind::Song);
    }
    if thumb.is_some_and(|url| url.contains("/vi/") || url.contains("/vi_webp/")) {
        return Some(ItemKind::Video);
    }
    // A view count with none of a song's own metadata reads as a video.
    if item.views.is_some() && item.album.is_none() && item.duration_seconds.is_none() && item.year.is_none() {
        return Some(ItemKind::Video);
    }
    Some(ItemKind::Song)
}

// -- channel content items (raw fallback parsing) -------------------------

/// Port of MusicClient._parse_channel_item: a best-effort read of a two-row
/// or responsive item into a browse item.
pub fn parse_channel_item(raw: &Value) -> Option<MediaItem> {
    for key in [MTRIR, MRLIR] {
        let Some(renderer) = raw.get(key) else { continue };
        let mut item = MediaItem::default();
        let mut title_runs: &[Value] = array_at(renderer, "/title/runs");
        if title_runs.is_empty() {
            for col in array_at(renderer, "/flexColumns") {
                let runs = array_at(col, "/musicResponsiveListItemFlexColumnRenderer/text/runs");
                if !runs.is_empty() {
                    title_runs = runs;
                    break;
                }
            }
        }
        let mut browse_id = None;
        let mut video_id = None;
        if let Some(first) = title_runs.first() {
            item.title = first.get("text").and_then(Value::as_str).unwrap_or_default().to_owned();
            browse_id = owned_at(first, "/navigationEndpoint/browseEndpoint/browseId");
            video_id = owned_at(first, "/navigationEndpoint/watchEndpoint/videoId");
        }
        for col in array_at(renderer, "/flexColumns").iter().skip(1) {
            let runs = array_at(col, "/musicResponsiveListItemFlexColumnRenderer/text/runs");
            let mut artists = Vec::new();
            for r in runs {
                let text = r.get("text").and_then(Value::as_str).unwrap_or_default();
                if let Some(id) = owned_at(r, "/navigationEndpoint/browseEndpoint/browseId") {
                    artists.push(Person { name: text.to_owned(), id: Some(id) });
                } else if !text.trim().is_empty() && !["•", "&", ","].contains(&text.trim()) {
                    artists.push(Person { name: text.trim().to_owned(), id: None });
                }
            }
            if !artists.is_empty() {
                item.artists = artists;
                break;
            }
        }
        if let Some(text) = renderer.pointer("/fixedColumns/0/musicResponsiveListItemFixedColumnRenderer/text/runs/0/text").and_then(Value::as_str) {
            item.duration_seconds = parse_duration(text);
        }
        let thumbs = array_at(renderer, "/thumbnailRenderer/musicThumbnailRenderer/thumbnail/thumbnails");
        let thumbs = if thumbs.is_empty() { array_at(renderer, "/thumbnail/musicThumbnailRenderer/thumbnail/thumbnails") } else { thumbs };
        item.thumb = last_thumbnail_url(thumbs);
        if video_id.is_none() {
            video_id = owned_at(renderer, "/overlay/musicItemThumbnailOverlayRenderer/content/musicPlayButtonRenderer/playNavigationEndpoint/watchEndpoint/videoId");
        }
        let subtitle_runs = array_at(renderer, "/subtitle/runs");
        if !subtitle_runs.is_empty() {
            item.description = Some(runs_text(subtitle_runs));
            if item.artists.is_empty() {
                item.artists = subtitle_runs.iter().filter_map(|r| Some(Person { name: r.get("text")?.as_str()?.to_owned(), id: Some(owned_at(r, "/navigationEndpoint/browseEndpoint/browseId")?) })).collect();
            }
            let parsed = parse_song_runs(subtitle_runs);
            item.year = parsed.year;
            if let Some(first) = subtitle_runs.first() {
                let text = first.get("text").and_then(Value::as_str).unwrap_or_default();
                if first.get("navigationEndpoint").is_none() && ["Album", "Single", "EP"].contains(&text) {
                    item.item_type = Some(text.to_owned());
                }
            }
        }
        if item.title.is_empty() {
            continue;
        }
        if let Some(v) = video_id {
            item.kind = ItemKind::Song;
            item.id = v;
        } else if let Some(b) = browse_id {
            item.kind = if b.starts_with("MPRE") || b.contains("release_detail") {
                ItemKind::Album
            } else if b.starts_with("UC") || b.contains("artist_detail") {
                ItemKind::Artist
            } else {
                ItemKind::Playlist
            };
            item.id = b.strip_prefix("VL").filter(|_| item.kind == ItemKind::Playlist).map(str::to_owned).unwrap_or(b);
        } else {
            continue;
        }
        return Some(item);
    }
    None
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn duration_parses_minutes_and_hours() {
        assert_eq!(parse_duration("3:07"), Some(187));
        assert_eq!(parse_duration("1:02:03"), Some(3723));
        assert_eq!(parse_duration("2,343"), None);
        assert_eq!(parse_duration(" "), None);
    }

    #[test]
    fn like_status_is_the_opposite_of_the_endpoint_action() {
        assert_eq!(parse_like_status(&json!({"likeEndpoint": {"status": "LIKE"}})), LikeStatus::Indifferent);
        assert_eq!(parse_like_status(&json!({"likeEndpoint": {"status": "INDIFFERENT"}})), LikeStatus::Like);
    }

    #[test]
    fn song_runs_split_artists_album_and_year() {
        let runs = json!([
            {"text": "Eminem", "navigationEndpoint": {"browseEndpoint": {"browseId": "UCx"}}},
            {"text": " • "},
            {"text": "Revival", "navigationEndpoint": {"browseEndpoint": {"browseId": "MPREb_1"}}},
            {"text": " • "},
            {"text": "2017"},
            {"text": " • "},
            {"text": "1.2M views"}
        ]);
        let parsed = parse_song_runs(runs.as_array().unwrap());
        assert_eq!(parsed.artists.len(), 1);
        assert_eq!(parsed.album.as_ref().map(|a| a.name.as_str()), Some("Revival"));
        assert_eq!(parsed.year.as_deref(), Some("2017"));
        assert_eq!(parsed.views.as_deref(), Some("1.2M"));
    }

    #[test]
    fn playlist_item_reads_ids_and_columns() {
        let data = json!({
            "flexColumns": [
                {"musicResponsiveListItemFlexColumnRenderer": {"text": {"runs": [{"text": "Lost", "navigationEndpoint": {"watchEndpoint": {"videoId": "abc"}}}]}}},
                {"musicResponsiveListItemFlexColumnRenderer": {"text": {"runs": [{"text": "Guest Who", "navigationEndpoint": {"browseEndpoint": {"browseId": "UC1", "browseEndpointContextSupportedConfigs": {"browseEndpointContextMusicConfig": {"pageType": "MUSIC_PAGE_TYPE_ARTIST"}}}}}]}}},
                {"musicResponsiveListItemFlexColumnRenderer": {"text": {"runs": [{"text": "Lost", "navigationEndpoint": {"browseEndpoint": {"browseId": "MPREb_x", "browseEndpointContextSupportedConfigs": {"browseEndpointContextMusicConfig": {"pageType": "MUSIC_PAGE_TYPE_ALBUM"}}}}}]}}}
            ],
            "fixedColumns": [{"musicResponsiveListItemFixedColumnRenderer": {"text": {"runs": [{"text": "2:58"}]}}}],
            "overlay": {"musicItemThumbnailOverlayRenderer": {"content": {"musicPlayButtonRenderer": {"playNavigationEndpoint": {"watchEndpoint": {"videoId": "abc"}}}}}},
            "menu": {"menuRenderer": {"items": [{"menuServiceItemRenderer": {"serviceEndpoint": {"playlistEditEndpoint": {"actions": [{"setVideoId": "SET1", "removedVideoId": "abc"}]}}}}], "topLevelButtons": [{"likeButtonRenderer": {"likeStatus": "LIKE"}}]}}
        });
        let track = parse_playlist_item(&data, false, false).unwrap();
        assert_eq!(track.video_id.as_str(), "abc");
        assert_eq!(track.title, "Lost");
        assert_eq!(track.artist, "Guest Who");
        assert_eq!(track.album.as_ref().and_then(|a| a.id.as_deref()), Some("MPREb_x"));
        assert_eq!(track.duration_seconds, Some(178));
        assert_eq!(track.set_video_id.as_deref(), Some("SET1"));
        assert_eq!(track.like_status, LikeStatus::Like);
        assert!(track.is_available);
    }
}

//! Playlist, album, upload-album and watch-panel endpoints on top of the
//! crate's transport. The crate parses playlists too, but drops the like
//! status the rows show, so the page parses the response itself. Everything
//! here is a port of the MusicClient method of the same name.

use std::sync::{Arc, LazyLock};

use regex::Regex;
use serde_json::{Value, json};
use ytmusicapi::YTMusicClient;

use crate::model::{HttpAuth, ItemKind, LikeStatus, MediaItem, Named, Person, Track};
use crate::net::cache::{LibraryIds, SortMetric};
use crate::net::items::*;
use crate::net::library::{self, parse_two_row};
use crate::net::search::{SearchFilter, search};
use crate::net::ytmusic::NetError;

const HEADER_SECTION: &str = "/contents/twoColumnBrowseResultsRenderer/tabs/0/tabRenderer/content/sectionListRenderer/contents/0";
const SECONDARY_SECTIONS: &str = "/contents/twoColumnBrowseResultsRenderer/secondaryContents/sectionListRenderer/contents";
const SINGLE_SECTIONS: &str = "/contents/singleColumnBrowseResultsRenderer/tabs/0/tabRenderer/content/sectionListRenderer/contents";
/// 100 rows per page, like MusicClient._MAX_METRIC_PAGES.
const MAX_METRIC_PAGES: usize = 60;
const MAX_CONTINUATION_PAGES: usize = 100;
const YT_WEB_BROWSE_URL: &str = "https://www.youtube.com/youtubei/v1/browse?prettyPrint=false";

static VIEWS_RE: LazyLock<Regex> = LazyLock::new(|| Regex::new(r"(?i)^([\d.,]+)\s*([KMB])?\s+views?$").unwrap());
static MPRE_RE: LazyLock<Regex> = LazyLock::new(|| Regex::new(r#""(MPRE[A-Za-z0-9_\-]+)""#).unwrap());

fn message(text: impl Into<String>) -> NetError {
    NetError::Message(text.into())
}

fn to_int(text: &str) -> Option<u32> {
    let digits: String = text.chars().filter(|c| c.is_ascii_digit()).collect();
    digits.parse().ok()
}

/// Everything the playlist page shows in its header, plus the rows.
#[derive(Debug, Clone, Default)]
pub struct PlaylistDetails {
    pub id: String,
    pub title: String,
    pub description: String,
    pub privacy: Option<String>,
    pub thumbnails: Vec<String>,
    pub author: Vec<Person>,
    /// "by X and 2 others" on collaborative playlists.
    pub collaborators: Option<String>,
    pub year: Option<String>,
    pub duration: Option<String>,
    pub duration_seconds: Option<u32>,
    pub track_count: Option<u32>,
    pub tracks: Vec<Track>,
    /// Albums: the OLAK playlist behind the browse id.
    pub audio_playlist_id: Option<String>,
    /// Albums: "Album", "Single" or "EP" as the header says.
    pub album_type: Option<String>,
    pub like_status: LikeStatus,
}

impl PlaylistDetails {
    fn sum_duration(&mut self) {
        self.duration_seconds = Some(self.tracks.iter().filter_map(|t| t.duration_seconds).sum());
    }
}

// -- playlists ------------------------------------------------------------

/// Port of ytmusicapi's get_playlist plus the MusicClient tweaks: LM gets the
/// "Your Likes" title, and every continuation is followed up to `limit`.
pub async fn get_playlist(api: &YTMusicClient, playlist_id: &str, limit: Option<usize>) -> Result<PlaylistDetails, NetError> {
    let browse_id = if playlist_id.starts_with("VL") { playlist_id.to_owned() } else { format!("VL{playlist_id}") };
    let response = api.send_request("browse", json!({ "browseId": browse_id })).await?;
    let mut details = parse_playlist_header(&response).ok_or_else(|| message(format!("playlist {playlist_id}: header missing")))?;
    if details.id.is_empty() {
        details.id = playlist_id.trim_start_matches("VL").to_owned();
    }
    let collaborative = details.collaborators.is_some();
    let shelf = response.pointer(&format!("{SECONDARY_SECTIONS}/0/musicPlaylistShelfRenderer"));
    // Like get_continuations_2025, `limit` bounds the rows that continuations
    // add, not the first page, and nothing is truncated afterwards.
    let limit = limit.unwrap_or(usize::MAX);
    let mut tracks = shelf.map(|s| parse_playlist_items(array_at(s, "/contents"), false, collaborative)).unwrap_or_default();
    let first_page = tracks.len();
    let mut token = shelf.and_then(|s| array_at(s, "/contents").last().and_then(item_continuation_token).or_else(|| next_continuation(s)));
    let mut pages = 0;
    while let Some(t) = token.take() {
        if tracks.len() - first_page >= limit || pages >= MAX_CONTINUATION_PAGES {
            break;
        }
        pages += 1;
        let next = api.send_request("browse", json!({ "continuation": t })).await?;
        let mut entries: Vec<&Value> = Vec::new();
        for action in array_at(&next, "/onResponseReceivedActions") {
            entries.extend(array_at(action, "/appendContinuationItemsAction/continuationItems"));
        }
        if let Some(legacy) = next.pointer("/continuationContents/musicPlaylistShelfContinuation") {
            entries.extend(array_at(legacy, "/contents"));
            token = next_continuation(legacy);
        }
        for entry in &entries {
            if let Some(t2) = item_continuation_token(entry) {
                token = Some(t2);
            }
        }
        let parsed = parse_playlist_items(entries.iter().copied(), false, collaborative);
        if parsed.is_empty() {
            break;
        }
        tracks.extend(parsed);
    }
    details.tracks = tracks;
    details.sum_duration();
    if playlist_id == "LM" {
        details.title = "Your Likes".to_owned();
        details.description = "Your liked songs from YouTube Music.".to_owned();
    }
    Ok(details)
}

fn parse_playlist_header(response: &Value) -> Option<PlaylistDetails> {
    let header_data = response.pointer(HEADER_SECTION)?;
    let owned = header_data.get("musicEditablePlaylistDetailHeaderRenderer").is_some();
    let (header, privacy, id) = if owned {
        let editable = &header_data["musicEditablePlaylistDetailHeaderRenderer"];
        (editable.pointer("/header/musicResponsiveHeaderRenderer")?, owned_at(editable, "/editHeader/musicPlaylistEditHeaderRenderer/privacy"), owned_at(editable, "/playlistId"))
    } else {
        let header = header_data.get("musicResponsiveHeaderRenderer")?;
        (header, Some("PUBLIC".to_owned()), owned_at(header, "/buttons/1/musicPlayButtonRenderer/playNavigationEndpoint/watchEndpoint/playlistId"))
    };
    let mut details = PlaylistDetails { id: id.unwrap_or_default(), privacy, ..PlaylistDetails::default() };
    details.description = runs_text(array_at(header, "/description/musicDescriptionShelfRenderer/description/runs"));
    details.title = runs_text(array_at(header, "/title/runs"));
    details.thumbnails = thumbnails_at(header, "/thumbnail/musicThumbnailRenderer/thumbnail/thumbnails");
    if let Some(face) = header.pointer("/facepile/avatarStackViewModel") {
        let command = face.pointer("/rendererContext/commandContext/onTap/innertubeCommand");
        if command.and_then(|c| str_at(c, "/showEngagementPanelEndpoint/identifier/tag")) == Some("PAplaylist_collaborate") {
            details.collaborators = owned_at(face, "/rendererContext/accessibilityContext/label");
        } else {
            details.author = vec![Person { name: owned_at(face, "/text/content").unwrap_or_default(), id: command.and_then(|c| owned_at(c, "/browseEndpoint/browseId")) }];
        }
    }
    let second = array_at(header, "/secondSubtitle/runs");
    if !second.is_empty() {
        let has_views = if second.len() > 3 { 2 } else { 0 };
        let has_duration = if second.len() > 1 { 2 } else { 0 };
        if has_duration > 0 {
            details.duration = second.get(has_views + has_duration).and_then(|r| owned_at(r, "/text"));
        }
        details.track_count = second.get(has_views).and_then(|r| str_at(r, "/text")).and_then(to_int);
    }
    let subtitle = array_at(header, "/subtitle/runs");
    let skip = 2 + if owned { 2 } else { 0 };
    details.year = parse_song_runs(subtitle.get(skip..).unwrap_or(&[])).year;
    Some(details)
}

// -- albums ---------------------------------------------------------------

/// Port of get_album with parse_album_header_2024: header, then the rows
/// parsed as album items with their album and artists filled from the header.
pub async fn get_album(api: &YTMusicClient, browse_id: &str) -> Result<PlaylistDetails, NetError> {
    if !browse_id.starts_with("MPRE") {
        return Err(message("Invalid album browseId provided, must start with MPRE."));
    }
    let response = api.send_request("browse", json!({ "browseId": browse_id })).await?;
    let header = response.pointer(&format!("{HEADER_SECTION}/musicResponsiveHeaderRenderer")).ok_or_else(|| message(format!("album {browse_id}: header missing")))?;
    let mut details = PlaylistDetails { id: browse_id.to_owned(), ..PlaylistDetails::default() };
    details.title = owned_at(header, "/title/runs/0/text").unwrap_or_default();
    details.album_type = owned_at(header, "/subtitle/runs/0/text");
    details.thumbnails = thumbnails_at(header, "/thumbnail/musicThumbnailRenderer/thumbnail/thumbnails");
    details.description = runs_text(array_at(header, "/description/musicDescriptionShelfRenderer/description/runs"));
    let subtitle = array_at(header, "/subtitle/runs");
    details.year = parse_song_runs(subtitle.get(2..).unwrap_or(&[])).year;
    details.author = parse_artists_runs(array_at(header, "/straplineTextOne/runs"));
    let second = array_at(header, "/secondSubtitle/runs");
    if second.len() > 1 {
        details.track_count = second.first().and_then(|r| str_at(r, "/text")).and_then(to_int);
        details.duration = second.get(2).and_then(|r| owned_at(r, "/text"));
    } else {
        details.duration = second.first().and_then(|r| owned_at(r, "/text"));
    }
    let buttons = array_at(header, "/buttons");
    let play = buttons.iter().find(|b| b.get("musicPlayButtonRenderer").is_some());
    details.audio_playlist_id = play.and_then(|b| owned_at(b, "/musicPlayButtonRenderer/playNavigationEndpoint/watchPlaylistEndpoint/playlistId").or_else(|| owned_at(b, "/musicPlayButtonRenderer/playNavigationEndpoint/watchEndpoint/playlistId")));
    if let Some(service) = buttons.iter().find_map(|b| b.pointer("/toggleButtonRenderer/defaultServiceEndpoint")) {
        details.like_status = parse_like_status(service);
    }
    let shelf = response.pointer(&format!("{SECONDARY_SECTIONS}/0/musicShelfRenderer"));
    let mut tracks = shelf.map(|s| parse_playlist_items(array_at(s, "/contents"), true, false)).unwrap_or_default();
    for track in &mut tracks {
        track.album = Some(Named { name: details.title.clone(), id: Some(browse_id.to_owned()) });
        if track.artists.is_empty() {
            track.artists = details.author.clone();
            track.artist = details.author.iter().map(|a| a.name.as_str()).collect::<Vec<_>>().join(", ");
        }
    }
    details.tracks = tracks;
    details.sum_duration();
    Ok(details)
}

/// Port of get_library_upload_album: the older detail header plus upload rows.
pub async fn get_upload_album(api: &YTMusicClient, browse_id: &str) -> Result<PlaylistDetails, NetError> {
    let response = api.send_request("browse", json!({ "browseId": browse_id })).await?;
    let header = response.pointer("/header/musicDetailHeaderRenderer").ok_or_else(|| message(format!("upload album {browse_id}: header missing")))?;
    let mut details = PlaylistDetails { id: browse_id.to_owned(), ..PlaylistDetails::default() };
    details.title = owned_at(header, "/title/runs/0/text").unwrap_or_default();
    details.album_type = owned_at(header, "/subtitle/runs/0/text");
    details.thumbnails = thumbnails_at(header, "/thumbnail/croppedSquareThumbnailRenderer/thumbnail/thumbnails");
    details.description = owned_at(header, "/description/runs/0/text").unwrap_or_default();
    let subtitle = array_at(header, "/subtitle/runs");
    let parsed = parse_song_runs(subtitle.get(2..).unwrap_or(&[]));
    details.author = parsed.artists;
    details.year = parsed.year;
    let second = array_at(header, "/secondSubtitle/runs");
    if second.len() > 1 {
        details.track_count = second.first().and_then(|r| str_at(r, "/text")).and_then(to_int);
        details.duration = second.get(2).and_then(|r| owned_at(r, "/text"));
    } else {
        details.duration = second.first().and_then(|r| owned_at(r, "/text"));
    }
    let shelf = response.pointer(&format!("{SINGLE_SECTIONS}/0/musicShelfRenderer"));
    details.tracks = shelf.map(|s| parse_uploaded_items(array_at(s, "/contents"))).unwrap_or_default();
    details.sum_duration();
    Ok(details)
}

/// Port of get_library_upload_songs(limit=None): every uploaded track.
pub async fn get_upload_songs(api: &YTMusicClient) -> Result<Vec<Track>, NetError> {
    let response = api.send_request("browse", json!({ "browseId": "FEmusic_library_privately_owned_tracks" })).await?;
    let sections = array_at(&response, SINGLE_SECTIONS);
    let shelf = sections.iter().find_map(|s| s.get("musicShelfRenderer").or_else(|| s.pointer("/itemSectionRenderer/contents/0/musicShelfRenderer")));
    let Some(shelf) = shelf else { return Ok(Vec::new()) };
    let mut songs = parse_uploaded_items(array_at(shelf, "/contents"));
    let mut token = next_continuation(shelf);
    let mut pages = 0;
    while let Some(t) = token.take() {
        if pages >= MAX_CONTINUATION_PAGES {
            break;
        }
        pages += 1;
        let next = api.send_request("browse", json!({ "continuation": t })).await?;
        let Some(cont) = next.pointer("/continuationContents/musicShelfContinuation") else { break };
        songs.extend(parse_uploaded_items(array_at(cont, "/contents")));
        token = next_continuation(cont);
    }
    Ok(songs)
}

// -- watch panel and radio ------------------------------------------------

#[derive(Debug, Clone, Default)]
pub struct WatchPlaylist {
    pub tracks: Vec<WatchTrack>,
    pub playlist_id: Option<String>,
}

/// Port of get_watch_playlist: the panel YouTube Music shows when a track
/// plays, or a radio for `radio`. Follows continuations until `limit`.
pub async fn get_watch_playlist(api: &YTMusicClient, video_id: Option<&str>, playlist_id: Option<&str>, limit: usize, radio: bool) -> Result<WatchPlaylist, NetError> {
    let mut body = json!({ "enablePersistentPlaylistPanel": true, "isAudioOnly": true, "tunerSettingValue": "AUTOMIX_SETTING_NORMAL" });
    if video_id.is_none() && playlist_id.is_none() {
        return Err(message("You must provide either a video id, a playlist id, or both"));
    }
    let mut playlist_id = playlist_id.map(str::to_owned);
    if let Some(v) = video_id {
        body["videoId"] = json!(v);
        if playlist_id.is_none() {
            playlist_id = Some(format!("RDAMVM{v}"));
        }
        if !radio {
            body["watchEndpointMusicSupportedConfigs"] = json!({ "watchEndpointMusicConfig": { "hasPersistentPlaylistPanel": true, "musicVideoType": "MUSIC_VIDEO_TYPE_ATV" } });
        }
    }
    let mut is_playlist = false;
    if let Some(p) = &playlist_id {
        let pid = p.trim_start_matches("VL");
        is_playlist = pid.starts_with("PL") || pid.starts_with("OLA");
        body["playlistId"] = json!(pid);
    }
    if radio {
        body["params"] = json!("wAEB");
    }
    let response = api.send_request("next", body.clone()).await?;
    let watch_next = response.pointer("/contents/singleColumnMusicWatchNextResultsRenderer/tabbedRenderer/watchNextTabbedResultsRenderer").ok_or_else(|| message("No content returned by the server."))?;
    let results = watch_next.pointer("/tabs/0/tabRenderer/content/musicQueueRenderer/content/playlistPanelRenderer").ok_or_else(|| {
        let mut msg = "No content returned by the server.".to_owned();
        if let Some(p) = &playlist_id {
            msg.push_str(&format!("\nEnsure you have access to {p} - a private playlist may cause this."));
        }
        message(msg)
    })?;
    let panel_playlist = array_at(results, "/contents").iter().find_map(|x| owned_at(x, "/playlistPanelVideoRenderer/navigationEndpoint/watchEndpoint/playlistId"));
    let mut tracks = parse_watch_playlist(array_at(results, "/contents"));
    // Continuations ride in the query string for this endpoint. The crate
    // appends its own "?alt=json" to whatever endpoint it is given, so the
    // token goes in front of a throwaway parameter that swallows that suffix.
    let key = if is_playlist { "/continuations/0/nextContinuationData/continuation" } else { "/continuations/0/nextRadioContinuationData/continuation" };
    let mut token = owned_at(results, key);
    let mut pages = 0;
    while let Some(t) = token.take() {
        if tracks.len() >= limit || pages >= 20 {
            break;
        }
        pages += 1;
        let endpoint = format!("next?ctoken={t}&continuation={t}&_=");
        let next = api.send_request(&endpoint, body.clone()).await?;
        let Some(cont) = next.pointer("/continuationContents/playlistPanelContinuation") else { break };
        tracks.extend(parse_watch_playlist(array_at(cont, "/contents")));
        token = owned_at(cont, key);
    }
    Ok(WatchPlaylist { tracks, playlist_id: panel_playlist })
}

/// Port of Player.start_radio's fetch: 50 radio tracks for a song or a playlist.
pub async fn radio_tracks(api: &YTMusicClient, video_id: Option<&str>, playlist_id: Option<&str>) -> Result<WatchPlaylist, NetError> {
    get_watch_playlist(api, video_id, playlist_id, 50, true).await
}

/// Port of MusicClient.find_audio_version: the song twin of a music video,
/// from YouTube Music's own pairing first, then a conservative song search.
pub async fn find_audio_version(api: &YTMusicClient, video_id: &str) -> Result<Option<Track>, NetError> {
    let watch = get_watch_playlist(api, Some(video_id), None, 1, false).await.unwrap_or_default();
    let Some(current) = watch.tracks.first() else { return Ok(None) };
    let cur_type = current.track.video_type.clone().unwrap_or_default().to_uppercase();
    if cur_type == "MUSIC_VIDEO_TYPE_ATV" {
        return Ok(None);
    }
    if let Some(cp) = &current.counterpart {
        if cp.video_id.as_str() != video_id && cp.video_type.as_deref().map(str::to_uppercase).as_deref() == Some("MUSIC_VIDEO_TYPE_ATV") {
            return Ok(Some(cp.clone()));
        }
    }
    let title = current.track.title.clone();
    let artists: Vec<String> = current.track.artists.iter().map(|a| a.name.to_lowercase()).collect();
    if title.is_empty() {
        return Ok(None);
    }
    let query = match artists.first() {
        Some(a) => format!("{title} {a}"),
        None => title.clone(),
    };
    let results = search(api, &query, Some(SearchFilter::Songs)).await?;
    let wanted = normalize_title(&title);
    for item in results.items.iter().filter(|i| i.kind == ItemKind::Song) {
        if normalize_title(&item.title) != wanted {
            continue;
        }
        let names: Vec<String> = item.artists.iter().map(|a| a.name.to_lowercase()).collect();
        if artists.is_empty() || artists.iter().any(|a| names.contains(a)) {
            return Ok(item.to_track());
        }
    }
    Ok(None)
}

fn normalize_title(title: &str) -> String {
    title.to_lowercase().chars().filter(|c| c.is_alphanumeric()).collect()
}

// -- ids and ratings ------------------------------------------------------

/// Port of get_album_browse_id: the MPRE id behind an OLAK playlist, read
/// off the playlist page's HTML.
pub async fn get_album_browse_id(http: &reqwest::Client, auth: Option<&HttpAuth>, audio_playlist_id: &str) -> Result<Option<String>, NetError> {
    let mut request = http.get(format!("https://music.youtube.com/playlist?list={audio_playlist_id}"));
    if let Some(auth) = auth {
        request = request.header("Cookie", &auth.cookie).header("User-Agent", &auth.user_agent);
    }
    let text = request.send().await?.text().await?;
    let decoded = text.replace("\\u003d", "=").replace("\\\"", "\"");
    Ok(MPRE_RE.captures(&decoded).map(|c| c[1].to_owned()))
}

/// Port of MusicClient.rate_playlist: LIKE saves, INDIFFERENT removes.
/// Strips a VL prefix and turns an MPRE browse id into its audio playlist.
pub async fn rate_playlist(api: &YTMusicClient, playlist_id: &str, rating: LikeStatus) -> Result<(), NetError> {
    let mut pid = playlist_id.trim_start_matches("VL").to_owned();
    if pid.starts_with("MPRE") {
        if let Ok(album) = get_album(api, &pid).await {
            if let Some(audio) = album.audio_playlist_id {
                pid = audio;
            }
        }
    }
    let endpoint = match rating {
        LikeStatus::Like => "like/like",
        LikeStatus::Dislike => "like/dislike",
        LikeStatus::Indifferent => "like/removelike",
    };
    api.send_request(endpoint, json!({ "target": { "playlistId": pid } })).await?;
    Ok(())
}

/// Port of edit_playlist for the fields the edit dialog offers.
pub async fn edit_playlist(api: &YTMusicClient, playlist_id: &str, title: Option<&str>, description: Option<&str>, privacy: Option<&str>) -> Result<(), NetError> {
    let mut actions = Vec::new();
    if let Some(t) = title.filter(|t| !t.is_empty()) {
        actions.push(json!({ "action": "ACTION_SET_PLAYLIST_NAME", "playlistName": t }));
    }
    if let Some(d) = description.filter(|d| !d.is_empty()) {
        actions.push(json!({ "action": "ACTION_SET_PLAYLIST_DESCRIPTION", "playlistDescription": d }));
    }
    if let Some(p) = privacy.filter(|p| !p.is_empty()) {
        actions.push(json!({ "action": "ACTION_SET_PLAYLIST_PRIVACY", "playlistPrivacy": p }));
    }
    if actions.is_empty() {
        return Ok(());
    }
    api.send_request("browse/edit_playlist", json!({ "playlistId": playlist_id.trim_start_matches("VL"), "actions": actions })).await?;
    Ok(())
}

/// Remove rows by (videoId, setVideoId), like remove_playlist_items.
pub async fn remove_playlist_items(api: &YTMusicClient, playlist_id: &str, items: &[(String, String)]) -> Result<(), NetError> {
    let actions: Vec<Value> = items.iter().map(|(video, set)| json!({ "setVideoId": set, "removedVideoId": video, "action": "ACTION_REMOVE_VIDEO" })).collect();
    if actions.is_empty() {
        return Err(message("Cannot remove songs, because setVideoId is missing"));
    }
    api.send_request("browse/edit_playlist", json!({ "playlistId": playlist_id.trim_start_matches("VL"), "actions": actions })).await?;
    Ok(())
}

/// Port of MusicClient.add_playlist_items. Single adds swap a music video
/// for its song version first, bulk adds do not.
pub async fn add_playlist_items(api: &YTMusicClient, playlist_id: &str, video_ids: Vec<String>, swap_to_audio: Option<bool>) -> Result<(), NetError> {
    let swap = swap_to_audio.unwrap_or(video_ids.len() == 1);
    let mut ids = Vec::with_capacity(video_ids.len());
    for vid in video_ids {
        if swap {
            match find_audio_version(api, &vid).await {
                Ok(Some(alt)) => {
                    ids.push(alt.video_id.0);
                    continue;
                }
                Ok(None) => {}
                Err(err) => tracing::warn!(%err, vid, "audio-version swap failed"),
            }
        }
        ids.push(vid);
    }
    api.add_playlist_items(playlist_id.trim_start_matches("VL"), &ids, false).await?;
    Ok(())
}

pub async fn delete_playlist(api: &YTMusicClient, playlist_id: &str) -> Result<(), NetError> {
    api.delete_playlist(playlist_id.trim_start_matches("VL")).await?;
    Ok(())
}

// -- library membership and editable playlists ----------------------------

/// Port of _populate_library_cache_async's fetch: every saved playlist and
/// album id, albums under their browse id and audio playlist id both.
pub async fn fetch_library_ids(api: Arc<YTMusicClient>) -> Result<(LibraryIds, Vec<MediaItem>), NetError> {
    let playlists = library::library_playlists(api.clone()).await.unwrap_or_default();
    let albums = library::library_albums(api).await.unwrap_or_default();
    let mut ids = LibraryIds::default();
    ids.playlists.extend(playlists.iter().map(|p| p.id.clone()));
    for album in &albums {
        ids.albums.insert(album.id.clone());
        if let Some(pid) = &album.playlist_id {
            ids.albums.insert(pid.clone());
        }
    }
    Ok((ids, playlists))
}

/// Port of get_editable_playlists: owned or collaborative playlists only.
pub fn editable_playlists(playlists: &[MediaItem], account_name: Option<&str>) -> Vec<MediaItem> {
    let user = account_name.map(|n| n.to_lowercase()).unwrap_or_default();
    playlists
        .iter()
        .filter(|p| {
            let pid = p.id.as_str();
            if !(pid.starts_with("PL") || pid.starts_with("VL")) || ["LM", "SE", "VLLM"].contains(&pid) {
                return false;
            }
            let author = p.artists.first().map(|a| a.name.to_lowercase()).unwrap_or_default();
            author.is_empty() || author == "you" || (!user.is_empty() && author == user)
        })
        .cloned()
        .collect()
}

/// Port of is_own_playlist: only PL/VL ids the signed-in account authored.
pub fn is_own_playlist(details: &PlaylistDetails, playlist_id: &str, account_name: Option<&str>) -> bool {
    let Some(user_name) = account_name.filter(|n| !n.is_empty()) else { return false };
    let pid = if playlist_id.is_empty() { details.id.as_str() } else { playlist_id };
    if ["LM", "SE", "VLLM"].contains(&pid) || !(pid.starts_with("PL") || pid.starts_with("VL")) {
        return false;
    }
    let author = match (&details.collaborators, details.author.first()) {
        (None, None) => return true,
        (Some(text), _) => text.clone(),
        (None, Some(a)) => a.name.clone(),
    };
    if details.collaborators.is_some() && author.contains(user_name) {
        return true;
    }
    author == user_name
}

// -- sort metrics ---------------------------------------------------------

fn normalize_browse_playlist_id(playlist_id: &str) -> Option<String> {
    let pid = playlist_id.trim();
    if pid.is_empty() {
        return None;
    }
    Some(if pid.starts_with("VL") { pid.to_owned() } else { format!("VL{pid}") })
}

/// Port of get_playlist_added_dates: videoId to the epoch seconds it was added.
pub async fn playlist_added_dates(api: &YTMusicClient, playlist_id: &str) -> Result<SortMetric, NetError> {
    let Some(browse_id) = normalize_browse_playlist_id(playlist_id) else { return Ok(SortMetric::new()) };
    let mut dates = SortMetric::new();
    let mut response = api.send_request("browse", json!({ "browseId": browse_id })).await?;
    for _ in 0..MAX_METRIC_PAGES {
        for item in walk_key(&response, "playlistItemData") {
            if let (Some(vid), Some(added)) = (str_at(item, "/videoId"), item.get("voteSortValue").and_then(Value::as_i64)) {
                dates.entry(vid.to_owned()).or_insert(added);
            }
        }
        let Some(token) = continuation_token_for_rows(&response, &[MRLIR]) else { break };
        response = match api.send_request("browse", json!({ "continuation": token })).await {
            Ok(r) => r,
            Err(err) => {
                tracing::warn!(%err, browse_id, "added-date fetch failed");
                break;
            }
        };
    }
    tracing::info!(count = dates.len(), browse_id, "added dates");
    Ok(dates)
}

fn parse_view_count(text: &str) -> Option<i64> {
    let text = text.trim();
    if text.to_lowercase().starts_with("no view") {
        return Some(0);
    }
    let caps = VIEWS_RE.captures(text)?;
    let number: f64 = caps[1].replace(',', "").parse().ok()?;
    let multiplier = match caps.get(2).map(|m| m.as_str().to_uppercase()).as_deref() {
        Some("K") => 1_000.0,
        Some("M") => 1_000_000.0,
        Some("B") => 1_000_000_000.0,
        _ => 1.0,
    };
    Some((number * multiplier) as i64)
}

fn harvest_view_counts(response: &Value, out: &mut SortMetric) {
    for lockup in walk_key(response, "lockupViewModel") {
        if str_at(lockup, "/contentType") != Some("LOCKUP_CONTENT_TYPE_VIDEO") {
            continue;
        }
        let Some(vid) = str_at(lockup, "/contentId") else { continue };
        if out.contains_key(vid) {
            continue;
        }
        'rows: for row in array_at(lockup, "/metadata/lockupMetadataViewModel/metadata/contentMetadataViewModel/metadataRows") {
            for part in array_at(row, "/metadataParts") {
                if let Some(count) = str_at(part, "/text/content").and_then(parse_view_count) {
                    out.insert(vid.to_owned(), count);
                    break 'rows;
                }
            }
        }
    }
    for item in walk_key(response, "playlistVideoRenderer") {
        let Some(vid) = str_at(item, "/videoId") else { continue };
        if out.contains_key(vid) {
            continue;
        }
        for run in array_at(item, "/videoInfo/runs") {
            if let Some(count) = str_at(run, "/text").and_then(parse_view_count) {
                out.insert(vid.to_owned(), count);
                break;
            }
        }
    }
}

/// SAPISIDHASH for an origin, what _yt_web_headers computed for www.youtube.com.
fn sapisid_hash(cookie: &str, origin: &str) -> Option<String> {
    use sha1::{Digest, Sha1};
    let sapisid = cookie.split(';').map(str::trim).find_map(|p| p.strip_prefix("__Secure-3PAPISID=").or_else(|| p.strip_prefix("SAPISID=")))?;
    let ts = std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).ok()?.as_secs();
    let digest = Sha1::digest(format!("{ts} {sapisid} {origin}").as_bytes());
    Some(format!("SAPISIDHASH {ts}_{digest:x}"))
}

/// Port of get_playlist_view_counts: youtube.com's copy of the playlist
/// carries a view count per row, YouTube Music's does not.
pub async fn playlist_view_counts(http: &reqwest::Client, auth: Option<&HttpAuth>, playlist_id: &str) -> Result<SortMetric, NetError> {
    let Some(browse_id) = normalize_browse_playlist_id(playlist_id) else { return Ok(SortMetric::new()) };
    let mut body = json!({ "context": { "client": { "clientName": "WEB", "clientVersion": "2.20240101.00.00", "hl": "en", "gl": "US" } }, "browseId": browse_id });
    let mut views = SortMetric::new();
    let send = |body: Value| {
        let mut request = http
            .post(YT_WEB_BROWSE_URL)
            .header("Content-Type", "application/json")
            .header("Accept-Language", "en-US,en;q=0.9")
            .header("User-Agent", "Mozilla/5.0 (Windows NT 10.0; Win64; x64) AppleWebKit/537.36 (KHTML, like Gecko) Chrome/120.0.0.0 Safari/537.36")
            .header("Origin", "https://www.youtube.com")
            .header("Referer", "https://www.youtube.com/");
        if let Some(auth) = auth {
            if let Some(hash) = sapisid_hash(&auth.cookie, "https://www.youtube.com") {
                request = request.header("Cookie", &auth.cookie).header("Authorization", hash).header("X-Origin", "https://www.youtube.com");
            }
        }
        request.json(&body).send()
    };
    let mut response: Value = match send(body.clone()).await {
        Ok(r) => r.json().await?,
        Err(err) => {
            tracing::warn!(%err, browse_id, "view-count fetch failed");
            return Ok(views);
        }
    };
    for _ in 0..MAX_METRIC_PAGES {
        harvest_view_counts(&response, &mut views);
        let Some(token) = continuation_token_for_rows(&response, &["lockupViewModel", "playlistVideoRenderer"]) else { break };
        if let Some(map) = body.as_object_mut() {
            map.remove("browseId");
            map.insert("continuation".into(), json!(token));
        }
        response = match send(body.clone()).await {
            Ok(r) => match r.json().await {
                Ok(v) => v,
                Err(_) => break,
            },
            Err(err) => {
                tracing::warn!(%err, browse_id, "view-count page failed");
                break;
            }
        };
    }
    tracing::info!(count = views.len(), browse_id, "view counts");
    Ok(views)
}

// -- raw parsing fallbacks ------------------------------------------------

/// Port of _fetch_continuation: follow tokens through both response shapes.
async fn fetch_continuation(api: &YTMusicClient, mut token: Option<String>) -> Vec<MediaItem> {
    let mut items = Vec::new();
    for _ in 0..MAX_CONTINUATION_PAGES {
        let Some(t) = token.take() else { break };
        let Ok(response) = api.send_request("browse", json!({ "continuation": t })).await else { break };
        for action in array_at(&response, "/onResponseReceivedActions") {
            for raw in array_at(action, "/appendContinuationItemsAction/continuationItems") {
                match item_continuation_token(raw) {
                    Some(next) => token = Some(next),
                    None => items.extend(parse_channel_item(raw)),
                }
            }
        }
        for key in ["musicPlaylistShelfContinuation", "gridContinuation", "musicShelfContinuation"] {
            let Some(renderer) = response.pointer(&format!("/continuationContents/{key}")) else { continue };
            for raw in array_at(renderer, "/contents").iter().chain(array_at(renderer, "/items")) {
                match item_continuation_token(raw) {
                    Some(next) => token = Some(next),
                    None => items.extend(parse_channel_item(raw)),
                }
            }
        }
    }
    items
}

/// Port of _raw_parse_channel_content.
pub async fn raw_parse_channel_content(api: &YTMusicClient, browse_id: &str, params: Option<&str>) -> Result<Vec<MediaItem>, NetError> {
    let mut body = json!({ "browseId": browse_id });
    if let Some(p) = params {
        body["params"] = json!(p);
    }
    let response = api.send_request("browse", body).await?;
    let mut items = Vec::new();
    for section in array_at(&response, SINGLE_SECTIONS) {
        for key in ["gridRenderer", "musicShelfRenderer", "musicPlaylistShelfRenderer", "musicCarouselShelfRenderer"] {
            let Some(renderer) = section.get(key) else { continue };
            let entries = if renderer.get("items").is_some() { array_at(renderer, "/items") } else { array_at(renderer, "/contents") };
            for raw in entries {
                items.extend(parse_channel_item(raw));
                if let Some(token) = item_continuation_token(raw) {
                    items.extend(fetch_continuation(api, Some(token)).await);
                }
            }
        }
    }
    Ok(items)
}

/// Port of _raw_parse_playlist for lists ytmusicapi cannot read, such as
/// OLAK chart playlists. Returns the header title with the rows.
pub async fn raw_parse_playlist(api: &YTMusicClient, browse_id: &str) -> Result<(Option<String>, Vec<MediaItem>), NetError> {
    let response = api.send_request("browse", json!({ "browseId": browse_id })).await?;
    let mut items = Vec::new();
    for section in array_at(&response, SECONDARY_SECTIONS) {
        for key in ["musicPlaylistShelfRenderer", "musicShelfRenderer"] {
            let Some(renderer) = section.get(key) else { continue };
            for raw in array_at(renderer, "/contents") {
                match item_continuation_token(raw) {
                    Some(token) => items.extend(fetch_continuation(api, Some(token)).await),
                    None => items.extend(parse_channel_item(raw)),
                }
            }
        }
    }
    if items.is_empty() {
        items = raw_parse_channel_content(api, browse_id, None).await?;
    }
    let title = response.get("header").and_then(Value::as_object).and_then(|h| h.values().find_map(|v| owned_at(v, "/title/runs/0/text")));
    Ok((title, items))
}

/// Port of MusicClient.get_artist_albums: the artist's albums grid with
/// its continuations, falling back to user playlists and raw parsing.
pub async fn artist_albums(api: &YTMusicClient, channel_id: &str, params: Option<&str>) -> Result<Vec<MediaItem>, NetError> {
    let mut body = json!({ "browseId": channel_id });
    if let Some(p) = params {
        body["params"] = json!(p);
    }
    if let Ok(response) = api.send_request("browse", body).await {
        let results = response.pointer(&format!("{SINGLE_SECTIONS}/0"));
        if let Some(results) = results {
            let grid = results.get("gridRenderer");
            let contents = grid.map(|g| array_at(g, "/items")).filter(|c| !c.is_empty()).unwrap_or_else(|| array_at(results, "/musicCarouselShelfRenderer/contents"));
            let mut albums: Vec<MediaItem> = contents.iter().filter_map(|c| parse_two_row(c, ItemKind::Album)).collect();
            let mut token = grid.and_then(next_continuation);
            let mut pages = 0;
            while let Some(t) = token.take() {
                if pages >= MAX_CONTINUATION_PAGES {
                    break;
                }
                pages += 1;
                let Ok(next) = api.send_request("browse", json!({ "continuation": t })).await else { break };
                let Some(cont) = next.pointer("/continuationContents/gridContinuation") else { break };
                albums.extend(array_at(cont, "/items").iter().filter_map(|c| parse_two_row(c, ItemKind::Album)));
                token = next_continuation(cont);
            }
            if !albums.is_empty() {
                return Ok(albums);
            }
            // get_user_playlists: the same grid read as playlists.
            let playlists: Vec<MediaItem> = contents.iter().filter_map(|c| parse_two_row(c, ItemKind::Playlist)).collect();
            if !playlists.is_empty() {
                return Ok(playlists);
            }
        }
    }
    raw_parse_channel_content(api, channel_id, params).await
}

/// Title, artist and album text of a track lowercased, what the filters match on.
pub fn track_search_text(track: &Track) -> (String, String, String) {
    (track.title.to_lowercase(), track.artist.to_lowercase(), track.album.as_ref().map(|a| a.name.to_lowercase()).unwrap_or_default())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn live_client() -> Arc<YTMusicClient> {
        let paths = crate::paths::Paths::discover();
        crate::net::ytmusic::YtMusic::new(&paths).unwrap().api()
    }

    #[tokio::test]
    #[ignore]
    async fn live_liked_songs_page() {
        let api = live_client();
        let d = get_playlist(&api, "LM", Some(60)).await.unwrap();
        println!("LM title={} desc={} author={:?} count={:?} tracks={} liked={}", d.title, d.description, d.author, d.track_count, d.tracks.len(), d.tracks.iter().filter(|t| t.like_status == LikeStatus::Like).count());
        println!("first={:?}", d.tracks.first().map(|t| (&t.title, &t.artist, t.duration_seconds, &t.set_video_id, &t.video_type)));
        assert_eq!(d.title, "Your Likes");
        assert!(!d.tracks.is_empty());
    }

    #[tokio::test]
    #[ignore]
    async fn live_user_playlist_and_album() {
        let api = live_client();
        let playlists = library::library_playlists(api.clone()).await.unwrap();
        let user = playlists.iter().find(|p| p.id.starts_with("PL")).unwrap();
        let d = get_playlist(&api, &user.id, Some(30)).await.unwrap();
        println!("playlist {} title={} privacy={:?} author={:?} year={:?} count={:?} duration={:?} tracks={} set_ids={}", user.id, d.title, d.privacy, d.author, d.year, d.track_count, d.duration, d.tracks.len(), d.tracks.iter().filter(|t| t.set_video_id.is_some()).count());
        assert!(!d.title.is_empty());
        let albums = library::library_albums(api.clone()).await.unwrap();
        let album = albums.first().unwrap();
        let a = get_album(&api, &album.id).await.unwrap();
        println!("album {} title={} type={:?} year={:?} artists={:?} count={:?} audio={:?} like={:?} tracks={} nums={:?}", album.id, a.title, a.album_type, a.year, a.author, a.track_count, a.audio_playlist_id, a.like_status, a.tracks.len(), a.tracks.iter().map(|t| t.track_number).collect::<Vec<_>>());
        assert!(!a.tracks.is_empty());
        if let Some(olak) = &a.audio_playlist_id {
            let paths = crate::paths::Paths::discover();
            let client = crate::net::ytmusic::YtMusic::new(&paths).unwrap();
            let back = get_album_browse_id(client.http(), client.media_auth().as_ref(), olak).await.unwrap();
            println!("olak {olak} -> {back:?}");
        }
    }

    #[tokio::test]
    #[ignore]
    async fn live_radio_and_metrics() {
        let api = live_client();
        let liked = get_playlist(&api, "LM", Some(5)).await.unwrap();
        let vid = liked.tracks[0].video_id.0.clone();
        let radio = radio_tracks(&api, Some(&vid), None).await.unwrap();
        println!("radio playlist={:?} tracks={} first={:?}", radio.playlist_id, radio.tracks.len(), radio.tracks.first().map(|t| (&t.track.title, &t.track.artist, t.counterpart.is_some())));
        assert!(radio.tracks.len() > 5);
        let playlists = library::library_playlists(api.clone()).await.unwrap();
        let user = playlists.iter().find(|p| p.id.starts_with("PL")).unwrap();
        let added = playlist_added_dates(&api, &user.id).await.unwrap();
        println!("added dates for {}: {}", user.id, added.len());
        let paths = crate::paths::Paths::discover();
        let client = crate::net::ytmusic::YtMusic::new(&paths).unwrap();
        let views = playlist_view_counts(client.http(), client.media_auth().as_ref(), &user.id).await.unwrap();
        println!("view counts for {}: {} sample={:?}", user.id, views.len(), views.iter().next());
    }

    #[test]
    fn view_counts_parse_suffixes() {
        assert_eq!(parse_view_count("36M views"), Some(36_000_000));
        assert_eq!(parse_view_count("1.8B views"), Some(1_800_000_000));
        assert_eq!(parse_view_count("1,234 views"), Some(1234));
        assert_eq!(parse_view_count("1 view"), Some(1));
        assert_eq!(parse_view_count("No views"), Some(0));
        assert_eq!(parse_view_count("Eminem"), None);
    }

    #[test]
    fn own_playlist_rules() {
        let mut d = PlaylistDetails { author: vec![Person { name: "Me".into(), id: None }], ..Default::default() };
        assert!(is_own_playlist(&d, "PLabc", Some("Me")));
        assert!(!is_own_playlist(&d, "PLabc", Some("Other")));
        assert!(!is_own_playlist(&d, "LM", Some("Me")));
        d.author.clear();
        assert!(is_own_playlist(&d, "VLPLx", Some("Me")));
        assert!(!is_own_playlist(&d, "RDAMPLx", Some("Me")));
    }

    #[test]
    fn editable_playlists_filter_by_author() {
        let mine = MediaItem { kind: ItemKind::Playlist, id: "PL1".into(), artists: vec![Person { name: "Mohamad Obeid".into(), id: None }], ..Default::default() };
        let theirs = MediaItem { kind: ItemKind::Playlist, id: "PL2".into(), artists: vec![Person { name: "Someone".into(), id: None }], ..Default::default() };
        let liked = MediaItem { kind: ItemKind::Playlist, id: "LM".into(), ..Default::default() };
        let out = editable_playlists(&[mine, theirs, liked], Some("Mohamad Obeid"));
        assert_eq!(out.len(), 1);
        assert_eq!(out[0].id, "PL1");
    }
}

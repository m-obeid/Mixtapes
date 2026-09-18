//! Port of MusicClient.get_artist on top of ytmusicapi's get_artist and
//! parse_channel_contents, plus the raw carousel scan and the deep fetches
//! the artist page did before rendering. Subscriptions ride the same
//! endpoints the Python client used.

use std::sync::Arc;
use std::time::Duration;

use serde_json::{Value, json};
use super::browse::Browse;

use crate::model::{ItemKind, MediaItem, Track};
use crate::net::items::*;
use crate::net::library::parse_two_row;
use crate::net::playlists;
use crate::net::ytmusic::NetError;

const SINGLE_SECTIONS: &str = "/contents/singleColumnBrowseResultsRenderer/tabs/0/tabRenderer/content/sectionListRenderer/contents";
const TWO_COLUMN_SECTIONS: &str = "/contents/twoColumnBrowseResultsRenderer/tabs/0/tabRenderer/content/sectionListRenderer/contents";
/// The page waited this long for the deep fetches before rendering.
const DEEP_FETCH_TIMEOUT: Duration = Duration::from_secs(10);
/// Albums and singles the artist page asks for. The discography page fetches the rest.
const ARTIST_PAGE_ALBUMS: usize = 10;

/// A carousel of cards with the "View All" pointer, what `{"results", "browseId", "params"}` was.
#[derive(Debug, Clone, Default)]
pub struct CardSection {
    pub browse_id: Option<String>,
    pub params: Option<String>,
    pub results: Vec<MediaItem>,
}

/// The top songs shelf: rows plus the playlist behind "View All".
#[derive(Debug, Clone, Default)]
pub struct SongSection {
    pub browse_id: Option<String>,
    pub results: Vec<Track>,
}

#[derive(Debug, Clone, Default)]
pub struct ArtistData {
    pub name: String,
    pub description: Option<String>,
    pub views: Option<String>,
    /// The id the subscribe endpoints take, which differs from the browse id.
    pub channel_id: Option<String>,
    pub shuffle_id: Option<String>,
    pub radio_id: Option<String>,
    pub subscribers: Option<String>,
    pub monthly_listeners: Option<String>,
    pub subscribed: bool,
    pub thumbnails: Vec<String>,
    pub banner: Vec<String>,
    /// A user channel rather than an artist: no songs, no play buttons.
    pub is_channel: bool,
    pub songs: Option<SongSection>,
    pub albums: Option<CardSection>,
    pub singles: Option<CardSection>,
    pub videos: Option<CardSection>,
    pub playlists: Option<CardSection>,
    pub featured_on: Option<CardSection>,
    pub related: Option<CardSection>,
}

fn sections(response: &Value) -> &[Value] {
    let single = array_at(response, SINGLE_SECTIONS);
    if single.is_empty() { array_at(response, TWO_COLUMN_SECTIONS) } else { single }
}

fn carousel_title_run(carousel: &Value) -> Option<&Value> {
    carousel.pointer("/header/musicCarouselShelfBasicHeaderRenderer/title/runs/0")
}

fn dot_separator_index(runs: &[Value]) -> usize {
    runs.iter().position(|r| r.get("text").and_then(Value::as_str) == Some(" • ")).unwrap_or(runs.len())
}

/// Port of parse_video for a `musicTwoRowItemRenderer`.
fn parse_video(data: &Value) -> Option<MediaItem> {
    let runs = array_at(data, "/subtitle/runs");
    let artists_len = dot_separator_index(runs);
    let video_id = owned_at(data, "/navigationEndpoint/watchEndpoint/videoId").or_else(|| array_at(data, "/menu/menuRenderer/items").iter().find_map(|e| owned_at(e, "/menuServiceItemRenderer/serviceEndpoint/queueAddEndpoint/queueTarget/videoId")))?;
    let artists = parse_artists_runs(&runs[..artists_len]);
    Some(MediaItem {
        kind: ItemKind::Video,
        id: video_id,
        title: owned_at(data, "/title/runs/0/text").unwrap_or_default(),
        artists,
        thumb: last_thumbnail_url(array_at(data, "/thumbnailRenderer/musicThumbnailRenderer/thumbnail/thumbnails")),
        views: runs.last().and_then(|r| r.get("text")).and_then(Value::as_str).and_then(|t| t.split(' ').next()).map(str::to_owned),
        ..MediaItem::default()
    })
}

/// Port of parse_related_artist.
fn parse_related_artist(data: &Value) -> Option<MediaItem> {
    Some(MediaItem {
        kind: ItemKind::Artist,
        id: owned_at(data, "/title/runs/0/navigationEndpoint/browseEndpoint/browseId")?,
        title: owned_at(data, "/title/runs/0/text").unwrap_or_default(),
        subscribers: str_at(data, "/subtitle/runs/0/text").and_then(|t| t.split(' ').next()).map(str::to_owned),
        thumb: last_thumbnail_url(array_at(data, "/thumbnailRenderer/musicThumbnailRenderer/thumbnail/thumbnails")),
        ..MediaItem::default()
    })
}

#[derive(Clone, Copy)]
enum Category {
    Albums,
    Singles,
    Videos,
    Playlists,
    Related,
}

impl Category {
    fn title(self) -> &'static str {
        match self {
            Category::Albums => "albums",
            Category::Singles => "singles & eps",
            Category::Videos => "videos",
            Category::Playlists => "playlists",
            // ytmusicapi's English locale names the carousel this way.
            Category::Related => "fans might also like",
        }
    }

    fn parse(self, entry: &Value) -> Option<MediaItem> {
        let data = entry.get(MTRIR)?;
        match self {
            Category::Albums | Category::Singles => parse_two_row(entry, ItemKind::Album),
            Category::Videos => parse_video(data),
            Category::Playlists => parse_two_row(entry, ItemKind::Playlist),
            Category::Related => parse_related_artist(data),
        }
    }
}

/// Port of parse_channel_contents for one category: the carousel whose title matches.
fn parse_category(sections: &[Value], category: Category) -> Option<CardSection> {
    let carousel = sections.iter().filter_map(|s| s.get("musicCarouselShelfRenderer")).find(|c| carousel_title_run(c).and_then(|r| r.get("text")).and_then(Value::as_str).is_some_and(|t| t.to_lowercase() == category.title()))?;
    let title_run = carousel_title_run(carousel);
    let browse_id = title_run.and_then(|r| owned_at(r, "/navigationEndpoint/browseEndpoint/browseId"));
    let params = title_run.and_then(|r| owned_at(r, "/navigationEndpoint/browseEndpoint/params"));
    let results = array_at(carousel, "/contents").iter().filter_map(|e| category.parse(e)).collect();
    Some(CardSection { browse_id, params, results })
}

/// The raw scan the page did for carousels ytmusicapi skips: any carousel
/// whose title mentions `needle`, read with the best-effort item parser.
fn scan_carousel(sections: &[Value], needle: &str) -> Option<CardSection> {
    for section in sections {
        let Some(carousel) = section.get("musicCarouselShelfRenderer") else { continue };
        let mut title = String::new();
        let mut browse_id = None;
        let mut params = None;
        for header in carousel.get("header").and_then(Value::as_object).map(|h| h.values().collect::<Vec<_>>()).unwrap_or_default() {
            if let Some(t) = str_at(header, "/title/runs/0/text") {
                title = t.to_owned();
            }
            if let Some(id) = owned_at(header, "/moreContentButton/buttonRenderer/navigationEndpoint/browseEndpoint/browseId") {
                browse_id = Some(id);
                params = owned_at(header, "/moreContentButton/buttonRenderer/navigationEndpoint/browseEndpoint/params");
            }
        }
        if title.is_empty() || !title.to_lowercase().contains(needle) {
            continue;
        }
        let results: Vec<MediaItem> = array_at(carousel, "/contents").iter().filter_map(parse_channel_item).collect();
        if !results.is_empty() {
            return Some(CardSection { browse_id, params, results });
        }
    }
    None
}

fn parse_artist_page(response: &Value) -> Option<ArtistData> {
    let header = response.pointer("/header/musicImmersiveHeaderRenderer")?;
    let sections = sections(response);
    let mut artist = ArtistData { name: owned_at(header, "/title/runs/0/text").unwrap_or_default(), ..ArtistData::default() };
    if let Some(shelf) = walk_key(response, "musicDescriptionShelfRenderer").into_iter().next() {
        let description = runs_text(array_at(shelf, "/description/runs"));
        if !description.is_empty() {
            artist.description = Some(description);
        }
        artist.views = owned_at(shelf, "/subheader/runs/0/text");
    }
    let subscription = header.pointer("/subscriptionButton/subscribeButtonRenderer");
    artist.channel_id = subscription.and_then(|s| owned_at(s, "/channelId"));
    artist.subscribers = subscription.and_then(|s| owned_at(s, "/subscriberCountText/runs/0/text"));
    artist.subscribed = subscription.and_then(|s| s.get("subscribed")).and_then(Value::as_bool).unwrap_or(false);
    artist.shuffle_id = owned_at(header, "/playButton/buttonRenderer/navigationEndpoint/watchEndpoint/playlistId");
    artist.radio_id = owned_at(header, "/startRadioButton/buttonRenderer/navigationEndpoint/watchEndpoint/playlistId");
    artist.monthly_listeners = owned_at(header, "/monthlyListenerCount/runs/0/text").map(|t| t.replace(" monthly audience", ""));
    artist.thumbnails = thumbnails_at(header, "/thumbnail/musicThumbnailRenderer/thumbnail/thumbnails");
    if let Some(shelf) = sections.first().and_then(|s| s.get("musicShelfRenderer")) {
        artist.songs = Some(SongSection { browse_id: owned_at(shelf, "/title/runs/0/navigationEndpoint/browseEndpoint/browseId"), results: parse_playlist_items(array_at(shelf, "/contents"), false, false) });
    }
    artist.albums = parse_category(sections, Category::Albums);
    artist.singles = parse_category(sections, Category::Singles);
    artist.videos = parse_category(sections, Category::Videos);
    artist.playlists = parse_category(sections, Category::Playlists);
    artist.related = parse_category(sections, Category::Related);
    if artist.playlists.is_none() {
        artist.playlists = scan_carousel(sections, "playlist");
    }
    artist.featured_on = scan_carousel(sections, "featured");
    Some(artist)
}

/// Port of the get_user fallback for plain channels, normalized like the
/// Python client did: `_is_channel`, avatar and banner off the visual header.
fn parse_channel_page(response: &Value) -> Option<ArtistData> {
    let sections = sections(response);
    let mut artist = ArtistData { is_channel: true, subscribers: Some(String::new()), ..ArtistData::default() };
    for key in ["musicVisualHeaderRenderer", "musicImmersiveHeaderRenderer"] {
        let Some(header) = response.pointer(&format!("/header/{key}")) else { continue };
        artist.name = owned_at(header, "/title/runs/0/text").unwrap_or_default();
        artist.thumbnails = thumbnails_at(header, "/foregroundThumbnail/musicThumbnailRenderer/thumbnail/thumbnails");
        artist.banner = thumbnails_at(header, "/thumbnail/musicThumbnailRenderer/thumbnail/thumbnails");
        if let Some(count) = owned_at(header, "/subscriptionButton/subscribeButtonRenderer/subscriberCountText/runs/0/text") {
            artist.subscribers = Some(count);
        }
        break;
    }
    if artist.name.is_empty() {
        return None;
    }
    artist.videos = parse_category(sections, Category::Videos);
    artist.playlists = parse_category(sections, Category::Playlists).or_else(|| scan_carousel(sections, "playlist"));
    if artist.thumbnails.is_empty() {
        for section in [&artist.playlists, &artist.videos].into_iter().flatten() {
            if let Some(thumb) = section.results.first().and_then(|i| i.thumb.clone()) {
                artist.thumbnails = vec![thumb];
                break;
            }
        }
    }
    Some(artist)
}

/// Port of MusicClient.get_artist plus ArtistPage._fetch_artist: the page
/// data with the deep fetches merged in (full top songs, detailed albums
/// and singles), each bounded like the joined worker threads were.
pub async fn get_artist(api: Arc<dyn Browse>, channel_id: &str) -> Result<ArtistData, NetError> {
    let channel_id = channel_id.strip_prefix("MPLA").unwrap_or(channel_id).to_owned();
    let response = api.post("browse", json!({ "browseId": channel_id })).await?;
    let mut artist = match parse_artist_page(&response) {
        Some(a) => a,
        None => parse_channel_page(&response).ok_or_else(|| NetError::Message(format!("artist {channel_id}: no header")))?,
    };
    if artist.is_channel {
        return Ok(artist);
    }
    let songs_browse = artist.songs.as_ref().and_then(|s| s.browse_id.clone());
    let albums_ptr = artist.albums.as_ref().and_then(|s| Some((s.browse_id.clone()?, s.params.clone())));
    let singles_ptr = artist.singles.as_ref().and_then(|s| Some((s.browse_id.clone()?, s.params.clone())));
    let (songs, albums, singles) = tokio::join!(
        async {
            match songs_browse {
                Some(id) => tokio::time::timeout(DEEP_FETCH_TIMEOUT, playlists::get_playlist(&api, &id, Some(100))).await.ok().and_then(Result::ok).map(|p| p.tracks).filter(|t| !t.is_empty()),
                None => None,
            }
        },
        async {
            match albums_ptr {
                Some((id, params)) => tokio::time::timeout(DEEP_FETCH_TIMEOUT, playlists::artist_albums(&api, &id, params.as_deref(), Some(ARTIST_PAGE_ALBUMS))).await.ok().and_then(Result::ok).filter(|a| !a.is_empty()),
                None => None,
            }
        },
        async {
            match singles_ptr {
                Some((id, params)) => tokio::time::timeout(DEEP_FETCH_TIMEOUT, playlists::artist_albums(&api, &id, params.as_deref(), Some(ARTIST_PAGE_ALBUMS))).await.ok().and_then(Result::ok).filter(|a| !a.is_empty()),
                None => None,
            }
        }
    );
    if let (Some(tracks), Some(section)) = (songs, artist.songs.as_mut()) {
        section.results = tracks;
    }
    if let (Some(items), Some(section)) = (albums, artist.albums.as_mut()) {
        section.results = items;
    }
    if let (Some(items), Some(section)) = (singles, artist.singles.as_mut()) {
        section.results = items;
    }
    Ok(artist)
}

/// Port of _resolve_artist_from_player's lookup: the channel behind a video, from the player response.
pub async fn channel_of_video(api: &dyn Browse, video_id: &str) -> Result<Option<(String, String)>, NetError> {
    let response = api.post("player", json!({ "videoId": video_id })).await?;
    let channel = owned_at(&response, "/videoDetails/channelId");
    let author = owned_at(&response, "/videoDetails/author").unwrap_or_else(|| "Artist".to_owned());
    Ok(channel.map(|c| (c, author)))
}

/// Port of resolve_channel_handle: turn the account's @handle into the
/// channel it opens, which is what "Your Channel" needs before it can push an
/// artist page.
pub async fn resolve_handle(api: &dyn Browse, handle: &str) -> Result<Option<String>, NetError> {
    let handle = handle.trim_start_matches('@');
    if handle.is_empty() {
        return Ok(None);
    }
    let body = json!({ "url": format!("https://music.youtube.com/@{handle}") });
    let response = api.post("navigation/resolve_url", body).await?;
    // The key nesting has moved between YouTube revisions; try both spellings.
    Ok(owned_at(&response, "/endpoint/browseEndpoint/browseId").or_else(|| owned_at(&response, "/endpoint/browse/browseId")))
}

pub async fn subscribe(api: &dyn Browse, channel_id: &str) -> Result<(), NetError> {
    api.post("subscription/subscribe", json!({ "channelIds": [channel_id] })).await?;
    Ok(())
}

pub async fn unsubscribe(api: &dyn Browse, channel_id: &str) -> Result<(), NetError> {
    api.post("subscription/unsubscribe", json!({ "channelIds": [channel_id] })).await?;
    Ok(())
}


#[cfg(test)]
mod tests {
    use super::*;

    /// An artist with a long singles list. The page asks for ten and must not wait for the rest.
    #[tokio::test]
    #[ignore]
    async fn live_prolific_artist_loads_quickly() {
        let paths = crate::paths::Paths::discover();
        let api = crate::net::ytmusic::YtMusic::new(&paths).unwrap().api();
        let started = std::time::Instant::now();
        let a = get_artist(api, "UCcMjgANx-HTMUCpPJuFlmag").await.unwrap();
        let took = started.elapsed();
        println!("{} in {took:?}: songs={} albums={} singles={}", a.name, a.songs.as_ref().map_or(0, |s| s.results.len()), a.albums.as_ref().map_or(0, |s| s.results.len()), a.singles.as_ref().map_or(0, |s| s.results.len()));
        assert!(took < Duration::from_secs(5), "took {took:?}");
        assert!(a.singles.as_ref().is_some_and(|s| !s.results.is_empty() && s.results.len() <= ARTIST_PAGE_ALBUMS));
    }

    #[tokio::test]
    #[ignore]
    async fn live_artist() {
        let paths = crate::paths::Paths::discover();
        let api = crate::net::ytmusic::YtMusic::new(&paths).unwrap().api();
        let subs = crate::net::library::library_subscriptions(api.clone()).await.unwrap();
        let first = subs.first().unwrap();
        println!("subscription {} {}", first.id, first.title);
        let a = get_artist(api, &first.id).await.unwrap();
        println!("name={} subs={:?} views={:?} radio={:?} shuffle={:?} subscribed={} thumbs={} desc={}", a.name, a.subscribers, a.views, a.radio_id, a.shuffle_id, a.subscribed, a.thumbnails.len(), a.description.as_deref().map(|d| d.len()).unwrap_or(0));
        for (label, section) in [("albums", &a.albums), ("singles", &a.singles), ("videos", &a.videos), ("playlists", &a.playlists), ("featured", &a.featured_on), ("related", &a.related)] {
            if let Some(s) = section {
                println!("{label}: {} items browse={:?} params={} first={:?}", s.results.len(), s.browse_id, s.params.is_some(), s.results.first().map(|i| (&i.title, &i.id)));
            }
        }
        if let Some(s) = &a.songs {
            println!("songs: {} browse={:?} first={:?}", s.results.len(), s.browse_id, s.results.first().map(|t| (&t.title, &t.artist)));
        }
        assert!(!a.name.is_empty());
    }
}

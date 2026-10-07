//! Explore endpoints: the feed, the mood and genre categories, the charts, and
//! the page behind a category pill. Ports of ytmusicapi's `get_explore`,
//! `get_mood_categories` and `get_charts` plus MusicClient.get_category_page,
//! kept row for row so the page sees what the Python page saw.

use std::sync::Arc;

use serde_json::{Value, json};

use super::browse::{Browse, Continuation};
use super::items::{MRLIR, MTRIR, THUMBNAIL_RENDERER, THUMBNAILS, array_at, detect_kind, is_year, last_thumbnail_url, owned_at, parse_album_card, parse_song_row, parse_video_card, str_at};
use crate::model::{ItemKind, MediaItem, Person};
use crate::net::ytmusic::NetError;

/// Where every browse response in this module keeps its shelves.
const SECTIONS: &str = "/contents/singleColumnBrowseResultsRenderer/tabs/0/tabRenderer/content/sectionListRenderer/contents";
/// The browse id on a carousel's own heading, which is what names the shelf.
const CAROUSEL_BROWSE_ID: &str = "/musicCarouselShelfRenderer/header/musicCarouselShelfBasicHeaderRenderer/title/runs/0/navigationEndpoint/browseEndpoint/browseId";
const CAROUSEL_TITLE: &str = "/header/musicCarouselShelfBasicHeaderRenderer/title/runs/0/text";
const CUSTOM_INDEX: &str = "/customIndexColumn/musicCustomIndexColumnRenderer";

/// One mood or genre pill: what it says and the params that open its page.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Category {
    pub title: String,
    pub params: String,
}

/// What `FEmusic_explore` returns. Podcast shelves and the premium-only top
/// songs chart are dropped: the Python page never drew them.
#[derive(Clone, Debug, Default)]
pub struct ExploreFeed {
    pub new_releases: Vec<MediaItem>,
    pub new_videos: Vec<MediaItem>,
    pub trending: Vec<MediaItem>,
    /// The feed's own pill row, shown when the categories call failed.
    pub moods_and_genres: Vec<Category>,
}

/// Which way a charted artist moved since the last ranking.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub enum Trend {
    Up,
    Down,
    #[default]
    Neutral,
}

/// A charted artist: the artist, plus where the chart put them.
/// Rank and trend are absent for an unauthenticated request.
#[derive(Clone, Debug)]
pub struct ChartArtist {
    pub item: MediaItem,
    pub rank: Option<String>,
    pub trend: Trend,
}

#[derive(Clone, Debug, Default)]
pub struct Charts {
    /// Country codes the chart menu offers, in the order YouTube listed them.
    pub countries: Vec<String>,
    pub videos: Vec<MediaItem>,
    /// A premium account gets daily and weekly carousels in place of `videos`.
    pub daily: Vec<MediaItem>,
    pub weekly: Vec<MediaItem>,
    /// US charts only.
    pub genres: Vec<MediaItem>,
    pub artists: Vec<ChartArtist>,
}

/// Everything the Explore page draws.
#[derive(Clone, Debug, Default)]
pub struct ExploreData {
    pub feed: ExploreFeed,
    /// The account's own row, which YouTube puts ahead of the other two.
    pub for_you: Vec<Category>,
    pub moods: Vec<Category>,
    pub genres: Vec<Category>,
    pub charts: Option<Charts>,
}

/// One carousel of a category page.
#[derive(Clone, Debug)]
pub struct CategorySection {
    pub title: String,
    pub items: Vec<MediaItem>,
}

// -- endpoints ------------------------------------------------------------

/// Port of _fetch_explore. The feed decides whether the page has anything to
/// show; the categories and the charts ride alongside it and are dropped if
/// they fail, exactly as the Python threads did.
pub async fn load_explore(api: Arc<dyn Browse>, country: String) -> Result<ExploreData, NetError> {
    let categories = {
        let api = api.clone();
        tokio::spawn(async move { get_mood_categories(&api).await })
    };
    let charts = {
        let api = api.clone();
        tokio::spawn(async move { get_charts(&api, &country).await })
    };

    let mut data = ExploreData { feed: get_explore(&api).await?, ..ExploreData::default() };
    match categories.await {
        Ok(Ok(sections)) => {
            data.for_you = section(&sections, "For you");
            data.moods = section(&sections, "Moods & moments");
            data.genres = section(&sections, "Genres");
        }
        Ok(Err(err)) => tracing::warn!(%err, "mood categories failed"),
        Err(_) => {}
    }
    match charts.await {
        Ok(Ok(charts)) => data.charts = Some(charts),
        Ok(Err(err)) => tracing::warn!(%err, "charts failed"),
        Err(_) => {}
    }
    Ok(data)
}

pub async fn get_explore(api: &dyn Browse) -> Result<ExploreFeed, NetError> {
    let response = api.post("browse", json!({ "browseId": "FEmusic_explore" })).await?;
    Ok(parse_explore(&response))
}

/// The "Moods & moments", "Genres" and "For you" grids, in response order.
pub async fn get_mood_categories(api: &dyn Browse) -> Result<Vec<(String, Vec<Category>)>, NetError> {
    let response = api.post("browse", json!({ "browseId": "FEmusic_moods_and_genres" })).await?;
    Ok(parse_mood_categories(&response))
}

/// Charts for one country. An empty code leaves the choice to YouTube.
pub async fn get_charts(api: &dyn Browse, country: &str) -> Result<Charts, NetError> {
    let mut body = json!({ "browseId": "FEmusic_charts" });
    if !country.is_empty() {
        body["formData"] = json!({ "selectedValues": [country] });
    }
    let response = api.post("browse", body).await?;
    Ok(parse_charts(&response, country))
}

/// The carousels behind one mood or genre pill.
pub async fn get_category_page(api: &dyn Browse, params: &str) -> Result<Vec<CategorySection>, NetError> {
    let body = json!({ "browseId": "FEmusic_moods_and_genres_category", "params": params });
    let response = api.post("browse", body).await?;
    Ok(parse_category_page(&response))
}

/// The pills of one named grid, empty when the grid is not in the response.
pub fn section(sections: &[(String, Vec<Category>)], name: &str) -> Vec<Category> {
    sections.iter().find(|(title, _)| title == name).map(|(_, items)| items.clone()).unwrap_or_default()
}

// -- parsing --------------------------------------------------------------

pub fn parse_explore(response: &Value) -> ExploreFeed {
    let mut feed = ExploreFeed::default();
    for shelf in array_at(response, SECTIONS) {
        let Some(browse_id) = str_at(shelf, CAROUSEL_BROWSE_ID) else { continue };
        let contents = array_at(shelf, "/musicCarouselShelfRenderer/contents");
        match browse_id {
            "FEmusic_new_releases_albums" => feed.new_releases = contents.iter().filter_map(|c| c.get(MTRIR)).map(parse_album_card).collect(),
            "FEmusic_new_releases_videos" => feed.new_videos = contents.iter().filter_map(|c| c.get(MTRIR)).filter_map(parse_video_card).collect(),
            "FEmusic_moods_and_genres" => feed.moods_and_genres = contents.iter().filter_map(parse_category).collect(),
            // The trending shelf is addressed by the playlist behind it.
            // Trending is a music chart. An episode that slips in stays out of it.
            id if id.starts_with("VLOLA") => feed.trending = contents.iter().filter_map(|c| c.get(MRLIR)).filter_map(|row| parse_song_row(row, "Trending")).filter(|item| item.item_type.as_deref() != Some("Episode")).collect(),
            _ => {}
        }
    }
    feed
}

pub fn parse_mood_categories(response: &Value) -> Vec<(String, Vec<Category>)> {
    array_at(response, SECTIONS)
        .iter()
        .filter_map(|shelf| {
            let title = owned_at(shelf, "/gridRenderer/header/gridHeaderRenderer/title/runs/0/text")?;
            Some((title, array_at(shelf, "/gridRenderer/items").iter().filter_map(parse_category).collect()))
        })
        .collect()
}

pub fn parse_charts(response: &Value, country: &str) -> Charts {
    let shelves = array_at(response, SECTIONS);
    let mut charts = Charts {
        countries: array_at(response, "/frameworkUpdates/entityBatchUpdate/mutations").iter().filter_map(|m| owned_at(m, "/payload/musicFormBooleanChoice/opaqueToken")).collect(),
        ..Charts::default()
    };

    // Shelf order, after the country menu in the first shelf: the video
    // playlists, a genre row on US only, then the artists. A premium account
    // gets daily and weekly playlists in place of the one video row, which is
    // the extra shelf this counts.
    let mut names: Vec<&str> = vec!["videos"];
    if country == "US" {
        names.push("genres");
    }
    names.push("artists");
    if shelves.len().saturating_sub(1) > names.len() {
        names.splice(0..1, ["daily", "weekly"]);
    }

    for (i, name) in names.iter().enumerate() {
        let Some(shelf) = shelves.get(i + 1) else { continue };
        let contents = array_at(shelf, "/musicCarouselShelfRenderer/contents");
        if *name == "artists" {
            charts.artists = contents.iter().filter_map(|c| c.get(MRLIR)).map(parse_chart_artist).collect();
            continue;
        }
        let playlists: Vec<MediaItem> = contents.iter().filter_map(|c| c.get(MTRIR)).filter_map(parse_chart_playlist).collect();
        match *name {
            "daily" => charts.daily = playlists,
            "weekly" => charts.weekly = playlists,
            "genres" => charts.genres = playlists,
            _ => charts.videos = playlists,
        }
    }
    charts
}

pub fn parse_category_page(response: &Value) -> Vec<CategorySection> {
    array_at(response, SECTIONS).iter().filter_map(parse_category_shelf).collect()
}

fn parse_category_shelf(shelf: &Value) -> Option<CategorySection> {
    let carousel = shelf.get("musicCarouselShelfRenderer")?;
    let title = owned_at(carousel, CAROUSEL_TITLE)?;
    let items: Vec<MediaItem> = array_at(carousel, "/contents").iter().filter_map(|entry| parse_category_item(entry, &title)).collect();
    (!items.is_empty()).then_some(CategorySection { title, items })
}

/// What the category page is handed to show podcasts instead of a mood.
pub const PODCASTS_KEY: &str = "podcasts";
/// Home's Podcasts chip as read on 2026-09-27, for when the chip is not found by name.
const PODCASTS_CHIP: &str = "ggNCSgQIDBADSgQIBxABSgQICRABSgQICBABSgQIDhABSgQIBBABSgQIDRABSgQIAxABSgQIChABSgQIBhABSgQIBRAB";
/// Shelves of the podcasts feed, paged in the way Home pages its own.
const PODCAST_SHELVES: usize = 20;

/// The podcasts feed YouTube Music shows behind Home's Podcasts chip: shelves
/// of episodes and shows by topic. The chip's params come from Home itself,
/// so a change on YouTube's side is followed. The saved params are the fallback.
pub async fn get_podcasts_page(api: &dyn Browse) -> Result<Vec<CategorySection>, NetError> {
    let home = api.post("browse", json!({ "browseId": "FEmusic_home" })).await?;
    let chips = array_at(&home, "/contents/singleColumnBrowseResultsRenderer/tabs/0/tabRenderer/content/sectionListRenderer/header/chipCloudRenderer/chips");
    let params = chips
        .iter()
        .find(|chip| str_at(chip, "/chipCloudChipRenderer/text/runs/0/text").is_some_and(|t| t.to_lowercase().contains("podcast")))
        .and_then(|chip| owned_at(chip, "/chipCloudChipRenderer/navigationEndpoint/browseEndpoint/params"))
        .unwrap_or_else(|| PODCASTS_CHIP.to_owned());
    let response = api.post("browse", json!({ "browseId": "FEmusic_home", "params": params })).await?;
    let mut sections = parse_category_page(&response);
    let token = owned_at(&response, "/contents/singleColumnBrowseResultsRenderer/tabs/0/tabRenderer/content/sectionListRenderer/continuations/0/nextContinuationData/continuation");
    let paged = Continuation::browse(api, token).limit(PODCAST_SHELVES.saturating_sub(sections.len())).collect(|page| page.iter().filter_map(|shelf| parse_category_shelf(shelf)).collect()).await;
    sections.extend(paged.items);
    Ok(sections)
}

/// Port of MusicClient.get_category_page's per-item read: the title, the one
/// endpoint the item carries, its picture, and only the runs that point at an
/// artist. A view count sits in the same column and is not one.
fn parse_category_item(entry: &Value, section_title: &str) -> Option<MediaItem> {
    if let Some(data) = entry.get(super::items::MMRLIR) {
        return super::items::parse_episode_card(data);
    }
    let (renderer, title, endpoint, thumbs, subtitle_runs) = match (entry.get(MRLIR), entry.get(MTRIR)) {
        (Some(r), _) => {
            let column = "/flexColumns/0/musicResponsiveListItemFlexColumnRenderer/text/runs/0";
            let endpoint = r.pointer("/navigationEndpoint").or_else(|| r.pointer(&format!("{column}/navigationEndpoint")));
            (r, owned_at(r, &format!("{column}/text"))?, endpoint, THUMBNAILS, "/flexColumns/1/musicResponsiveListItemFlexColumnRenderer/text/runs")
        }
        (None, Some(r)) => {
            let endpoint = r.pointer("/navigationEndpoint").or_else(|| r.pointer("/title/runs/0/navigationEndpoint"));
            (r, owned_at(r, "/title/runs/0/text")?, endpoint, THUMBNAIL_RENDERER, "/subtitle/runs")
        }
        (None, None) => return None,
    };

    let mut item = MediaItem { title, thumb: last_thumbnail_url(array_at(renderer, thumbs)), ..MediaItem::default() };
    for run in array_at(renderer, subtitle_runs) {
        match str_at(run, "/navigationEndpoint/browseEndpoint/browseEndpointContextSupportedConfigs/browseEndpointContextMusicConfig/pageType") {
            Some("MUSIC_PAGE_TYPE_ARTIST") => item.artists.push(Person { name: str_at(run, "/text").unwrap_or_default().to_owned(), id: owned_at(run, "/navigationEndpoint/browseEndpoint/browseId") }),
            Some(_) => {}
            None => {
                let text = str_at(run, "/text").unwrap_or_default().trim();
                if is_year(text) {
                    item.year = Some(text.to_owned());
                } else if ["Album", "Single", "EP", "Playlist"].contains(&text) {
                    item.item_type = Some(text.to_owned());
                }
            }
        }
    }

    // What the item opens decides what it is, the way _detect_kind reads it.
    let endpoint = endpoint?;
    if let Some(video_id) = owned_at(endpoint, "/watchEndpoint/videoId") {
        let video_type = str_at(endpoint, "/watchEndpoint/watchEndpointMusicSupportedConfigs/watchEndpointMusicConfig/musicVideoType");
        item.kind = detect_kind(video_type, item.thumb.as_deref(), section_title, &item)?;
        item.id = video_id;
    } else {
        let browse_id = owned_at(endpoint, "/browseEndpoint/browseId")?;
        item.kind = if browse_id.starts_with("MPRE") {
            ItemKind::Album
        } else if browse_id.starts_with("UC") {
            ItemKind::Artist
        } else {
            ItemKind::Playlist
        };
        if browse_id.starts_with("MPSP") {
            item.item_type = Some("Podcast".to_owned());
        }
        item.id = if item.kind == ItemKind::Playlist { browse_id.trim_start_matches("VL").to_owned() } else { browse_id };
    }
    Some(item)
}

fn parse_category(entry: &Value) -> Option<Category> {
    Some(Category {
        title: owned_at(entry, "/musicNavigationButtonRenderer/buttonText/runs/0/text")?,
        params: owned_at(entry, "/musicNavigationButtonRenderer/clickCommand/browseEndpoint/params")?,
    })
}

/// Port of parse_chart_playlist: a chart carousel card is a playlist and
/// nothing else, its browse id carrying the usual VL prefix.
fn parse_chart_playlist(data: &Value) -> Option<MediaItem> {
    let browse_id = str_at(data, "/title/runs/0/navigationEndpoint/browseEndpoint/browseId")?;
    Some(MediaItem {
        kind: ItemKind::Playlist,
        id: browse_id.trim_start_matches("VL").to_owned(),
        title: owned_at(data, "/title/runs/0/text").unwrap_or_default(),
        thumb: last_thumbnail_url(array_at(data, THUMBNAIL_RENDERER)),
        ..MediaItem::default()
    })
}

/// Port of parse_chart_artist plus parse_ranking.
fn parse_chart_artist(data: &Value) -> ChartArtist {
    let item = MediaItem {
        kind: ItemKind::Artist,
        id: owned_at(data, "/navigationEndpoint/browseEndpoint/browseId").unwrap_or_default(),
        title: owned_at(data, "/flexColumns/0/musicResponsiveListItemFlexColumnRenderer/text/runs/0/text").unwrap_or_default(),
        // "9.62M subscribers" is shown as the count alone.
        subscribers: str_at(data, "/flexColumns/1/musicResponsiveListItemFlexColumnRenderer/text/runs/0/text").map(|text| text.split(' ').next().unwrap_or_default().to_owned()),
        thumb: last_thumbnail_url(array_at(data, THUMBNAILS)),
        ..MediaItem::default()
    };
    ChartArtist {
        item,
        rank: owned_at(data, &format!("{CUSTOM_INDEX}/text/runs/0/text")),
        trend: match str_at(data, &format!("{CUSTOM_INDEX}/icon/iconType")) {
            Some("ARROW_DROP_UP") => Trend::Up,
            Some("ARROW_DROP_DOWN") => Trend::Down,
            _ => Trend::Neutral,
        },
    }
}

// -- country menu ---------------------------------------------------------

/// The country names search.py spells out for the chart menu. A code with no
/// name here shows as the code itself.
const COUNTRY_NAMES: &[(&str, &str)] = &[
    ("ZZ", tr_noop!("Global")), ("AR", tr_noop!("Argentina")), ("AU", tr_noop!("Australia")), ("AT", tr_noop!("Austria")),
    ("BE", tr_noop!("Belgium")), ("BO", tr_noop!("Bolivia")), ("BR", tr_noop!("Brazil")), ("CA", tr_noop!("Canada")),
    ("CL", tr_noop!("Chile")), ("CO", tr_noop!("Colombia")), ("CR", tr_noop!("Costa Rica")), ("CZ", tr_noop!("Czechia")),
    ("DK", tr_noop!("Denmark")), ("DO", tr_noop!("Dominican Republic")), ("EC", tr_noop!("Ecuador")),
    ("EG", tr_noop!("Egypt")), ("SV", tr_noop!("El Salvador")), ("EE", tr_noop!("Estonia")), ("FI", tr_noop!("Finland")),
    ("FR", tr_noop!("France")), ("DE", tr_noop!("Germany")), ("GT", tr_noop!("Guatemala")), ("HN", tr_noop!("Honduras")),
    ("HU", tr_noop!("Hungary")), ("IS", tr_noop!("Iceland")), ("IN", tr_noop!("India")), ("ID", tr_noop!("Indonesia")),
    ("IE", tr_noop!("Ireland")), ("IL", tr_noop!("Israel")), ("IT", tr_noop!("Italy")), ("JP", tr_noop!("Japan")),
    ("KE", tr_noop!("Kenya")), ("LU", tr_noop!("Luxembourg")), ("MX", tr_noop!("Mexico")), ("NL", tr_noop!("Netherlands")),
    ("NZ", tr_noop!("New Zealand")), ("NI", tr_noop!("Nicaragua")), ("NG", tr_noop!("Nigeria")), ("NO", tr_noop!("Norway")),
    ("PA", tr_noop!("Panama")), ("PY", tr_noop!("Paraguay")), ("PE", tr_noop!("Peru")), ("PH", tr_noop!("Philippines")),
    ("PL", tr_noop!("Poland")), ("PT", tr_noop!("Portugal")), ("RO", tr_noop!("Romania")), ("RU", tr_noop!("Russia")),
    ("SA", tr_noop!("Saudi Arabia")), ("RS", tr_noop!("Serbia")), ("ZA", tr_noop!("South Africa")),
    ("KR", tr_noop!("South Korea")), ("ES", tr_noop!("Spain")), ("SE", tr_noop!("Sweden")), ("CH", tr_noop!("Switzerland")),
    ("TZ", tr_noop!("Tanzania")), ("TR", tr_noop!("Turkey")), ("UG", tr_noop!("Uganda")), ("UA", tr_noop!("Ukraine")),
    ("AE", tr_noop!("UAE")), ("GB", tr_noop!("United Kingdom")), ("US", tr_noop!("United States")),
    ("UY", tr_noop!("Uruguay")), ("VE", tr_noop!("Venezuela")), ("VN", tr_noop!("Vietnam")), ("ZW", tr_noop!("Zimbabwe")),
];

/// Chart countries as (code, name), Global first and the rest by name.
pub fn country_options(codes: &[String]) -> Vec<(String, String)> {
    let mut options: Vec<(String, String)> = codes
        .iter()
        .map(|code| {
            let name = COUNTRY_NAMES.iter().find(|(c, _)| c == code).map(|(_, n)| crate::i18n::gettext(n)).unwrap_or_else(|| code.clone());
            (code.clone(), name)
        })
        .collect();
    options.sort_by(|a, b| {
        let key = |(code, name): &(String, String)| if code == "ZZ" { String::new() } else { name.clone() };
        key(a).cmp(&key(b))
    });
    options
}

#[cfg(test)]
mod tests {
    use super::*;

    fn carousel(browse_id: &str, contents: Value) -> Value {
        json!({ "musicCarouselShelfRenderer": {
            "header": { "musicCarouselShelfBasicHeaderRenderer": { "title": { "runs": [{ "text": "Shelf", "navigationEndpoint": { "browseEndpoint": { "browseId": browse_id } } }] } } },
            "contents": contents
        }})
    }

    fn feed(shelves: Vec<Value>) -> Value {
        json!({ "contents": { "singleColumnBrowseResultsRenderer": { "tabs": [{ "tabRenderer": { "content": { "sectionListRenderer": { "contents": shelves } } } }] } } })
    }

    #[test]
    fn an_album_card_keeps_its_type_year_and_audio_playlist() {
        let card = json!({ MTRIR: {
            "title": { "runs": [{ "text": "Hangang", "navigationEndpoint": { "browseEndpoint": { "browseId": "MPREb_rGl39ZNEl95" } } }] },
            "subtitle": { "runs": [{ "text": "Album" }, { "text": " \u{2022} " }, { "text": "2024" }, { "text": " \u{2022} " }, { "text": "Dept", "navigationEndpoint": { "browseEndpoint": { "browseId": "UCpo4" } } }] },
            "thumbnailRenderer": { "musicThumbnailRenderer": { "thumbnail": { "thumbnails": [{ "url": "small" }, { "url": "large" }] } } },
            "subtitleBadges": [{ "musicInlineBadgeRenderer": { "accessibilityData": { "accessibilityData": { "label": "Explicit" } } } }],
            "thumbnailOverlay": { "musicItemThumbnailOverlayRenderer": { "content": { "musicPlayButtonRenderer": { "playNavigationEndpoint": { "watchPlaylistEndpoint": { "playlistId": "OLAK5uy_m" } } } } } }
        }});
        let releases = parse_explore(&feed(vec![carousel("FEmusic_new_releases_albums", json!([card]))])).new_releases;
        let album = &releases[0];
        assert_eq!(album.kind, ItemKind::Album);
        assert_eq!(album.id, "MPREb_rGl39ZNEl95");
        assert_eq!(album.item_type.as_deref(), Some("Album"));
        assert_eq!(album.year.as_deref(), Some("2024"));
        assert_eq!(album.artists_text(), "Dept");
        assert_eq!(album.thumb.as_deref(), Some("large"));
        assert_eq!(album.playlist_id.as_deref(), Some("OLAK5uy_m"));
        assert!(album.explicit);
    }

    #[test]
    fn a_video_card_splits_artists_from_views() {
        let card = json!({ MTRIR: {
            "title": { "runs": [{ "text": "EVERY CHANCE I GET" }] },
            "subtitle": { "runs": [{ "text": "DJ Khaled", "navigationEndpoint": { "browseEndpoint": { "browseId": "UC0K" } } }, { "text": " \u{2022} " }, { "text": "46M views" }] },
            "navigationEndpoint": { "watchEndpoint": { "videoId": "BTivsHlVcGU" } },
            "thumbnailRenderer": { "musicThumbnailRenderer": { "thumbnail": { "thumbnails": [{ "url": "t" }] } } }
        }});
        let videos = parse_explore(&feed(vec![carousel("FEmusic_new_releases_videos", json!([card]))])).new_videos;
        assert_eq!(videos[0].kind, ItemKind::Video);
        assert_eq!(videos[0].id, "BTivsHlVcGU");
        assert_eq!(videos[0].artists_text(), "DJ Khaled");
        assert_eq!(videos[0].views.as_deref(), Some("46M"));
    }

    #[test]
    fn a_trending_row_drops_the_kind_word_and_podcast_episodes() {
        let row = |video_type: &str, title: &str| {
            json!({ MRLIR: {
                "flexColumns": [
                    { "musicResponsiveListItemFlexColumnRenderer": { "text": { "runs": [{ "text": title }] } } },
                    { "musicResponsiveListItemFlexColumnRenderer": { "text": { "runs": [{ "text": "Song" }, { "text": " \u{2022} " }, { "text": "BTS", "navigationEndpoint": { "browseEndpoint": { "browseId": "UC9v" } } }] } } }
                ],
                "thumbnail": { "musicThumbnailRenderer": { "thumbnail": { "thumbnails": [{ "url": "t" }] } } },
                "overlay": { "musicItemThumbnailOverlayRenderer": { "content": { "musicPlayButtonRenderer": { "playNavigationEndpoint": { "watchEndpoint": {
                    "videoId": "CuklIb9d3fI",
                    "watchEndpointMusicSupportedConfigs": { "watchEndpointMusicConfig": { "musicVideoType": video_type } }
                }}}}}}
            }})
        };
        let contents = json!([row("MUSIC_VIDEO_TYPE_ATV", "Permission to Dance"), row("MUSIC_VIDEO_TYPE_PODCAST_EPISODE", "An episode")]);
        let trending = parse_explore(&feed(vec![carousel("VLOLAK5uy_k", contents)])).trending;
        assert_eq!(trending.len(), 1, "the podcast episode is dropped");
        assert_eq!(trending[0].kind, ItemKind::Song);
        assert_eq!(trending[0].artists_text(), "BTS", "the leading \"Song\" run is not an artist");
    }

    #[test]
    fn mood_grids_come_back_by_name() {
        let grid = |title: &str, pill: &str, params: &str| {
            json!({ "gridRenderer": {
                "header": { "gridHeaderRenderer": { "title": { "runs": [{ "text": title }] } } },
                "items": [{ "musicNavigationButtonRenderer": { "buttonText": { "runs": [{ "text": pill }] }, "clickCommand": { "browseEndpoint": { "params": params } } } }]
            }})
        };
        let response = feed(vec![grid("Moods & moments", "Chill", "p1"), grid("Genres", "Pop", "p2")]);
        let sections = parse_mood_categories(&response);
        assert_eq!(section(&sections, "Genres"), vec![Category { title: "Pop".into(), params: "p2".into() }]);
        assert_eq!(section(&sections, "Moods & moments")[0].title, "Chill");
        assert!(section(&sections, "For you").is_empty());
    }

    #[test]
    fn charts_read_the_country_menu_and_every_shelf() {
        let playlist = json!({ MTRIR: {
            "title": { "runs": [{ "text": "Daily Top Music Videos", "navigationEndpoint": { "browseEndpoint": { "browseId": "VLPL4fGSI1pDJn" } } }] },
            "thumbnailRenderer": { "musicThumbnailRenderer": { "thumbnail": { "thumbnails": [{ "url": "t" }] } } }
        }});
        let artist = json!({ MRLIR: {
            "flexColumns": [
                { "musicResponsiveListItemFlexColumnRenderer": { "text": { "runs": [{ "text": "YoungBoy" }] } } },
                { "musicResponsiveListItemFlexColumnRenderer": { "text": { "runs": [{ "text": "9.62M subscribers" }] } } }
            ],
            "navigationEndpoint": { "browseEndpoint": { "browseId": "UCR28" } },
            "thumbnail": { "musicThumbnailRenderer": { "thumbnail": { "thumbnails": [{ "url": "a" }] } } },
            "customIndexColumn": { "musicCustomIndexColumnRenderer": { "text": { "runs": [{ "text": "1" }] }, "icon": { "iconType": "ARROW_DROP_UP" } } }
        }});
        let mut response = feed(vec![json!({ "musicShelfRenderer": {} }), carousel("x", json!([playlist])), carousel("x", json!([artist]))]);
        response["frameworkUpdates"] = json!({ "entityBatchUpdate": { "mutations": [
            { "payload": { "musicFormBooleanChoice": { "opaqueToken": "DE" } } },
            { "payload": { "other": {} } },
            { "payload": { "musicFormBooleanChoice": { "opaqueToken": "ZZ" } } }
        ]}});

        let charts = parse_charts(&response, "ZZ");
        assert_eq!(charts.countries, ["DE", "ZZ"]);
        assert_eq!(charts.videos[0].id, "PL4fGSI1pDJn", "the VL prefix is stripped");
        assert_eq!(charts.videos[0].kind, ItemKind::Playlist);
        assert!(charts.genres.is_empty(), "genre charts are US only");
        assert_eq!(charts.artists[0].rank.as_deref(), Some("1"));
        assert_eq!(charts.artists[0].trend, Trend::Up);
        assert_eq!(charts.artists[0].item.subscribers.as_deref(), Some("9.62M"));
    }

    #[test]
    fn a_premium_response_puts_daily_and_weekly_where_videos_was() {
        let playlist = |title: &str| {
            json!({ MTRIR: { "title": { "runs": [{ "text": title, "navigationEndpoint": { "browseEndpoint": { "browseId": "VLPL1" } } }] } } })
        };
        let shelves = vec![
            json!({ "musicShelfRenderer": {} }),
            carousel("x", json!([playlist("Daily")])),
            carousel("x", json!([playlist("Weekly")])),
            carousel("x", json!([])),
        ];
        let charts = parse_charts(&feed(shelves), "ZZ");
        assert_eq!(charts.daily[0].title, "Daily");
        assert_eq!(charts.weekly[0].title, "Weekly");
        assert!(charts.videos.is_empty());
    }

    /// Hits the network. `cargo test -- --ignored live_explore --nocapture`
    #[tokio::test]
    #[ignore]
    async fn live_explore() {
        let paths = crate::paths::Paths::discover();
        let api = crate::net::ytmusic::YtMusic::new(&paths).unwrap().api();
        let data = load_explore(api, "ZZ".into()).await.expect("explore");
        println!("moods {} genres {}", data.moods.len(), data.genres.len());
        println!("first mood {:?}", data.moods.first());
        println!("first genre {:?}", data.genres.first());
        for item in data.feed.new_releases.iter().take(3) {
            println!("release {:?} {} type={:?} year={:?} by {} olak={:?}", item.kind, item.title, item.item_type, item.year, item.artists_text(), item.playlist_id);
        }
        for item in data.feed.new_videos.iter().take(3) {
            println!("video {:?} {} by {} views={:?}", item.kind, item.title, item.artists_text(), item.views);
        }
        for item in data.feed.trending.iter().take(3) {
            println!("trending {:?} {} by {} album={:?}", item.kind, item.title, item.artists_text(), item.album.as_ref().map(|a| &a.name));
        }
        let charts = data.charts.expect("charts");
        println!("countries {} videos {} daily {} weekly {} genres {} artists {}", charts.countries.len(), charts.videos.len(), charts.daily.len(), charts.weekly.len(), charts.genres.len(), charts.artists.len());
        for playlist in charts.videos.iter().chain(charts.daily.iter()).take(3) {
            println!("chart playlist {} -> {}", playlist.title, playlist.id);
        }
        for artist in charts.artists.iter().take(5) {
            println!("chart artist #{:?} {:?} {} {} ({:?})", artist.rank, artist.trend, artist.item.title, artist.item.id, artist.item.subscribers);
        }
        assert!(!data.feed.new_releases.is_empty());
        assert!(data.feed.new_releases.iter().all(|a| !a.id.is_empty()));

        let pill = data.genres.first().or(data.moods.first()).expect("a pill");
        let api = crate::net::ytmusic::YtMusic::new(&paths).unwrap().api();
        let sections = get_category_page(&api, &pill.params).await.expect("category");
        println!("category {} -> {} sections", pill.title, sections.len());
        for section in &sections {
            println!("  {} ({} items) first={:?}", section.title, section.items.len(), section.items.first().map(|i| (i.kind, &i.title, &i.id)));
        }
        assert!(!sections.is_empty());
    }

    /// `cargo test -- --ignored live_mood_sections --nocapture`
    #[tokio::test]
    #[ignore]
    async fn live_mood_sections() {
        let paths = crate::paths::Paths::discover();
        let api = crate::net::ytmusic::YtMusic::new(&paths).unwrap().api();
        for (title, items) in get_mood_categories(&api).await.expect("categories") {
            println!("{title}: {} -> {:?}", items.len(), items.iter().take(4).map(|c| c.title.clone()).collect::<Vec<_>>());
        }
    }

    #[test]
    fn the_country_menu_puts_global_first_and_sorts_the_rest() {
        let codes: Vec<String> = ["US", "DE", "ZZ", "XX"].iter().map(|c| c.to_string()).collect();
        let options = country_options(&codes);
        let names: Vec<&str> = options.iter().map(|(_, name)| name.as_str()).collect();
        assert_eq!(names, ["Global", "Germany", "United States", "XX"]);
    }

    /// `cargo test -- --ignored the_podcasts_page_loads --nocapture`
    #[tokio::test]
    #[ignore]
    async fn the_podcasts_page_loads() {
        let dir = tempfile::tempdir().unwrap();
        let client = crate::net::ytmusic::YtMusic::new(&crate::paths::Paths::for_tests(dir.path())).unwrap();
        let sections = get_podcasts_page(&client.api()).await.unwrap();
        for s in &sections {
            let first = s.items.first().unwrap();
            println!("{}: {} items, first {:?} {:?} show={:?} {:?}s", s.title, s.items.len(), first.item_type, first.title, first.album.as_ref().map(|a| (&a.name, &a.id)), first.duration_seconds);
        }
        assert!(sections.len() >= 3);
        assert!(sections.iter().flat_map(|s| &s.items).any(|i| i.item_type.as_deref() == Some("Episode") && i.duration_seconds.is_some() && i.album.is_some()));
    }
}

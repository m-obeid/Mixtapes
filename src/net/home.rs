//! The home feed. Port of ytmusicapi's `get_home` and `parse_mixed_content`
//! plus the sidecar pass MusicClient.get_home_full made for the per-shelf
//! header art.
//!
//! Python parsed the response twice: once through ytmusicapi for the rows,
//! once by hand for the strapline thumbnail and the video types its parser
//! drops, then stitched the two together by shelf title. One pass here keeps
//! all three, so a shelf with a duplicate title cannot take another's art.

use std::sync::Arc;

use serde_json::{Value, json};

use super::browse::{Browse, Continuation};
use super::items::{array_at, last_thumbnail_url, owned_at, parse_mixed_item};
use crate::model::MediaItem;
use crate::net::ytmusic::NetError;

/// Where the first page keeps its shelves, and the node that carries the token.
const TAB_CONTENT: &str = "/contents/singleColumnBrowseResultsRenderer/tabs/0/tabRenderer/content";

/// One titled row of the feed.
#[derive(Clone, Debug, Default)]
pub struct HomeSection {
    pub title: String,
    pub items: Vec<MediaItem>,
    /// The seed's picture on a "Based on ..." row, which the heading shows.
    pub strapline_thumb: Option<String>,
    /// The small line above the title: "Similar to", "Discover the hits", "Stations".
    pub strapline: Option<String>,
}

/// The feed, paged until `limit` sections have arrived.
///
/// YouTube answers with three rows and a token, so the limit is what decides
/// how much of the feed there is. A page that fails keeps what came before it:
/// a short feed beats an error screen.
pub async fn get_home(api: Arc<dyn Browse>, limit: usize) -> Result<Vec<HomeSection>, NetError> {
    let response = api.post("browse", json!({ "browseId": "FEmusic_home" })).await?;
    let mut sections = parse_home(&response);
    let token = owned_at(&response, &format!("{TAB_CONTENT}/sectionListRenderer/continuations/0/nextContinuationData/continuation"));
    let remaining = limit.saturating_sub(sections.len());
    if token.is_some() && remaining > 0 {
        let paged = Continuation::browse(&api, token).limit(remaining).collect(|page| page.iter().filter_map(|shelf| parse_section(shelf)).collect()).await;
        if let Some(err) = &paged.error {
            tracing::warn!(%err, "home continuation failed");
        }
        sections.extend(paged.items);
    }
    sections.truncate(limit);
    Ok(sections)
}

pub fn parse_home(response: &Value) -> Vec<HomeSection> {
    array_at(response, &format!("{TAB_CONTENT}/sectionListRenderer/contents")).iter().filter_map(parse_section).collect()
}

/// One shelf: the carousel kinds, and the description shelf that has prose
/// where the others have cards.
fn parse_section(shelf: &Value) -> Option<HomeSection> {
    if let Some(description) = shelf.get("musicDescriptionShelfRenderer") {
        let title = owned_at(description, "/header/runs/0/text")?;
        return Some(HomeSection { title, ..HomeSection::default() });
    }
    // Whatever single renderer the row holds, so a shelf kind this has not
    // seen still comes through with its title and cards.
    let renderer = shelf.as_object()?.values().find(|v| v.get("contents").is_some())?;
    let header = renderer.pointer("/header/musicCarouselShelfBasicHeaderRenderer").or_else(|| renderer.pointer("/header/musicImmersiveCarouselShelfBasicHeaderRenderer"));
    let title = header.and_then(|h| owned_at(h, "/title/runs/0/text"))?;
    let items = array_at(renderer, "/contents").iter().filter_map(|entry| parse_mixed_item(entry, &title)).collect();
    let strapline_thumb = header.and_then(|h| last_thumbnail_url(array_at(h, "/thumbnail/musicThumbnailRenderer/thumbnail/thumbnails")));
    let strapline = header.and_then(|h| owned_at(h, "/strapline/runs/0/text")).filter(|s| !s.trim().is_empty());
    Some(HomeSection { title, items, strapline_thumb, strapline })
}

/// Shelves the feed shows that no page here draws. Long listens stay: those are mixes, not podcasts.
pub fn is_podcast_section(title: &str) -> bool {
    let low = title.to_lowercase();
    ["shows for you", "podcast", "episode"].iter().any(|k| low.contains(k))
}

/// The "Long listens" shelf: hour-long mixes, shown as rows with their length.
pub fn is_long_listens(title: &str) -> bool {
    title.to_lowercase().contains("long listen")
}

/// Port of home.py _classify_section: the four rows that lead the feed,
/// whatever order YouTube sent them in.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Bucket {
    Library,
    ListenAgain,
    Discover,
    Forgotten,
}

/// In the order they are shown.
pub const BUCKETS: [Bucket; 4] = [Bucket::Library, Bucket::ListenAgain, Bucket::Discover, Bucket::Forgotten];

impl Bucket {
    fn keys(self) -> &'static [&'static str] {
        match self {
            Bucket::Library => &["your library", "from your library"],
            Bucket::ListenAgain => &["listen again", "your favorites", "recent activity"],
            Bucket::Discover => &["daily discover", "discover mix", "discovery mix", "made for you", "recommended for today"],
            Bucket::Forgotten => &["forgotten favorites", "hidden gems", "rediscover"],
        }
    }
}

pub fn classify_section(title: &str) -> Option<Bucket> {
    let low = title.to_lowercase();
    BUCKETS.into_iter().find(|bucket| bucket.keys().iter().any(|k| low.contains(k)))
}

/// The icon beside a heading, or none when the title says nothing.
pub fn section_icon(title: &str) -> Option<&'static str> {
    let low = title.to_lowercase();
    const RULES: &[(&[&str], &str)] = &[
        (&["from your library", "your library"], "media-optical-symbolic"),
        (&["forgotten", "rediscover", "hidden gem"], "starred-symbolic"),
        (&["daily discover", "discovery mix", "discover mix"], "compass2-symbolic"),
        (&["listen again"], "media-playback-start-symbolic"),
        (&["mix"], "media-playlist-shuffle-symbolic"),
        (&["new release", "new album", "new single"], "star-new-symbolic"),
        (&["music video"], "video-x-generic-symbolic"),
        (&["mood", "moment"], "emoji-objects-symbolic"),
        (&["recap"], "media-playback-start-symbolic"),
        (&["quick pick"], "media-playback-start-symbolic"),
    ];
    RULES.iter().find(|(keys, _)| keys.iter().any(|k| low.contains(k))).map(|(_, icon)| *icon)
}

/// Port of _populate_feed's ordering: drop what no page draws, take the
/// quick-picks row out for the dial, then lead with the four named rows.
pub fn arrange(sections: Vec<HomeSection>) -> (Vec<MediaItem>, Vec<HomeSection>) {
    let mut sections: Vec<HomeSection> = sections.into_iter().filter(|s| !s.items.is_empty() && !is_podcast_section(&s.title)).collect();

    let quick = sections.iter().position(|s| s.title.to_lowercase().contains("quick pick"));
    let mut dial = match quick {
        Some(index) => sections.remove(index).items,
        None => Vec::new(),
    };
    // Without a quick-picks row the dial borrows one: Listen again, or
    // failing that whatever came first.
    if dial.is_empty() {
        let fallback = sections.iter().position(|s| classify_section(&s.title) == Some(Bucket::ListenAgain)).unwrap_or(0);
        dial = sections.get(fallback).map(|s| s.items.clone()).unwrap_or_default();
    }

    let mut leading: Vec<Option<HomeSection>> = BUCKETS.iter().map(|_| None).collect();
    let mut rest = Vec::new();
    for section in sections {
        match classify_section(&section.title).map(|b| BUCKETS.iter().position(|x| *x == b).unwrap_or_default()) {
            Some(slot) if leading[slot].is_none() => leading[slot] = Some(section),
            _ => rest.push(section),
        }
    }
    let ordered: Vec<HomeSection> = leading.into_iter().flatten().chain(rest).collect();
    (dial, ordered)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::model::ItemKind;

    fn card(title: &str, page_type: Option<&str>, browse_id: &str) -> Value {
        let mut nav = json!({ "browseEndpoint": { "browseId": browse_id } });
        if let Some(page_type) = page_type {
            nav["browseEndpoint"]["browseEndpointContextSupportedConfigs"] = json!({ "browseEndpointContextMusicConfig": { "pageType": page_type } });
        }
        json!({ "musicTwoRowItemRenderer": {
            "title": { "runs": [{ "text": title, "navigationEndpoint": nav }] },
            "subtitle": { "runs": [{ "text": "Someone", "navigationEndpoint": { "browseEndpoint": { "browseId": "UC1" } } }] },
            "thumbnailRenderer": { "musicThumbnailRenderer": { "thumbnail": { "thumbnails": [{ "url": "t" }] } } }
        }})
    }

    fn song_card(title: &str, video_type: Option<&str>) -> Value {
        let mut watch = json!({ "videoId": "vid1" });
        if let Some(video_type) = video_type {
            watch["watchEndpointMusicSupportedConfigs"] = json!({ "watchEndpointMusicConfig": { "musicVideoType": video_type } });
        }
        json!({ "musicTwoRowItemRenderer": {
            "title": { "runs": [{ "text": title }] },
            "subtitle": { "runs": [{ "text": "Artist", "navigationEndpoint": { "browseEndpoint": { "browseId": "UC2" } } }] },
            "navigationEndpoint": { "watchEndpoint": watch },
            "thumbnailRenderer": { "musicThumbnailRenderer": { "thumbnail": { "thumbnails": [{ "url": "t" }] } } }
        }})
    }

    fn shelf(title: &str, contents: Value, strapline: bool) -> Value {
        let mut header = json!({ "musicCarouselShelfBasicHeaderRenderer": { "title": { "runs": [{ "text": title }] } } });
        if strapline {
            header["musicCarouselShelfBasicHeaderRenderer"]["thumbnail"] = json!({ "musicThumbnailRenderer": { "thumbnail": { "thumbnails": [{ "url": "seed.jpg" }] } } });
            header["musicCarouselShelfBasicHeaderRenderer"]["strapline"] = json!({ "runs": [{ "text": "SIMILAR TO" }] });
        }
        json!({ "musicCarouselShelfRenderer": { "header": header, "contents": contents } })
    }

    fn feed(shelves: Vec<Value>) -> Value {
        json!({ "contents": { "singleColumnBrowseResultsRenderer": { "tabs": [{ "tabRenderer": { "content": { "sectionListRenderer": { "contents": shelves } } } }] } } })
    }

    #[test]
    fn a_station_card_is_marked_live() {
        let mut station = card("DECO*27 - MV STATION", None, "");
        station["musicTwoRowItemRenderer"]["navigationEndpoint"] = json!({ "watchEndpoint": { "videoId": "h4hy2Gn-FVE" } });
        station["musicTwoRowItemRenderer"]["subtitleBadges"] = json!([{ "liveBadgeRenderer": { "label": { "runs": [{ "text": "Live" }] } } }]);
        let response = feed(vec![shelf("Listen together", json!([station]), false)]);
        let sections = parse_home(&response);
        let item = &sections[0].items[0];
        assert!(item.is_live);
        assert_eq!(item.id, "h4hy2Gn-FVE");
        assert_eq!(item.duration_text(), None);
        assert!(item.to_track().unwrap().is_live);
    }

    #[test]
    fn a_shelf_keeps_its_strapline_art_beside_its_cards() {
        let response = feed(vec![shelf("Based on Boards of Canada", json!([card("Geogaddi", Some("MUSIC_PAGE_TYPE_ALBUM"), "MPREb_1")]), true)]);
        let sections = parse_home(&response);
        assert_eq!(sections.len(), 1);
        assert_eq!(sections[0].strapline_thumb.as_deref(), Some("seed.jpg"));
        assert_eq!(sections[0].strapline.as_deref(), Some("SIMILAR TO"));
        assert_eq!(sections[0].items[0].kind, ItemKind::Album);
    }

    #[test]
    fn a_card_is_read_by_the_page_its_title_opens() {
        let response = feed(vec![shelf(
            "Mixed for you",
            json!([
                card("An album", Some("MUSIC_PAGE_TYPE_ALBUM"), "MPREb_1"),
                card("An artist", Some("MUSIC_PAGE_TYPE_ARTIST"), "UCabc"),
                card("A playlist", Some("MUSIC_PAGE_TYPE_PLAYLIST"), "VLPL1"),
                card("A podcast", Some("MUSIC_PAGE_TYPE_PODCAST_SHOW_DETAIL_PAGE"), "MPSPabc"),
                song_card("A song", Some("MUSIC_VIDEO_TYPE_ATV")),
            ]),
            false,
        )]);
        let items = &parse_home(&response)[0].items;
        let kinds: Vec<ItemKind> = items.iter().map(|i| i.kind).collect();
        assert_eq!(kinds, [ItemKind::Album, ItemKind::Artist, ItemKind::Playlist, ItemKind::Song], "the podcast show is dropped");
        assert_eq!(items[2].id, "PL1", "a playlist card loses its VL prefix");
    }

    #[test]
    fn a_card_with_no_video_type_is_told_apart_by_its_shelf() {
        let untyped = json!([song_card("Untitled", None)]);
        let in_videos = parse_home(&feed(vec![shelf("New music videos", untyped.clone(), false)]));
        let in_songs = parse_home(&feed(vec![shelf("Listen again", untyped, false)]));
        assert_eq!(in_videos[0].items[0].kind, ItemKind::Video);
        assert_eq!(in_songs[0].items[0].kind, ItemKind::Song);
    }

    /// Hits the network. `cargo test -- --ignored live_home --nocapture`
    #[tokio::test]
    #[ignore]
    async fn live_home() {
        let paths = crate::paths::Paths::discover();
        let api = crate::net::ytmusic::YtMusic::new(&paths).unwrap().api();
        let sections = get_home(api, 25).await.expect("home");
        println!("{} sections", sections.len());
        for section in &sections {
            let kinds: Vec<String> = section.items.iter().take(3).map(|i| format!("{:?}", i.kind)).collect();
            println!("{:<34} {:>3} items  strapline={:?} {:?}", section.title.chars().take(32).collect::<String>(), section.items.len(), section.strapline_thumb.is_some(), kinds);
            for item in section.items.iter().take(2) {
                println!("      {:?} {:<28} id={:<24} thumb={:?}", item.kind, item.title.chars().take(26).collect::<String>(), item.id, item.thumb);
            }
        }
        let (dial, ordered) = arrange(sections);
        println!("dial {} items; feed: {:?}", dial.len(), ordered.iter().map(|s| s.title.clone()).collect::<Vec<_>>());
        assert!(!ordered.is_empty());
        assert!(ordered.iter().all(|s| s.items.iter().all(|i| !i.id.is_empty())));
    }

    #[test]
    fn the_named_rows_lead_whatever_order_they_arrived_in() {
        let section = |title: &str| HomeSection { title: title.into(), items: vec![MediaItem::default()], ..HomeSection::default() };
        let sections = vec![
            section("Quick picks"),
            section("Covers and remixes"),
            section("Forgotten favorites"),
            section("From your library"),
            section("Listen again"),
        ];
        let (dial, ordered) = arrange(sections);
        assert_eq!(dial.len(), 1, "the quick picks row feeds the dial and leaves the feed");
        let titles: Vec<&str> = ordered.iter().map(|s| s.title.as_str()).collect();
        assert_eq!(titles, ["From your library", "Listen again", "Forgotten favorites", "Covers and remixes"]);
    }

    #[test]
    fn without_quick_picks_the_dial_borrows_listen_again() {
        let with_items = |title: &str, n: usize| HomeSection { title: title.into(), items: vec![MediaItem::default(); n], ..HomeSection::default() };
        let (dial, ordered) = arrange(vec![with_items("New releases", 2), with_items("Listen again", 5)]);
        assert_eq!(dial.len(), 5);
        assert_eq!(ordered.len(), 2, "the borrowed row still shows in the feed");
    }

    #[test]
    fn podcast_shelves_and_empty_ones_are_dropped() {
        let section = |title: &str, n: usize| HomeSection { title: title.into(), items: vec![MediaItem::default(); n], ..HomeSection::default() };
        let (_, ordered) = arrange(vec![section("Shows for you", 3), section("Episodes for you", 3), section("Nothing here", 0), section("Mixed for you", 2)]);
        assert_eq!(ordered.iter().map(|s| s.title.as_str()).collect::<Vec<_>>(), ["Mixed for you"]);
    }
}

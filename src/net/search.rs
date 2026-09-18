//! Search endpoint and response parser. The `ytmusicapi` crate has no search,
//! so this builds on its `send_request`, following ytmusicapi (Python)
//! `search` and `parse_search_results`: one unfiltered call for the mixed
//! shelves plus filtered calls, merged and deduplicated like search.py did.

use std::sync::Arc;

use serde_json::{Value, json};
use super::browse::Browse;

use crate::model::{ItemKind, MediaItem, Named, Person};
use crate::net::ytmusic::NetError;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
#[allow(dead_code)]
pub enum SearchFilter {
    Songs,
    Videos,
    Albums,
    Artists,
    Playlists,
    CommunityPlaylists,
    FeaturedPlaylists,
}

impl SearchFilter {
    /// The `params` blobs ytmusicapi sends for each filter.
    fn params(self) -> &'static str {
        match self {
            SearchFilter::Songs => "EgWKAQIIAWoKEAoQAxAEEAkQBQ%3D%3D",
            SearchFilter::Videos => "EgWKAQIQAWoKEAoQAxAEEAkQBQ%3D%3D",
            SearchFilter::Albums => "EgWKAQIYAWoKEAoQAxAEEAkQBQ%3D%3D",
            SearchFilter::Artists => "EgWKAQIgAWoKEAoQAxAEEAkQBQ%3D%3D",
            SearchFilter::Playlists => "Eg-KAQwIABAAGAAgACgBMABqChAEEAMQCRAFEAo%3D",
            SearchFilter::CommunityPlaylists => "EgeKAQQoAEABagoQBBADEAkQBRAK",
            SearchFilter::FeaturedPlaylists => "EgeKAQQoADgBagwQDhAKEAMQBBAJEAU%3D",
        }
    }

    fn kind(self) -> ItemKind {
        match self {
            SearchFilter::Songs => ItemKind::Song,
            SearchFilter::Videos => ItemKind::Video,
            SearchFilter::Albums => ItemKind::Album,
            SearchFilter::Artists => ItemKind::Artist,
            _ => ItemKind::Playlist,
        }
    }
}

#[derive(Clone, Debug, Default)]
pub struct SearchResults {
    /// The "Top result" card, when the unfiltered search offered one.
    pub top_result: Option<MediaItem>,
    pub items: Vec<MediaItem>,
}

/// One search call. `filter` narrows the results to a kind.
pub async fn search(client: &dyn Browse, query: &str, filter: Option<SearchFilter>) -> Result<SearchResults, NetError> {
    let mut body = json!({ "query": query });
    if let Some(filter) = filter {
        body["params"] = json!(filter.params());
    }
    let response = client.post("search", body).await?;
    Ok(parse_search_response(&response, filter.map(SearchFilter::kind)))
}

/// Unfiltered plus songs, artists, playlists and albums in parallel, merged in that order.
pub async fn search_all(client: Arc<dyn Browse>, query: String) -> Result<SearchResults, NetError> {
    let filters = [None, Some(SearchFilter::Songs), Some(SearchFilter::Artists), Some(SearchFilter::CommunityPlaylists), Some(SearchFilter::Albums)];
    let calls = filters.iter().map(|f| {
        let client = client.clone();
        let query = query.clone();
        let filter = *f;
        async move { search(&client, &query, filter).await }
    });
    let outcomes = futures_join_all(calls).await;

    let mut merged = SearchResults::default();
    let mut seen = std::collections::HashSet::new();
    let mut first_error = None;
    for outcome in outcomes {
        match outcome {
            Ok(results) => {
                if merged.top_result.is_none() {
                    merged.top_result = results.top_result;
                }
                for item in results.items {
                    if item.id.is_empty() || seen.insert(item.id.clone()) {
                        merged.items.push(item);
                    }
                }
            }
            Err(err) => {
                tracing::warn!(%err, "search call failed");
                first_error.get_or_insert(err);
            }
        }
    }
    if merged.items.is_empty() && merged.top_result.is_none() {
        if let Some(err) = first_error {
            return Err(err);
        }
    }
    if let Some(top) = &merged.top_result {
        merged.items.retain(|i| i.id != top.id);
    }
    Ok(merged)
}

/// Await a set of futures concurrently without adding a futures dependency.
async fn futures_join_all<F>(futures: impl IntoIterator<Item = F>) -> Vec<F::Output>
where
    F: std::future::Future + Send + 'static,
    F::Output: Send + 'static,
{
    let handles: Vec<_> = futures.into_iter().map(tokio::spawn).collect();
    let mut out = Vec::with_capacity(handles.len());
    for handle in handles {
        if let Ok(value) = handle.await {
            out.push(value);
        }
    }
    out
}

// -- parsing -------------------------------------------------------------

/// Walk every shelf of a search response into items. `filter_kind` is the
/// kind a filtered call asked for, since those shelves carry no type run.
pub fn parse_search_response(response: &Value, filter_kind: Option<ItemKind>) -> SearchResults {
    let mut results = SearchResults::default();
    let Some(sections) = response
        .pointer("/contents/tabbedSearchResultsRenderer/tabs/0/tabRenderer/content/sectionListRenderer/contents")
        .and_then(Value::as_array)
    else {
        return results;
    };
    for section in sections {
        if let Some(card) = section.get("musicCardShelfRenderer") {
            let top = parse_card_shelf(card);
            // Songs listed under an artist card carry no artist of their own.
            let inherited: Vec<Person> = match &top {
                Some(t) if t.kind == ItemKind::Artist => vec![Person { name: t.title.clone(), id: Some(t.id.clone()) }],
                Some(t) => t.artists.clone(),
                None => Vec::new(),
            };
            if let Some(item) = top {
                results.top_result.get_or_insert(item);
            }
            if let Some(contents) = card.get("contents").and_then(Value::as_array) {
                for entry in contents {
                    if let Some(renderer) = entry.get("musicResponsiveListItemRenderer") {
                        if let Some(mut item) = parse_list_item(renderer, None) {
                            if item.artists.is_empty() && item.kind.is_playable() {
                                item.artists = inherited.clone();
                            }
                            results.items.push(item);
                        }
                    }
                }
            }
            continue;
        }
        let Some(shelf) = section.get("musicShelfRenderer") else { continue };
        let category = shelf.pointer("/title/runs/0/text").and_then(Value::as_str).unwrap_or("");
        let shelf_kind = filter_kind.or_else(|| kind_from_category(category));
        if is_skipped_category(category) {
            continue;
        }
        let Some(contents) = shelf.get("contents").and_then(Value::as_array) else { continue };
        for entry in contents {
            if let Some(renderer) = entry.get("musicResponsiveListItemRenderer") {
                if let Some(item) = parse_list_item(renderer, shelf_kind) {
                    results.items.push(item);
                }
            }
        }
    }
    results
}

fn is_skipped_category(category: &str) -> bool {
    let low = category.to_lowercase();
    low.contains("podcast") || low.contains("episode") || low.contains("profile")
}

fn kind_from_category(category: &str) -> Option<ItemKind> {
    let low = category.to_lowercase();
    if low.starts_with("song") {
        Some(ItemKind::Song)
    } else if low.starts_with("video") {
        Some(ItemKind::Video)
    } else if low.starts_with("album") {
        Some(ItemKind::Album)
    } else if low.starts_with("artist") {
        Some(ItemKind::Artist)
    } else if low.contains("playlist") {
        Some(ItemKind::Playlist)
    } else {
        None
    }
}

fn kind_from_type_word(word: &str) -> Option<ItemKind> {
    match word.trim().to_lowercase().as_str() {
        "song" => Some(ItemKind::Song),
        "video" => Some(ItemKind::Video),
        "album" | "single" | "ep" => Some(ItemKind::Album),
        "artist" => Some(ItemKind::Artist),
        "playlist" => Some(ItemKind::Playlist),
        _ => None,
    }
}

fn thumbnails_last(renderer: &Value) -> Option<String> {
    renderer
        .pointer("/thumbnail/musicThumbnailRenderer/thumbnail/thumbnails")
        .and_then(Value::as_array)
        .and_then(|t| t.last())
        .and_then(|t| t.get("url"))
        .and_then(Value::as_str)
        .map(str::to_owned)
}

fn is_explicit(renderer: &Value) -> bool {
    renderer
        .get("badges")
        .and_then(Value::as_array)
        .map(|badges| badges.iter().any(|b| b.pointer("/musicInlineBadgeRenderer/icon/iconType").and_then(Value::as_str) == Some("MUSIC_EXPLICIT_BADGE")))
        .unwrap_or(false)
}

/// Text runs of a flex column with the " • " separators dropped, keeping browse ids.
fn column_runs(renderer: &Value, index: usize) -> Vec<(String, Option<String>)> {
    renderer
        .pointer(&format!("/flexColumns/{index}/musicResponsiveListItemFlexColumnRenderer/text/runs"))
        .and_then(Value::as_array)
        .map(|runs| {
            runs.iter()
                .filter_map(|run| {
                    let text = run.get("text").and_then(Value::as_str)?.to_owned();
                    if text.trim() == "•" || text.trim().is_empty() {
                        return None;
                    }
                    let id = run.pointer("/navigationEndpoint/browseEndpoint/browseId").and_then(Value::as_str).map(str::to_owned);
                    Some((text, id))
                })
                .collect()
        })
        .unwrap_or_default()
}

fn watch_video_id(renderer: &Value) -> Option<(String, Option<String>)> {
    let endpoint = renderer
        .pointer("/overlay/musicItemThumbnailOverlayRenderer/content/musicPlayButtonRenderer/playNavigationEndpoint/watchEndpoint")
        .or_else(|| renderer.pointer("/flexColumns/0/musicResponsiveListItemFlexColumnRenderer/text/runs/0/navigationEndpoint/watchEndpoint"));
    let video_id = renderer
        .pointer("/playlistItemData/videoId")
        .and_then(Value::as_str)
        .or_else(|| endpoint.and_then(|e| e.get("videoId")).and_then(Value::as_str))?
        .to_owned();
    let video_type = endpoint
        .and_then(|e| e.pointer("/watchEndpointMusicSupportedConfigs/watchEndpointMusicConfig/musicVideoType"))
        .and_then(Value::as_str)
        .map(str::to_owned);
    Some((video_id, video_type))
}

fn browse_target(renderer: &Value) -> Option<(String, Option<String>)> {
    let endpoint = renderer.pointer("/navigationEndpoint/browseEndpoint")?;
    let id = endpoint.get("browseId").and_then(Value::as_str)?.to_owned();
    let page_type = endpoint
        .pointer("/browseEndpointContextSupportedConfigs/browseEndpointContextMusicConfig/pageType")
        .and_then(Value::as_str)
        .map(str::to_owned);
    Some((id, page_type))
}

fn is_duration(text: &str) -> bool {
    let parts: Vec<&str> = text.trim().split(':').collect();
    (2..=3).contains(&parts.len()) && parts.iter().all(|p| !p.is_empty() && p.chars().all(|c| c.is_ascii_digit()))
}

fn parse_duration(text: &str) -> Option<u32> {
    let mut total = 0u32;
    for part in text.trim().split(':') {
        total = total * 60 + part.parse::<u32>().ok()?;
    }
    Some(total)
}

fn is_year(text: &str) -> bool {
    text.trim().len() == 4 && text.trim().chars().all(|c| c.is_ascii_digit())
}

/// One `musicResponsiveListItemRenderer` in a shelf.
fn parse_list_item(renderer: &Value, shelf_kind: Option<ItemKind>) -> Option<MediaItem> {
    let title = renderer
        .pointer("/flexColumns/0/musicResponsiveListItemFlexColumnRenderer/text/runs/0/text")
        .and_then(Value::as_str)?
        .to_owned();
    let mut runs = column_runs(renderer, 1);
    runs.extend(column_runs(renderer, 2));

    // Unfiltered shelves lead the subtitle with the type word.
    let leading_kind = runs.first().and_then(|(t, _)| kind_from_type_word(t));
    let mut item_type = None;
    if let Some(kind) = leading_kind {
        if kind == ItemKind::Album {
            item_type = Some(runs[0].0.trim().to_owned());
        }
        runs.remove(0);
    }

    let watch = watch_video_id(renderer);
    let browse = browse_target(renderer);
    let kind = match (&watch, &browse) {
        (Some((_, video_type)), _) => match video_type.as_deref() {
            Some("MUSIC_VIDEO_TYPE_ATV") => ItemKind::Song,
            // A podcast episode plays like any other video. The row says what it is.
            Some(t) if t.contains("PODCAST") || t.contains("EPISODE") => {
                item_type = Some("Episode".to_owned());
                ItemKind::Video
            }
            Some(_) => ItemKind::Video,
            None => leading_kind.or(shelf_kind).unwrap_or(ItemKind::Song),
        },
        (None, Some((id, page_type))) => {
            if id.starts_with("MPRE") || page_type.as_deref() == Some("MUSIC_PAGE_TYPE_ALBUM") {
                ItemKind::Album
            } else if id.starts_with("UC") || page_type.as_deref() == Some("MUSIC_PAGE_TYPE_ARTIST") {
                ItemKind::Artist
            } else if id.starts_with("VL") || id.starts_with("PL") || id.starts_with("RD") || page_type.as_deref() == Some("MUSIC_PAGE_TYPE_PLAYLIST") {
                ItemKind::Playlist
            } else {
                leading_kind.or(shelf_kind)?
            }
        }
        (None, None) => leading_kind.or(shelf_kind)?,
    };

    let mut item = MediaItem { kind, title, thumb: thumbnails_last(renderer), explicit: is_explicit(renderer), item_type, ..MediaItem::default() };
    match kind {
        ItemKind::Song | ItemKind::Video => {
            item.id = watch.as_ref().map(|(id, _)| id.clone()).unwrap_or_default();
            for (text, id) in runs {
                if is_duration(&text) {
                    item.duration_seconds = parse_duration(&text);
                } else if text.contains("views") || text.contains("plays") {
                    item.views = Some(text);
                } else if id.as_deref().is_some_and(|i| i.starts_with("MPRE")) {
                    item.album = Some(Named { name: text, id });
                } else if id.is_some() || item.artists.is_empty() {
                    item.artists.push(Person { name: text, id });
                } else if kind == ItemKind::Song && item.album.is_none() {
                    item.album = Some(Named { name: text, id: None });
                }
            }
        }
        ItemKind::Album => {
            item.id = browse.as_ref().map(|(id, _)| id.clone()).unwrap_or_default();
            for (text, id) in runs {
                if is_year(&text) {
                    item.year = Some(text.trim().to_owned());
                } else {
                    item.artists.push(Person { name: text, id });
                }
            }
            if item.item_type.is_none() {
                item.item_type = Some("Album".to_owned());
            }
        }
        ItemKind::Artist => {
            item.id = browse.as_ref().map(|(id, _)| id.clone()).unwrap_or_default();
            item.subscribers = runs.into_iter().map(|(t, _)| t).find(|t| t.chars().any(|c| c.is_ascii_digit()));
        }
        ItemKind::Playlist => {
            item.id = browse.as_ref().map(|(id, _)| id.trim_start_matches("VL").to_owned()).unwrap_or_default();
            for (text, id) in runs {
                let low = text.to_lowercase();
                if low.contains("song") || low.contains("track") || low.contains("view") {
                    item.count = Some(text.split_whitespace().next().unwrap_or_default().to_owned());
                    if low.contains("view") {
                        item.views = Some(text);
                    }
                } else {
                    item.artists.push(Person { name: text, id });
                }
            }
        }
    }
    if item.id.is_empty() {
        return None;
    }
    Some(item)
}

/// The "Top result" `musicCardShelfRenderer`.
fn parse_card_shelf(card: &Value) -> Option<MediaItem> {
    let title = card.pointer("/title/runs/0/text").and_then(Value::as_str)?.to_owned();
    let on_tap = card.pointer("/title/runs/0/navigationEndpoint").or_else(|| card.get("onTap"))?;
    let subtitle: Vec<(String, Option<String>)> = card
        .pointer("/subtitle/runs")
        .and_then(Value::as_array)
        .map(|runs| {
            runs.iter()
                .filter_map(|run| {
                    let text = run.get("text").and_then(Value::as_str)?.to_owned();
                    if text.trim() == "•" {
                        return None;
                    }
                    let id = run.pointer("/navigationEndpoint/browseEndpoint/browseId").and_then(Value::as_str).map(str::to_owned);
                    Some((text, id))
                })
                .collect()
        })
        .unwrap_or_default();
    let thumb = card
        .pointer("/thumbnail/musicThumbnailRenderer/thumbnail/thumbnails")
        .and_then(Value::as_array)
        .and_then(|t| t.last())
        .and_then(|t| t.get("url"))
        .and_then(Value::as_str)
        .map(str::to_owned);

    let mut item = MediaItem { title, thumb, ..MediaItem::default() };
    let type_word = subtitle.first().and_then(|(t, _)| kind_from_type_word(t));
    let mut runs = subtitle;
    if type_word.is_some() {
        runs.remove(0);
    }
    if let Some(video_id) = on_tap.pointer("/watchEndpoint/videoId").and_then(Value::as_str) {
        let video_type = on_tap.pointer("/watchEndpoint/watchEndpointMusicSupportedConfigs/watchEndpointMusicConfig/musicVideoType").and_then(Value::as_str);
        item.kind = match video_type {
            Some("MUSIC_VIDEO_TYPE_ATV") => ItemKind::Song,
            Some(_) => ItemKind::Video,
            None => type_word.unwrap_or(ItemKind::Song),
        };
        item.id = video_id.to_owned();
        for (text, id) in runs {
            if is_duration(&text) {
                item.duration_seconds = parse_duration(&text);
            } else if text.contains("views") {
                item.views = Some(text);
            } else if id.as_deref().is_some_and(|i| i.starts_with("MPRE")) {
                item.album = Some(Named { name: text, id });
            } else {
                item.artists.push(Person { name: text, id });
            }
        }
    } else {
        let browse_id = on_tap.pointer("/browseEndpoint/browseId").and_then(Value::as_str)?;
        let page_type = on_tap.pointer("/browseEndpoint/browseEndpointContextSupportedConfigs/browseEndpointContextMusicConfig/pageType").and_then(Value::as_str);
        item.kind = if browse_id.starts_with("MPRE") || page_type == Some("MUSIC_PAGE_TYPE_ALBUM") {
            ItemKind::Album
        } else if browse_id.starts_with("UC") || page_type == Some("MUSIC_PAGE_TYPE_ARTIST") {
            ItemKind::Artist
        } else {
            ItemKind::Playlist
        };
        item.id = browse_id.trim_start_matches("VL").to_owned();
        match item.kind {
            ItemKind::Artist => item.subscribers = runs.into_iter().map(|(t, _)| t).find(|t| t.chars().any(|c| c.is_ascii_digit())),
            ItemKind::Album => {
                item.item_type = Some("Album".to_owned());
                for (text, id) in runs {
                    if is_year(&text) {
                        item.year = Some(text.trim().to_owned());
                    } else {
                        item.artists.push(Person { name: text, id });
                    }
                }
            }
            _ => {
                for (text, id) in runs {
                    item.artists.push(Person { name: text, id });
                }
            }
        }
    }
    Some(item)
}

#[cfg(test)]
mod tests {
    use super::*;
    use ytmusicapi::YTMusicClient;

    #[test]
    fn duration_and_year_detection() {
        assert!(is_duration("3:42"));
        assert!(is_duration("1:02:03"));
        assert!(!is_duration("2017"));
        assert!(is_year("2017"));
        assert_eq!(parse_duration("1:02:03"), Some(3723));
    }

    #[test]
    fn parses_a_song_row() {
        let renderer = json!({
            "flexColumns": [
                {"musicResponsiveListItemFlexColumnRenderer": {"text": {"runs": [{"text": "Bohemian Rhapsody"}]}}},
                {"musicResponsiveListItemFlexColumnRenderer": {"text": {"runs": [
                    {"text": "Song"}, {"text": " • "},
                    {"text": "Queen", "navigationEndpoint": {"browseEndpoint": {"browseId": "UCiMhD4jzUqG-IgPzUmmytRQ"}}},
                    {"text": " • "},
                    {"text": "A Night at the Opera", "navigationEndpoint": {"browseEndpoint": {"browseId": "MPREb_abc"}}},
                    {"text": " • "}, {"text": "5:55"}
                ]}}}
            ],
            "playlistItemData": {"videoId": "fJ9rUzIMcZQ"},
            "overlay": {"musicItemThumbnailOverlayRenderer": {"content": {"musicPlayButtonRenderer": {"playNavigationEndpoint": {"watchEndpoint": {
                "videoId": "fJ9rUzIMcZQ",
                "watchEndpointMusicSupportedConfigs": {"watchEndpointMusicConfig": {"musicVideoType": "MUSIC_VIDEO_TYPE_ATV"}}
            }}}}}},
            "badges": [{"musicInlineBadgeRenderer": {"icon": {"iconType": "MUSIC_EXPLICIT_BADGE"}}}]
        });
        let item = parse_list_item(&renderer, None).expect("song");
        assert_eq!(item.kind, ItemKind::Song);
        assert_eq!(item.id, "fJ9rUzIMcZQ");
        assert_eq!(item.artists_text(), "Queen");
        assert_eq!(item.album.as_ref().map(|a| a.name.as_str()), Some("A Night at the Opera"));
        assert_eq!(item.duration_seconds, Some(355));
        assert!(item.explicit);
    }

    /// Prints the raw sub-items of the top-result card. `cargo test -- --ignored live_dump_card --nocapture`.
    #[tokio::test]
    #[ignore]
    async fn live_dump_card() {
        let client = YTMusicClient::builder().build().unwrap();
        let response = client.post("search", json!({ "query": "queen" })).await.expect("search");
        let sections = response
            .pointer("/contents/tabbedSearchResultsRenderer/tabs/0/tabRenderer/content/sectionListRenderer/contents")
            .and_then(Value::as_array)
            .expect("sections");
        for section in sections {
            if let Some(card) = section.get("musicCardShelfRenderer") {
                let first = card.pointer("/contents/0/musicResponsiveListItemRenderer").expect("card item");
                println!("FLEX1 {}", serde_json::to_string_pretty(first.pointer("/flexColumns").unwrap_or(&Value::Null)).unwrap());
                println!("KEYS {:?}", first.as_object().map(|o| o.keys().collect::<Vec<_>>()));
            }
        }
    }

    /// Prints thumbnail URLs per result. `cargo test -- --ignored live_thumbs --nocapture`.
    #[tokio::test]
    #[ignore]
    async fn live_thumbs() {
        let client = Arc::new(YTMusicClient::builder().build().unwrap());
        let query = std::env::var("MX_QUERY").unwrap_or_else(|_| "goreshit".into());
        let results = search_all(client, query).await.expect("search");
        if let Some(t) = &results.top_result {
            println!("TOP {:?} {} -> {:?}", t.kind, t.title, t.thumb);
        }
        for item in results.items.iter().take(8) {
            println!("{:?} {} -> {:?}", item.kind, item.title, item.thumb);
        }
    }

    /// Hits the network. Run with `cargo test -- --ignored live_search --nocapture`.
    #[tokio::test]
    #[ignore]
    async fn live_search() {
        let client = Arc::new(YTMusicClient::builder().build().unwrap());
        let results = search_all(client, "queen".into()).await.expect("search");
        println!("top: {:?}", results.top_result.as_ref().map(|t| (t.kind, &t.title)));
        for item in &results.items {
            println!("{:?} {:<40} {:<30} {:?} {:?}", item.kind, item.title, item.artists_text(), item.album.as_ref().map(|a| &a.name), item.duration_seconds);
        }
        assert!(!results.items.is_empty());
        assert!(results.items.iter().any(|i| i.kind == ItemKind::Song));
        assert!(results.items.iter().any(|i| i.kind == ItemKind::Artist));
    }
}

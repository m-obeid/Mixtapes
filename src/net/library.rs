//! Library endpoints on the crate's `send_request`, following ytmusicapi's
//! `get_library_playlists`, `get_library_albums`, `get_library_subscriptions`,
//! `get_library_upload_albums` and `get_library_upload_artists`, plus playlist
//! contents through the crate's own parsers.

use std::sync::Arc;

use serde_json::{Value, json};
use super::browse::{Browse, Continuation};

use crate::model::{ItemKind, MediaItem, Person};
use crate::net::ytmusic::NetError;


/// Playlists in the library, every page. Automatic playlists (two-letter ids) sort first, as the Python page did.
pub async fn library_playlists(api: Arc<dyn Browse>) -> Result<Vec<MediaItem>, NetError> {
    let entries = browse_all(&api, "FEmusic_liked_playlists", Container::Grid).await?;
    let mut items: Vec<MediaItem> = entries.iter().filter_map(|r| parse_two_row(r, ItemKind::Playlist)).collect();
    items.sort_by_key(|p| if p.id.len() == 2 { 0 } else { 1 });
    Ok(items)
}

pub async fn library_albums(api: Arc<dyn Browse>) -> Result<Vec<MediaItem>, NetError> {
    let entries = browse_all(&api, "FEmusic_liked_albums", Container::Grid).await?;
    Ok(entries.iter().filter_map(|r| parse_two_row(r, ItemKind::Album)).collect())
}

/// Subscribed artists, what library.py shows in its Artists section.
pub async fn library_subscriptions(api: Arc<dyn Browse>) -> Result<Vec<MediaItem>, NetError> {
    let entries = browse_all(&api, "FEmusic_library_corpus_artists", Container::Shelf).await?;
    Ok(entries.iter().filter_map(parse_artist_row).collect())
}

pub async fn upload_albums(api: Arc<dyn Browse>) -> Result<Vec<MediaItem>, NetError> {
    let entries = browse_all(&api, "FEmusic_library_privately_owned_releases", Container::Grid).await?;
    Ok(entries.iter().filter_map(|r| parse_two_row(r, ItemKind::Album)).collect())
}

pub async fn upload_artists(api: Arc<dyn Browse>) -> Result<Vec<MediaItem>, NetError> {
    let entries = browse_all(&api, "FEmusic_library_privately_owned_artists", Container::Shelf).await?;
    Ok(entries.iter().filter_map(parse_artist_row).collect())
}

#[derive(Clone, Copy)]
enum Container {
    Grid,
    Shelf,
}

/// Safety cap on continuation pages, like ytmusicapi's limit=None with a sane bound.
const MAX_PAGES: usize = 40;

/// All entries of a library browse, first page here and the rest through
/// `Continuation`, which knows both response shapes.
async fn browse_all(api: &dyn Browse, browse_id: &str, container: Container) -> Result<Vec<Value>, NetError> {
    let response = api.post("browse", json!({ "browseId": browse_id })).await?;
    let (container_key, items_key) = match container {
        Container::Grid => ("gridRenderer", "items"),
        Container::Shelf => ("musicShelfRenderer", "contents"),
    };
    let mut entries = Vec::new();
    let mut token = None;
    for section in crate::net::items::library_sections(&response) {
        let node = section.get(container_key).or_else(|| section.pointer(&format!("/itemSectionRenderer/contents/0/{container_key}")));
        if let Some(node) = node {
            entries.extend(node.get(items_key).and_then(Value::as_array).cloned().unwrap_or_default());
            token = token.or_else(|| continuation_token(node));
        }
    }
    let rest = Continuation::browse(api, token).pages(MAX_PAGES).collect(|page| page.iter().map(|e| (*e).clone()).collect()).await;
    entries.extend(rest.strict()?);
    Ok(entries)
}

fn continuation_token(node: &Value) -> Option<String> {
    node.pointer("/continuations/0/nextContinuationData/continuation").and_then(Value::as_str).map(str::to_owned)
}

// -- parsing -------------------------------------------------------------

/* #[cfg(test)]
fn grid_items(response: &Value) -> Vec<Value> {
    let Some(sections) = response.pointer(SECTIONS).and_then(Value::as_array) else { return Vec::new() };
    sections
        .iter()
        .filter_map(|s| s.pointer("/gridRenderer/items").or_else(|| s.pointer("/itemSectionRenderer/contents/0/gridRenderer/items")))
        .filter_map(Value::as_array)
        .flat_map(|items| items.iter().cloned())
        .collect()
} */

fn last_thumbnail(node: &Value, path: &str) -> Option<String> {
    node.pointer(path).and_then(Value::as_array).and_then(|t| t.last()).and_then(|t| t.get("url")).and_then(Value::as_str).map(str::to_owned)
}

fn subtitle_runs(renderer: &Value) -> Vec<(String, Option<String>)> {
    renderer
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
        .unwrap_or_default()
}

/// "41 tracks" gives "41"; "Your queued episodes" is a description, not a count.
fn count_from_text(text: &str) -> Option<String> {
    let low = text.to_lowercase();
    if !(low.contains("song") || low.contains("track") || low.contains("episode")) {
        return None;
    }
    let first = text.split_whitespace().next()?;
    first.chars().next().filter(|c| c.is_ascii_digit()).map(|_| first.to_owned())
}

/// A `musicTwoRowItemRenderer` grid card for an album or a playlist.
pub(crate) fn parse_two_row(entry: &Value, kind: ItemKind) -> Option<MediaItem> {
    let renderer = entry.get("musicTwoRowItemRenderer")?;
    let title = renderer.pointer("/title/runs/0/text").and_then(Value::as_str)?.to_owned();
    let browse_id = renderer.pointer("/navigationEndpoint/browseEndpoint/browseId").and_then(Value::as_str)?;
    let id = if kind == ItemKind::Playlist { browse_id.trim_start_matches("VL").to_owned() } else { browse_id.to_owned() };
    let runs = subtitle_runs(renderer);
    let mut item = MediaItem { kind, id, title, thumb: last_thumbnail(renderer, "/thumbnailRenderer/musicThumbnailRenderer/thumbnail/thumbnails"), ..MediaItem::default() };
    item.playlist_id = renderer.pointer("/menu/menuRenderer/items/0/menuNavigationItemRenderer/navigationEndpoint/watchPlaylistEndpoint/playlistId").and_then(Value::as_str).map(str::to_owned);
    match kind {
        ItemKind::Album => {
            for (i, (text, id)) in runs.into_iter().enumerate() {
                let trimmed = text.trim();
                if i == 0 && ["Album", "Single", "EP"].contains(&trimmed) {
                    item.item_type = Some(trimmed.to_owned());
                } else if trimmed.len() == 4 && trimmed.chars().all(|c| c.is_ascii_digit()) {
                    item.year = Some(trimmed.to_owned());
                } else {
                    item.artists.push(Person { name: text, id });
                }
            }
            if item.item_type.is_none() {
                item.item_type = Some("Album".to_owned());
            }
        }
        _ => {
            // Playlist subtitle: "Auto playlist", or "Playlist • Author • N songs".
            let full = runs.iter().map(|(t, _)| t.trim()).collect::<Vec<_>>().join(" • ");
            for (text, id) in runs {
                if let Some(count) = count_from_text(&text) {
                    item.count = Some(count);
                } else if text.trim() != "Playlist" {
                    item.artists.push(Person { name: text, id });
                }
            }
            item.description = Some(full);
        }
    }
    Some(item)
}

/// A subscribed or uploaded artist row.
fn parse_artist_row(entry: &Value) -> Option<MediaItem> {
    let renderer = entry.get("musicResponsiveListItemRenderer")?;
    let name = renderer.pointer("/flexColumns/0/musicResponsiveListItemFlexColumnRenderer/text/runs/0/text").and_then(Value::as_str)?.to_owned();
    let id = renderer.pointer("/navigationEndpoint/browseEndpoint/browseId").and_then(Value::as_str)?.to_owned();
    let subscribers = renderer.pointer("/flexColumns/1/musicResponsiveListItemFlexColumnRenderer/text/runs/0/text").and_then(Value::as_str).map(str::to_owned);
    Some(MediaItem { kind: ItemKind::Artist, id, title: name, subscribers, thumb: last_thumbnail(renderer, "/thumbnail/musicThumbnailRenderer/thumbnail/thumbnails"), ..MediaItem::default() })
}

#[cfg(test)]
mod tests {
    use super::*;
    use ytmusicapi::YTMusicClient;

    #[test]
    fn parses_album_card() {
        let entry = json!({ "musicTwoRowItemRenderer": {
            "title": { "runs": [{ "text": "Sorry I'm Not From Winnipeg" }] },
            "navigationEndpoint": { "browseEndpoint": { "browseId": "MPREb_peNcc6DGRcS" } },
            "subtitle": { "runs": [{ "text": "Album" }, { "text": " • " }, { "text": "Status: Expunged", "navigationEndpoint": { "browseEndpoint": { "browseId": "UCx" } } }, { "text": " • " }, { "text": "2024" }] },
            "thumbnailRenderer": { "musicThumbnailRenderer": { "thumbnail": { "thumbnails": [{ "url": "a" }, { "url": "b" }] } } }
        }});
        let item = parse_two_row(&entry, ItemKind::Album).unwrap();
        assert_eq!(item.id, "MPREb_peNcc6DGRcS");
        assert_eq!(item.artists_text(), "Status: Expunged");
        assert_eq!(item.year.as_deref(), Some("2024"));
        assert_eq!(item.thumb.as_deref(), Some("b"));
    }

    #[test]
    fn parses_playlist_card_count() {
        let entry = json!({ "musicTwoRowItemRenderer": {
            "title": { "runs": [{ "text": "Mix" }] },
            "navigationEndpoint": { "browseEndpoint": { "browseId": "VLPLabc" } },
            "subtitle": { "runs": [{ "text": "Playlist" }, { "text": " • " }, { "text": "Me" }, { "text": " • " }, { "text": "12 songs" }] }
        }});
        let item = parse_two_row(&entry, ItemKind::Playlist).unwrap();
        assert_eq!(item.id, "PLabc");
        assert_eq!(item.count.as_deref(), Some("12"));
        assert_eq!(item.artists_text(), "Me");
    }

    /// Prints every playlist grid entry's title and endpoint. `cargo test -- --ignored live_playlist_entries --nocapture`
    /// `cargo test -- --ignored live_continuation --nocapture`
    #[tokio::test]
    #[ignore]
    async fn live_continuation() {
        let auth = ytmusicapi::BrowserAuth::from_file(std::env::var("HOME").unwrap() + "/.local/share/muse/headers_auth.json").unwrap();
        let api = YTMusicClient::builder().with_browser_auth(auth).build().unwrap();
        let response = api.post("browse", json!({ "browseId": "FEmusic_liked_playlists" })).await.unwrap();
        let grid = crate::net::items::library_sections(&response).first().and_then(|s| s.get("gridRenderer")).unwrap();
        println!("grid keys {:?}", grid.as_object().map(|o| o.keys().cloned().collect::<Vec<_>>()));
        let items = grid.get("items").and_then(Value::as_array).unwrap();
        println!("items {} last keys {:?}", items.len(), items.last().and_then(Value::as_object).map(|o| o.keys().cloned().collect::<Vec<_>>()));
        if let Some(c) = grid.get("continuations") { println!("continuations {}", serde_json::to_string(c).unwrap().chars().take(300).collect::<String>()); }
        let token = items.last().and_then(|i| i.pointer("/continuationItemRenderer/continuationEndpoint/continuationCommand/token")).and_then(Value::as_str).map(str::to_owned)
            .or_else(|| grid.pointer("/continuations/0/nextContinuationData/continuation").and_then(Value::as_str).map(str::to_owned));
        println!("token {:?}", token.as_ref().map(|t| t.len()));
        if let Some(token) = token {
            let next = api.post("browse", json!({ "continuation": token })).await.unwrap();
            println!("next top keys {:?}", next.as_object().map(|o| o.keys().cloned().collect::<Vec<_>>()));
            for path in ["/onResponseReceivedActions/0/appendContinuationItemsAction/continuationItems", "/continuationContents/gridContinuation/items"] {
                if let Some(arr) = next.pointer(path).and_then(Value::as_array) {
                    println!("{path}: {} items; titles {:?}", arr.len(), arr.iter().filter_map(|i| i.pointer("/musicTwoRowItemRenderer/title/runs/0/text").and_then(Value::as_str)).take(6).collect::<Vec<_>>());
                    println!("last keys {:?}", arr.last().and_then(Value::as_object).map(|o| o.keys().cloned().collect::<Vec<_>>()));
                }
            }
        }
    }

    /// Do two fetches in a row return the same card addresses?
    /// `cargo test -- --ignored live_card_stability --nocapture`
    #[tokio::test]
    #[ignore]
    async fn live_card_stability() {
        let paths = crate::paths::Paths::discover();
        let client = crate::net::ytmusic::YtMusic::new(&paths).unwrap();
        let api: std::sync::Arc<dyn crate::net::browse::Browse> = client.api();
        let first = library_playlists(api.clone()).await.unwrap();
        tokio::time::sleep(std::time::Duration::from_secs(2)).await;
        let second = library_playlists(api).await.unwrap();

        let mut same_url = 0;
        let mut same_path = 0;
        let mut identical = 0;
        for (a, b) in first.iter().zip(second.iter()) {
            if a.thumb == b.thumb {
                same_url += 1;
            }
            let base = |t: &Option<String>| t.as_deref().unwrap_or("").split('?').next().unwrap_or("").to_owned();
            if base(&a.thumb) == base(&b.thumb) {
                same_path += 1;
            }
            if a == b {
                identical += 1;
            }
        }
        println!("{} cards: {same_url} same address, {same_path} same path, {identical} equal as items", first.len());
        for card in second.iter().filter(|c| c.thumb.as_deref().is_some_and(|t| t.contains("/pl_c/"))).take(1) {
            let signed = card.thumb.clone().unwrap();
            let bare = signed.split('?').next().unwrap().to_owned();
            let http = client.http();
            for url in [signed, bare] {
                let status = http.get(&url).send().await.map(|r| r.status().as_u16()).unwrap_or(0);
                println!("HTTP {status} for {}", if url.contains('?') { "signed" } else { "unsigned" });
            }
        }
        if let (Some(a), Some(b)) = (first.first(), second.first()) {
            println!("first card before: {:?}", a.thumb);
            println!("first card after:  {:?}", b.thumb);
        }
    }

    /// What a library card carries, for the ownership decision.
    /// `cargo test -- --ignored live_library_cards --nocapture`
    #[tokio::test]
    #[ignore]
    async fn live_library_cards() {
        let paths = crate::paths::Paths::discover();
        let client = crate::net::ytmusic::YtMusic::new(&paths).unwrap();
        let api: std::sync::Arc<dyn crate::net::browse::Browse> = client.api();
        let playlists = library_playlists(api).await.unwrap();
        println!("account: {:?}", client.auth_state());
        for p in playlists.iter().take(12) {
            println!("{:<26} id={:<24} artists={:?} desc={:?}", p.title.chars().take(24).collect::<String>(), p.id, p.artists.iter().map(|a| a.name.clone()).collect::<Vec<_>>(), p.description);
        }
    }

    /// `cargo test -- --ignored live_library --nocapture`
    #[tokio::test]
    #[ignore]
    async fn live_library() {
        let auth = ytmusicapi::BrowserAuth::from_file(std::env::var("HOME").unwrap() + "/.local/share/muse/headers_auth.json").unwrap();
        let api = Arc::new(YTMusicClient::builder().with_browser_auth(auth).build().unwrap());
        let playlists = library_playlists(api.clone()).await.unwrap();
        for p in playlists.iter().take(4) {
            println!("playlist {} {} count={:?} desc={:?}", p.id, p.title, p.count, p.description);
        }
        let albums = library_albums(api.clone()).await.unwrap();
        println!("albums {} first {:?}", albums.len(), albums.first().map(|a| (&a.title, a.artists_text(), &a.year)));
        let artists = library_subscriptions(api.clone()).await.unwrap();
        println!("artists {} first {:?}", artists.len(), artists.first().map(|a| (&a.title, &a.subscribers)));
        let liked = crate::net::playlists::get_playlist(&api, "LM", Some(5)).await.unwrap();
        println!("liked {} tracks of {:?}: {:?}", liked.tracks.len(), liked.track_count, liked.tracks.first().map(|t| &t.title));
    }
}

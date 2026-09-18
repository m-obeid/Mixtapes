//! The uploaded music library: sending files to it, reading an artist's
//! uploads, and removing what is there.
//!
//! Port of ytmusicapi's uploads mixin. The upload itself is a resumable POST
//! to upload.youtube.com with the browser session, the same shape as the
//! playlist cover upload; everything else is an InnerTube call.

use std::collections::BTreeMap;
use std::path::Path;

use serde_json::json;

use crate::model::Track;

use super::browse::{Browse, Continuation};
use super::items::{array_at, next_continuation, parse_uploaded_items};
use super::ytmusic::NetError;

/// What YouTube Music accepts.
pub const SUPPORTED: [&str; 5] = ["mp3", "m4a", "wma", "flac", "ogg"];

/// The size YouTube refuses beyond, 300MB.
const SIZE_LIMIT: u64 = 314_572_800;

/// Send one file to the uploaded library.
///
/// Two steps: ask upload.youtube.com where to put it, then put it there. The
/// answer is only a status, so the song appears in the library a moment later,
/// once YouTube has processed it.
pub async fn upload_song(http: &reqwest::Client, headers: &BTreeMap<String, String>, file: &Path) -> Result<(), NetError> {
    let extension = file.extension().and_then(|e| e.to_str()).unwrap_or_default().to_lowercase();
    if !SUPPORTED.contains(&extension.as_str()) {
        return Err(NetError::Message(format!("YouTube Music does not take {extension} files. It takes {}.", SUPPORTED.join(", "))));
    }
    let name = file.file_name().and_then(|n| n.to_str()).unwrap_or("upload").to_owned();
    let size = tokio::fs::metadata(file).await?.len();
    if size >= SIZE_LIMIT {
        return Err(NetError::Message(format!("{name} is larger than the 300MB limit.")));
    }

    let authuser = headers.get("X-Goog-AuthUser").map(String::as_str).unwrap_or("0");
    let session = |mut request: reqwest::RequestBuilder| {
        for (key, value) in headers.iter().filter(|(key, _)| !matches!(key.as_str(), "Content-Type" | "Content-Encoding" | "Content-Length")) {
            request = request.header(key, value);
        }
        request.header("Content-Type", "application/x-www-form-urlencoded;charset=utf-8")
    };

    let start = session(http.post(format!("https://upload.youtube.com/upload/usermusic/http?authuser={authuser}")))
        .header("X-Goog-Upload-Command", "start")
        .header("X-Goog-Upload-Protocol", "resumable")
        .header("X-Goog-Upload-Header-Content-Length", size.to_string())
        .body(format!("filename={name}"))
        .send()
        .await?;
    let upload_url = start
        .headers()
        .get("X-Goog-Upload-URL")
        .and_then(|value| value.to_str().ok())
        .map(str::to_owned)
        .ok_or_else(|| NetError::Message(format!("the upload was refused: HTTP {}", start.status())))?;

    let bytes = tokio::fs::read(file).await?;
    let finish = session(http.post(&upload_url))
        .header("X-Goog-Upload-Command", "upload, finalize")
        .header("X-Goog-Upload-Offset", "0")
        .body(bytes)
        .send()
        .await?;
    let status = finish.status();
    let body = finish.text().await.unwrap_or_default();
    tracing::debug!(%status, body = %body.chars().take(200).collect::<String>(), "upload answer");
    match status.is_success() {
        true => Ok(()),
        false => Err(NetError::Message(format!("the upload failed: HTTP {status}"))),
    }
}

/// Remove an uploaded song or album.
pub async fn delete_entity(api: &dyn Browse, entity_id: &str) -> Result<(), NetError> {
    api.post("music/delete_privately_owned_entity", json!({ "entityId": entity_key(entity_id) })).await?;
    Ok(())
}

/// An album's entity id arrives wrapped in its browse id. Only the key counts.
fn entity_key(entity_id: &str) -> &str {
    entity_id.trim_start_matches("FEmusic_library_privately_owned_release_detail")
}

/// The uploaded songs of one artist, following continuations to `limit`.
pub async fn artist_songs(api: &dyn Browse, browse_id: &str, limit: usize) -> Result<Vec<Track>, NetError> {
    let response = api.post("browse", json!({ "browseId": browse_id })).await?;
    let Some(shelf) = super::items::library_sections(&response).iter().find_map(|section| section.get("musicShelfRenderer")) else { return Ok(Vec::new()) };
    let mut songs = parse_uploaded_items(array_at(shelf, "/contents"));
    let rest = Continuation::browse(api, next_continuation(shelf))
        .limit(limit.saturating_sub(songs.len()))
        .collect(|entries| parse_uploaded_items(entries.iter().copied()))
        .await;
    songs.extend(rest.items);
    Ok(songs)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn an_album_entity_id_sheds_its_browse_prefix() {
        assert_eq!(entity_key("FEmusic_library_privately_owned_release_detailb_po_12345"), "b_po_12345");
        assert_eq!(entity_key("t_po_98765"), "t_po_98765");
    }

    #[test]
    fn only_the_formats_youtube_takes_are_offered() {
        assert!(SUPPORTED.contains(&"flac"));
        assert!(!SUPPORTED.contains(&"wav"), "wav is not on YouTube's list");
    }

    /// Uploads a short file, waits for it to appear, then deletes it.
    /// `cargo test -- --ignored live_upload_round_trip --nocapture`
    #[tokio::test]
    #[ignore]
    async fn live_upload_round_trip() {
        let paths = crate::paths::Paths::discover();
        let client = crate::net::ytmusic::YtMusic::new(&paths).unwrap();
        let api: std::sync::Arc<dyn Browse> = client.api();
        let headers = client.browser_headers().expect("a signed in session");

        let title = format!("Mixtapes upload test {}", std::process::id());
        let file = std::env::temp_dir().join(format!("{title}.mp3"));
        let made = std::process::Command::new("ffmpeg")
            .args(["-y", "-f", "lavfi", "-i", "sine=frequency=440:duration=3", "-metadata"])
            .arg(format!("title={title}"))
            .args(["-metadata", "artist=Mixtapes Test"])
            .arg(&file)
            .stdout(std::process::Stdio::null())
            .stderr(std::process::Stdio::null())
            .status();
        if !made.map(|s| s.success()).unwrap_or(false) {
            println!("ffmpeg missing, skipping");
            return;
        }

        upload_song(client.http(), &headers, &file).await.expect("upload");
        println!("uploaded {}", file.display());

        // YouTube takes a while to process an upload before it is listed.
        // YouTube transcodes an upload before listing it, which takes minutes.
        let mut found = None;
        for attempt in 0..20 {
            tokio::time::sleep(std::time::Duration::from_secs(15)).await;
            let songs = crate::net::playlists::get_upload_songs(&api).await.unwrap_or_default();
            found = songs.into_iter().find(|song| song.title == title);
            if found.is_some() {
                println!("listed after about {} seconds", (attempt + 1) * 15);
                break;
            }
        }
        let _ = std::fs::remove_file(&file);

        let song = found.expect("the upload shows in the library");
        println!("song: {} - {} entity={:?}", song.artist, song.title, song.entity_id);
        let entity = song.entity_id.clone().expect("an uploaded song carries an entity id");
        delete_entity(&api, &entity).await.expect("delete");

        tokio::time::sleep(std::time::Duration::from_secs(5)).await;
        let after = crate::net::playlists::get_upload_songs(&api).await.unwrap_or_default();
        assert!(!after.iter().any(|s| s.title == title), "and it is gone once deleted");
        println!("round trip complete, nothing left behind");
    }






    /// Removes uploads left behind by a test run.
    /// `cargo test -- --ignored live_clean_test_uploads --nocapture`
    #[tokio::test]
    #[ignore]
    async fn live_clean_test_uploads() {
        let paths = crate::paths::Paths::discover();
        let client = crate::net::ytmusic::YtMusic::new(&paths).unwrap();
        let api: std::sync::Arc<dyn Browse> = client.api();
        let songs = crate::net::playlists::get_upload_songs(&api).await.expect("upload songs");
        for song in songs.iter().filter(|song| song.artist == "Mixtapes Test") {
            let Some(entity) = song.entity_id.clone() else { continue };
            match delete_entity(&api, &entity).await {
                Ok(()) => println!("deleted {} - {}", song.artist, song.title),
                Err(err) => println!("could not delete {}: {err}", song.title),
            }
        }
    }

}

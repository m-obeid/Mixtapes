//! Port of save_playlist_cover_async: mirror a playlist's cover under the
//! music folder so later opens render it at once and offline. A `.url`
//! sidecar records which cover the bytes came from, so an edit on either
//! side re-downloads instead of waiting out the freshness window.

use std::path::{Path, PathBuf};
use std::time::{Duration, SystemTime};

use crate::model::HttpAuth;

const COVER_FRESHNESS: Duration = Duration::from_secs(24 * 60 * 60);

/// The cover minus its signing query, what the sidecar stores.
fn cover_identity(url: &str) -> &str {
    url.split('?').next().unwrap_or(url)
}

/// Download `url` to `path` unless a fresh copy of the same cover is there.
/// Video thumbnails are skipped, they are not playlist art.
/// Returns whether the file on disk was replaced, which tells the caller the
/// picture behind that path changed.
pub async fn save_playlist_cover(http: reqwest::Client, auth: Option<HttpAuth>, path: PathBuf, url: String) -> bool {
    if url.contains("i.ytimg.com/vi/") {
        return false;
    }
    match mirror(&http, auth.as_ref(), &path, &url).await {
        Ok(replaced) => replaced,
        Err(err) => {
            tracing::debug!(%err, ?path, "cover mirror failed");
            false
        }
    }
}

/// Record that a mirrored cover stands in for `url`.
///
/// A cover the listener just set is already the right picture, so the mirror
/// must not replace it with the one it is replacing. Once YouTube serves the
/// new image its address differs from this note and the mirror refreshes.
pub async fn mark_mirror(path: &Path, url: &str) {
    if url.is_empty() {
        return;
    }
    let sidecar = path.with_extension("jpg.url");
    if let Err(err) = tokio::fs::write(&sidecar, cover_identity(url)).await {
        tracing::debug!(%err, ?sidecar, "cover sidecar not written");
    }
}

async fn mirror(http: &reqwest::Client, auth: Option<&HttpAuth>, path: &Path, url: &str) -> anyhow::Result<bool> {
    let Some(dir) = path.parent() else { return Ok(false) };
    tokio::fs::create_dir_all(dir).await?;
    let sidecar = path.with_extension("jpg.url");
    if let Ok(meta) = tokio::fs::metadata(path).await {
        let fresh = meta.modified().ok().and_then(|m| SystemTime::now().duration_since(m).ok()).is_some_and(|age| age < COVER_FRESHNESS);
        if fresh {
            let saved = tokio::fs::read_to_string(&sidecar).await.ok();
            if saved.as_deref().map(str::trim) == Some(cover_identity(url)) {
                return Ok(false);
            }
        }
    }
    let mut request = http.get(url).header("User-Agent", "Mozilla/5.0");
    // Custom playlist covers need the signed-in cookie.
    if let Some(auth) = auth.filter(|_| url.contains("/pl_c/")) {
        request = request.header("Cookie", &auth.cookie);
    }
    let response = request.send().await?.error_for_status()?;
    let bytes = response.bytes().await?;
    if bytes.is_empty() {
        anyhow::bail!("empty cover");
    }
    let tmp = path.with_extension("jpg.tmp");
    tokio::fs::write(&tmp, &bytes).await?;
    tokio::fs::rename(&tmp, path).await?;
    tokio::fs::write(&sidecar, cover_identity(url)).await?;
    Ok(true)
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The cover a listener just set must survive the mirror that runs on the
    /// next page load, which is what made a new cover look like it did nothing.
    #[tokio::test]
    async fn a_marked_mirror_is_left_alone_while_the_address_holds() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("My Mix.jpg");
        std::fs::write(&path, b"the picture the listener chose").unwrap();
        let url = "https://i.ytimg.com/pl_c/ABC/studio_square_thumbnail.jpg?sqp=signature";
        mark_mirror(&path, url).await;

        // A mirror of the same cover, signature and all, leaves the file be.
        let http = reqwest::Client::new();
        mirror(&http, None, &path, url).await.expect("same cover");
        assert_eq!(std::fs::read(&path).unwrap(), b"the picture the listener chose");
    }

    #[test]
    fn the_signature_is_not_part_of_a_cover_identity() {
        assert_eq!(cover_identity("https://host/a.jpg?sqp=one"), cover_identity("https://host/a.jpg?sqp=two"));
        assert_ne!(cover_identity("https://host/a.jpg"), cover_identity("https://host/b.jpg"));
    }
}

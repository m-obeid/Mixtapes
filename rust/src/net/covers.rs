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
pub async fn save_playlist_cover(http: reqwest::Client, auth: Option<HttpAuth>, path: PathBuf, url: String) {
    if url.contains("i.ytimg.com/vi/") {
        return;
    }
    if let Err(err) = mirror(&http, auth.as_ref(), &path, &url).await {
        tracing::debug!(%err, ?path, "cover mirror failed");
    }
}

async fn mirror(http: &reqwest::Client, auth: Option<&HttpAuth>, path: &Path, url: &str) -> anyhow::Result<()> {
    let Some(dir) = path.parent() else { return Ok(()) };
    tokio::fs::create_dir_all(dir).await?;
    let sidecar = path.with_extension("jpg.url");
    if let Ok(meta) = tokio::fs::metadata(path).await {
        let fresh = meta.modified().ok().and_then(|m| SystemTime::now().duration_since(m).ok()).is_some_and(|age| age < COVER_FRESHNESS);
        if fresh {
            let saved = tokio::fs::read_to_string(&sidecar).await.ok();
            if saved.as_deref().map(str::trim) == Some(cover_identity(url)) {
                return Ok(());
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
    Ok(())
}

//! In-memory caches MusicClient kept: full playlist track lists, the
//! view-count and date-added maps behind the sort dropdown, and the ids of
//! everything saved in the library for the Add to Library toggle. Shared by
//! the GTK thread and tokio tasks, hence the mutexes.

use std::collections::{HashMap, HashSet};
use std::path::{Path, PathBuf};
use std::sync::Mutex;

use serde::{Deserialize, Serialize};

use crate::model::{MediaItem, Person, Track};
use crate::paths::Paths;

#[derive(Debug, Clone, Default)]
pub struct LibraryIds {
    pub playlists: HashSet<String>,
    pub albums: HashSet<String>,
}

impl LibraryIds {
    /// Port of MusicClient.is_in_library's lookup once the cache is warm.
    pub fn contains(&self, id: &str) -> bool {
        let pid = id.strip_prefix("VL").unwrap_or(id);
        self.playlists.contains(pid) || self.albums.contains(pid)
    }
}

/// `{videoId: number}` behind a metric sort.
pub type SortMetric = HashMap<String, i64>;

const ALBUM_TRACKS_FILE: &str = "album_track_counts.json";

fn read_album_tracks(path: &std::path::Path) -> HashMap<String, u32> {
    std::fs::read(path).ok().and_then(|bytes| serde_json::from_slice(&bytes).ok()).unwrap_or_default()
}

pub struct Caches {
    disk: PlaylistDiskCache,
    playlist_tracks: Mutex<HashMap<String, Vec<Track>>>,
    sort_metrics: Mutex<HashMap<(String, String), SortMetric>>,
    library_ids: Mutex<Option<LibraryIds>>,
    library_playlists: Mutex<Vec<MediaItem>>,
    subscribed_artists: Mutex<HashSet<String>>,
    /// Track counts of albums that were opened, by browse id. Saved to disk.
    album_tracks: Mutex<HashMap<String, u32>>,
    album_tracks_file: std::path::PathBuf,
}

impl Caches {
    pub fn new(paths: &Paths) -> Self {
        Self { disk: PlaylistDiskCache::new(paths), playlist_tracks: Mutex::default(), sort_metrics: Mutex::default(), library_ids: Mutex::default(), library_playlists: Mutex::default(), subscribed_artists: Mutex::default(), album_tracks: Mutex::new(read_album_tracks(&paths.data_dir.join(ALBUM_TRACKS_FILE))), album_tracks_file: paths.data_dir.join(ALBUM_TRACKS_FILE) }
    }

    /// "Single", "EP" or "Album" by track count, the rule the album page uses.
    pub fn release_kind(track_count: u32) -> &'static str {
        match track_count {
            1 => "Single",
            2..=6 => "EP",
            _ => "Album",
        }
    }

    /// The label for an album card. YouTube files a six-track EP under
    /// "Single" on artist pages, so a count learned from the album itself wins.
    pub fn release_kind_for(&self, album_id: &str) -> Option<&'static str> {
        self.album_tracks.lock().unwrap().get(album_id).copied().map(Self::release_kind)
    }

    pub fn set_album_track_count(&self, album_id: &str, track_count: u32) {
        let snapshot = {
            let mut counts = self.album_tracks.lock().unwrap();
            if track_count == 0 || counts.insert(album_id.to_owned(), track_count) == Some(track_count) {
                return;
            }
            counts.clone()
        };
        match serde_json::to_vec(&snapshot) {
            Ok(bytes) => {
                if let Err(err) = std::fs::write(&self.album_tracks_file, bytes) {
                    tracing::debug!(%err, "album track counts not saved");
                }
            }
            Err(err) => tracing::debug!(%err, "album track counts not encoded"),
        }
    }

    /// The on-disk playlist store, what DownloadDB's library_cache table was.
    pub fn disk(&self) -> &PlaylistDiskCache {
        &self.disk
    }

    pub fn cached_tracks(&self, playlist_id: &str) -> Option<Vec<Track>> {
        self.playlist_tracks.lock().unwrap().get(playlist_id).cloned()
    }

    pub fn set_cached_tracks(&self, playlist_id: &str, tracks: Vec<Track>) {
        self.playlist_tracks.lock().unwrap().insert(playlist_id.to_owned(), tracks);
    }

    pub fn drop_cached_tracks(&self, playlist_id: &str) {
        self.playlist_tracks.lock().unwrap().remove(playlist_id);
    }

    pub fn sort_metric(&self, kind: &str, browse_id: &str) -> Option<SortMetric> {
        self.sort_metrics.lock().unwrap().get(&(kind.to_owned(), browse_id.to_owned())).cloned()
    }

    pub fn set_sort_metric(&self, kind: &str, browse_id: &str, metric: SortMetric) {
        self.sort_metrics.lock().unwrap().insert((kind.to_owned(), browse_id.to_owned()), metric);
    }

    pub fn drop_sort_metrics(&self, browse_id: &str) {
        self.sort_metrics.lock().unwrap().retain(|(_, b), _| b != browse_id);
    }

    pub fn library_ids(&self) -> Option<LibraryIds> {
        self.library_ids.lock().unwrap().clone()
    }

    pub fn set_library_ids(&self, ids: LibraryIds) {
        self.library_ids.lock().unwrap().replace(ids);
    }

    /// Forget the saved ids so the next check refetches, as after a rating change.
    pub fn clear_library_ids(&self) {
        self.library_ids.lock().unwrap().take();
    }

    /// The library playlists from the last fetch, what get_editable_playlists filtered.
    pub fn library_playlists(&self) -> Vec<MediaItem> {
        self.library_playlists.lock().unwrap().clone()
    }

    pub fn set_library_playlists(&self, playlists: Vec<MediaItem>) {
        *self.library_playlists.lock().unwrap() = playlists;
    }

    /// Port of is_subscribed_artist against the local subscription set.
    pub fn is_subscribed(&self, channel_id: &str) -> bool {
        self.subscribed_artists.lock().unwrap().contains(channel_id)
    }

    pub fn set_subscribed(&self, channel_id: &str, subscribed: bool) {
        let mut set = self.subscribed_artists.lock().unwrap();
        if subscribed {
            set.insert(channel_id.to_owned());
        } else {
            set.remove(channel_id);
        }
    }

    pub fn add_subscriptions(&self, ids: impl IntoIterator<Item = String>) {
        self.subscribed_artists.lock().unwrap().extend(ids);
    }
}

// -- on-disk playlist cache -----------------------------------------------

/// Header fields beyond title and author, what DownloadDB stored as meta_json.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct CachedMeta {
    #[serde(default)]
    pub description: String,
    #[serde(default)]
    pub year: Option<String>,
    #[serde(default)]
    pub privacy: Option<String>,
    #[serde(default)]
    pub author_raw: Vec<Person>,
    #[serde(default)]
    pub collaborators: Option<String>,
    #[serde(default)]
    pub thumbnails: Vec<String>,
    #[serde(default)]
    pub duration_seconds: Option<u32>,
    #[serde(default)]
    pub audio_playlist_id: Option<String>,
    #[serde(default)]
    pub album_type: Option<String>,
}

/// One playlist or album as the page last saw it, rendered on the next open
/// before the live fetch and used as the whole page offline.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct CachedPlaylist {
    pub playlist_id: String,
    pub title: String,
    #[serde(default)]
    pub author: String,
    #[serde(default)]
    pub track_count: Option<u32>,
    #[serde(default)]
    pub last_synced: String,
    #[serde(default)]
    pub tracks: Vec<Track>,
    #[serde(default)]
    pub meta: CachedMeta,
}

/// Port of DownloadDB.cache_playlist, get_cached_playlist and
/// invalidate_playlist_cache as one JSON file per playlist under the data
/// directory. Blocking file IO: call from a blocking task.
pub struct PlaylistDiskCache {
    dir: PathBuf,
}

impl PlaylistDiskCache {
    pub fn new(paths: &Paths) -> Self {
        Self { dir: paths.data_dir.join("playlist_cache") }
    }

    fn path(&self, playlist_id: &str) -> PathBuf {
        let safe: String = playlist_id.chars().map(|c| if c.is_ascii_alphanumeric() || c == '-' || c == '_' { c } else { '_' }).collect();
        self.dir.join(format!("{safe}.json"))
    }

    fn rejects(playlist_id: &str) -> bool {
        // A mix that changes on every fetch is never cached.
        playlist_id.is_empty() || playlist_id.starts_with("RDTMAK")
    }

    pub fn get(&self, playlist_id: &str) -> Option<CachedPlaylist> {
        if Self::rejects(playlist_id) {
            return None;
        }
        let text = std::fs::read_to_string(self.path(playlist_id)).ok()?;
        serde_json::from_str(&text).ok()
    }

    /// Whether a cached copy with rows exists, what the library's offline greying asks.
    pub fn has_tracks(&self, playlist_id: &str) -> bool {
        self.get(playlist_id).is_some_and(|c| !c.tracks.is_empty())
    }

    /// Write unless it would replace a richer copy with a partial fetch.
    pub fn put(&self, entry: &CachedPlaylist) {
        if Self::rejects(&entry.playlist_id) {
            return;
        }
        let existing_count = self.get(&entry.playlist_id).map(|c| c.tracks.len()).unwrap_or(0);
        let new_count = entry.tracks.len();
        let is_full_fetch = entry.track_count.is_some_and(|c| new_count as u32 >= c);
        if existing_count > new_count && !is_full_fetch {
            tracing::info!(playlist_id = %entry.playlist_id, new_count, existing_count, "cache_playlist: skipping regression");
            return;
        }
        if let Err(err) = self.write(entry) {
            tracing::warn!(%err, playlist_id = %entry.playlist_id, "playlist cache write failed");
        }
    }

    fn write(&self, entry: &CachedPlaylist) -> std::io::Result<()> {
        std::fs::create_dir_all(&self.dir)?;
        let path = self.path(&entry.playlist_id);
        let tmp = path.with_extension("json.tmp");
        std::fs::write(&tmp, serde_json::to_vec(entry)?)?;
        std::fs::rename(&tmp, &path)
    }

    pub fn invalidate(&self, playlist_id: &str) {
        if playlist_id.is_empty() {
            return;
        }
        let _ = std::fs::remove_file(self.path(playlist_id));
    }

    #[allow(dead_code)]
    pub fn dir(&self) -> &Path {
        &self.dir
    }
}

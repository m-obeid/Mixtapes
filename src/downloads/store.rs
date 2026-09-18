//! The download library: one SQLite table of what is on disk.
//!
//! The file is `<music>/.mixtapes/library.db`, the same database the Python
//! app writes, so a song downloaded in either app is known to both. The
//! `downloads` table keeps Python's columns exactly.
//!
//! Rows are read from any thread. `is_downloaded` answers from an in-memory
//! map because the UI asks it once per row while a list binds.

use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::sync::{Mutex, RwLock};

use rusqlite::Connection;

use crate::model::{LikeStatus, Named, Person, Track, VideoId};

/// One downloaded track as the database holds it.
#[derive(Debug, Clone, Default)]
pub struct Entry {
    pub video_id: String,
    pub title: String,
    pub artist: String,
    pub artist_id: String,
    pub album: String,
    pub album_id: String,
    pub track_number: Option<u32>,
    pub duration_seconds: Option<u32>,
    pub file_path: PathBuf,
    pub thumbnail_url: String,
    pub downloaded_at: String,
    pub file_size: i64,
    pub format: String,
    pub like_status: LikeStatus,
}

impl Entry {
    /// The row as the rest of the app sees tracks.
    pub fn track(&self) -> Track {
        Track {
            video_id: VideoId(self.video_id.clone()),
            title: self.title.clone(),
            artist: self.artist.clone(),
            artists: self.artist.split(',').map(str::trim).filter(|a| !a.is_empty()).map(|name| Person { name: name.to_owned(), id: (!self.artist_id.is_empty()).then(|| self.artist_id.clone()) }).collect(),
            album: (!self.album.is_empty()).then(|| Named { name: self.album.clone(), id: (!self.album_id.is_empty()).then(|| self.album_id.clone()) }),
            thumb: (!self.thumbnail_url.is_empty()).then(|| self.thumbnail_url.clone()),
            duration_seconds: self.duration_seconds,
            like_status: self.like_status,
            ..Track::default()
        }
    }
}

/// The folder name a pre-rename release used.
const LEGACY_DIR: &str = "YouTube Music";

/// Rename `~/Music/YouTube Music` to the current folder, once.
///
/// Port of _maybe_migrate_legacy_music_dir. Only a folder holding our own
/// library.db is touched, and only when the new one is not already in use.
/// Rows keep absolute paths, so they are rewritten to match.
pub fn migrate_legacy_folder(music_dir: &Path) {
    let Some(parent) = music_dir.parent() else { return };
    let legacy = parent.join(LEGACY_DIR);
    if !legacy.join(".mixtapes").join("library.db").is_file() {
        return;
    }
    if music_dir.exists() {
        if music_dir.join(".mixtapes").join("library.db").is_file() {
            tracing::warn!(legacy = %legacy.display(), "both music folders hold a library, leaving the old one");
            return;
        }
        // A stub an earlier launch created, with no library in it.
        let _ = std::fs::remove_dir_all(music_dir);
    }
    if let Err(err) = std::fs::rename(&legacy, music_dir) {
        tracing::warn!(%err, from = %legacy.display(), "could not rename the music folder");
        return;
    }
    tracing::info!(from = %legacy.display(), to = %music_dir.display(), "renamed the music folder");
    rewrite_paths(music_dir, &legacy);
    // The same release recased `playlists`, distinct from `Playlists` on Linux.
    let (old, new) = (music_dir.join("playlists"), music_dir.join("Playlists"));
    if old.is_dir() && !new.exists() {
        let _ = std::fs::rename(old, new);
    }
}

/// Point rows at the renamed folder.
fn rewrite_paths(music_dir: &Path, old_root: &Path) {
    let db_path = music_dir.join(".mixtapes").join("library.db");
    let Ok(db) = Connection::open(&db_path) else { return };
    let old_prefix = format!("{}/", old_root.to_string_lossy());
    let new_prefix = format!("{}/", music_dir.to_string_lossy());
    for column in ["file_path", "cover_path"] {
        let sql = format!("UPDATE downloads SET {column} = ?1 || substr({column}, ?2) WHERE substr({column}, 1, ?3) = ?4");
        if let Err(err) = db.execute(&sql, rusqlite::params![new_prefix, old_prefix.len() as i64 + 1, old_prefix.len() as i64, old_prefix]) {
            tracing::warn!(%err, column, "path rewrite failed");
        }
    }
}

pub struct Store {
    db: Mutex<Option<Connection>>,
    path: PathBuf,
    /// video id to file path, seeded at start and kept in step with writes.
    known: RwLock<HashMap<String, PathBuf>>,
}

impl Store {
    /// Open the library, creating the folder and table when they are missing.
    pub fn open(music_dir: &Path) -> Self {
        migrate_legacy_folder(music_dir);
        let path = music_dir.join(".mixtapes").join("library.db");
        let store = Self { db: Mutex::new(None), path, known: RwLock::new(HashMap::new()) };
        store.with_db(|db| {
            db.execute_batch(SCHEMA)?;
            let mut stmt = db.prepare("SELECT video_id, file_path FROM downloads WHERE file_path IS NOT NULL")?;
            let rows = stmt.query_map([], |row| Ok((row.get::<_, String>(0)?, row.get::<_, String>(1)?)))?;
            let mut known = store.known.write().unwrap();
            for row in rows.flatten() {
                if !row.0.is_empty() && !row.1.is_empty() {
                    known.insert(row.0, PathBuf::from(row.1));
                }
            }
            Ok(())
        });
        tracing::info!(count = store.known.read().unwrap().len(), path = %store.path.display(), "download library");
        store
    }

    /// Whether the track has a file on disk.
    ///
    /// A row whose file was deleted behind the app's back is dropped here, so
    /// the answer never outlives the file.
    pub fn is_downloaded(&self, video_id: &str) -> bool {
        let path = self.known.read().unwrap().get(video_id).cloned();
        let Some(path) = path else { return false };
        if path.exists() {
            return true;
        }
        self.forget(video_id);
        false
    }

    /// The file to play, when one is there.
    pub fn local_path(&self, video_id: &str) -> Option<PathBuf> {
        self.is_downloaded(video_id).then(|| self.known.read().unwrap().get(video_id).cloned()).flatten()
    }

    pub fn count(&self) -> usize {
        self.known.read().unwrap().len()
    }

    /// Which track owns a file, for the name clash check.
    pub fn owner_of(&self, file_path: &Path) -> Option<String> {
        let wanted = file_path.to_string_lossy().into_owned();
        self.known.read().unwrap().iter().find(|(_, path)| path.to_string_lossy() == wanted).map(|(id, _)| id.clone())
    }

    /// One track's row, when its file is still there.
    pub fn entry(&self, video_id: &str) -> Option<Entry> {
        if !self.is_downloaded(video_id) {
            return None;
        }
        self.all().into_iter().find(|e| e.video_id == video_id)
    }

    /// Everything downloaded, newest first, the order the Downloads page shows.
    pub fn all(&self) -> Vec<Entry> {
        let mut out = Vec::new();
        self.with_db(|db| {
            let mut stmt = db.prepare(
                "SELECT video_id, title, artist, artist_id, album, album_id, track_number, duration_seconds, \
                 file_path, thumbnail_url, downloaded_at, file_size, format, like_status \
                 FROM downloads ORDER BY downloaded_at DESC",
            )?;
            let rows = stmt.query_map([], |row| {
                Ok(Entry {
                    video_id: row.get::<_, Option<String>>(0)?.unwrap_or_default(),
                    title: row.get::<_, Option<String>>(1)?.unwrap_or_default(),
                    artist: row.get::<_, Option<String>>(2)?.unwrap_or_default(),
                    artist_id: row.get::<_, Option<String>>(3)?.unwrap_or_default(),
                    album: row.get::<_, Option<String>>(4)?.unwrap_or_default(),
                    album_id: row.get::<_, Option<String>>(5)?.unwrap_or_default(),
                    track_number: row.get::<_, Option<i64>>(6)?.map(|n| n as u32),
                    duration_seconds: row.get::<_, Option<i64>>(7)?.map(|n| n as u32),
                    file_path: PathBuf::from(row.get::<_, Option<String>>(8)?.unwrap_or_default()),
                    thumbnail_url: row.get::<_, Option<String>>(9)?.unwrap_or_default(),
                    downloaded_at: row.get::<_, Option<String>>(10)?.unwrap_or_default(),
                    file_size: row.get::<_, Option<i64>>(11)?.unwrap_or_default(),
                    format: row.get::<_, Option<String>>(12)?.unwrap_or_default(),
                    like_status: like_status_from(&row.get::<_, Option<String>>(13)?.unwrap_or_default()),
                })
            })?;
            out = rows.flatten().filter(|e| e.file_path.exists()).collect();
            Ok(())
        });
        out
    }

    /// Record a finished download.
    pub fn add(&self, entry: &Entry) {
        let path = entry.file_path.to_string_lossy().into_owned();
        self.with_db(|db| {
            db.execute(
                "INSERT OR REPLACE INTO downloads \
                 (video_id, title, artist, artist_id, album, album_id, track_number, duration_seconds, \
                  file_path, cover_path, thumbnail_url, downloaded_at, file_size, format, like_status) \
                 VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, NULL, ?10, ?11, ?12, ?13, ?14)",
                rusqlite::params![
                    entry.video_id,
                    entry.title,
                    entry.artist,
                    entry.artist_id,
                    entry.album,
                    entry.album_id,
                    entry.track_number,
                    entry.duration_seconds,
                    path,
                    entry.thumbnail_url,
                    entry.downloaded_at,
                    entry.file_size,
                    entry.format,
                    like_status_name(entry.like_status),
                ],
            )?;
            Ok(())
        });
        self.known.write().unwrap().insert(entry.video_id.clone(), entry.file_path.clone());
    }

    /// Drop a track from the library. The file is the caller's business.
    /// The cached listening history, as the JSON blob both apps keep in one
    /// row of this database. Returned raw: the shape is ytmusicapi's, and
    /// `net::history` is what knows how to read it.
    pub fn history_cache(&self) -> Option<String> {
        let mut json = None;
        self.with_db(|db| {
            db.execute_batch(HISTORY_SCHEMA)?;
            json = db.query_row("SELECT data_json FROM history_cache WHERE id = 1", [], |row| row.get::<_, String>(0)).ok();
            Ok(())
        });
        json
    }

    /// Replace the cached history. Python writes the whole list at once too.
    pub fn set_history_cache(&self, json: &str) {
        self.with_db(|db| {
            db.execute_batch(HISTORY_SCHEMA)?;
            db.execute("DELETE FROM history_cache", [])?;
            db.execute("INSERT INTO history_cache (id, data_json, last_synced) VALUES (1, ?1, ?2)", rusqlite::params![json, crate::downloads::timestamp()])?;
            Ok(())
        });
    }

    pub fn forget(&self, video_id: &str) {
        self.known.write().unwrap().remove(video_id);
        let id = video_id.to_owned();
        self.with_db(move |db| {
            db.execute("DELETE FROM downloads WHERE video_id = ?1", [&id])?;
            Ok(())
        });
    }

    /// Point a row at a file that moved.
    pub fn moved(&self, video_id: &str, new_path: &Path) {
        let path = new_path.to_string_lossy().into_owned();
        let id = video_id.to_owned();
        self.with_db(move |db| {
            db.execute("UPDATE downloads SET file_path = ?1 WHERE video_id = ?2", rusqlite::params![path, id])?;
            Ok(())
        });
        let mut known = self.known.write().unwrap();
        if let Some(slot) = known.get_mut(video_id) {
            *slot = new_path.to_owned();
        }
    }

    /// Run one statement batch, opening the database on first use.
    ///
    /// A failure here is logged and swallowed: a missing music folder or a
    /// read-only disk must not take the app down, it only means nothing is
    /// downloaded.
    fn with_db(&self, work: impl FnOnce(&Connection) -> rusqlite::Result<()>) {
        let mut guard = self.db.lock().unwrap();
        if guard.is_none() {
            if let Some(dir) = self.path.parent() {
                if let Err(err) = std::fs::create_dir_all(dir) {
                    tracing::warn!(%err, dir = %dir.display(), "download library folder");
                    return;
                }
            }
            match Connection::open(&self.path) {
                Ok(db) => *guard = Some(db),
                Err(err) => {
                    tracing::warn!(%err, path = %self.path.display(), "download library unavailable");
                    return;
                }
            }
        }
        if let Some(db) = guard.as_ref() {
            if let Err(err) = work(db) {
                tracing::warn!(%err, "download library write failed");
            }
        }
    }
}

/// Python's history cache: one row holding the whole list as JSON.
const HISTORY_SCHEMA: &str = "CREATE TABLE IF NOT EXISTS history_cache (
    id INTEGER PRIMARY KEY,
    data_json TEXT,
    last_synced TEXT
)";

/// Python's table, column for column. `cover_path` stays for compatibility
/// even though covers are embedded in the file.
const SCHEMA: &str = "CREATE TABLE IF NOT EXISTS downloads (
    video_id TEXT PRIMARY KEY,
    title TEXT,
    artist TEXT,
    artist_id TEXT,
    album TEXT,
    album_id TEXT,
    track_number INTEGER,
    duration_seconds INTEGER,
    file_path TEXT,
    cover_path TEXT,
    thumbnail_url TEXT,
    downloaded_at TEXT,
    file_size INTEGER,
    format TEXT,
    like_status TEXT
)";

fn like_status_from(value: &str) -> LikeStatus {
    match value {
        "LIKE" => LikeStatus::Like,
        "DISLIKE" => LikeStatus::Dislike,
        _ => LikeStatus::Indifferent,
    }
}

fn like_status_name(status: LikeStatus) -> &'static str {
    match status {
        LikeStatus::Like => "LIKE",
        LikeStatus::Dislike => "DISLIKE",
        LikeStatus::Indifferent => "INDIFFERENT",
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn temp_store() -> (tempfile::TempDir, Store) {
        let dir = tempfile::tempdir().expect("temp dir");
        let store = Store::open(dir.path());
        (dir, store)
    }

    fn entry(dir: &Path, id: &str, title: &str) -> Entry {
        let file = dir.join(format!("{id}.opus"));
        std::fs::write(&file, b"audio").unwrap();
        Entry { video_id: id.to_owned(), title: title.to_owned(), artist: "Artist".to_owned(), file_path: file, format: "opus".to_owned(), ..Entry::default() }
    }

    #[test]
    fn the_old_music_folder_is_renamed_with_its_rows() {
        let root = tempfile::tempdir().unwrap();
        let legacy = root.path().join("YouTube Music");
        let current = root.path().join("Mixtapes");
        let song = legacy.join("Artist").join("One.opus");
        std::fs::create_dir_all(song.parent().unwrap()).unwrap();
        std::fs::write(&song, b"audio").unwrap();
        {
            let store = Store::open(&legacy);
            store.add(&Entry { video_id: "a".into(), title: "One".into(), file_path: song.clone(), ..Default::default() });
        }

        let store = Store::open(&current);
        assert!(!legacy.exists(), "the old folder is gone");
        assert_eq!(store.local_path("a"), Some(current.join("Artist").join("One.opus")));
    }

    #[test]
    fn a_folder_that_is_not_ours_is_left_where_it_is() {
        let root = tempfile::tempdir().unwrap();
        let legacy = root.path().join("YouTube Music");
        std::fs::create_dir_all(legacy.join("Someone Else")).unwrap();
        Store::open(&root.path().join("Mixtapes"));
        assert!(legacy.is_dir(), "a folder with no library of ours stays put");
    }

    #[test]
    fn a_row_survives_a_reopen() {
        let (dir, store) = temp_store();
        store.add(&entry(dir.path(), "a", "Song"));
        assert!(store.is_downloaded("a"));
        let reopened = Store::open(dir.path());
        assert!(reopened.is_downloaded("a"));
        assert_eq!(reopened.all().len(), 1);
        assert_eq!(reopened.all()[0].title, "Song");
    }

    #[test]
    fn a_deleted_file_stops_counting_as_downloaded() {
        let (dir, store) = temp_store();
        let entry = entry(dir.path(), "a", "Song");
        store.add(&entry);
        std::fs::remove_file(&entry.file_path).unwrap();
        assert!(!store.is_downloaded("a"));
        assert!(store.local_path("a").is_none());
        assert_eq!(store.count(), 0);
    }

    #[test]
    fn a_file_that_moved_keeps_its_row() {
        let (dir, store) = temp_store();
        let mut entry = entry(dir.path(), "a", "Song");
        store.add(&entry);
        let moved = dir.path().join("moved.opus");
        std::fs::rename(&entry.file_path, &moved).unwrap();
        store.moved("a", &moved);
        entry.file_path = moved.clone();
        assert_eq!(store.local_path("a"), Some(moved));
    }

    #[test]
    fn the_owner_of_a_path_is_the_track_that_wrote_it() {
        let (dir, store) = temp_store();
        let first = entry(dir.path(), "a", "Song");
        store.add(&first);
        assert_eq!(store.owner_of(&first.file_path), Some("a".to_owned()));
        assert_eq!(store.owner_of(&dir.path().join("other.opus")), None);
    }

    #[test]
    fn forgetting_a_track_leaves_nothing_behind() {
        let (dir, store) = temp_store();
        store.add(&entry(dir.path(), "a", "Song"));
        store.forget("a");
        assert!(!store.is_downloaded("a"));
        assert!(Store::open(dir.path()).all().is_empty());
    }
}

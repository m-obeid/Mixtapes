//! Playlists and likes kept on this device, for listeners without a Google
//! account. A SQLite file in the data dir, one row per track, so a like or an
//! added song writes one row rather than the whole library. Ids carry the
//! `LOCAL_` prefix so every page can tell them from YouTube's and route writes
//! here instead of to the network. Tracks are stored whole as JSON in their
//! row, so a page renders them offline.

use std::path::PathBuf;
use std::sync::{Arc, Mutex};
use std::time::{SystemTime, UNIX_EPOCH};

use rusqlite::{Connection, OptionalExtension, params};

use crate::model::{ItemKind, LikeStatus, MediaItem, Person, Track, VideoId};
use crate::net::playlists::PlaylistDetails;
use crate::paths::Paths;

pub const PREFIX: &str = "LOCAL_";
/// The likes list. It is a playlist to the pages, never edited or deleted.
pub const LIKED_ID: &str = "LOCAL_LIKED";
/// English, as the id of the text. `liked_title` is what the pages show.
pub const LIKED_TITLE: &str = tr_noop!("Liked Songs");
/// YouTube's own art for its likes list, so both lists wear the same cover.
pub const LIKED_ART: &str = "https://www.gstatic.com/youtube/media/ytm/images/pbg/liked-songs-delhi-576.png";
/// What cards and headers say instead of an author. English, pass it to `i18n::gettext` where it is shown.
pub const HERE: &str = tr_noop!("On this device");

const SCHEMA: &str = "
CREATE TABLE IF NOT EXISTS playlists (
    id TEXT PRIMARY KEY,
    title TEXT NOT NULL,
    description TEXT NOT NULL DEFAULT '',
    created INTEGER NOT NULL,
    modified INTEGER NOT NULL
);
CREATE TABLE IF NOT EXISTS playlist_tracks (
    playlist_id TEXT NOT NULL REFERENCES playlists(id) ON DELETE CASCADE,
    video_id TEXT NOT NULL,
    position INTEGER NOT NULL,
    track_json TEXT NOT NULL,
    PRIMARY KEY (playlist_id, video_id)
);
CREATE INDEX IF NOT EXISTS playlist_tracks_order ON playlist_tracks (playlist_id, position);
CREATE TABLE IF NOT EXISTS liked (
    video_id TEXT PRIMARY KEY,
    liked_at INTEGER NOT NULL,
    track_json TEXT NOT NULL
);
CREATE TABLE IF NOT EXISTS plays (
    id INTEGER PRIMARY KEY,
    video_id TEXT NOT NULL,
    played_at INTEGER NOT NULL,
    track_json TEXT NOT NULL
);
CREATE INDEX IF NOT EXISTS plays_by_time ON plays (played_at);
CREATE TABLE IF NOT EXISTS artists (
    id TEXT PRIMARY KEY,
    name TEXT NOT NULL,
    thumb TEXT,
    subscribed_at INTEGER NOT NULL
);
";

/// Columns added after a table first shipped. Each runs once; on a database that
/// has the column already the statement fails and is ignored.
const MIGRATIONS: [&str; 1] = ["ALTER TABLE artists ADD COLUMN subscribers TEXT"];

/// Plays older than this leave the log, so the feed follows current taste.
const PLAYS_KEPT_SECS: u64 = 180 * 24 * 3600;
/// And never more rows than this.
const PLAYS_KEPT_ROWS: i64 = 5000;

pub fn is_local(id: &str) -> bool {
    id.starts_with(PREFIX)
}

pub struct LocalLibrary {
    db: Mutex<Option<Connection>>,
    path: PathBuf,
}

impl LocalLibrary {
    pub fn open(paths: &Paths) -> Arc<Self> {
        let path = paths.data_dir.join("local.db");
        let library = Self { db: Mutex::new(None), path };
        library.with_db(|db| {
            db.execute_batch(SCHEMA)?;
            for migration in MIGRATIONS {
                let _ = db.execute_batch(migration);
            }
            Ok(())
        });
        Arc::new(library)
    }

    /// Run a query on the open connection, opening it on the first call. A
    /// failure is logged and answered with the default, never a crash: the
    /// library is a convenience, not the app.
    fn with_db<T: Default>(&self, f: impl FnOnce(&Connection) -> rusqlite::Result<T>) -> T {
        let mut guard = self.db.lock().unwrap();
        if guard.is_none() {
            if let Some(dir) = self.path.parent() {
                let _ = std::fs::create_dir_all(dir);
            }
            match Connection::open(&self.path) {
                Ok(db) => {
                    let _ = db.execute_batch("PRAGMA foreign_keys = ON; PRAGMA journal_mode = WAL;");
                    *guard = Some(db);
                }
                Err(err) => {
                    tracing::warn!(%err, path = %self.path.display(), "local library unavailable");
                    return T::default();
                }
            }
        }
        match f(guard.as_ref().unwrap()) {
            Ok(value) => value,
            Err(err) => {
                tracing::warn!(%err, "local library query failed");
                T::default()
            }
        }
    }

    // -- reading -----------------------------------------------------------

    /// Cards for the library page and the add-to-playlist popover: likes first, then playlists, newest change first.
    pub fn items(&self) -> Vec<MediaItem> {
        let mut items = vec![self.liked_item()];
        items.extend(self.playlist_items());
        items
    }

    pub fn playlist_items(&self) -> Vec<MediaItem> {
        self.with_db(|db| {
            let mut stmt = db.prepare(
                "SELECT p.id, p.title, COUNT(t.video_id), \
                 (SELECT track_json FROM playlist_tracks WHERE playlist_id = p.id ORDER BY position LIMIT 1) \
                 FROM playlists p LEFT JOIN playlist_tracks t ON t.playlist_id = p.id \
                 GROUP BY p.id ORDER BY p.modified DESC",
            )?;
            let rows = stmt.query_map([], |row| {
                let first: Option<String> = row.get(3)?;
                Ok(item(row.get::<_, String>(0)?, row.get::<_, String>(1)?, row.get::<_, i64>(2)? as usize, first.as_deref().and_then(thumb_of)))
            })?;
            Ok(rows.flatten().collect())
        })
    }

    fn liked_item(&self) -> MediaItem {
        let count = self.with_db(|db| db.query_row("SELECT COUNT(*) FROM liked", [], |r| r.get::<_, i64>(0)));
        item(LIKED_ID.to_owned(), liked_title(), count as usize, Some(LIKED_ART.to_owned()))
    }

    pub fn title_of(&self, id: &str) -> Option<String> {
        if id == LIKED_ID {
            return Some(liked_title());
        }
        self.with_db(|db| db.query_row("SELECT title FROM playlists WHERE id = ?1", params![id], |r| r.get(0)).optional())
    }

    fn tracks_of(&self, id: &str) -> Vec<Track> {
        self.with_db(|db| {
            let rows: Vec<String> = if id == LIKED_ID {
                let mut stmt = db.prepare("SELECT track_json FROM liked ORDER BY liked_at DESC")?;
                stmt.query_map([], |r| r.get::<_, String>(0))?.flatten().collect()
            } else {
                let mut stmt = db.prepare("SELECT track_json FROM playlist_tracks WHERE playlist_id = ?1 ORDER BY position")?;
                stmt.query_map(params![id], |r| r.get::<_, String>(0))?.flatten().collect()
            };
            Ok(rows.iter().filter_map(|json| serde_json::from_str::<Track>(json).ok()).collect())
        })
    }

    /// What the playlist page renders, in the shape the network gives it.
    pub fn details(&self, id: &str) -> Option<PlaylistDetails> {
        let (title, description) = if id == LIKED_ID {
            (liked_title(), String::new())
        } else {
            self.with_db(|db| db.query_row("SELECT title, description FROM playlists WHERE id = ?1", params![id], |r| Ok((r.get(0)?, r.get(1)?))).optional())?
        };
        let tracks = self.tracks_of(id);
        let duration_seconds: u32 = tracks.iter().filter_map(|t| t.duration_seconds).sum();
        Some(PlaylistDetails {
            id: id.to_owned(),
            title,
            description,
            privacy: None,
            thumbnails: if id == LIKED_ID { vec![LIKED_ART.to_owned()] } else { tracks.iter().filter_map(|t| t.thumb.clone()).take(1).collect() },
            author: vec![Person { name: crate::i18n::gettext(HERE), id: None }],
            collaborators: None,
            year: None,
            duration: None,
            duration_seconds: Some(duration_seconds),
            track_count: Some(tracks.len() as u32),
            tracks,
            audio_playlist_id: None,
            album_type: None,
            like_status: LikeStatus::Indifferent,
        })
    }

    pub fn is_liked(&self, video_id: &str) -> bool {
        self.with_db(|db| db.query_row("SELECT 1 FROM liked WHERE video_id = ?1", params![video_id], |_| Ok(true)).optional().map(|found| found.unwrap_or(false)))
    }

    pub fn liked(&self) -> Vec<Track> {
        self.tracks_of(LIKED_ID)
    }

    // -- writing -----------------------------------------------------------

    pub fn create(&self, title: &str, description: &str) -> String {
        let now = now();
        let id = format!("{PREFIX}{now:x}{:04x}", std::process::id() & 0xffff);
        self.with_db(|db| {
            // A second list within the same second gets a suffix.
            let mut candidate = id.clone();
            let mut n = 0;
            while db.query_row("SELECT 1 FROM playlists WHERE id = ?1", params![candidate], |_| Ok(())).optional()?.is_some() {
                n += 1;
                candidate = format!("{id}{n}");
            }
            db.execute("INSERT INTO playlists (id, title, description, created, modified) VALUES (?1, ?2, ?3, ?4, ?4)", params![candidate, title, description, now as i64])?;
            Ok(candidate)
        })
    }

    pub fn edit(&self, id: &str, title: Option<&str>, description: Option<&str>) {
        self.with_db(|db| {
            if let Some(title) = title {
                db.execute("UPDATE playlists SET title = ?2, modified = ?3 WHERE id = ?1", params![id, title, now() as i64])?;
            }
            if let Some(description) = description {
                db.execute("UPDATE playlists SET description = ?2, modified = ?3 WHERE id = ?1", params![id, description, now() as i64])?;
            }
            Ok(())
        });
    }

    pub fn delete(&self, id: &str) {
        self.with_db(|db| {
            db.execute("DELETE FROM playlist_tracks WHERE playlist_id = ?1", params![id])?;
            db.execute("DELETE FROM playlists WHERE id = ?1", params![id])?;
            Ok(())
        });
    }

    /// Append what is not there yet. Returns how many were new.
    pub fn add_tracks(&self, id: &str, tracks: &[Track]) -> usize {
        self.with_db(|db| {
            let exists: Option<i64> = db.query_row("SELECT 1 FROM playlists WHERE id = ?1", params![id], |r| r.get(0)).optional()?;
            if exists.is_none() {
                return Ok(0);
            }
            let mut position: i64 = db.query_row("SELECT COALESCE(MAX(position), -1) FROM playlist_tracks WHERE playlist_id = ?1", params![id], |r| r.get(0))?;
            let mut added = 0;
            let tx = db.unchecked_transaction()?;
            for track in tracks.iter().filter(|t| !t.video_id.as_str().is_empty()) {
                position += 1;
                let json = serde_json::to_string(&with_set_id(track.clone())).unwrap_or_default();
                added += tx.execute("INSERT OR IGNORE INTO playlist_tracks (playlist_id, video_id, position, track_json) VALUES (?1, ?2, ?3, ?4)", params![id, track.video_id.as_str(), position, json])?;
            }
            tx.execute("UPDATE playlists SET modified = ?2 WHERE id = ?1", params![id, now() as i64])?;
            tx.commit()?;
            Ok(added)
        })
    }

    /// Remove by video id. The likes list unlikes instead.
    pub fn remove_tracks(&self, id: &str, video_ids: &[String]) {
        self.with_db(|db| {
            let tx = db.unchecked_transaction()?;
            for video_id in video_ids {
                if id == LIKED_ID {
                    tx.execute("DELETE FROM liked WHERE video_id = ?1", params![video_id])?;
                } else {
                    tx.execute("DELETE FROM playlist_tracks WHERE playlist_id = ?1 AND video_id = ?2", params![id, video_id])?;
                }
            }
            if id != LIKED_ID {
                tx.execute("UPDATE playlists SET modified = ?2 WHERE id = ?1", params![id, now() as i64])?;
            }
            tx.commit()?;
            Ok(())
        });
    }

    pub fn set_liked(&self, track: &Track, liked: bool) {
        self.with_db(|db| {
            if liked {
                let mut track = with_set_id(track.clone());
                track.like_status = LikeStatus::Like;
                let json = serde_json::to_string(&track).unwrap_or_default();
                db.execute("INSERT OR REPLACE INTO liked (video_id, liked_at, track_json) VALUES (?1, ?2, ?3)", params![track.video_id.as_str(), now_millis(), json])?;
            } else {
                db.execute("DELETE FROM liked WHERE video_id = ?1", params![track.video_id.as_str()])?;
            }
            Ok(())
        });
    }

    // -- subscriptions -------------------------------------------------------

    /// Artists followed on this device, newest first, as library cards.
    pub fn subscriptions(&self) -> Vec<MediaItem> {
        self.with_db(|db| {
            let mut stmt = db.prepare("SELECT id, name, thumb, subscribers FROM artists ORDER BY subscribed_at DESC, rowid DESC")?;
            let rows = stmt.query_map([], |r| {
                Ok(MediaItem { kind: ItemKind::Artist, id: r.get(0)?, title: r.get(1)?, thumb: r.get(2)?, subscribers: r.get(3)?, ..MediaItem::default() })
            })?;
            Ok(rows.flatten().collect())
        })
    }

    pub fn is_subscribed(&self, artist_id: &str) -> bool {
        self.with_db(|db| db.query_row("SELECT 1 FROM artists WHERE id = ?1", params![artist_id], |_| Ok(true)).optional().map(|found| found.unwrap_or(false)))
    }

    /// Follow or drop an artist. `artist` carries the card: name, picture and
    /// the subscriber count as the page shows it, "19.1M".
    pub fn set_subscribed(&self, artist: &MediaItem, subscribed: bool) {
        self.with_db(|db| {
            if subscribed {
                db.execute(
                    "INSERT OR REPLACE INTO artists (id, name, thumb, subscribers, subscribed_at) VALUES (?1, ?2, ?3, ?4, ?5)",
                    params![artist.id, artist.title, artist.thumb, artist.subscribers, now_millis()],
                )?;
            } else {
                db.execute("DELETE FROM artists WHERE id = ?1", params![artist.id])?;
            }
            Ok(())
        });
    }

    /// Fresh name, picture and count for a followed artist, keeping its place in the list.
    pub fn refresh_subscription(&self, artist: &MediaItem) {
        self.with_db(|db| {
            db.execute(
                "UPDATE artists SET name = ?2, thumb = COALESCE(?3, thumb), subscribers = COALESCE(?4, subscribers) WHERE id = ?1",
                params![artist.id, artist.title, artist.thumb, artist.subscribers],
            )?;
            Ok(())
        });
    }

    // -- listening -----------------------------------------------------------

    /// One real listen, for the feed built without an account.
    pub fn log_play(&self, track: &Track) {
        let json = serde_json::to_string(track).unwrap_or_default();
        let now = now() as i64;
        self.with_db(|db| {
            db.execute("INSERT INTO plays (video_id, played_at, track_json) VALUES (?1, ?2, ?3)", params![track.video_id.as_str(), now, json])?;
            db.execute(
                "DELETE FROM plays WHERE played_at < ?1 OR id <= (SELECT id FROM plays ORDER BY id DESC LIMIT 1 OFFSET ?2)",
                params![now - PLAYS_KEPT_SECS as i64, PLAYS_KEPT_ROWS],
            )?;
            Ok(())
        });
    }

    /// The last songs played, newest first, each once.
    pub fn recent_plays(&self, limit: usize) -> Vec<Track> {
        self.with_db(|db| {
            let mut stmt = db.prepare("SELECT track_json FROM plays GROUP BY video_id ORDER BY MAX(played_at) DESC, MAX(id) DESC LIMIT ?1")?;
            let rows = stmt.query_map(params![limit as i64], |r| r.get::<_, String>(0))?;
            Ok(rows.flatten().filter_map(|json| serde_json::from_str::<Track>(&json).ok()).collect())
        })
    }

    /// Every play, newest first, with its time in seconds, for the history page.
    pub fn play_history(&self, limit: usize) -> Vec<(Track, i64)> {
        self.with_db(|db| {
            let mut stmt = db.prepare("SELECT track_json, played_at FROM plays ORDER BY played_at DESC, id DESC LIMIT ?1")?;
            let rows = stmt.query_map(params![limit as i64], |r| Ok((r.get::<_, String>(0)?, r.get::<_, i64>(1)?)))?;
            Ok(rows.flatten().filter_map(|(json, at)| serde_json::from_str::<Track>(&json).ok().map(|t| (t, at))).collect())
        })
    }

    /// Drop every play of a song from the log.
    pub fn forget_plays(&self, video_id: &str) {
        self.with_db(|db| db.execute("DELETE FROM plays WHERE video_id = ?1", params![video_id]).map(|_| ()));
    }

    /// The songs played most since `since_secs` ago, most first.
    pub fn top_plays(&self, since_secs: u64, limit: usize) -> Vec<Track> {
        let since = now().saturating_sub(since_secs) as i64;
        self.with_db(|db| {
            let mut stmt = db.prepare("SELECT track_json FROM plays WHERE played_at >= ?1 GROUP BY video_id ORDER BY COUNT(*) DESC, MAX(played_at) DESC LIMIT ?2")?;
            let rows = stmt.query_map(params![since, limit as i64], |r| r.get::<_, String>(0))?;
            Ok(rows.flatten().filter_map(|json| serde_json::from_str::<Track>(&json).ok()).collect())
        })
    }

    /// Every play since `since_secs` ago, one track per play, for counting artists.
    pub fn plays_since(&self, since_secs: u64) -> Vec<Track> {
        let since = now().saturating_sub(since_secs) as i64;
        self.with_db(|db| {
            let mut stmt = db.prepare("SELECT track_json FROM plays WHERE played_at >= ?1")?;
            let rows = stmt.query_map(params![since], |r| r.get::<_, String>(0))?;
            Ok(rows.flatten().filter_map(|json| serde_json::from_str::<Track>(&json).ok()).collect())
        })
    }

    /// A bare track for a like made from a card that carries no more than an id.
    pub fn track_or_stub(&self, video_id: &VideoId, known: Option<Track>) -> Track {
        known.unwrap_or_else(|| Track { video_id: video_id.clone(), title: "Unknown".to_owned(), ..Track::default() })
    }
}

/// Rows are removed by (video id, set video id). Ours are the video id, which is unique per list.
fn with_set_id(mut track: Track) -> Track {
    track.set_video_id = Some(track.video_id.0.clone());
    track
}

fn now() -> u64 {
    SystemTime::now().duration_since(UNIX_EPOCH).map(|d| d.as_secs()).unwrap_or(0)
}

/// Likes are ordered by this, so two in one second keep their order.
fn now_millis() -> i64 {
    SystemTime::now().duration_since(UNIX_EPOCH).map(|d| d.as_millis() as i64).unwrap_or(0)
}

fn thumb_of(track_json: &str) -> Option<String> {
    serde_json::from_str::<Track>(track_json).ok().and_then(|t| t.thumb)
}

/// The likes list's name in the app's language. Never stored, so a language switch renames it.
fn liked_title() -> String {
    crate::i18n::gettext(LIKED_TITLE)
}

fn item(id: String, title: String, count: usize, thumb: Option<String>) -> MediaItem {
    let here = crate::i18n::gettext(HERE);
    MediaItem {
        kind: ItemKind::Playlist,
        id,
        title,
        thumb,
        // The count leads, before the dot: MediaItem::playlist_detail reads it from there.
        description: Some(format!("{} • {here}", trn!("{n} song", "{n} songs", count))),
        artists: vec![Person { name: here.clone(), id: None }],
        ..MediaItem::default()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn library() -> (tempfile::TempDir, Arc<LocalLibrary>) {
        let dir = tempfile::tempdir().unwrap();
        let paths = Paths::for_tests(dir.path());
        (dir, LocalLibrary::open(&paths))
    }

    fn track(id: &str) -> Track {
        Track { video_id: VideoId(id.to_owned()), title: id.to_uppercase(), duration_seconds: Some(100), thumb: Some(format!("{id}.jpg")), ..Track::default() }
    }

    #[test]
    fn a_playlist_holds_each_song_once_and_removes_by_id() {
        let (_dir, lib) = library();
        let id = lib.create("Mix", "");
        assert!(is_local(&id));
        assert_eq!(lib.add_tracks(&id, &[track("a"), track("b"), track("a")]), 2);
        assert_eq!(lib.add_tracks(&id, &[track("b")]), 0);
        let details = lib.details(&id).unwrap();
        assert_eq!(details.track_count, Some(2));
        assert_eq!(details.duration_seconds, Some(200));
        assert_eq!(details.tracks[0].set_video_id.as_deref(), Some("a"));
        assert_eq!(lib.playlist_items()[0].thumb.as_deref(), Some("a.jpg"));
        lib.remove_tracks(&id, &["a".to_owned()]);
        assert_eq!(lib.details(&id).unwrap().tracks.len(), 1);
        lib.delete(&id);
        assert!(lib.details(&id).is_none());
        assert!(lib.playlist_items().is_empty());
    }

    #[test]
    fn likes_are_newest_first_and_toggle() {
        let (_dir, lib) = library();
        lib.set_liked(&track("a"), true);
        std::thread::sleep(std::time::Duration::from_millis(2));
        lib.set_liked(&track("b"), true);
        assert!(lib.is_liked("a"));
        assert_eq!(lib.liked()[0].video_id.as_str(), "b");
        assert_eq!(lib.liked()[0].like_status, LikeStatus::Like);
        lib.set_liked(&track("a"), false);
        assert!(!lib.is_liked("a"));
        assert_eq!(lib.items()[0].id, LIKED_ID);
        assert_eq!(lib.items()[0].description.as_deref(), Some("1 song • On this device"));
        assert_eq!(lib.details(LIKED_ID).unwrap().tracks.len(), 1);
    }

    #[test]
    fn the_file_round_trips_and_two_lists_in_one_second_differ() {
        let (dir, lib) = library();
        let first = lib.create("Mix", "desc");
        let second = lib.create("Other", "");
        assert_ne!(first, second);
        lib.add_tracks(&first, &[track("a")]);
        let again = LocalLibrary::open(&Paths::for_tests(dir.path()));
        let details = again.details(&first).unwrap();
        assert_eq!(details.description, "desc");
        assert_eq!(details.tracks.len(), 1);
        assert_eq!(again.title_of(&second).as_deref(), Some("Other"));
    }

    #[test]
    fn a_subscription_is_kept_newest_first_until_dropped() {
        let (_dir, lib) = library();
        let artist = |id: &str, subs: Option<&str>| MediaItem { kind: ItemKind::Artist, id: id.to_owned(), title: id.to_owned(), subscribers: subs.map(str::to_owned), ..MediaItem::default() };
        lib.set_subscribed(&artist("UCa", Some("1.2M")), true);
        lib.set_subscribed(&artist("UCb", None), true);
        let ids: Vec<String> = lib.subscriptions().into_iter().map(|a| a.id).collect();
        assert_eq!(ids, ["UCb", "UCa"]);
        assert!(lib.is_subscribed("UCa"));
        lib.refresh_subscription(&artist("UCb", Some("300K")));
        let subs: Vec<Option<String>> = lib.subscriptions().into_iter().map(|a| a.subscribers).collect();
        assert_eq!(subs, [Some("300K".to_owned()), Some("1.2M".to_owned())], "a refresh keeps the order");
        lib.set_subscribed(&artist("UCa", None), false);
        assert!(!lib.is_subscribed("UCa"));
        assert_eq!(lib.subscriptions().len(), 1);
    }

    #[test]
    fn history_lists_every_play_newest_first_and_forgets_a_song() {
        let (_dir, lib) = library();
        lib.log_play(&track("a"));
        lib.log_play(&track("b"));
        lib.log_play(&track("a"));
        let ids: Vec<String> = lib.play_history(10).into_iter().map(|(t, _)| t.video_id.0).collect();
        assert_eq!(ids, ["a", "b", "a"]);
        lib.forget_plays("a");
        assert_eq!(lib.play_history(10).len(), 1);
    }
}

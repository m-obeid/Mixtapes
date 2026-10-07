//! The queue between runs: what was queued, which song was up and how far in,
//! written to one file so the next start can pick up from there.

use std::path::{Path, PathBuf};

use serde::{Deserialize, Serialize};

use crate::paths::Paths;
use crate::queue::QueueSnapshot;

#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
pub struct Session {
    pub queue: QueueSnapshot,
    /// Seconds into the current song.
    #[serde(default)]
    pub position: f64,
}

pub fn file(paths: &Paths) -> PathBuf {
    paths.data_dir.join("session.json")
}

/// The saved session, or None when there is none or it cannot be read.
pub fn load(file: &Path) -> Option<Session> {
    let raw = std::fs::read_to_string(file).ok()?;
    match serde_json::from_str::<Session>(&raw) {
        Ok(session) => Some(session).filter(|s| !s.queue.tracks.is_empty()),
        Err(err) => {
            tracing::warn!(%err, "saved queue unreadable");
            None
        }
    }
}

/// Written beside the file and renamed over it, so a crash mid-write leaves the old one.
pub fn save(file: &Path, session: &Session) {
    let write = || -> std::io::Result<()> {
        if let Some(dir) = file.parent() {
            std::fs::create_dir_all(dir)?;
        }
        let partial = file.with_extension("json.part");
        std::fs::write(&partial, serde_json::to_vec(session)?)?;
        std::fs::rename(&partial, file)
    };
    if let Err(err) = write() {
        tracing::warn!(%err, "saving the queue failed");
    }
}

pub fn clear(file: &Path) {
    let _ = std::fs::remove_file(file);
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::model::{Track, VideoId};

    #[test]
    fn a_session_comes_back_as_it_was_saved() {
        let dir = tempfile::tempdir().unwrap();
        let file = dir.path().join("deep/session.json");
        let track = |id: &str| Track { video_id: VideoId(id.into()), title: id.to_uppercase(), ..Track::default() };
        let session = Session { queue: QueueSnapshot { tracks: vec![track("a"), track("b")], current: Some(1), source_title: "Mix".into(), ..QueueSnapshot::default() }, position: 42.5 };
        save(&file, &session);
        assert_eq!(load(&file), Some(session));
        clear(&file);
        assert_eq!(load(&file), None);
    }

    #[test]
    fn an_empty_or_broken_file_is_no_session() {
        let dir = tempfile::tempdir().unwrap();
        let file = dir.path().join("session.json");
        std::fs::write(&file, "{ not json").unwrap();
        assert_eq!(load(&file), None);
        save(&file, &Session::default());
        assert_eq!(load(&file), None, "an empty queue is nothing to restore");
    }
}

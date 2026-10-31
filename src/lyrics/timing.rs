//! Lyrics that run early or late, nudged by the listener and remembered per song and source.
//! A delay usually belongs to one source, so a song can sit right in one and be shifted in another.

use std::collections::HashMap;
use std::path::PathBuf;
use std::sync::Mutex;

use crate::paths::Paths;

pub struct Offsets {
    path: PathBuf,
    /// Seconds the lyrics move ahead of the song, by "<video id>|<source>". Negative holds them back.
    known: Mutex<HashMap<String, f64>>,
}

fn key(video_id: &str, source: &str) -> String {
    format!("{video_id}|{source}")
}

impl Offsets {
    pub fn new(paths: &Paths) -> Self {
        let path = paths.data_dir.join("lyrics_offsets.json");
        let known = std::fs::read_to_string(&path).ok().and_then(|text| serde_json::from_str(&text).ok()).unwrap_or_default();
        Self { path, known: Mutex::new(known) }
    }

    pub fn get(&self, video_id: &str, source: &str) -> f64 {
        self.known.lock().unwrap().get(&key(video_id, source)).copied().unwrap_or(0.0)
    }

    /// Keep an offset, and forget one set back to zero.
    pub fn set(&self, video_id: &str, source: &str, seconds: f64) {
        let mut known = self.known.lock().unwrap();
        if seconds == 0.0 {
            known.remove(&key(video_id, source));
        } else {
            known.insert(key(video_id, source), seconds);
        }
        match serde_json::to_string(&*known) {
            Ok(json) => {
                if let Err(err) = std::fs::write(&self.path, json) {
                    tracing::warn!(%err, "lyrics offsets not written");
                }
            }
            Err(err) => tracing::warn!(%err, "lyrics offsets not serialized"),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn an_offset_belongs_to_one_song_and_source_and_survives_a_restart() {
        let dir = tempfile::tempdir().unwrap();
        let paths = Paths::for_tests(dir.path());
        let offsets = Offsets::new(&paths);
        offsets.set("vid", "LRCLIB", 0.5);
        assert_eq!(Offsets::new(&paths).get("vid", "LRCLIB"), 0.5);
        assert_eq!(offsets.get("vid", "NetEase"), 0.0);
        offsets.set("vid", "LRCLIB", 0.0);
        assert_eq!(Offsets::new(&paths).get("vid", "LRCLIB"), 0.0);
    }
}

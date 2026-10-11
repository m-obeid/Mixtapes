//! Audio twins: the song a music video stands in for, remembered so a list
//! can show and play the song from the start.
//!
//! YouTube Music only pairs the two for some accounts, so a twin is found by
//! a song search, which costs a request. A found twin is kept on disk at
//! `<cache>/audio_twins.json`. A video with no twin is remembered for the
//! session only, since a song can be released later.

use std::collections::HashMap;
use std::path::PathBuf;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{LazyLock, Mutex, OnceLock};

use crate::model::Track;
use crate::paths::Paths;

/// The pref that turns the swap off, for a listener who wants the videos as YouTube lists them.
pub const PREF: &str = "prefer_song_versions";

static PATH: OnceLock<PathBuf> = OnceLock::new();
static ENABLED: AtomicBool = AtomicBool::new(true);
static KNOWN: LazyLock<Mutex<HashMap<String, Option<Track>>>> = LazyLock::new(|| Mutex::new(HashMap::new()));

/// Read what earlier runs found. Without this call nothing touches the disk, which is how the tests run.
pub fn init(paths: &Paths) {
    set_enabled(paths.read_prefs().get(PREF).and_then(|v| v.as_bool()).unwrap_or(true));
    let path = paths.cache_dir.join("audio_twins.json");
    if let Some(saved) = std::fs::read_to_string(&path).ok().and_then(|text| serde_json::from_str::<HashMap<String, Track>>(&text).ok()) {
        let mut known = KNOWN.lock().unwrap();
        for (video_id, twin) in saved {
            // A twin is its own audio version, so it is never asked about.
            known.insert(twin.video_id.0.clone(), None);
            known.insert(video_id, Some(twin));
        }
    }
    let _ = PATH.set(path);
}

/// Whether music videos are swapped for their song versions at all. Off, every lookup and every swap answers as if no video had a twin.
pub fn enabled() -> bool {
    ENABLED.load(Ordering::Relaxed)
}

pub fn set_enabled(on: bool) {
    ENABLED.store(on, Ordering::Relaxed);
}

/// `Some(Some(twin))` for a video with a song version, `Some(None)` for a track known to have none, `None` when nobody has asked yet.
pub fn known(video_id: &str) -> Option<Option<Track>> {
    KNOWN.lock().unwrap().get(video_id).cloned()
}

/// The id to hand out for a track: its song version when one is known, its own otherwise.
pub fn preferred_id(video_id: &str) -> String {
    match known(video_id) {
        Some(Some(twin)) if enabled() => twin.video_id.0,
        _ => video_id.to_owned(),
    }
}

/// Remember an answer. Call `save` once the batch is done.
pub fn remember(video_id: &str, twin: Option<Track>) {
    let mut known = KNOWN.lock().unwrap();
    if let Some(twin) = &twin {
        known.insert(twin.video_id.0.clone(), None);
    }
    known.insert(video_id.to_owned(), twin);
}

/// Write the found twins to disk.
pub fn save() {
    let Some(path) = PATH.get() else { return };
    let found: HashMap<String, Track> = KNOWN.lock().unwrap().iter().filter_map(|(id, twin)| Some((id.clone(), twin.clone()?))).collect();
    match serde_json::to_string(&found) {
        Ok(json) => {
            if let Err(err) = std::fs::write(path, json) {
                tracing::warn!(%err, "audio twins not written");
            }
        }
        Err(err) => tracing::warn!(%err, "audio twins would not serialize"),
    }
}

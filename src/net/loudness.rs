//! How loud YouTube measured each song, kept on disk so a stream served from the
//! cache and a downloaded file are turned down like a fresh stream.
//!
//! The figure is `loudnessDb`: decibels above the level YouTube normalizes to.
//! Like YouTube, loud songs go down and quiet ones stay, so nothing can clip.

use std::collections::HashMap;
use std::path::PathBuf;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{LazyLock, Mutex, OnceLock};

use crate::paths::Paths;

/// The pref that turns normalization off.
pub const PREF: &str = "normalize_volume";

/// The most a song is turned down, against a broken figure.
const MAX_CUT_DB: f64 = 20.0;

static PATH: OnceLock<PathBuf> = OnceLock::new();
static ENABLED: AtomicBool = AtomicBool::new(true);
static KNOWN: LazyLock<Mutex<HashMap<String, f64>>> = LazyLock::new(|| Mutex::new(HashMap::new()));

/// Read what earlier runs measured. Without this call nothing touches the disk, which is how the tests run.
pub fn init(paths: &Paths) {
    set_enabled(paths.read_prefs().get(PREF).and_then(|v| v.as_bool()).unwrap_or(true));
    let path = paths.cache_dir.join("loudness.json");
    if let Some(saved) = std::fs::read_to_string(&path).ok().and_then(|text| serde_json::from_str::<HashMap<String, f64>>(&text).ok()) {
        KNOWN.lock().unwrap().extend(saved);
    }
    let _ = PATH.set(path);
}

/// Whether this song or the other upload of it was ever measured.
pub fn known(video_id: &str) -> bool {
    measured(video_id).is_some()
}

fn measured(video_id: &str) -> Option<f64> {
    let known = KNOWN.lock().unwrap();
    known.get(video_id).copied().or_else(|| crate::net::twins::other_ids(video_id).iter().find_map(|id| known.get(id).copied()))
}

pub fn set_enabled(on: bool) {
    ENABLED.store(on, Ordering::Relaxed);
}

/// Keep a measurement and write the list out.
pub fn remember(video_id: &str, loudness_db: f64) {
    let mut known = KNOWN.lock().unwrap();
    if known.insert(video_id.to_owned(), loudness_db) == Some(loudness_db) {
        return;
    }
    let Some(path) = PATH.get() else { return };
    match serde_json::to_string(&*known) {
        Ok(json) => {
            if let Err(err) = std::fs::write(path, json) {
                tracing::warn!(%err, "loudness list not written");
            }
        }
        Err(err) => tracing::warn!(%err, "loudness list not serialized"),
    }
}

/// What to multiply this song's samples by: 1.0 when normalization is off or the song was never measured.
pub fn gain(video_id: &str) -> f64 {
    if !ENABLED.load(Ordering::Relaxed) {
        return 1.0;
    }
    measured(video_id).map_or(1.0, gain_for)
}

fn gain_for(loudness_db: f64) -> f64 {
    10f64.powf(-loudness_db.clamp(0.0, MAX_CUT_DB) / 20.0)
}

#[cfg(test)]
mod tests {
    use super::gain_for;

    #[test]
    fn loud_songs_go_down_and_quiet_ones_stay() {
        assert!((gain_for(6.0) - 0.501).abs() < 0.001);
        assert_eq!(gain_for(0.0), 1.0);
        assert_eq!(gain_for(-9.0), 1.0, "a quiet song is never pushed towards clipping");
        assert!((gain_for(90.0) - 0.1).abs() < 0.001, "a broken figure stops at the limit");
    }
}

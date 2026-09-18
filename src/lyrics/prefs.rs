//! Lyrics preferences, port of player/lyrics_prefs.py.
//!
//! Everything lives in the prefs.json both apps share, under the same keys with the same defaults and clamps:
//!
//! - `lyrics_provider_order`: the search queue, provider display names top-down.
//! - `lyrics_providers_disabled`: names switched off. Kept apart from the order so switching one back on restores its position.
//! - `lyrics_match_mode`: `quality` walks past a plain hit looking for synced lyrics, `strict` takes the first provider that returns anything.
//! - `lyrics_second_line`, `lyrics_line_sweep`, `lyrics_font_scale`, `lyrics_active_scale`, `lyrics_effects`: display options for the view.
//!
//! Reads are cached against the file's mtime: the chain asks for the order on every track change and the view asks per row build.

use std::collections::BTreeSet;
use std::sync::Mutex;
use std::time::SystemTime;

use serde_json::{Map, Value, json};

use crate::paths::Paths;

/// Canonical provider names in the order the chain used before the queue was configurable.
pub const DEFAULT_PROVIDER_ORDER: [&str; 6] = ["Apple Music", "BetterLyrics", "BiniLyrics", "NetEase", "LRCLIB", "YouTube Music"];

pub const MATCH_QUALITY: &str = "quality";
pub const MATCH_STRICT: &str = "strict";

pub const SECOND_LINE_MODES: [&str; 5] = ["off", "auto", "romanization", "translation", "background"];
pub const SECOND_LINE_DEFAULT: &str = "auto";
pub const EFFECTS_LEVELS: [&str; 3] = ["off", "subtle", "full"];
pub const EFFECTS_DEFAULT: &str = "full";

/// Multiplier on the lyric column's resting type size.
pub const FONT_SCALE_MIN: f64 = 0.5;
pub const FONT_SCALE_MAX: f64 = 1.45;
pub const FONT_SCALE_DEFAULT: f64 = 1.0;
/// How much bigger the active line is drawn than the resting ones.
pub const ACTIVE_SCALE_MIN: f64 = 1.0;
pub const ACTIVE_SCALE_MAX: f64 = 1.3;
pub const ACTIVE_SCALE_DEFAULT: f64 = 1.2;

const KEY_ORDER: &str = "lyrics_provider_order";
const KEY_DISABLED: &str = "lyrics_providers_disabled";
const KEY_MATCH_MODE: &str = "lyrics_match_mode";
const KEY_SECOND_LINE: &str = "lyrics_second_line";
const KEY_LINE_SWEEP: &str = "lyrics_line_sweep";
const KEY_FONT_SCALE: &str = "lyrics_font_scale";
const KEY_ACTIVE_SCALE: &str = "lyrics_active_scale";
const KEY_EFFECTS: &str = "lyrics_effects";

/// The lyrics slice of prefs.json. Shared by the fetch chain on tokio and the view on GTK.
pub struct LyricsPrefs {
    paths: Paths,
    /// The last parse of prefs.json and the mtime it was read at.
    cache: Mutex<Option<(SystemTime, Map<String, Value>)>>,
}

#[allow(dead_code)]
impl LyricsPrefs {
    pub fn new(paths: &Paths) -> Self {
        Self { paths: paths.clone(), cache: Mutex::new(None) }
    }

    /// The whole prefs object, re-read only when the file's mtime moved.
    fn read(&self) -> Map<String, Value> {
        let Ok(mtime) = std::fs::metadata(&self.paths.prefs_file).and_then(|m| m.modified()) else {
            return Map::new();
        };
        let mut cache = self.cache.lock().unwrap();
        if let Some((seen, data)) = cache.as_ref()
            && *seen == mtime
        {
            return data.clone();
        }
        let data = self.paths.read_prefs();
        *cache = Some((mtime, data.clone()));
        data
    }

    fn write(&self, key: &str, value: Value) {
        self.paths.update_prefs(|prefs| {
            prefs.insert(key.to_owned(), value);
        });
        self.invalidate();
    }

    /// Drop the mtime cache. Needed when something else rewrites prefs.json within the same mtime tick.
    pub fn invalidate(&self) {
        *self.cache.lock().unwrap() = None;
    }

    /// Every known provider in the user's order, disabled ones included. Saved names the app no longer ships are dropped, and providers missing from the saved order are appended in catalog order.
    pub fn full_provider_order(&self) -> Vec<String> {
        let prefs = self.read();
        let Some(saved) = prefs.get(KEY_ORDER).and_then(Value::as_array) else {
            return default_order();
        };
        let mut out: Vec<String> = Vec::new();
        for name in saved.iter().filter_map(Value::as_str) {
            if DEFAULT_PROVIDER_ORDER.contains(&name) && !out.iter().any(|n| n == name) {
                out.push(name.to_owned());
            }
        }
        for name in DEFAULT_PROVIDER_ORDER {
            if !out.iter().any(|n| n == name) {
                out.push(name.to_owned());
            }
        }
        out
    }

    pub fn disabled_providers(&self) -> BTreeSet<String> {
        let prefs = self.read();
        let Some(saved) = prefs.get(KEY_DISABLED).and_then(Value::as_array) else {
            return BTreeSet::new();
        };
        saved.iter().filter_map(Value::as_str).map(str::to_owned).collect()
    }

    /// The search queue: enabled providers in user order. Never empty, since switching everything off would leave the view blank with no way to tell why.
    pub fn provider_order(&self) -> Vec<String> {
        let disabled = self.disabled_providers();
        let order: Vec<String> = self.full_provider_order().into_iter().filter(|n| !disabled.contains(n)).collect();
        if order.is_empty() { default_order() } else { order }
    }

    pub fn set_provider_order<S: AsRef<str>>(&self, order: &[S]) {
        self.write(KEY_ORDER, json!(order.iter().map(|s| s.as_ref()).collect::<Vec<_>>()));
    }

    pub fn set_provider_enabled(&self, name: &str, enabled: bool) {
        let mut disabled = self.disabled_providers();
        if enabled {
            disabled.remove(name);
        } else {
            disabled.insert(name.to_owned());
        }
        // A BTreeSet iterates sorted, which is what the Python app stores.
        self.write(KEY_DISABLED, json!(disabled));
    }

    pub fn match_mode(&self) -> String {
        self.one_of(KEY_MATCH_MODE, &[MATCH_QUALITY, MATCH_STRICT], MATCH_QUALITY)
    }

    pub fn set_match_mode(&self, mode: &str) {
        self.write(KEY_MATCH_MODE, json!(mode));
    }

    pub fn second_line_mode(&self) -> String {
        self.one_of(KEY_SECOND_LINE, &SECOND_LINE_MODES, SECOND_LINE_DEFAULT)
    }

    /// An unknown mode renders a blank second line that reads as "off", so it is normalized rather than stored.
    pub fn set_second_line_mode(&self, mode: &str) {
        let mode = if SECOND_LINE_MODES.contains(&mode) { mode } else { SECOND_LINE_DEFAULT };
        self.write(KEY_SECOND_LINE, json!(mode));
    }

    /// Write the default out when the key is missing or unusable, so the setting is never left implicit.
    pub fn ensure_second_line_mode(&self) -> String {
        let stored = self.read().get(KEY_SECOND_LINE).and_then(Value::as_str).map(str::to_owned);
        if !stored.as_deref().is_some_and(|v| SECOND_LINE_MODES.contains(&v)) {
            self.write(KEY_SECOND_LINE, json!(SECOND_LINE_DEFAULT));
        }
        self.second_line_mode()
    }

    /// Whether a line-synced source gets synthesized per-word timing so its highlight travels across the line.
    pub fn line_sweep(&self) -> bool {
        self.read().get(KEY_LINE_SWEEP).is_none_or(truthy)
    }

    pub fn set_line_sweep(&self, enabled: bool) {
        self.write(KEY_LINE_SWEEP, json!(enabled));
    }

    pub fn font_scale(&self) -> f64 {
        self.clamped(KEY_FONT_SCALE, FONT_SCALE_DEFAULT, FONT_SCALE_MIN, FONT_SCALE_MAX)
    }

    pub fn set_font_scale(&self, value: f64) {
        self.write(KEY_FONT_SCALE, json!(value));
    }

    pub fn active_scale(&self) -> f64 {
        self.clamped(KEY_ACTIVE_SCALE, ACTIVE_SCALE_DEFAULT, ACTIVE_SCALE_MIN, ACTIVE_SCALE_MAX)
    }

    pub fn set_active_scale(&self, value: f64) {
        self.write(KEY_ACTIVE_SCALE, json!(value));
    }

    pub fn effects_level(&self) -> String {
        self.one_of(KEY_EFFECTS, &EFFECTS_LEVELS, EFFECTS_DEFAULT)
    }

    pub fn set_effects_level(&self, level: &str) {
        self.write(KEY_EFFECTS, json!(level));
    }

    fn one_of(&self, key: &str, allowed: &[&str], default: &str) -> String {
        let prefs = self.read();
        let value = prefs.get(key).and_then(Value::as_str).unwrap_or(default);
        if allowed.contains(&value) { value.to_owned() } else { default.to_owned() }
    }

    /// Port of _clamped_float: anything float() would not take reads as the default.
    fn clamped(&self, key: &str, default: f64, low: f64, high: f64) -> f64 {
        let prefs = self.read();
        let value = match prefs.get(key) {
            None => Some(default),
            Some(Value::Number(n)) => n.as_f64(),
            Some(Value::String(s)) => s.trim().parse::<f64>().ok(),
            Some(Value::Bool(b)) => Some(if *b { 1.0 } else { 0.0 }),
            Some(_) => None,
        };
        match value {
            Some(v) if !v.is_nan() => v.clamp(low, high),
            _ => default,
        }
    }
}

fn default_order() -> Vec<String> {
    DEFAULT_PROVIDER_ORDER.iter().map(|s| (*s).to_owned()).collect()
}

/// Python truthiness of a JSON value, which is what bool() applied to the stored pref.
fn truthy(value: &Value) -> bool {
    match value {
        Value::Null => false,
        Value::Bool(b) => *b,
        Value::Number(n) => n.as_f64().is_some_and(|v| v != 0.0),
        Value::String(s) => !s.is_empty(),
        Value::Array(a) => !a.is_empty(),
        Value::Object(o) => !o.is_empty(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn prefs() -> (tempfile::TempDir, LyricsPrefs) {
        let dir = tempfile::tempdir().unwrap();
        let prefs = LyricsPrefs::new(&Paths::for_tests(dir.path()));
        (dir, prefs)
    }

    fn put(prefs: &LyricsPrefs, value: Value) {
        std::fs::write(&prefs.paths.prefs_file, serde_json::to_vec(&value).unwrap()).unwrap();
        prefs.invalidate();
    }

    #[test]
    fn defaults_with_no_file() {
        let (_dir, prefs) = prefs();
        assert_eq!(prefs.full_provider_order(), DEFAULT_PROVIDER_ORDER);
        assert_eq!(prefs.provider_order(), DEFAULT_PROVIDER_ORDER);
        assert!(prefs.disabled_providers().is_empty());
        assert_eq!(prefs.match_mode(), MATCH_QUALITY);
        assert_eq!(prefs.second_line_mode(), "auto");
        assert_eq!(prefs.effects_level(), "full");
        assert!(prefs.line_sweep());
        assert_eq!(prefs.font_scale(), FONT_SCALE_DEFAULT);
        assert_eq!(prefs.active_scale(), ACTIVE_SCALE_DEFAULT);
    }

    #[test]
    fn saved_order_drops_unknown_names_and_appends_new_providers() {
        let (_dir, prefs) = prefs();
        put(&prefs, json!({"lyrics_provider_order": ["LRCLIB", "Gone", "LRCLIB", 7, "NetEase"]}));
        assert_eq!(prefs.full_provider_order(), ["LRCLIB", "NetEase", "Apple Music", "BetterLyrics", "BiniLyrics", "YouTube Music"]);
    }

    #[test]
    fn disabling_keeps_the_position_and_everything_off_falls_back() {
        let (_dir, prefs) = prefs();
        prefs.set_provider_order(&["NetEase", "LRCLIB", "Apple Music", "BetterLyrics", "BiniLyrics", "YouTube Music"]);
        prefs.set_provider_enabled("LRCLIB", false);
        assert_eq!(prefs.provider_order()[..2], ["NetEase", "Apple Music"]);
        assert_eq!(prefs.full_provider_order()[1], "LRCLIB");
        prefs.set_provider_enabled("LRCLIB", true);
        assert_eq!(prefs.provider_order()[1], "LRCLIB");

        for name in DEFAULT_PROVIDER_ORDER {
            prefs.set_provider_enabled(name, false);
        }
        assert_eq!(prefs.provider_order(), DEFAULT_PROVIDER_ORDER);
        // Stored sorted, like sorted(disabled) in the Python app.
        let stored = prefs.paths.read_prefs();
        let names: Vec<&str> = stored[KEY_DISABLED].as_array().unwrap().iter().filter_map(Value::as_str).collect();
        let mut sorted = names.clone();
        sorted.sort_unstable();
        assert_eq!(names, sorted);
    }

    #[test]
    fn writes_keep_the_other_apps_keys() {
        let (_dir, prefs) = prefs();
        put(&prefs, json!({"history_mode": "never"}));
        prefs.set_match_mode(MATCH_STRICT);
        assert_eq!(prefs.match_mode(), MATCH_STRICT);
        assert_eq!(prefs.paths.read_prefs()["history_mode"], "never");
    }

    #[test]
    fn unusable_values_read_as_defaults() {
        let (_dir, prefs) = prefs();
        put(&prefs, json!({"lyrics_match_mode": "fuzzy", "lyrics_second_line": 3, "lyrics_effects": "max", "lyrics_font_scale": "big", "lyrics_active_scale": [1]}));
        assert_eq!(prefs.match_mode(), MATCH_QUALITY);
        assert_eq!(prefs.second_line_mode(), SECOND_LINE_DEFAULT);
        assert_eq!(prefs.effects_level(), EFFECTS_DEFAULT);
        assert_eq!(prefs.font_scale(), FONT_SCALE_DEFAULT);
        assert_eq!(prefs.active_scale(), ACTIVE_SCALE_DEFAULT);
    }

    #[test]
    fn scales_are_clamped() {
        let (_dir, prefs) = prefs();
        prefs.set_font_scale(9.0);
        prefs.set_active_scale(0.2);
        assert_eq!(prefs.font_scale(), FONT_SCALE_MAX);
        assert_eq!(prefs.active_scale(), ACTIVE_SCALE_MIN);
        put(&prefs, json!({"lyrics_font_scale": "0.75", "lyrics_active_scale": 1}));
        assert_eq!(prefs.font_scale(), 0.75);
        assert_eq!(prefs.active_scale(), 1.0);
    }

    #[test]
    fn second_line_mode_is_normalized_and_ensured() {
        let (_dir, prefs) = prefs();
        assert!(!prefs.paths.read_prefs().contains_key(KEY_SECOND_LINE));
        assert_eq!(prefs.ensure_second_line_mode(), "auto");
        assert_eq!(prefs.paths.read_prefs()[KEY_SECOND_LINE], "auto");
        prefs.set_second_line_mode("translation");
        assert_eq!(prefs.ensure_second_line_mode(), "translation");
        prefs.set_second_line_mode("sideways");
        assert_eq!(prefs.second_line_mode(), "auto");
    }

    #[test]
    fn line_sweep_follows_python_truthiness() {
        let (_dir, prefs) = prefs();
        prefs.set_line_sweep(false);
        assert!(!prefs.line_sweep());
        put(&prefs, json!({"lyrics_line_sweep": 0}));
        assert!(!prefs.line_sweep());
        put(&prefs, json!({"lyrics_line_sweep": "yes"}));
        assert!(prefs.line_sweep());
    }

    #[test]
    fn effects_level_round_trips() {
        let (_dir, prefs) = prefs();
        prefs.set_effects_level("subtle");
        assert_eq!(prefs.effects_level(), "subtle");
    }
}

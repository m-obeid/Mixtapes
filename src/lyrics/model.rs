//! The shape every provider's lyrics are normalized into.
//!
//! These serialize to the exact dicts api/client.py produced, because the disk cache is shared with the Python app: `start` is always written (null when unsynced), everything optional is left out when absent.

use serde::{Deserialize, Serialize};

/// Word-level timing: every line can carry its own words.
pub const RANK_WORD: u8 = 3;
/// Line-level timing.
pub const RANK_LINE: u8 = 2;
/// Plain text, no timing.
pub const RANK_PLAIN: u8 = 1;

/// One timed word or syllable of a line, or of its background vocal.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct LyricPart {
    /// Seconds from the start of the track.
    #[serde(default)]
    pub start: Option<f64>,
    #[serde(default)]
    pub end: Option<f64>,
    pub text: String,
    /// Whether the source put whitespace after this word. CJK syllables carry none.
    #[serde(default = "yes")]
    pub space_after: bool,
}

/// Which edge a line sits against. A duet's second voice is set against the far one.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum Align {
    End,
    #[serde(other)]
    Start,
}

/// One lyric line and everything the view can put under it.
#[derive(Clone, Debug, Default, PartialEq, Serialize, Deserialize)]
pub struct LyricLine {
    /// Seconds from the start of the track. None for unsynced lyrics.
    #[serde(default)]
    pub start: Option<f64>,
    #[serde(default)]
    pub text: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub end: Option<f64>,
    /// Word-level timing for the karaoke sweep. Empty when the source is line-synced.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub parts: Vec<LyricPart>,
    /// Background vocals answering this line, as one string.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub bg_text: Option<String>,
    /// The background vocal's own word timing, when the source has it.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub bg: Vec<LyricPart>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub align: Option<Align>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub translation: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub romanization: Option<String>,
}

impl LyricLine {
    pub fn new(start: Option<f64>, text: impl Into<String>) -> Self {
        Self { start, text: text.into(), ..Self::default() }
    }

    /// True when the second voice sings this line.
    #[allow(dead_code)]
    pub fn opposite_voice(&self) -> bool {
        self.align == Some(Align::End)
    }

    pub(crate) fn has_romanization(&self) -> bool {
        self.romanization.as_deref().is_some_and(|r| !r.is_empty())
    }
}

/// What a provider returned for a track.
#[derive(Clone, Debug, Default, PartialEq, Serialize, Deserialize)]
pub struct LyricsResult {
    #[serde(default)]
    pub lines: Vec<LyricLine>,
    /// True when every line has a start time.
    #[serde(default)]
    pub synced: bool,
    /// Display name of the provider, or the credit YouTube Music reports.
    #[serde(default)]
    pub source: String,
    /// Set on a match the listener picked by hand. Survives a pipeline bump.
    #[serde(default, skip_serializing_if = "is_false")]
    pub user_choice: bool,
}

impl LyricsResult {
    /// A result whose synced flag follows its lines.
    pub fn from_lines(lines: Vec<LyricLine>, source: &str) -> Self {
        let synced = lines.iter().all(|l| l.start.is_some());
        Self { lines, synced, source: source.to_owned(), user_choice: false }
    }

    /// Port of _result_rank: 3 word-level, 2 line-synced, 1 plain, 0 nothing usable.
    pub fn rank(&self) -> u8 {
        if self.lines.is_empty() {
            0
        } else if self.is_word_level() {
            RANK_WORD
        } else if self.synced {
            RANK_LINE
        } else {
            RANK_PLAIN
        }
    }

    pub fn is_word_level(&self) -> bool {
        self.lines.iter().any(|l| !l.parts.is_empty())
    }

    pub(crate) fn any_romanization(&self) -> bool {
        self.lines.iter().any(LyricLine::has_romanization)
    }
}

/// Rank of an optional result, the way the chain compares them.
pub fn rank_of(result: Option<&LyricsResult>) -> u8 {
    result.map_or(0, LyricsResult::rank)
}

/// One candidate in the match browser or the manual search.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct LyricsMatch {
    /// The candidate's title as the provider lists it.
    pub label: String,
    /// Artist and m:ss duration joined by a middle dot, whichever halves are known.
    pub detail: String,
    pub result: LyricsResult,
    /// Provider display name. Set by the manual search, which mixes providers.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub source: Option<String>,
}

/// One provider's answer during `fetch_alternatives`. None means it had nothing.
#[derive(Clone, Debug, PartialEq)]
pub struct Alternative {
    pub source: String,
    pub result: Option<LyricsResult>,
}

fn yes() -> bool {
    true
}

fn is_false(value: &bool) -> bool {
    !*value
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn part(text: &str) -> LyricPart {
        LyricPart { start: Some(1.0), end: Some(2.0), text: text.to_owned(), space_after: true }
    }

    #[test]
    fn rank_follows_the_richest_shape() {
        assert_eq!(rank_of(None), 0);
        assert_eq!(LyricsResult::default().rank(), 0);
        let plain = LyricsResult::from_lines(vec![LyricLine::new(None, "a")], "X");
        assert_eq!(plain.rank(), RANK_PLAIN);
        let line = LyricsResult::from_lines(vec![LyricLine::new(Some(1.0), "a")], "X");
        assert!(line.synced);
        assert_eq!(line.rank(), RANK_LINE);
        let mut word = line.clone();
        word.lines[0].parts.push(part("a"));
        assert_eq!(word.rank(), RANK_WORD);
    }

    #[test]
    fn a_plain_line_serializes_like_the_python_dict() {
        let result = LyricsResult::from_lines(vec![LyricLine::new(None, "hello")], "LRCLIB");
        assert_eq!(serde_json::to_value(&result).unwrap(), json!({"lines": [{"start": null, "text": "hello"}], "synced": false, "source": "LRCLIB"}));
    }

    #[test]
    fn a_rich_line_round_trips() {
        let raw = json!({
            "lines": [{
                "start": 12, "text": "la la", "end": 14.5, "align": "end",
                "parts": [{"start": 12.0, "end": 13.0, "text": "la"}, {"start": 13.0, "end": null, "text": "la", "space_after": false}],
                "bg_text": "ooh", "bg": [{"start": 13.5, "end": 14.0, "text": "ooh", "space_after": true}],
                "translation": "t", "romanization": "r"
            }],
            "synced": true, "source": "Apple Music", "user_choice": true
        });
        let result: LyricsResult = serde_json::from_value(raw).unwrap();
        let line = &result.lines[0];
        assert_eq!(line.start, Some(12.0));
        assert!(line.opposite_voice());
        assert!(line.parts[0].space_after, "a missing space_after reads as true, like the view");
        assert!(!line.parts[1].space_after);
        assert!(result.user_choice);
        let back: LyricsResult = serde_json::from_value(serde_json::to_value(&result).unwrap()).unwrap();
        assert_eq!(back, result);
    }

    #[test]
    fn an_unknown_alignment_is_not_an_error() {
        let line: LyricLine = serde_json::from_value(json!({"start": null, "text": "a", "align": "center"})).unwrap();
        assert!(!line.opposite_voice());
    }
}

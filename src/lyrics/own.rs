//! Lyrics the listener wrote, one LRC or plain text file per song. They live
//! beside the data rather than in the cache, which a refresh or a clear wipes.

use std::path::PathBuf;

use super::lrc::{parse_lrc_text, plain_lines, read_word_stamps, to_lrc};
use super::model::{LyricLine, LyricsResult};
use super::ttml::ttml_to_lines;
use crate::paths::Paths;

/// The source name of written lyrics, in the cache's pin and the picker.
pub const SOURCE: &str = tr_noop!("My Lyrics");

pub struct OwnLyrics {
    dir: PathBuf,
}

/// TTML is read as it comes. Otherwise timestamps make synced lyrics, word stamps make
/// them word-level, and anything else is plain text.
pub fn parse(text: &str) -> LyricsResult {
    let lines = if text.trim_start().starts_with("<tt") || text.trim_start().starts_with("<?xml") {
        ttml_to_lines(text)
    } else {
        let mut lines = parse_lrc_text(text);
        read_word_stamps(&mut lines);
        if lines.is_empty() { plain_lines(text) } else { lines }
    };
    LyricsResult::from_lines(lines, SOURCE)
}

/// Lines as text to edit.
pub fn to_text(lines: &[LyricLine]) -> String {
    to_lrc(lines)
}

impl OwnLyrics {
    pub fn new(paths: &Paths) -> Self {
        Self { dir: paths.data_dir.join("my_lyrics") }
    }

    fn path(&self, video_id: &str) -> Option<PathBuf> {
        let safe = !video_id.is_empty() && video_id.chars().all(|c| c.is_ascii_alphanumeric() || c == '-' || c == '_');
        safe.then(|| self.dir.join(format!("{video_id}.lrc")))
    }

    pub fn text(&self, video_id: &str) -> Option<String> {
        std::fs::read_to_string(self.path(video_id)?).ok().filter(|t| !t.trim().is_empty())
    }

    pub fn get(&self, video_id: &str) -> Option<LyricsResult> {
        self.text(video_id).map(|text| parse(&text)).filter(|r| !r.lines.is_empty())
    }

    pub fn save(&self, video_id: &str, text: &str) -> std::io::Result<()> {
        let Some(path) = self.path(video_id) else { return Err(std::io::Error::other("no song to save lyrics for")) };
        std::fs::create_dir_all(&self.dir)?;
        std::fs::write(path, text)
    }

    pub fn delete(&self, video_id: &str) {
        if let Some(path) = self.path(video_id) {
            let _ = std::fs::remove_file(path);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn timestamps_make_lyrics_synced_and_plain_text_stays_plain() {
        assert!(parse("[00:01.50]one\n[00:03.00]two").synced);
        let plain = parse("one\ntwo");
        assert!(!plain.synced);
        assert_eq!(plain.lines.len(), 2);
    }

    #[test]
    fn word_stamps_make_lyrics_word_level() {
        let result = parse("[00:12.00]<00:12.00>Hey <00:12.40>you <00:13.00>\n[00:14.00]<00:14.00>and <00:14.50>me");
        assert_eq!(result.rank(), 3);
        let first = &result.lines[0];
        assert_eq!(first.text, "Hey you");
        assert_eq!(first.parts.iter().map(|p| p.text.as_str()).collect::<Vec<_>>(), ["Hey", "you"]);
        assert_eq!((first.parts[0].start, first.parts[0].end), (Some(12.0), Some(12.4)));
        assert_eq!(first.parts[1].end, Some(13.0), "the closing stamp ends the last word");
        assert_eq!(result.lines[1].parts[1].end, Some(16.0), "with no closing stamp a word gets a moment of its own");
    }

    #[test]
    fn a_syllable_with_no_space_after_it_stays_joined() {
        let result = parse("[00:01.00]<00:01.00>少<00:01.50>年 <00:02.00>よ");
        assert_eq!(result.lines[0].text, "少年 よ");
        assert!(!result.lines[0].parts[0].space_after);
        assert!(result.lines[0].parts[1].space_after);
    }

    #[test]
    fn word_timing_survives_a_trip_through_the_editor() {
        let lines = parse("[00:12.00]<00:12.00>Hey <00:12.40>you <00:13.00>\n[00:14.00]plain line").lines;
        assert_eq!(parse(&to_text(&lines)).lines, lines);
    }

    #[test]
    fn pasted_ttml_is_word_level() {
        let ttml = r#"<tt xmlns="http://www.w3.org/ns/ttml"><body><div><p begin="1.0" end="2.0"><span begin="1.0" end="1.5">Hey</span> <span begin="1.5" end="2.0">you</span></p></div></body></tt>"#;
        assert_eq!(parse(ttml).rank(), 3);
    }

    #[test]
    fn timed_lines_survive_a_trip_through_the_editor() {
        let lines = parse("[01:02.50]one\n[00:03.00]two").lines;
        assert_eq!(parse(&to_text(&lines)).lines, lines);
    }

    #[test]
    fn saved_lyrics_read_back_and_deleting_forgets_them() {
        let dir = tempfile::tempdir().unwrap();
        let own = OwnLyrics::new(&Paths::for_tests(dir.path()));
        own.save("vid", "la la").unwrap();
        assert_eq!(own.get("vid").unwrap().source, SOURCE);
        own.delete("vid");
        assert!(own.get("vid").is_none());
        assert!(own.save("../escape", "x").is_err());
    }
}

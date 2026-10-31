//! Lyrics the listener wrote, one LRC or plain text file per song. They live
//! beside the data rather than in the cache, which a refresh or a clear wipes.

use std::path::PathBuf;

use super::lrc::{parse_lrc_text, plain_lines};
use super::model::{LyricLine, LyricsResult};
use crate::paths::Paths;

/// The source name of written lyrics, in the cache's pin and the picker.
pub const SOURCE: &str = tr_noop!("My Lyrics");

pub struct OwnLyrics {
    dir: PathBuf,
}

/// Lines with a timestamp make synced lyrics, anything else is plain text.
pub fn parse(text: &str) -> LyricsResult {
    let synced = parse_lrc_text(text);
    let lines = if synced.is_empty() { plain_lines(text) } else { synced };
    LyricsResult::from_lines(lines, SOURCE)
}

/// Lines as text to edit: LRC when timed.
pub fn to_text(lines: &[LyricLine]) -> String {
    let line = |l: &LyricLine| match l.start {
        Some(start) => {
            let hundredths = (start * 100.0).round() as u64;
            format!("[{:02}:{:02}.{:02}]{}", hundredths / 6000, hundredths / 100 % 60, hundredths % 100, l.text)
        }
        None => l.text.clone(),
    };
    lines.iter().map(line).collect::<Vec<_>>().join("\n")
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

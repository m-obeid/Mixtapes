//! Playlist mirrors under <music>/Playlists.
//!
//! Port of _write_playlist_m3u and _prune_m3us_missing_files: a downloaded
//! playlist gets an .m3u8 next to the audio so other players can open it.
//! Only tracks that are actually on disk are listed, and paths are relative
//! so the music folder can move.

use std::path::{Path, PathBuf};

use crate::model::Track;
use crate::paths::Paths;

use super::naming::{name_max, playlists_dir, safe_component};
use super::store::Store;

/// Write or rewrite one playlist mirror. Albums are skipped: they already
/// live in their own folder.
pub fn write(paths: &Paths, store: &Store, playlist_id: &str, title: &str, tracks: &[Track]) {
    if playlist_id.starts_with("MPRE") || playlist_id.starts_with("OLAK") {
        return;
    }
    let dir = playlists_dir(paths);
    if let Err(err) = std::fs::create_dir_all(&dir) {
        tracing::warn!(%err, dir = %dir.display(), "playlists folder");
        return;
    }
    let budget = name_max(&dir).saturating_sub(".m3u8".len()).max(1);
    let path = dir.join(format!("{}.m3u8", safe_component(title, budget)));

    let mut body = format!("#EXTM3U\n#PLAYLIST:{title}\n");
    for track in tracks {
        // The row a page passed may be bare, so the library fills the gaps:
        // after a download it holds the real title, artist and length.
        let Some(entry) = store.entry(&track.video_id.0) else { continue };
        let Some(relative) = relative_to(&dir, &entry.file_path) else { continue };
        let seconds = track.duration_seconds.or(entry.duration_seconds).unwrap_or(0);
        let song = first_filled(&track.title, &entry.title);
        let artist = first_filled(&track.artist, &entry.artist);
        body.push_str(&format!("#EXTINF:{seconds},{artist} - {song}\n{}\n", relative.display()));
    }
    if let Err(err) = std::fs::write(&path, body) {
        tracing::warn!(%err, path = %path.display(), "playlist mirror");
    }
}

/// Drop entries whose file is gone, after a download is deleted.
pub fn prune(paths: &Paths) {
    let dir = playlists_dir(paths);
    let Ok(entries) = std::fs::read_dir(&dir) else { return };
    for entry in entries.flatten() {
        let path = entry.path();
        let is_playlist = path.extension().and_then(|e| e.to_str()).is_some_and(|e| e.eq_ignore_ascii_case("m3u") || e.eq_ignore_ascii_case("m3u8"));
        if !is_playlist {
            continue;
        }
        let Ok(text) = std::fs::read_to_string(&path) else { continue };
        let mut kept = String::new();
        let mut lines = text.lines().peekable();
        while let Some(line) = lines.next() {
            if let Some(rest) = line.strip_prefix("#EXTINF:") {
                let _ = rest;
                let Some(target) = lines.next() else { break };
                if dir.join(target).exists() {
                    kept.push_str(line);
                    kept.push('\n');
                    kept.push_str(target);
                    kept.push('\n');
                }
                continue;
            }
            kept.push_str(line);
            kept.push('\n');
        }
        if kept != text {
            if let Err(err) = std::fs::write(&path, kept) {
                tracing::warn!(%err, path = %path.display(), "playlist prune");
            }
        }
    }
}

/// Point mirrors at files that moved, after a folder layout change.
pub fn repoint(paths: &Paths, moves: &[(PathBuf, PathBuf)]) {
    let dir = playlists_dir(paths);
    let Ok(entries) = std::fs::read_dir(&dir) else { return };
    for entry in entries.flatten() {
        let path = entry.path();
        let is_playlist = path.extension().and_then(|e| e.to_str()).is_some_and(|e| e.eq_ignore_ascii_case("m3u") || e.eq_ignore_ascii_case("m3u8"));
        if !is_playlist {
            continue;
        }
        let Ok(text) = std::fs::read_to_string(&path) else { continue };
        let mut out = String::new();
        for line in text.lines() {
            let target = (!line.starts_with('#')).then(|| normalize(&dir.join(line)));
            let moved = target.and_then(|old| moves.iter().find(|(from, _)| normalize(from) == old)).and_then(|(_, to)| relative_to(&dir, to));
            match moved {
                Some(relative) => out.push_str(&relative.to_string_lossy()),
                None => out.push_str(line),
            }
            out.push('\n');
        }
        if out != text {
            if let Err(err) = std::fs::write(&path, out) {
                tracing::warn!(%err, path = %path.display(), "playlist repoint");
            }
        }
    }
}

/// The first of the two that has something in it.
fn first_filled<'a>(preferred: &'a str, fallback: &'a str) -> &'a str {
    if preferred.is_empty() { fallback } else { preferred }
}

/// A path with `.` and `..` folded away, so two spellings of one file match.
/// Lexical only: the file may already have moved.
fn normalize(path: &Path) -> PathBuf {
    let mut out = PathBuf::new();
    for part in path.components() {
        match part {
            std::path::Component::ParentDir => {
                out.pop();
            }
            std::path::Component::CurDir => {}
            other => out.push(other),
        }
    }
    out
}

/// `file` seen from `dir`, walking up with `..` when they share a root.
fn relative_to(dir: &Path, file: &Path) -> Option<PathBuf> {
    let mut shared = 0;
    let base: Vec<_> = dir.components().collect();
    let target: Vec<_> = file.components().collect();
    while shared < base.len() && shared < target.len() && base[shared] == target[shared] {
        shared += 1;
    }
    if shared == 0 {
        return None;
    }
    let mut out = PathBuf::new();
    for _ in shared..base.len() {
        out.push("..");
    }
    for part in &target[shared..] {
        out.push(part);
    }
    Some(out)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::model::VideoId;

    fn track(id: &str, title: &str, seconds: u32) -> Track {
        Track { video_id: VideoId(id.into()), title: title.into(), artist: "Artist".into(), duration_seconds: Some(seconds), ..Track::default() }
    }

    #[test]
    fn two_spellings_of_one_path_normalize_the_same() {
        let direct = Path::new("/music/Mixtapes/Artist/One.opus");
        let walked = Path::new("/music/Mixtapes/Playlists/../Artist/./One.opus");
        assert_eq!(normalize(direct), normalize(walked));
    }

    #[test]
    fn a_path_below_the_playlists_folder_is_written_relative() {
        let dir = Path::new("/music/Mixtapes/Playlists");
        let file = Path::new("/music/Mixtapes/Artist/Album/Song.opus");
        assert_eq!(relative_to(dir, file).unwrap(), Path::new("../Artist/Album/Song.opus"));
    }

    #[test]
    fn only_downloaded_tracks_reach_the_mirror() {
        let home = tempfile::tempdir().unwrap();
        let paths = crate::paths::Paths::for_tests(home.path());
        let store = Store::open(&paths.music_dir());
        let songs = paths.music_dir().join("Artist");
        std::fs::create_dir_all(&songs).unwrap();
        let file = songs.join("One.opus");
        std::fs::write(&file, b"audio").unwrap();
        store.add(&super::super::store::Entry { video_id: "a".into(), title: "One".into(), file_path: file, ..Default::default() });

        write(&paths, &store, "PL123", "My Mix", &[track("a", "One", 61), track("b", "Two", 70)]);
        let body = std::fs::read_to_string(playlists_dir(&paths).join("My Mix.m3u8")).unwrap();
        assert!(body.starts_with("#EXTM3U\n#PLAYLIST:My Mix\n"));
        assert!(body.contains("#EXTINF:61,Artist - One"));
        assert!(body.contains("../Artist/One.opus"));
        assert!(!body.contains("Two"), "a track with no file has nothing to point at");
    }

    #[test]
    fn pruning_drops_entries_whose_file_went_away() {
        let home = tempfile::tempdir().unwrap();
        let paths = crate::paths::Paths::for_tests(home.path());
        let dir = playlists_dir(&paths);
        std::fs::create_dir_all(&dir).unwrap();
        std::fs::write(dir.join("kept.opus"), b"audio").unwrap();
        let mirror = dir.join("Mix.m3u8");
        std::fs::write(&mirror, "#EXTM3U\n#PLAYLIST:Mix\n#EXTINF:1,A - Gone\ngone.opus\n#EXTINF:2,A - Kept\nkept.opus\n").unwrap();
        prune(&paths);
        let body = std::fs::read_to_string(&mirror).unwrap();
        assert!(!body.contains("gone.opus"));
        assert!(body.contains("kept.opus"));
        assert!(body.starts_with("#EXTM3U\n#PLAYLIST:Mix\n"));
    }

    #[test]
    fn an_album_never_gets_a_mirror() {
        let home = tempfile::tempdir().unwrap();
        let paths = crate::paths::Paths::for_tests(home.path());
        let store = Store::open(&paths.music_dir());
        write(&paths, &store, "MPREb_123", "Some Album", &[track("a", "One", 10)]);
        assert!(!playlists_dir(&paths).join("Some Album.m3u8").exists());
    }
}

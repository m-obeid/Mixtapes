//! Where a download lands and what it is called.
//!
//! Port of the path half of downloads.py: the format table, the folder
//! structure preference, and the filename rules that keep a name inside the
//! filesystem's per-component byte limit.

use std::path::{Path, PathBuf};

use crate::paths::{Paths, sanitize_filename};

/// Audio formats offered, in the order the settings list shows them.
/// `codec` is what yt-dlp is asked to extract; the extension follows it.
pub const FORMATS: [(&str, &str); 5] = [("opus", "opus"), ("mp3", "mp3"), ("m4a", "aac"), ("flac", "flac"), ("ogg", "vorbis")];

pub const DEFAULT_FORMAT: &str = "opus";

/// Folder layouts, in settings order: Artist/Album/Song, Artist/Song, no folders.
pub const FOLDER_STRUCTURES: [&str; 3] = ["artist_album", "artist", "flat"];

pub const DEFAULT_FOLDER_STRUCTURE: &str = "artist_album";

/// What a filename may take before the extension and any disambiguating suffix.
const FILENAME_BYTE_BUDGET: usize = 200;

/// Fallback when the filesystem does not report a limit.
const NAME_MAX_BYTES: usize = 255;

/// The chosen download format, falling back to opus.
pub fn preferred_format(paths: &Paths) -> String {
    let value = paths.read_prefs().get("download_format").and_then(|v| v.as_str()).map(str::to_owned);
    value.filter(|f| FORMATS.iter().any(|(name, _)| name == f)).unwrap_or_else(|| DEFAULT_FORMAT.to_owned())
}

/// The codec yt-dlp extracts for a format name.
pub fn codec_for(format: &str) -> &'static str {
    FORMATS.iter().find(|(name, _)| *name == format).map(|(_, codec)| *codec).unwrap_or("opus")
}

pub fn folder_structure(paths: &Paths) -> String {
    let value = paths.read_prefs().get("download_folder_structure").and_then(|v| v.as_str()).map(str::to_owned);
    value.filter(|s| FOLDER_STRUCTURES.contains(&s.as_str())).unwrap_or_else(|| DEFAULT_FOLDER_STRUCTURE.to_owned())
}

pub fn use_songs_subdir(paths: &Paths) -> bool {
    paths.read_prefs().get("use_songs_subdir").and_then(|v| v.as_bool()).unwrap_or(false)
}

/// Trim `name` so its UTF-8 form fits in `max_bytes` without splitting a character.
pub fn truncate_bytes(name: &str, max_bytes: usize) -> String {
    if name.len() <= max_bytes {
        return name.to_owned();
    }
    let mut end = max_bytes;
    while end > 0 && !name.is_char_boundary(end) {
        end -= 1;
    }
    name[..end].to_owned()
}

/// The longest filename the filesystem holding `dir` accepts, in bytes.
///
/// ext4 and APFS report 255, an encrypted home reports about 143, some network
/// mounts less. The directory may not exist yet, so probe the nearest ancestor
/// that does.
pub fn name_max(dir: &Path) -> usize {
    let mut probe = dir;
    while !probe.exists() {
        match probe.parent() {
            Some(parent) if parent != probe => probe = parent,
            _ => break,
        }
    }
    let Ok(c_path) = std::ffi::CString::new(probe.as_os_str().as_encoded_bytes()) else {
        return NAME_MAX_BYTES;
    };
    // SAFETY: the path is a valid NUL-terminated string for the length of the call.
    let value = unsafe { libc::pathconf(c_path.as_ptr(), libc::_PC_NAME_MAX) };
    if value > 0 { value as usize } else { NAME_MAX_BYTES }
}

/// A filename component: invalid characters removed, length capped.
pub fn safe_component(name: &str, max_bytes: usize) -> String {
    let cleaned = sanitize_filename(name);
    let cut = truncate_bytes(&cleaned, max_bytes.min(FILENAME_BYTE_BUDGET));
    let trimmed = cut.trim_matches(|c| c == '.' || c == ' ');
    if trimmed.is_empty() { "Unknown".to_owned() } else { trimmed.to_owned() }
}

/// Where a track's file goes under the chosen structure.
///
/// Songs sit in <music>/Songs when the preference is on, playlists always in
/// <music>/Playlists. Only the first artist names the folder.
pub fn download_dir(paths: &Paths, artist: &str, album: &str) -> PathBuf {
    let music = paths.music_dir();
    let base = if use_songs_subdir(paths) { music.join("Songs") } else { music };
    let structure = folder_structure(paths);
    if structure == "flat" {
        return base;
    }
    let budget = name_max(&base);
    let first_artist = artist.split(',').next().unwrap_or(artist).trim();
    let artist_dir = base.join(safe_component(first_artist, budget));
    if structure == "artist_album" && !album.is_empty() {
        return artist_dir.join(safe_component(album, budget));
    }
    artist_dir
}

/// The file a track downloads to, extension included.
///
/// The name leaves room for the extension and for a `[video id]` suffix, which
/// is what keeps two songs of the same name in one folder apart.
pub fn file_name(dir: &Path, title: &str, extension: &str) -> String {
    format!("{}.{extension}", stem(dir, title, extension))
}

/// The same name with the video id in it, for the second song of that title.
pub fn disambiguated_name(dir: &Path, title: &str, extension: &str, video_id: &str) -> String {
    format!("{} [{video_id}].{extension}", stem(dir, title, extension))
}

fn stem(dir: &Path, title: &str, extension: &str) -> String {
    let reserve = 11 + extension.len() + 8;
    let budget = name_max(dir).saturating_sub(reserve).max(1);
    safe_component(title, budget)
}

/// Where playlist mirrors live: <music>/Playlists.
pub fn playlists_dir(paths: &Paths) -> PathBuf {
    paths.music_dir().join("Playlists")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_long_name_is_cut_on_a_character_boundary() {
        let name = "ト".repeat(100);
        let cut = truncate_bytes(&name, 200);
        assert_eq!(cut.len(), 198, "three-byte characters cannot fill 200 exactly");
        assert!(name.starts_with(&cut));
    }

    #[test]
    fn a_component_drops_path_characters_and_never_empties() {
        assert_eq!(safe_component("AC/DC: Back?", 255), "ACDC Back");
        assert_eq!(safe_component("...", 255), "Unknown");
        assert_eq!(safe_component("", 255), "Unknown");
    }

    #[test]
    fn the_file_name_leaves_room_for_the_extension_and_the_id() {
        let dir = Path::new("/tmp");
        let name = file_name(dir, &"a".repeat(300), "opus");
        assert!(name.len() <= name_max(dir), "{}", name.len());
        assert!(disambiguated_name(dir, &"a".repeat(300), "opus", "abc12345678").len() <= name_max(dir));
        assert!(name.ends_with(".opus"));
        let with_id = disambiguated_name(dir, "Song", "opus", "abc123");
        assert_eq!(with_id, "Song [abc123].opus");
    }

    #[test]
    fn codecs_follow_the_format_names() {
        assert_eq!(codec_for("m4a"), "aac");
        assert_eq!(codec_for("ogg"), "vorbis");
        assert_eq!(codec_for("nonsense"), "opus");
    }
}

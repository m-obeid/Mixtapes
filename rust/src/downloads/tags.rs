//! Tags and cover art on a finished download.
//!
//! Port of _tag_file: title, artist, album, album artist, track number and
//! total, year, the cover, and the YouTube Music ids in a comment so the file
//! can be matched back to its track later. Formats differ in how they store
//! these; `lofty` picks the right tag for the container, the way mutagen did.

use std::path::Path;

use lofty::config::WriteOptions;
use lofty::file::{AudioFile, TaggedFileExt};
use lofty::picture::{MimeType, Picture, PictureType};
use lofty::tag::{ItemKey, Tag};

/// What goes on the file.
#[derive(Debug, Default, Clone)]
pub struct Tags {
    pub title: String,
    pub artist: String,
    pub album: String,
    pub album_artist: String,
    pub track_number: Option<u32>,
    pub track_total: Option<u32>,
    pub year: String,
    pub video_id: String,
    pub album_id: String,
}

/// Write the tags and, when there are bytes for it, the cover.
///
/// A failure here leaves a playable file with poor metadata, so it is logged
/// and the download still counts.
pub fn write(path: &Path, tags: &Tags, cover: Option<&[u8]>) {
    if let Err(err) = apply(path, tags, cover) {
        tracing::warn!(%err, path = %path.display(), "tagging failed");
    }
}

fn apply(path: &Path, tags: &Tags, cover: Option<&[u8]>) -> lofty::error::Result<()> {
    let mut file = lofty::read_from_path(path)?;
    let kind = file.primary_tag_type();
    if file.primary_tag().is_none() {
        file.insert_tag(Tag::new(kind));
    }
    let Some(tag) = file.primary_tag_mut() else { return Ok(()) };

    tag.insert_text(ItemKey::TrackTitle, tags.title.clone());
    tag.insert_text(ItemKey::TrackArtist, tags.artist.clone());
    if !tags.album.is_empty() {
        tag.insert_text(ItemKey::AlbumTitle, tags.album.clone());
    }
    if !tags.album_artist.is_empty() {
        tag.insert_text(ItemKey::AlbumArtist, tags.album_artist.clone());
    }
    if let Some(number) = tags.track_number {
        tag.insert_text(ItemKey::TrackNumber, number.to_string());
    }
    if let Some(total) = tags.track_total.filter(|t| *t > 0) {
        tag.insert_text(ItemKey::TrackTotal, total.to_string());
    }
    if !tags.year.is_empty() {
        tag.insert_text(ItemKey::RecordingDate, tags.year.clone());
    }
    // The ids ride in the comment, which every container we write supports.
    tag.insert_text(ItemKey::Comment, ytm_comment(&tags.video_id, &tags.album_id));

    if let Some(bytes) = cover.filter(|b| !b.is_empty()) {
        tag.push_picture(Picture::unchecked(bytes.to_vec()).pic_type(PictureType::CoverFront).mime_type(MimeType::Jpeg).description("Cover").build());
    }
    file.save_to_path(path, WriteOptions::default())
}

/// The JSON the Python app wrote, so files stay readable by both.
fn ytm_comment(video_id: &str, album_id: &str) -> String {
    serde_json::json!({ "videoId": video_id, "albumId": album_id, "source": "YouTube Music (Mixtapes)" }).to_string()
}

/// The cover embedded in a downloaded file, for showing art offline.
pub fn embedded_cover(path: &Path) -> Option<Vec<u8>> {
    let file = lofty::read_from_path(path).ok()?;
    let tag = file.primary_tag().or_else(|| file.first_tag())?;
    let picture = tag.pictures().iter().find(|p| p.pic_type() == PictureType::CoverFront).or_else(|| tag.pictures().first())?;
    Some(picture.data().to_vec())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_comment_carries_both_ids() {
        let comment = ytm_comment("vid123", "MPREabc");
        let parsed: serde_json::Value = serde_json::from_str(&comment).unwrap();
        assert_eq!(parsed["videoId"], "vid123");
        assert_eq!(parsed["albumId"], "MPREabc");
        assert!(parsed["source"].as_str().unwrap().contains("Mixtapes"));
    }

    /// Writes a real file, so it also proves the lofty round trip.
    #[test]
    fn tags_and_cover_survive_a_round_trip() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("song.flac");
        let made = std::process::Command::new("ffmpeg")
            .args(["-y", "-f", "lavfi", "-i", "anullsrc=r=48000:cl=mono", "-t", "1"])
            .arg(&path)
            .stdout(std::process::Stdio::null())
            .stderr(std::process::Stdio::null())
            .status();
        if !made.map(|s| s.success()).unwrap_or(false) {
            println!("ffmpeg missing, skipping");
            return;
        }
        let cover = std::fs::read("tests/assets/cover.jpg").unwrap_or_else(|_| vec![0xff, 0xd8, 0xff, 0xe0, 0, 0, 0, 0]);
        let tags = Tags {
            title: "Song".into(),
            artist: "Artist".into(),
            album: "Album".into(),
            album_artist: "Album Artist".into(),
            track_number: Some(3),
            track_total: Some(12),
            year: "2024".into(),
            video_id: "vid123".into(),
            album_id: "MPREabc".into(),
        };
        write(&path, &tags, Some(&cover));

        let file = lofty::read_from_path(&path).unwrap();
        let tag = file.primary_tag().unwrap();
        assert_eq!(tag.get_string(ItemKey::TrackTitle), Some("Song"));
        assert_eq!(tag.get_string(ItemKey::AlbumArtist), Some("Album Artist"));
        assert_eq!(tag.get_string(ItemKey::TrackNumber), Some("3"));
        assert!(tag.get_string(ItemKey::Comment).unwrap().contains("vid123"));
        assert_eq!(embedded_cover(&path).as_deref(), Some(cover.as_slice()));
    }
}

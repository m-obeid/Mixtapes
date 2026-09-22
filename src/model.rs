//! Plain data types shared by every layer. No GTK, no GStreamer, no reqwest.
//! Everything here is Send + Sync so it can cross threads by value.

use std::sync::LazyLock;

use regex::Regex;
use serde::{Deserialize, Serialize};

/// The "161 songs" or "50K views" a playlist description leads or trails with.
static PLAYLIST_UNIT_RE: LazyLock<Regex> = LazyLock::new(|| Regex::new(r"(?i)\b\d[\d.,]*\s*[KMB]?\s*(songs?|episodes?|videos?|tracks?|views?|plays?|subscribers?|monthly listeners?|listeners?)\b").unwrap());

/// YouTube video id. Newtype so it never gets mixed up with browse or playlist ids.
#[derive(Clone, Debug, Default, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(transparent)]
pub struct VideoId(pub String);

impl VideoId {
    pub fn as_str(&self) -> &str {
        &self.0
    }
}

impl std::fmt::Display for VideoId {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(&self.0)
    }
}

#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct Person {
    pub name: String,
    #[serde(default)]
    pub id: Option<String>,
}

#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct Named {
    pub name: String,
    #[serde(default)]
    pub id: Option<String>,
}

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "UPPERCASE")]
pub enum LikeStatus {
    Like,
    Dislike,
    #[default]
    Indifferent,
}

impl LikeStatus {
    pub fn parse(value: &str) -> Self {
        match value {
            "LIKE" => LikeStatus::Like,
            "DISLIKE" => LikeStatus::Dislike,
            _ => LikeStatus::Indifferent,
        }
    }

    pub fn as_str(self) -> &'static str {
        match self {
            LikeStatus::Like => "LIKE",
            LikeStatus::Dislike => "DISLIKE",
            LikeStatus::Indifferent => "INDIFFERENT",
        }
    }
}

/// One queue entry. Replaces the loosely keyed dict the Python player carried.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct Track {
    pub video_id: VideoId,
    pub title: String,
    /// Display string, already joined. Kept alongside `artists` like the Python dict did.
    pub artist: String,
    #[serde(default)]
    pub artists: Vec<Person>,
    #[serde(default)]
    pub album: Option<Named>,
    #[serde(default)]
    pub thumb: Option<String>,
    #[serde(default)]
    pub duration_seconds: Option<u32>,
    #[serde(default)]
    pub like_status: LikeStatus,
    /// MUSIC_VIDEO_TYPE_ATV, OMV, UGC ... drives the audio-version swap.
    #[serde(default)]
    pub video_type: Option<String>,
    /// Set for upload-locker tracks. Those bypass the stream cache.
    #[serde(default)]
    pub entity_id: Option<String>,
    #[serde(default)]
    pub is_explicit: bool,
    /// A live stream: no duration, no seeking, a LIVE badge.
    #[serde(default)]
    pub is_live: bool,
    /// Playlist item id, needed to remove or move the row in its playlist.
    #[serde(default)]
    pub set_video_id: Option<String>,
    /// False when YouTube greys the row out (deleted or region-locked).
    #[serde(default = "default_true")]
    pub is_available: bool,
    /// Position on an album page, from the row's index column.
    #[serde(default)]
    pub track_number: Option<u32>,
}

fn default_true() -> bool {
    true
}

impl Default for Track {
    fn default() -> Self {
        Self {
            video_id: VideoId::default(),
            title: String::new(),
            artist: String::new(),
            artists: Vec::new(),
            album: None,
            thumb: None,
            duration_seconds: None,
            like_status: LikeStatus::default(),
            video_type: None,
            entity_id: None,
            is_explicit: false,
            is_live: false,
            set_video_id: None,
            is_available: true,
            track_number: None,
        }
    }
}

impl Track {
    pub fn is_upload(&self) -> bool {
        self.entity_id.is_some()
    }
}

/// Logical playback state shown to the UI. Registered as a GLib enum so it can be a GObject property.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, glib::Enum)]
#[enum_type(name = "MxPlaybackStatus")]
pub enum PlaybackStatus {
    #[default]
    Stopped,
    Loading,
    Playing,
    Paused,
}

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, glib::Enum)]
#[enum_type(name = "MxRepeatMode")]
pub enum RepeatMode {
    #[default]
    Off,
    Track,
    All,
}

/// Result of resolving a video id to something playbin can open.
#[derive(Clone, Debug, Default, Serialize, Deserialize)]
pub struct StreamInfo {
    pub uri: String,
    #[serde(default)]
    pub format_id: Option<String>,
    #[serde(default)]
    pub protocol: Option<String>,
    #[serde(default)]
    pub ext: Option<String>,
    #[serde(default)]
    pub acodec: Option<String>,
    #[serde(default)]
    pub title: Option<String>,
    #[serde(default)]
    pub uploader: Option<String>,
    #[serde(default)]
    pub thumbnail: Option<String>,
    /// True for file:// URIs (downloads, tmpfs staging).
    #[serde(default)]
    pub is_local: bool,
    /// True when the URI came from the disk cache rather than a fresh resolution.
    #[serde(default)]
    pub from_cache: bool,
}

/// What a browse surface item is. Drives icons, subtitles and activation.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum ItemKind {
    #[default]
    Song,
    Video,
    Album,
    Playlist,
    Artist,
}

impl ItemKind {
    pub fn icon(self) -> &'static str {
        match self {
            ItemKind::Song => "audio-x-generic-symbolic",
            ItemKind::Video => "video-x-generic-symbolic",
            ItemKind::Album => "media-optical-symbolic",
            ItemKind::Playlist => "view-list-symbolic",
            ItemKind::Artist => "avatar-default-symbolic",
        }
    }

    pub fn is_playable(self) -> bool {
        matches!(self, ItemKind::Song | ItemKind::Video)
    }
}

/// One card or row on Home, Explore, Library or search results.
#[derive(Clone, Debug, Default, PartialEq, Serialize, Deserialize)]
pub struct MediaItem {
    pub kind: ItemKind,
    /// videoId, browseId or playlistId depending on `kind`.
    pub id: String,
    pub title: String,
    #[serde(default)]
    pub artists: Vec<Person>,
    #[serde(default)]
    pub album: Option<Named>,
    #[serde(default)]
    pub thumb: Option<String>,
    #[serde(default)]
    pub year: Option<String>,
    /// Album, Single, EP.
    #[serde(default)]
    pub item_type: Option<String>,
    #[serde(default)]
    pub explicit: bool,
    /// A live stream, from the Live badge beside the subtitle.
    #[serde(default)]
    pub is_live: bool,
    #[serde(default)]
    pub duration_seconds: Option<u32>,
    #[serde(default)]
    pub views: Option<String>,
    #[serde(default)]
    pub count: Option<String>,
    #[serde(default)]
    pub subscribers: Option<String>,
    #[serde(default)]
    pub description: Option<String>,
    /// Album cards: the OLAK playlist behind the browse id, from the card menu.
    #[serde(default)]
    pub playlist_id: Option<String>,
    /// What the like button starts on. Rows that come from a playlist or the
    /// history know it; cards off a carousel do not, and say so with None.
    #[serde(default)]
    pub like_status: Option<LikeStatus>,
}

impl MediaItem {
    pub fn artists_text(&self) -> String {
        self.artists.iter().map(|a| a.name.as_str()).filter(|n| !n.is_empty()).collect::<Vec<_>>().join(", ")
    }

    /// The small icon beside the subtitle. A live stream shows an antenna, whatever its kind.
    pub fn kind_icon(&self) -> &'static str {
        if self.is_live { "triangular-antenna-symbolic" } else { self.kind.icon() }
    }

    pub fn kind_word(&self) -> String {
        if self.is_live {
            return "Live".to_owned();
        }
        match self.kind {
            ItemKind::Album => self.item_type.clone().unwrap_or_else(|| "Album".to_owned()),
            ItemKind::Song => "Song".to_owned(),
            ItemKind::Video => "Video".to_owned(),
            ItemKind::Playlist => "Playlist".to_owned(),
            ItemKind::Artist => "Artist".to_owned(),
        }
    }

    /// "3:45", or "1:59:59" past an hour, as a long mix reads. Nothing for a live stream.
    pub fn duration_text(&self) -> Option<String> {
        if self.is_live {
            return None;
        }
        self.duration_seconds.map(|s| if s >= 3600 { format!("{}:{:02}:{:02}", s / 3600, (s % 3600) / 60, s % 60) } else { format!("{}:{:02}", s / 60, s % 60) })
    }

    /// Port of home.py _detail_for: the secondary line under a title.
    pub fn detail(&self) -> String {
        let join = |parts: Vec<String>| parts.into_iter().filter(|p| !p.is_empty()).collect::<Vec<_>>().join(" · ");
        match self.kind {
            ItemKind::Playlist => self.playlist_detail(),
            ItemKind::Video => join(vec![self.artists_text(), self.views.clone().unwrap_or_default(), self.duration_text().unwrap_or_default()]),
            ItemKind::Album => join(vec![self.artists_text(), self.year.clone().unwrap_or_default()]),
            ItemKind::Artist => match self.subscribers.as_deref().map(str::trim).filter(|s| !s.is_empty()) {
                // "12M" alone is a count; "12M monthly listeners" says so itself.
                Some(subs) if subs.chars().any(char::is_alphabetic) => subs.to_owned(),
                Some(subs) => format!("{subs} subscribers"),
                None => String::new(),
            },
            // An album named after its only song says nothing twice.
            ItemKind::Song => {
                let album = self.album.as_ref().map(|a| a.name.as_str()).filter(|name| *name != self.title).unwrap_or_default();
                join(vec![self.artists_text(), album.to_owned(), self.duration_text().unwrap_or_default()])
            }
        }
    }

    /// Port of _playlist_detail: the count or view total out of the
    /// description, whatever the rest of it says.
    fn playlist_detail(&self) -> String {
        if let Some(description) = self.description.as_deref().map(str::trim).filter(|d| !d.is_empty()) {
            if let Some(unit) = PLAYLIST_UNIT_RE.find(description) {
                return unit.as_str().to_owned();
            }
            if let Some(last) = description.rsplit(['\u{2022}', '\u{b7}']).map(str::trim).find(|part| !part.is_empty()) {
                return last.to_owned();
            }
        }
        match self.count.as_deref().filter(|c| !c.is_empty()) {
            Some(count) => format!("{count} songs"),
            None => self.artists_text(),
        }
    }

    /// Queue entry for songs and videos, None for collections.
    pub fn to_track(&self) -> Option<Track> {
        if !self.kind.is_playable() || self.id.is_empty() {
            return None;
        }
        Some(Track {
            video_id: VideoId(self.id.clone()),
            title: self.title.clone(),
            artist: self.artists_text(),
            artists: self.artists.clone(),
            album: self.album.clone(),
            thumb: self.thumb.clone(),
            duration_seconds: self.duration_seconds,
            like_status: self.like_status.unwrap_or_default(),
            video_type: Some(if self.kind == ItemKind::Song { "MUSIC_VIDEO_TYPE_ATV".to_owned() } else { "MUSIC_VIDEO_TYPE_OMV".to_owned() }),
            is_explicit: self.explicit,
            is_live: self.is_live,
            ..Track::default()
        })
    }
}

/// Snapshot of the signed-in session that the media layers attach to HTTP requests.
/// Produced by the network client, consumed by yt-dlp and souphttpsrc.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct HttpAuth {
    pub cookie: String,
    pub user_agent: String,
    pub authorization: Option<String>,
}

//! Mock catalog standing in for the YouTube Music endpoints. Every song
//! carries a real video id, so activating one plays through the resolver.

use crate::model::{ItemKind, MediaItem, Named, Person};

pub struct HomeSection {
    pub title: String,
    pub items: Vec<MediaItem>,
}

pub struct ExploreData {
    pub moods: Vec<String>,
    pub genres: Vec<String>,
    pub new_releases: Vec<MediaItem>,
    pub new_videos: Vec<MediaItem>,
    pub trending: Vec<MediaItem>,
    pub chart_videos: Vec<MediaItem>,
    pub chart_genres: Vec<MediaItem>,
    pub chart_artists: Vec<MediaItem>,
    pub countries: Vec<(&'static str, &'static str)>,
}

fn thumb(video_id: &str) -> Option<String> {
    Some(format!("https://i.ytimg.com/vi/{video_id}/hqdefault.jpg"))
}

fn person(name: &str, id: &str) -> Person {
    Person { name: name.to_owned(), id: Some(id.to_owned()) }
}

fn song(video_id: &str, title: &str, artist: (&str, &str), album: &str, seconds: u32, year: &str) -> MediaItem {
    MediaItem {
        kind: ItemKind::Song,
        id: video_id.to_owned(),
        title: title.to_owned(),
        artists: vec![person(artist.0, artist.1)],
        album: Some(Named { name: album.to_owned(), id: Some(format!("MPREb_{}", album.replace(' ', ""))) }),
        thumb: thumb(video_id),
        year: Some(year.to_owned()),
        duration_seconds: Some(seconds),
        ..MediaItem::default()
    }
}

fn video(video_id: &str, title: &str, artist: (&str, &str), views: &str, seconds: u32) -> MediaItem {
    MediaItem {
        kind: ItemKind::Video,
        id: video_id.to_owned(),
        title: title.to_owned(),
        artists: vec![person(artist.0, artist.1)],
        thumb: thumb(video_id),
        views: Some(views.to_owned()),
        duration_seconds: Some(seconds),
        ..MediaItem::default()
    }
}

fn album(id: &str, title: &str, artist: (&str, &str), year: &str, cover_video: &str, item_type: &str) -> MediaItem {
    MediaItem {
        kind: ItemKind::Album,
        id: id.to_owned(),
        title: title.to_owned(),
        artists: vec![person(artist.0, artist.1)],
        thumb: thumb(cover_video),
        year: Some(year.to_owned()),
        item_type: Some(item_type.to_owned()),
        ..MediaItem::default()
    }
}

fn playlist(id: &str, title: &str, description: &str, count: &str, cover_video: &str) -> MediaItem {
    MediaItem {
        kind: ItemKind::Playlist,
        id: id.to_owned(),
        title: title.to_owned(),
        description: Some(description.to_owned()),
        count: Some(count.to_owned()),
        thumb: thumb(cover_video),
        ..MediaItem::default()
    }
}

fn artist(id: &str, name: &str, subscribers: &str, cover_video: &str) -> MediaItem {
    MediaItem {
        kind: ItemKind::Artist,
        id: id.to_owned(),
        title: name.to_owned(),
        subscribers: Some(subscribers.to_owned()),
        thumb: thumb(cover_video),
        ..MediaItem::default()
    }
}

const RICK: (&str, &str) = ("Rick Astley", "UCuAXFkgsw1L7xaCfnd5JJOw");
const PSY: (&str, &str) = ("PSY", "UCrDkAvwZum-UTjHmzDI2iIw");
const FONSI: (&str, &str) = ("Luis Fonsi", "UCxoq-PAQeAdk_zyg8YS0JqA");
const ED: (&str, &str) = ("Ed Sheeran", "UC0C-w0YjGpqDXGB8IHb662A");
const WIZ: (&str, &str) = ("Wiz Khalifa", "UCKQmIadQLZmg8ir2LGfuPsA");
const QUEEN: (&str, &str) = ("Queen", "UCiMhD4jzUqG-IgPzUmmytRQ");
const RONSON: (&str, &str) = ("Mark Ronson", "UCq_MIBiUdBnc_RVrnnMxaVg");
const KATY: (&str, &str) = ("Katy Perry", "UCYvmuw-JtVrTZQ-7Y4kd63Q");
const ONE_REP: (&str, &str) = ("OneRepublic", "UCQ5kHOKpF3-1_UCKaqXARRg");
const WALKER: (&str, &str) = ("Alan Walker", "UCJrOtniJ0-NWz37R30urifQ");
const ADELE: (&str, &str) = ("Adele", "UComP_epzeKzvBX156r6pm1Q");
const LINKIN: (&str, &str) = ("Linkin Park", "UCZU9T1ceaOgwfLRq7OKFU4Q");
const TAYLOR: (&str, &str) = ("Taylor Swift", "UCqECaJ8Gagnn7YCbPEzWH6g");

pub fn songs() -> Vec<MediaItem> {
    vec![
        song("dQw4w9WgXcQ", "Never Gonna Give You Up", RICK, "Whenever You Need Somebody", 213, "1987"),
        song("9bZkp7q19f0", "Gangnam Style", PSY, "PSY 6 (Six Rules), Part 1", 252, "2012"),
        song("kJQP7kiw5Fk", "Despacito", FONSI, "Vida", 282, "2017"),
        song("JGwWNGJdvx8", "Shape of You", ED, "÷ (Divide)", 263, "2017"),
        song("RgKAFK5djSk", "See You Again", WIZ, "Furious 7", 229, "2015"),
        song("fJ9rUzIMcZQ", "Bohemian Rhapsody", QUEEN, "A Night at the Opera", 354, "1975"),
        song("OPf0YbXqDm0", "Uptown Funk", RONSON, "Uptown Special", 270, "2014"),
        song("CevxZvSJLk8", "Roar", KATY, "Prism", 269, "2013"),
        song("hT_nvWreIhg", "Counting Stars", ONE_REP, "Native", 257, "2013"),
        song("60ItHLz5WEA", "Faded", WALKER, "Different World", 212, "2015"),
        song("YQHsXMglC9A", "Hello", ADELE, "25", 355, "2015"),
        song("2Vv-BfVoq4g", "Perfect", ED, "÷ (Divide)", 263, "2017"),
        song("kXYiU_JCYtU", "Numb", LINKIN, "Meteora", 187, "2003"),
        song("e-ORhEE9VVg", "Blank Space", TAYLOR, "1989", 231, "2014"),
    ]
}

pub fn videos() -> Vec<MediaItem> {
    vec![
        video("9bZkp7q19f0", "Gangnam Style (Official Video)", PSY, "5.2B views", 252),
        video("kJQP7kiw5Fk", "Despacito (Official Video)", FONSI, "8.6B views", 282),
        video("OPf0YbXqDm0", "Uptown Funk (Official Video)", RONSON, "5.3B views", 270),
        video("CevxZvSJLk8", "Roar (Official Video)", KATY, "4B views", 269),
        video("60ItHLz5WEA", "Faded (Official Video)", WALKER, "3.6B views", 212),
    ]
}

pub fn albums() -> Vec<MediaItem> {
    vec![
        album("MPREb_divide", "÷ (Divide)", ED, "2017", "JGwWNGJdvx8", "Album"),
        album("MPREb_prism", "Prism", KATY, "2013", "CevxZvSJLk8", "Album"),
        album("MPREb_25", "25", ADELE, "2015", "YQHsXMglC9A", "Album"),
        album("MPREb_meteora", "Meteora", LINKIN, "2003", "kXYiU_JCYtU", "Album"),
        album("MPREb_1989", "1989", TAYLOR, "2014", "e-ORhEE9VVg", "Album"),
        album("MPREb_faded", "Faded", WALKER, "2015", "60ItHLz5WEA", "Single"),
        album("MPREb_nightopera", "A Night at the Opera", QUEEN, "1975", "fJ9rUzIMcZQ", "Album"),
        album("MPREb_native", "Native", ONE_REP, "2013", "hT_nvWreIhg", "Album"),
    ]
}

pub fn playlists() -> Vec<MediaItem> {
    vec![
        playlist("RDCLAK5uy_mock1", "Pop Hits 2010s", "The decade's biggest pop songs", "50", "JGwWNGJdvx8"),
        playlist("RDCLAK5uy_mock2", "Feel-Good Classics", "Songs everyone knows", "40", "OPf0YbXqDm0"),
        playlist("RDCLAK5uy_mock3", "Late Night Drive", "Moody electronic and pop", "35", "60ItHLz5WEA"),
        playlist("RDCLAK5uy_mock4", "Rock Anthems", "Stadium-sized rock", "45", "fJ9rUzIMcZQ"),
        playlist("RDCLAK5uy_mock5", "Ballads", "Big voices, big feelings", "30", "YQHsXMglC9A"),
    ]
}

pub fn artists() -> Vec<MediaItem> {
    vec![
        artist(ED.1, ED.0, "55M subscribers", "JGwWNGJdvx8"),
        artist(ADELE.1, ADELE.0, "32M subscribers", "YQHsXMglC9A"),
        artist(QUEEN.1, QUEEN.0, "18M subscribers", "fJ9rUzIMcZQ"),
        artist(TAYLOR.1, TAYLOR.0, "60M subscribers", "e-ORhEE9VVg"),
        artist(LINKIN.1, LINKIN.0, "15M subscribers", "kXYiU_JCYtU"),
        artist(WALKER.1, WALKER.0, "45M subscribers", "60ItHLz5WEA"),
    ]
}

pub fn home_sections() -> Vec<HomeSection> {
    let songs = songs();
    vec![
        HomeSection { title: "Quick picks".into(), items: songs.iter().take(9).cloned().collect() },
        HomeSection { title: "Listen again".into(), items: songs.iter().skip(4).take(6).cloned().collect() },
        HomeSection { title: "Mixed for you".into(), items: playlists() },
        HomeSection { title: "Recommended albums".into(), items: albums() },
        HomeSection { title: "New music videos".into(), items: videos() },
        HomeSection { title: "Similar artists".into(), items: artists() },
    ]
}

pub fn explore() -> ExploreData {
    let songs = songs();
    ExploreData {
        moods: ["Chill", "Commute", "Energy Boosters", "Feel Good", "Focus", "Party", "Romance", "Sad", "Sleep", "Workout"].iter().map(|s| s.to_string()).collect(),
        genres: ["Pop", "Hip-Hop", "Rock", "Electronic", "R&B", "Latin", "K-Pop", "Country", "Jazz", "Classical", "Metal", "Indie"].iter().map(|s| s.to_string()).collect(),
        new_releases: albums(),
        new_videos: videos(),
        trending: songs.iter().skip(8).take(5).cloned().collect(),
        chart_videos: playlists().into_iter().take(3).collect(),
        chart_genres: playlists().into_iter().skip(2).collect(),
        chart_artists: artists(),
        countries: vec![("ZZ", "Global"), ("US", "United States"), ("GB", "United Kingdom"), ("DE", "Germany"), ("FR", "France"), ("JP", "Japan"), ("BR", "Brazil"), ("KR", "South Korea"), ("IN", "India"), ("MX", "Mexico")],
    }
}

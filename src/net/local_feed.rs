//! Home shelves for listeners without an account. YouTube's signed-out feed
//! is the same for everyone in a region, so these come from what this device
//! knows: the play log and the likes in local.db. Everything fetched here
//! works anonymously: a radio per seed song and an artist page per favorite.

use std::collections::{HashMap, HashSet};
use std::sync::Arc;
use std::time::{SystemTime, UNIX_EPOCH};

use crate::local_library::LocalLibrary;
use crate::model::{ItemKind, MediaItem, Person, Track};
use crate::net::browse::Browse;
use crate::net::home::HomeSection;
use crate::net::search::futures_join_all;

const DAY: u64 = 24 * 3600;
/// Songs the quick picks are radios of.
const SEEDS: usize = 3;
/// Artists the "Similar to" and new release shelves look at.
const ARTISTS: usize = 3;
/// Followed artists whose pages are fetched for new releases, on top of the most heard.
const FOLLOWED: usize = 5;
/// "Similar to" shelves, so the feed does not turn into a wall of artists.
const SIMILAR_SHELVES: usize = 3;
const QUICK_PICKS: usize = 20;
const RADIO_LENGTH: usize = 25;
const LISTEN_AGAIN: usize = 20;
/// Fewer plays than this and the shelf would repeat the last hour.
const LISTEN_AGAIN_MIN: usize = 4;
const NEW_RELEASES: usize = 12;
/// A like counts for this many plays when ranking artists.
const LIKE_WEIGHT: usize = 2;

pub const QUICK_PICKS_TITLE: &str = "Quick picks";
/// English, because `home::classify_section` reads it. Home passes it to `i18n::gettext` where it is shown.
pub const LISTEN_AGAIN_TITLE: &str = tr_noop!("Listen again");

/// What the device knows about the listener, read on the GTK thread.
#[derive(Clone, Debug, Default)]
pub struct Signals {
    pub recent: Vec<Track>,
    pub seeds: Vec<Track>,
    pub artists: Vec<Person>,
}

impl Signals {
    pub fn read(local: &LocalLibrary) -> Self {
        let liked = local.liked();
        let mut seeds: Vec<Track> = Vec::new();
        for track in local.top_plays(30 * DAY, SEEDS * 2).into_iter().chain(liked.iter().cloned()) {
            if seeds.len() == SEEDS {
                break;
            }
            if is_video_id(track.video_id.as_str()) && !seeds.iter().any(|s| s.video_id == track.video_id) {
                seeds.push(track);
            }
        }
        let plays = local.plays_since(90 * DAY);
        let weighted = plays.iter().map(|t| (t, 1)).chain(liked.iter().map(|t| (t, LIKE_WEIGHT)));
        let followed = local.subscriptions().into_iter().take(FOLLOWED).map(|a| Person { name: a.title, id: Some(a.id) }).collect();
        Self { recent: local.recent_plays(LISTEN_AGAIN), seeds, artists: with_followed(top_artists(weighted, ARTISTS), followed) }
    }

    pub fn is_empty(&self) -> bool {
        self.recent.is_empty() && self.seeds.is_empty() && self.artists.is_empty()
    }
}

/// The most heard artists, then the followed ones not among them.
fn with_followed(mut artists: Vec<Person>, followed: Vec<Person>) -> Vec<Person> {
    for artist in followed {
        if !artists.iter().any(|a| a.id == artist.id) {
            artists.push(artist);
        }
    }
    artists
}

/// A radio seed has to be a real YouTube id, not a demo or local stand-in.
fn is_video_id(id: &str) -> bool {
    id.len() == 11 && id.bytes().all(|b| b.is_ascii_alphanumeric() || b == b'-' || b == b'_')
}

/// The artists heard most, by the first credited artist that has a channel.
fn top_artists<'a>(tracks: impl Iterator<Item = (&'a Track, usize)>, limit: usize) -> Vec<Person> {
    let mut counts: HashMap<String, (usize, usize, String)> = HashMap::new();
    for (order, (track, weight)) in tracks.enumerate() {
        let Some(artist) = track.artists.iter().find(|a| a.id.as_deref().is_some_and(|id| id.starts_with("UC"))) else { continue };
        let entry = counts.entry(artist.id.clone().unwrap_or_default()).or_insert((0, order, artist.name.clone()));
        entry.0 += weight;
    }
    let mut ranked: Vec<(String, (usize, usize, String))> = counts.into_iter().collect();
    // Most heard first; a tie goes to the one heard first in the log.
    ranked.sort_by(|a, b| b.1.0.cmp(&a.1.0).then(a.1.1.cmp(&b.1.1)));
    ranked.into_iter().take(limit).map(|(id, (_, _, name))| Person { name, id: Some(id) }).collect()
}

/// The shelves, in the order Home shows them. Each fetch that fails costs its
/// shelf only.
pub async fn build(api: Arc<dyn Browse>, signals: Signals) -> Vec<HomeSection> {
    let seed_ids: Vec<String> = signals.seeds.iter().map(|t| t.video_id.0.clone()).collect();
    let artists = futures_join_all(signals.artists.iter().map(|artist| {
        let (api, id) = (api.clone(), artist.id.clone().unwrap_or_default());
        async move { crate::net::artist::get_artist(api, &id).await }
    }));
    let (picks, artists) = tokio::join!(quick_picks(api.clone(), seed_ids), artists);

    let mut sections = Vec::new();
    sections.extend(picks);
    if signals.recent.len() >= LISTEN_AGAIN_MIN {
        sections.push(HomeSection { title: LISTEN_AGAIN_TITLE.to_owned(), items: signals.recent.iter().map(MediaItem::from_track).collect(), strapline_thumb: None, strapline: None });
    }

    let artists: Vec<_> = artists.into_iter().filter_map(|artist| artist.inspect_err(|err| tracing::warn!(%err, "local feed artist failed")).ok()).collect();
    let followed: HashSet<&str> = signals.artists.iter().filter_map(|a| a.id.as_deref()).collect();
    let this_year = current_year();
    let per_artist: Vec<Vec<MediaItem>> = artists
        .iter()
        .map(|artist| {
            let mut recent: Vec<MediaItem> = [&artist.singles, &artist.albums].into_iter().flatten().flat_map(|section| section.results.iter()).filter(|r| r.year.as_deref().and_then(|y| y.parse::<i32>().ok()).is_some_and(|y| y >= this_year - 1)).cloned().collect();
            recent.sort_by(|a, b| b.year.cmp(&a.year));
            recent
        })
        .collect();
    // Artists take turns, so the one with the busiest year fills no more than its share.
    let releases = interleave(per_artist, NEW_RELEASES, |item| item.id.clone());
    if !releases.is_empty() {
        sections.push(HomeSection { title: tr!("New from artists you like"), items: releases, strapline_thumb: None, strapline: None });
    }
    for artist in artists.iter().take(SIMILAR_SHELVES) {
        let Some(related) = &artist.related else { continue };
        let items: Vec<MediaItem> = related.results.iter().filter(|r| r.kind == ItemKind::Artist && !followed.contains(r.id.as_str())).cloned().collect();
        if items.is_empty() {
            continue;
        }
        // Translators: a small line above an artist's name, which is the shelf title below it.
        sections.push(HomeSection { title: artist.name.clone(), items, strapline_thumb: artist.thumbnails.first().cloned(), strapline: Some(tr!("Similar to")) });
    }
    sections
}

/// Quick picks made the way YouTube made them: a radio per seed song, taken
/// in turns. YouTube stopped sending its own row in late 2026, so the signed
/// in feed builds it too, seeded from Listen again.
pub async fn quick_picks(api: Arc<dyn Browse>, seed_ids: Vec<String>) -> Option<HomeSection> {
    let radios = futures_join_all(seed_ids.iter().map(|id| {
        let (api, id) = (api.clone(), id.clone());
        async move { crate::net::playlists::get_watch_playlist(&api, Some(&id), None, RADIO_LENGTH, true).await }
    }))
    .await;
    let radios: Vec<Vec<Track>> = radios
        .into_iter()
        .filter_map(|radio| radio.inspect_err(|err| tracing::warn!(%err, "quick picks radio failed")).ok())
        .map(|radio| radio.tracks.into_iter().map(|t| t.track).collect())
        .collect();
    // Each radio opens with its seed, which Listen again already shows.
    let seeds: HashSet<&str> = seed_ids.iter().map(String::as_str).collect();
    let radios = radios.into_iter().map(|radio| radio.into_iter().filter(|t| !seeds.contains(t.video_id.as_str())).collect()).collect();
    let picks = interleave(radios, QUICK_PICKS, |t| t.video_id.0.clone());
    (!picks.is_empty()).then(|| HomeSection { title: QUICK_PICKS_TITLE.to_owned(), items: picks.iter().map(MediaItem::from_track).collect(), strapline_thumb: None, strapline: None })
}

/// Songs to seed quick picks with, from a feed that came without them:
/// Listen again first, then Forgotten favorites. None when the feed has its own row.
pub fn quick_pick_seeds(sections: &[HomeSection]) -> Option<Vec<String>> {
    use crate::net::home::{Bucket, classify_section};
    if sections.iter().any(|s| s.title.to_lowercase().contains("quick pick")) {
        return None;
    }
    let mut seeds = Vec::new();
    for bucket in [Bucket::ListenAgain, Bucket::Forgotten] {
        for section in sections.iter().filter(|s| classify_section(&s.title) == Some(bucket)) {
            for item in section.items.iter().filter(|i| matches!(i.kind, ItemKind::Song | ItemKind::Video) && is_video_id(&i.id)) {
                if seeds.len() < SEEDS && !seeds.contains(&item.id) {
                    seeds.push(item.id.clone());
                }
            }
        }
    }
    Some(seeds)
}

/// One from each list in turn, each entry once.
fn interleave<T: Clone>(lists: Vec<Vec<T>>, limit: usize, key: impl Fn(&T) -> String) -> Vec<T> {
    let mut seen = HashSet::new();
    let mut out = Vec::new();
    let longest = lists.iter().map(Vec::len).max().unwrap_or(0);
    for i in 0..longest {
        for list in &lists {
            if out.len() == limit {
                return out;
            }
            if let Some(entry) = list.get(i) {
                if seen.insert(key(entry)) {
                    out.push(entry.clone());
                }
            }
        }
    }
    out
}

fn current_year() -> i32 {
    let days = SystemTime::now().duration_since(UNIX_EPOCH).map(|d| d.as_secs() / DAY).unwrap_or(0);
    1970 + (days as f64 / 365.2425) as i32
}

/// The local shelves ahead of YouTube's, minus YouTube's own rows of the same name.
pub fn merge(local: Vec<HomeSection>, remote: Vec<HomeSection>) -> Vec<HomeSection> {
    let taken: HashSet<String> = local.iter().map(|s| s.title.to_lowercase()).collect();
    local.into_iter().chain(remote.into_iter().filter(|s| !taken.contains(&s.title.to_lowercase()))).collect()
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::model::VideoId;

    fn track(id: &str, artist: Option<&str>) -> Track {
        Track {
            video_id: VideoId(id.to_owned()),
            title: id.to_owned(),
            artists: artist.map(|a| vec![Person { name: a.to_owned(), id: Some(format!("UC{a}")) }]).unwrap_or_default(),
            ..Track::default()
        }
    }

    fn section(title: &str) -> HomeSection {
        HomeSection { title: title.to_owned(), items: vec![MediaItem::default()], strapline_thumb: None, strapline: None }
    }

    #[test]
    fn radios_take_turns_and_repeat_nothing() {
        let a = vec![track("a1", None), track("shared", None), track("a3", None)];
        let b = vec![track("b1", None), track("shared", None)];
        let key = |t: &Track| t.video_id.0.clone();
        let ids: Vec<String> = interleave(vec![a, b], 10, key).into_iter().map(|t| t.video_id.0).collect();
        assert_eq!(ids, ["a1", "b1", "shared", "a3"]);
        assert_eq!(interleave(vec![vec![track("x", None), track("y", None)]], 1, key).len(), 1);
    }

    #[test]
    fn likes_count_double_and_ties_go_to_the_earlier_artist() {
        let plays = [track("1", Some("Alpha")), track("2", Some("Beta")), track("3", Some("Beta")), track("4", None)];
        let liked = [track("5", Some("Gamma"))];
        let weighted = plays.iter().map(|t| (t, 1)).chain(liked.iter().map(|t| (t, LIKE_WEIGHT)));
        let names: Vec<String> = top_artists(weighted, 3).into_iter().map(|p| p.name).collect();
        assert_eq!(names, ["Beta", "Gamma", "Alpha"]);
    }

    #[test]
    fn followed_artists_join_the_most_heard_once() {
        let person = |id: &str| Person { name: id.to_owned(), id: Some(id.to_owned()) };
        let ids: Vec<String> = with_followed(vec![person("UCa"), person("UCb")], vec![person("UCb"), person("UCc")]).into_iter().filter_map(|p| p.id).collect();
        assert_eq!(ids, ["UCa", "UCb", "UCc"]);
    }

    #[test]
    fn only_real_video_ids_seed_a_radio() {
        assert!(is_video_id("dQw4w9WgXcQ"));
        assert!(!is_video_id("demo:1"));
        assert!(!is_video_id("dQw4w9WgXc"));
    }

    #[test]
    fn local_rows_replace_youtubes_rows_of_the_same_name() {
        let merged = merge(vec![section("Quick picks")], vec![section("quick picks"), section("Charts")]);
        let titles: Vec<&str> = merged.iter().map(|s| s.title.as_str()).collect();
        assert_eq!(titles, ["Quick picks", "Charts"]);
    }

    #[test]
    fn signals_come_from_plays_first_then_likes() {
        let dir = tempfile::tempdir().unwrap();
        let local = LocalLibrary::open(&crate::paths::Paths::for_tests(dir.path()));
        assert!(Signals::read(&local).is_empty());
        local.log_play(&track("aaaaaaaaaaa", Some("Alpha")));
        local.log_play(&track("aaaaaaaaaaa", Some("Alpha")));
        local.log_play(&track("bbbbbbbbbbb", Some("Beta")));
        local.log_play(&track("demo:1", Some("Beta")));
        local.set_liked(&track("ccccccccccc", Some("Gamma")), true);
        let signals = Signals::read(&local);
        let seeds: Vec<&str> = signals.seeds.iter().map(|t| t.video_id.as_str()).collect();
        assert_eq!(seeds, ["aaaaaaaaaaa", "bbbbbbbbbbb", "ccccccccccc"]);
        assert_eq!(signals.recent.first().map(|t| t.video_id.as_str()), Some("demo:1"));
        assert_eq!(signals.recent.len(), 3);
        local.set_subscribed(&MediaItem { kind: ItemKind::Artist, id: "UCZeta".to_owned(), title: "Zeta".to_owned(), ..MediaItem::default() }, true);
        let names: Vec<String> = Signals::read(&local).artists.into_iter().map(|a| a.name).collect();
        assert_eq!(names.last().map(String::as_str), Some("Zeta"), "a followed artist counts without a single play");
    }

    /// Builds the shelves on a signed-out client from two well-known songs.
    /// `cargo test -- --ignored shelves_build_signed_out --nocapture`
    #[tokio::test]
    #[ignore]
    async fn shelves_build_signed_out() {
        let dir = tempfile::tempdir().unwrap();
        let client = crate::net::ytmusic::YtMusic::new(&crate::paths::Paths::for_tests(dir.path())).unwrap();
        assert!(!client.is_authenticated());
        let seed = |id: &str, artist: &str, channel: &str| Track { video_id: VideoId(id.to_owned()), title: id.to_owned(), artists: vec![Person { name: artist.to_owned(), id: Some(channel.to_owned()) }], ..Track::default() };
        let seeds = vec![seed("dQw4w9WgXcQ", "Rick Astley", "UCuAXFkgsw1L7xaCfnd5JJOw"), seed("fJ9rUzIMcZQ", "Queen", "UCiMhD4jzUqG-IgPzUmmytRQ")];
        let signals = Signals { recent: [seeds.clone(), seeds.clone()].concat(), artists: seeds.iter().map(|t| t.artists[0].clone()).collect(), seeds };
        let started = std::time::Instant::now();
        let sections = build(client.api(), signals).await;
        println!("built in {:?}", started.elapsed());
        for section in &sections {
            println!("{:?} / {} : {} items, first {:?}", section.strapline, section.title, section.items.len(), section.items.first().map(|i| &i.title));
        }
        let picks = sections.iter().find(|s| s.title == QUICK_PICKS_TITLE).expect("quick picks from the radios");
        assert!(picks.items.len() >= 10 && picks.items.iter().all(|i| i.id != "dQw4w9WgXcQ" && i.id != "fJ9rUzIMcZQ"), "picks leave the seeds out");
        assert!(sections.iter().any(|s| s.strapline.as_deref() == Some("Similar to")), "a similar artists shelf");
    }
}

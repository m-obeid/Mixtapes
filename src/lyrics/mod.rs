//! Lyrics: six providers, the chain that ranks them, the per-track disk cache and the preferences. Port of the lyrics half of api/client.py plus player/lyrics_cache.py and player/lyrics_prefs.py. No widgets live here.
//!
//! `Lyrics` is the one handle the UI needs. It is cheap to clone and every call is an async fn meant for the tokio runtime:
//!
//! ```ignore
//! let lyrics = lyrics.clone();
//! let handle = net.spawn(async move { lyrics.get_lyrics(&query).await });
//! ```
//!
//! The cache file and the prefs keys are the Python app's, so both apps show the same lyrics, pins and settings.

pub mod cache;
pub mod chain;
pub mod lrc;
pub mod matching;
pub mod model;
pub mod prefs;
pub mod providers;
pub mod romanize;
pub mod script;
pub mod ttml;

use std::sync::Arc;

use tokio::task::JoinSet;

use crate::net::ytmusic::YtMusic;
use crate::paths::Paths;
use cache::LyricsCache;
use chain::Chain;
pub use chain::TrackQuery;
// The result types the view draws from. Unused until it is ported.
#[allow(unused_imports)]
pub use model::{Align, Alternative, LyricLine, LyricPart, LyricsMatch, LyricsResult};
use prefs::LyricsPrefs;
use providers::{Cooldowns, Live, Provider, Source};

/// Matches shown per provider in the match browser.
#[allow(dead_code)]
pub const DEFAULT_MATCH_LIMIT: usize = 8;
/// Matches taken from each provider by the manual search.
#[allow(dead_code)]
pub const DEFAULT_MANUAL_SEARCH_LIMIT: usize = 4;

struct Inner {
    cache: LyricsCache,
    prefs: Arc<LyricsPrefs>,
    chain: Chain,
}

/// The lyrics service. `Send + Sync`, shared by cloning.
#[derive(Clone)]
pub struct Lyrics {
    inner: Arc<Inner>,
}

#[allow(dead_code)]
impl Lyrics {
    /// `http` carries the plain provider requests, `ytm` the YouTube Music ones.
    pub fn new(paths: &Paths, http: reqwest::Client, ytm: Arc<YtMusic>) -> Self {
        let cooldowns = Arc::new(Cooldowns::default());
        let source = Arc::new(Live::new(http, ytm, cooldowns.clone()));
        Self::with_source(paths, source, cooldowns)
    }

    /// The service over any source, which is how the tests run it offline.
    fn with_source(paths: &Paths, source: Arc<dyn Source>, cooldowns: Arc<Cooldowns>) -> Self {
        let prefs = Arc::new(LyricsPrefs::new(paths));
        let chain = Chain::new(source, prefs.clone(), cooldowns);
        Self { inner: Arc::new(Inner { cache: LyricsCache::new(paths), prefs, chain }) }
    }

    /// The preferences, for the settings page and the view.
    pub fn prefs(&self) -> &LyricsPrefs {
        &self.inner.prefs
    }

    /// The disk cache. `cache().clear_all()` is what Settings calls after the queue changes.
    pub fn cache(&self) -> &LyricsCache {
        &self.inner.cache
    }

    /// Every provider's display name, in catalog order.
    pub fn provider_names() -> [&'static str; 6] {
        prefs::DEFAULT_PROVIDER_ORDER
    }

    /// The lyrics for a track, or None when nothing was found.
    ///
    /// Reads the disk cache first, then runs the provider chain on a miss and caches what it finds. A provider pinned through `set_preferred_source` wins over the ranking.
    pub async fn get_lyrics(&self, track: &TrackQuery) -> Option<LyricsResult> {
        if track.video_id.is_empty() {
            return None;
        }
        let order = self.inner.prefs.provider_order();
        if let Some(cached) = self.inner.cache.get_result(&track.video_id, Some(&order), self.inner.chain.accept_rank()) {
            return Some(cached);
        }
        let result = self.inner.chain.run(track).await?;
        self.inner.cache.add_result(&track.video_id, &result, false);
        Some(result)
    }

    /// Every provider already cached for this video, richest first. Use `fetch_alternatives` to fill in the ones that are not.
    pub async fn alternatives(&self, video_id: &str) -> Vec<(String, LyricsResult)> {
        self.inner.cache.get_alternatives(video_id)
    }

    /// The provider the listener pinned for this track.
    pub async fn preferred_source(&self, video_id: &str) -> Option<String> {
        self.inner.cache.get_preferred(video_id)
    }

    /// Pin `source`, a provider display name, as what `get_lyrics` answers for this track. None clears the pin.
    pub async fn set_preferred_source(&self, video_id: &str, source: Option<&str>) {
        self.inner.cache.set_preferred(video_id, source);
    }

    /// Forget a hand-picked source for this track, and the match picked with it.
    pub async fn clear_preference(&self, video_id: &str) {
        self.inner.cache.clear_user_choice(video_id);
    }

    /// Ask every provider at once and send each answer as it arrives.
    ///
    /// What is cached already is sent first. A provider in a back-off window is left out entirely, and one that had nothing sends None. The future resolves once every provider has answered, and dropping the receiver stops the sends but not the caching.
    pub async fn fetch_alternatives(&self, track: &TrackQuery, results: async_channel::Sender<Alternative>) {
        if track.video_id.is_empty() {
            return;
        }
        let cached = self.inner.cache.get_alternatives(&track.video_id);
        let cached_sources: Vec<String> = cached.iter().map(|(source, _)| source.clone()).collect();
        for (source, result) in cached {
            let _ = results.send(Alternative { source, result: Some(result) }).await;
        }

        let mut running = JoinSet::new();
        for provider in Provider::ALL {
            if cached_sources.iter().any(|s| s == provider.name()) || !self.inner.chain.provider_ready(provider) {
                continue;
            }
            let inner = self.inner.clone();
            let track = track.clone();
            let results = results.clone();
            running.spawn(async move {
                let mut found = inner.chain.fetch_one(provider, &track).await;
                if let Some(result) = found.take() {
                    // The label follows the display name, since some providers carry a more specific one.
                    let renamed = LyricsResult { source: provider.name().to_owned(), ..result };
                    let augmented = inner.chain.augment(renamed, &track).await;
                    inner.cache.add_result(&track.video_id, &augmented, false);
                    found = Some(augmented);
                }
                let _ = results.send(Alternative { source: provider.name().to_owned(), result: found }).await;
            });
        }
        while let Some(joined) = running.join_next().await {
            if let Err(err) = joined {
                tracing::warn!(%err, "a lyrics provider task failed");
            }
        }
    }

    /// Whether `fetch_provider_matches` has anything to list for this provider.
    pub fn provider_supports_matches(source_name: &str) -> bool {
        Provider::from_name(source_name).is_some_and(Provider::supports_matches)
    }

    /// Every usable match one provider has for this track, best guess first.
    ///
    /// Looser than the chain's gate on purpose: the point is to show what is there, near-misses included, since a listener can tell a live take from a studio one at a glance. `DEFAULT_MATCH_LIMIT` is the usual limit.
    pub async fn fetch_provider_matches(&self, source_name: &str, track: &TrackQuery, limit: usize) -> Vec<LyricsMatch> {
        let Some(provider) = Provider::from_name(source_name) else { return Vec::new() };
        self.inner.chain.provider_matches(provider, &track.title, &track.artist, track.duration, limit).await
    }

    /// Search every browsable provider for a query the listener typed. `DEFAULT_MANUAL_SEARCH_LIMIT` is the usual `per_provider`.
    pub async fn search_manually(&self, query: &str, artist: Option<&str>, duration: Option<u32>, per_provider: usize) -> Vec<LyricsMatch> {
        self.inner.chain.search_manually(query, artist.unwrap_or_default(), duration.unwrap_or(0), per_provider).await
    }

    /// Give a result the second line the automatic pick would have had.
    pub async fn augment_result(&self, result: LyricsResult, track: &TrackQuery) -> LyricsResult {
        self.inner.chain.augment(result, track).await
    }

    /// Take a match the listener picked: give it its second line, store it as that provider's result, and pin the provider so the choice survives the next play. Answers the result to show.
    pub async fn choose_match(&self, track: &TrackQuery, source_name: &str, result: LyricsResult) -> LyricsResult {
        let mut chosen = self.inner.chain.augment(result, track).await;
        chosen.source = source_name.to_owned();
        self.inner.cache.add_result(&track.video_id, &chosen, true);
        self.inner.cache.set_preferred(&track.video_id, Some(source_name));
        chosen.user_choice = true;
        chosen
    }
}

#[cfg(test)]
mod tests {
    use super::chain::tests::{Scripted, plain, synced, word};
    use super::*;

    fn service() -> (tempfile::TempDir, Arc<Scripted>, Lyrics) {
        let dir = tempfile::tempdir().unwrap();
        let source = Arc::new(Scripted::default());
        let lyrics = Lyrics::with_source(&Paths::for_tests(dir.path()), source.clone(), Arc::new(Cooldowns::default()));
        (dir, source, lyrics)
    }

    fn track() -> TrackQuery {
        TrackQuery::new("vid", Some("Song"), Some("Artist"), Some(200))
    }

    fn assert_send_sync<T: Send + Sync + Clone + 'static>() {}

    #[test]
    fn the_service_crosses_threads() {
        assert_send_sync::<Lyrics>();
    }

    #[tokio::test]
    async fn the_futures_can_be_spawned_on_the_runtime() {
        let (_dir, source, lyrics) = service();
        source.answer(Provider::Lrclib, "Song", synced("LRCLIB", "la"));
        let spawned = tokio::spawn({
            let lyrics = lyrics.clone();
            async move { lyrics.get_lyrics(&track()).await }
        });
        assert_eq!(spawned.await.unwrap().unwrap().source, "LRCLIB");
    }

    #[tokio::test]
    async fn a_hit_is_cached_and_the_second_read_asks_nobody() {
        let (_dir, source, lyrics) = service();
        source.answer(Provider::AppleMusic, "Song", word("Apple Music", "la"));
        assert_eq!(lyrics.get_lyrics(&track()).await.unwrap().source, "Apple Music");
        let asked = source.calls.lock().unwrap().len();
        assert_eq!(lyrics.get_lyrics(&track()).await.unwrap().source, "Apple Music");
        assert_eq!(source.calls.lock().unwrap().len(), asked);
        assert!(lyrics.get_lyrics(&TrackQuery::default()).await.is_none());
    }

    #[tokio::test]
    async fn a_miss_is_not_cached() {
        let (_dir, source, lyrics) = service();
        assert!(lyrics.get_lyrics(&track()).await.is_none());
        source.answer(Provider::NetEase, "Song", synced("NetEase", "la"));
        assert_eq!(lyrics.get_lyrics(&track()).await.unwrap().source, "NetEase");
    }

    #[tokio::test]
    async fn a_cached_provider_that_was_switched_off_sends_the_chain_out_again() {
        let (_dir, source, lyrics) = service();
        source.answer(Provider::NetEase, "Song", synced("NetEase", "la"));
        source.answer(Provider::Lrclib, "Song", synced("LRCLIB", "la"));
        assert_eq!(lyrics.get_lyrics(&track()).await.unwrap().source, "NetEase");
        lyrics.prefs().set_provider_enabled("NetEase", false);
        assert_eq!(lyrics.get_lyrics(&track()).await.unwrap().source, "LRCLIB");
    }

    #[tokio::test]
    async fn a_pin_decides_what_is_shown_until_it_is_cleared() {
        let (_dir, source, lyrics) = service();
        source.answer(Provider::AppleMusic, "Song", word("Apple Music", "la"));
        lyrics.get_lyrics(&track()).await.unwrap();
        lyrics.cache().add_result("vid", &synced("LRCLIB", "la"), false);

        lyrics.set_preferred_source("vid", Some("LRCLIB")).await;
        assert_eq!(lyrics.preferred_source("vid").await.as_deref(), Some("LRCLIB"));
        assert_eq!(lyrics.get_lyrics(&track()).await.unwrap().source, "LRCLIB");
        lyrics.clear_preference("vid").await;
        assert_eq!(lyrics.preferred_source("vid").await, None);
        assert_eq!(lyrics.get_lyrics(&track()).await.unwrap().source, "Apple Music");
        let names: Vec<String> = lyrics.alternatives("vid").await.into_iter().map(|(n, _)| n).collect();
        assert_eq!(names, ["Apple Music", "LRCLIB"]);
    }

    #[tokio::test]
    async fn alternatives_arrive_cached_first_then_one_per_provider() {
        let (_dir, source, lyrics) = service();
        lyrics.cache().add_result("vid", &synced("LRCLIB", "cached"), false);
        source.answer(Provider::Lrclib, "Song", synced("LRCLIB", "never asked"));
        source.answer(Provider::NetEase, "Song", synced("NetEase", "la"));
        source.answer(Provider::YouTubeMusic, "vid", plain("Musixmatch", "la"));

        let (sender, receiver) = async_channel::unbounded();
        lyrics.fetch_alternatives(&track(), sender).await;
        let mut got: Vec<Alternative> = Vec::new();
        while let Ok(item) = receiver.try_recv() {
            got.push(item);
        }
        assert_eq!(got.len(), 6);
        assert_eq!(got[0].source, "LRCLIB");
        assert_eq!(got[0].result.as_ref().unwrap().lines[0].text, "cached");
        let found = |name: &str| got.iter().find(|a| a.source == name).unwrap().result.clone();
        assert!(found("NetEase").is_some());
        assert!(found("Apple Music").is_none());
        // The label follows the display name, not YouTube's credit.
        assert_eq!(found("YouTube Music").unwrap().source, "YouTube Music");
        assert!(!source.called().contains(&Provider::Lrclib));

        // What was found is cached under the display name.
        let names: Vec<String> = lyrics.alternatives("vid").await.into_iter().map(|(n, _)| n).collect();
        assert_eq!(names, ["LRCLIB", "NetEase", "YouTube Music"]);
    }

    #[tokio::test]
    async fn a_chosen_match_is_stored_pinned_and_survives_a_refetch() {
        let (_dir, source, lyrics) = service();
        source.answer(Provider::AppleMusic, "Song", word("Apple Music", "automatic"));
        lyrics.get_lyrics(&track()).await.unwrap();

        let chosen = lyrics.choose_match(&track(), "NetEase", synced("NetEase", "사랑해")).await;
        assert_eq!(chosen.lines[0].romanization.as_deref(), Some("saranghae"), "a picked match gets its second line too");
        let shown = lyrics.get_lyrics(&track()).await.unwrap();
        assert_eq!(shown.source, "NetEase");
        assert!(shown.user_choice);

        lyrics.clear_preference("vid").await;
        assert_eq!(lyrics.get_lyrics(&track()).await.unwrap().source, "Apple Music");
        assert!(!lyrics.cache().has_source("vid", "NetEase"));
    }

    #[tokio::test]
    async fn match_browsing_is_for_three_providers() {
        let (_dir, _source, lyrics) = service();
        assert!(Lyrics::provider_supports_matches("Apple Music"));
        assert!(Lyrics::provider_supports_matches("NetEase"));
        assert!(Lyrics::provider_supports_matches("LRCLIB"));
        assert!(!Lyrics::provider_supports_matches("BetterLyrics"));
        assert!(!Lyrics::provider_supports_matches("Nope"));
        assert!(lyrics.fetch_provider_matches("Nope", &track(), DEFAULT_MATCH_LIMIT).await.is_empty());
        assert!(lyrics.search_manually("typed", None, None, DEFAULT_MANUAL_SEARCH_LIMIT).await.is_empty());
        assert_eq!(Lyrics::provider_names().len(), 6);
    }

    /// Live checks, one per provider. Run with `cargo test lyrics::tests::live -- --ignored --nocapture --test-threads=1`.
    mod live {
        use super::*;

        fn live() -> (tempfile::TempDir, Lyrics) {
            let dir = tempfile::tempdir().unwrap();
            // The real session when there is one, so YouTube Music answers as it does in the app.
            let ytm = YtMusic::new(&Paths::discover()).unwrap();
            let http = reqwest::Client::builder().gzip(true).build().unwrap();
            let lyrics = Lyrics::new(&Paths::for_tests(dir.path()), http, ytm);
            (dir, lyrics)
        }

        fn describe(name: &str, result: Option<&LyricsResult>) {
            match result {
                Some(r) => println!("{name}: {} lines, rank {}, synced {}, source {:?}, first {:?}", r.lines.len(), r.rank(), r.synced, r.source, r.lines.iter().find(|l| !l.text.is_empty()).map(|l| &l.text)),
                None => println!("{name}: nothing"),
            }
        }

        async fn one(provider: Provider, track: &TrackQuery) -> Option<LyricsResult> {
            let (_dir, lyrics) = live();
            let result = lyrics.inner.chain.fetch_one(provider, track).await;
            describe(provider.name(), result.as_ref());
            result
        }

        fn blinding_lights() -> TrackQuery {
            TrackQuery::new("4NRXx6U8ABQ", Some("Blinding Lights"), Some("The Weeknd"), Some(200))
        }

        #[tokio::test]
        #[ignore = "network"]
        async fn live_apple_music() {
            assert!(one(Provider::AppleMusic, &blinding_lights()).await.is_some());
        }

        #[tokio::test]
        #[ignore = "network"]
        async fn live_betterlyrics() {
            assert!(one(Provider::BetterLyrics, &blinding_lights()).await.is_some());
        }

        #[tokio::test]
        #[ignore = "network"]
        async fn live_binilyrics() {
            assert!(one(Provider::BiniLyrics, &blinding_lights()).await.is_some());
        }

        #[tokio::test]
        #[ignore = "network"]
        async fn live_netease() {
            assert!(one(Provider::NetEase, &blinding_lights()).await.is_some());
        }

        #[tokio::test]
        #[ignore = "network"]
        async fn live_lrclib() {
            assert!(one(Provider::Lrclib, &blinding_lights()).await.is_some());
        }

        #[tokio::test]
        #[ignore = "network"]
        async fn live_youtube_music() {
            // The music video has no lyrics tab, so the song itself is looked up first.
            let ytm = YtMusic::new(&Paths::discover()).unwrap();
            let found = crate::net::search::search(&ytm.api(), "Blinding Lights The Weeknd", Some(crate::net::search::SearchFilter::Songs)).await.unwrap();
            let song = found.items.first().expect("a song result");
            println!("song {} {:?}", song.id, song.title);
            let result = one(Provider::YouTubeMusic, &TrackQuery::new(&song.id, None, None, None)).await;
            assert!(result.is_some());
        }

        #[tokio::test]
        #[ignore = "network"]
        async fn live_chain_and_romanization() {
            let (_dir, lyrics) = live();
            let track = TrackQuery::new("live-test-senbonzakura", Some("千本桜"), Some("黒うさP"), Some(244));
            let result = lyrics.get_lyrics(&track).await;
            describe("chain", result.as_ref());
            let result = result.expect("the chain finds 千本桜");
            let romanized = result.lines.iter().filter(|l| l.romanization.is_some()).count();
            println!("romanized {romanized}/{} lines, e.g. {:?}", result.lines.len(), result.lines.iter().find_map(|l| l.romanization.clone()));
            assert!(romanized > 0);
        }

        #[tokio::test]
        #[ignore = "network"]
        async fn live_match_browser() {
            let (_dir, lyrics) = live();
            for name in ["Apple Music", "NetEase", "LRCLIB"] {
                let found = lyrics.fetch_provider_matches(name, &blinding_lights(), 3).await;
                println!("{name}: {:?}", found.iter().map(|m| format!("{} [{}] rank {}", m.label, m.detail, m.result.rank())).collect::<Vec<_>>());
                assert!(!found.is_empty(), "{name}");
            }
        }
    }
}

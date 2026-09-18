//! The provider chain: walk the listener's queue, settle on a result, and give it a second line. Port of `_run_lyrics_chain`, `augment_result` and the match browser from api/client.py.
//!
//! The queue comes from `LyricsPrefs`, with disabled providers already out of it. Each provider declares the richest shape it can produce, so one that cannot beat the result in hand is skipped without a network call.

use std::sync::{Arc, Mutex};

use super::matching::title_variants;
use super::model::{LyricLine, LyricsMatch, LyricsResult, RANK_LINE, RANK_PLAIN, rank_of};
use super::prefs::{LyricsPrefs, MATCH_STRICT};
use super::providers::{Cooldowns, Provider, Request, Source};
use super::romanize::{fill_romanization_gaps, merge_romanization_by_text, romanize_locally};
use super::script::{is_non_latin_char, needs_reading};

/// How many opening lines decide which script the lyrics are in.
const SCRIPT_SAMPLE_LINES: usize = 12;

/// The track lyrics are wanted for. An empty string or a zero means unknown.
#[derive(Clone, Debug, Default, PartialEq, Eq, Hash)]
pub struct TrackQuery {
    pub video_id: String,
    pub title: String,
    pub artist: String,
    /// Seconds.
    pub duration: u32,
}

impl TrackQuery {
    #[allow(dead_code)]
    pub fn new(video_id: &str, title: Option<&str>, artist: Option<&str>, duration: Option<u32>) -> Self {
        Self { video_id: video_id.to_owned(), title: title.unwrap_or_default().to_owned(), artist: artist.unwrap_or_default().to_owned(), duration: duration.unwrap_or(0) }
    }

    fn request<'a>(&'a self, title: &'a str, artist: &'a str, strict: bool) -> Request<'a> {
        Request { video_id: &self.video_id, title, artist, duration: self.duration, strict }
    }
}

impl From<&crate::model::Track> for TrackQuery {
    fn from(track: &crate::model::Track) -> Self {
        Self { video_id: track.video_id.0.clone(), title: track.title.clone(), artist: track.artist.clone(), duration: track.duration_seconds.unwrap_or(0) }
    }
}

/// The last romanization a provider handed over, so switching sources on one track does not search for the same reading again.
struct RomaMemo {
    provider: Provider,
    track: TrackQuery,
    lines: Vec<LyricLine>,
}

pub struct Chain {
    source: Arc<dyn Source>,
    prefs: Arc<LyricsPrefs>,
    cooldowns: Arc<Cooldowns>,
    roma_memo: Mutex<Option<RomaMemo>>,
}

impl Chain {
    pub fn new(source: Arc<dyn Source>, prefs: Arc<LyricsPrefs>, cooldowns: Arc<Cooldowns>) -> Self {
        Self { source, prefs, cooldowns, roma_memo: Mutex::new(None) }
    }

    /// The result quality that ends the search.
    ///
    /// `strict` takes the first provider that returns anything at all. `quality`, the default, treats a plain hit as a fallback and keeps walking the queue for something synced, so a provider high in the order still wins whenever it has timed lyrics.
    pub fn accept_rank(&self) -> u8 {
        if self.prefs.match_mode() == MATCH_STRICT { RANK_PLAIN } else { RANK_LINE }
    }

    pub fn provider_ready(&self, provider: Provider) -> bool {
        self.cooldowns.ready(provider)
    }

    /// One provider's answer for a track: the first title variant that has anything, or the video id for the provider that takes nothing else.
    pub async fn fetch_one(&self, provider: Provider, track: &TrackQuery) -> Option<LyricsResult> {
        if provider.takes_video_id_only() {
            return self.source.fetch(provider, track.request("", "", true)).await;
        }
        for variant in title_variants(&track.title) {
            let found = self.source.fetch(provider, track.request(&variant, &track.artist, true)).await;
            if found.as_ref().is_some_and(|r| !r.lines.is_empty()) {
                return found;
            }
        }
        None
    }

    /// Walk the queue and answer the result it settles on, second line included.
    ///
    /// YouTube Music titles for international tracks often arrive as `Original - Translation` or carry `(feat. X)` and `(Remastered)` suffixes the lyrics databases do not, so a few variants are tried.
    pub async fn run(&self, track: &TrackQuery) -> Option<LyricsResult> {
        let variants = title_variants(&track.title);
        let accept_rank = self.accept_rank();
        let mut best: Option<LyricsResult> = None;

        'queue: for provider in self.prefs.provider_order().iter().filter_map(|name| Provider::from_name(name)) {
            // This provider's ceiling is no better than what is already held.
            if rank_of(best.as_ref()) >= provider.native_rank() || !self.provider_ready(provider) {
                continue;
            }
            let video_only = [String::new()];
            let titles: &[String] = if provider.takes_video_id_only() { &video_only } else { &variants };
            for title in titles {
                let artist = if provider.takes_video_id_only() { "" } else { track.artist.as_str() };
                let found = self.source.fetch(provider, track.request(title, artist, true)).await;
                if rank_of(found.as_ref()) > rank_of(best.as_ref()) {
                    best = found;
                }
                if rank_of(best.as_ref()) >= accept_rank {
                    break 'queue;
                }
            }
        }
        Some(self.augment(best?, track).await)
    }

    /// Give a result its second line.
    ///
    /// Every path that puts lyrics on screen goes through this, not only the chain: switching provider in the picker, or choosing one of a provider's other matches, has to produce the second line the automatic pick would have had.
    pub async fn augment(&self, mut result: LyricsResult, track: &TrackQuery) -> LyricsResult {
        self.augment_romanization(&mut result, track).await;
        fill_romanization_gaps(&mut result);
        result
    }

    /// Fill in a romanization when the provider that won the lyrics has none.
    ///
    /// The provider with the best timing is often not the one with the reading: LRCLIB has the synced Senbonzakura and no romaji, NetEase has the romaji. Only runs when the second line is set to show a romanization, and only for non-Latin lyrics.
    async fn augment_romanization(&self, result: &mut LyricsResult, track: &TrackQuery) {
        if result.lines.is_empty() || !matches!(self.prefs.second_line_mode().as_str(), "auto" | "romanization") || result.any_romanization() {
            return;
        }
        let sample: String = result.lines.iter().take(SCRIPT_SAMPLE_LINES).map(|l| l.text.as_str()).collect::<Vec<_>>().join(" ");
        if !sample.chars().any(is_non_latin_char) {
            return;
        }

        // What can be transliterated here goes first: exact, instant, no network, and it covers every track rather than the ones a provider happened to romanize.
        let mut generated = 0;
        for line in &mut result.lines {
            if let Some(roman) = romanize_locally(&line.text).filter(|r| *r != line.text) {
                line.romanization = Some(roman);
                generated += 1;
            }
        }
        if generated > 0 {
            tracing::debug!(generated, of = result.lines.len(), "romanization generated locally");
            return;
        }

        // What is left needs a reading. Any other script has nothing a provider could answer, so no round trip is spent finding that out.
        if !sample.chars().any(needs_reading) {
            return;
        }
        self.augment_from_providers(result, track).await;
        fill_romanization_gaps(result);
    }

    async fn augment_from_providers(&self, result: &mut LyricsResult, track: &TrackQuery) {
        let enabled = self.prefs.provider_order();
        for provider in Provider::ROMANIZATION_SOURCES {
            if provider.name() == result.source || !enabled.iter().any(|n| n == provider.name()) || !self.provider_ready(provider) {
                continue;
            }
            if let Some(remembered) = self.remembered(provider, track) {
                if merge_romanization_by_text(&mut result.lines, &remembered) > 0 {
                    return;
                }
                continue;
            }

            // The artist is whatever YouTube Music credits, which is often not how the romanization source spells it: NetEase finds nothing for the kanji title plus "Hatsune Miku" and everything for the title alone. A second pass without the artist is safe here in a way it would not be for the lyrics themselves, because the merge only copies a reading onto a line whose text matches, so a wrong song contributes nothing.
            let variants = title_variants(&track.title);
            let attempts = variants.iter().map(|v| (v, track.artist.as_str())).chain(variants.iter().map(|v| (v, "")));
            for (variant, artist) in attempts {
                let Some(other) = self.source.fetch(provider, track.request(variant, artist, false)).await.filter(|o| !o.lines.is_empty()) else { continue };
                let merged = merge_romanization_by_text(&mut result.lines, &other.lines);
                *self.roma_memo.lock().unwrap() = Some(RomaMemo { provider, track: track.clone(), lines: other.lines });
                if merged > 0 {
                    tracing::debug!(merged, of = result.lines.len(), from = provider.name(), "romanization merged");
                    return;
                }
            }
        }
    }

    fn remembered(&self, provider: Provider, track: &TrackQuery) -> Option<Vec<LyricLine>> {
        let memo = self.roma_memo.lock().unwrap();
        memo.as_ref().filter(|m| m.provider == provider && m.track.title == track.title && m.track.artist == track.artist && m.track.duration == track.duration).map(|m| m.lines.clone())
    }

    /// Every usable match one provider has for this track, best guess first.
    pub async fn provider_matches(&self, provider: Provider, title: &str, artist: &str, duration: u32, limit: usize) -> Vec<LyricsMatch> {
        if !provider.supports_matches() {
            return Vec::new();
        }
        let request = Request { video_id: "", title, artist, duration, strict: true };
        let mut found = self.source.matches(provider, request, limit).await;
        for entry in &mut found {
            entry.result.source = provider.name().to_owned();
        }
        found
    }

    /// Search every browsable provider for a query the listener typed.
    ///
    /// Titles that carry their credits inline defeat automatic matching, and no amount of cleaning catches every shape. Typing the name is the reliable answer.
    pub async fn search_manually(&self, query: &str, artist: &str, duration: u32, per_provider: usize) -> Vec<LyricsMatch> {
        let enabled = self.prefs.provider_order();
        let mut out = Vec::new();
        for provider in Provider::MATCH_BROWSERS.into_iter().filter(|p| enabled.iter().any(|n| n == p.name())) {
            for mut entry in self.provider_matches(provider, query, artist, duration, per_provider).await {
                entry.source = Some(provider.name().to_owned());
                out.push(entry);
            }
        }
        out
    }
}

#[cfg(test)]
pub(crate) mod tests {
    use super::*;
    use crate::lyrics::model::LyricPart;
    use crate::lyrics::providers::{Fetched, Matched};
    use crate::paths::Paths;
    use std::collections::HashMap;
    use std::time::Duration;

    /// One recorded fetch: provider, title tried, artist sent, strict flag.
    pub type Call = (Provider, String, String, bool);

    /// Answers from a table keyed by provider and title, and records every call.
    #[derive(Default)]
    pub struct Scripted {
        pub answers: Mutex<HashMap<(Provider, String), LyricsResult>>,
        pub matches: Mutex<HashMap<Provider, Vec<LyricsMatch>>>,
        pub calls: Mutex<Vec<Call>>,
    }

    impl Scripted {
        pub fn answer(&self, provider: Provider, title: &str, result: LyricsResult) {
            self.answers.lock().unwrap().insert((provider, title.to_owned()), result);
        }

        pub fn called(&self) -> Vec<Provider> {
            self.calls.lock().unwrap().iter().map(|c| c.0).collect()
        }
    }

    impl Source for Scripted {
        fn fetch<'a>(&'a self, provider: Provider, request: Request<'a>) -> Fetched<'a> {
            self.calls.lock().unwrap().push((provider, request.title.to_owned(), request.artist.to_owned(), request.strict));
            let key = if provider.takes_video_id_only() { request.video_id } else { request.title };
            let answer = self.answers.lock().unwrap().get(&(provider, key.to_owned())).cloned();
            Box::pin(async move { answer })
        }

        fn matches<'a>(&'a self, provider: Provider, _request: Request<'a>, limit: usize) -> Matched<'a> {
            let found: Vec<LyricsMatch> = self.matches.lock().unwrap().get(&provider).cloned().unwrap_or_default().into_iter().take(limit).collect();
            Box::pin(async move { found })
        }
    }

    pub fn plain(source: &str, text: &str) -> LyricsResult {
        LyricsResult::from_lines(vec![LyricLine::new(None, text)], source)
    }

    pub fn synced(source: &str, text: &str) -> LyricsResult {
        LyricsResult::from_lines(vec![LyricLine::new(Some(1.0), text)], source)
    }

    pub fn word(source: &str, text: &str) -> LyricsResult {
        let mut result = synced(source, text);
        result.lines[0].parts.push(LyricPart { start: Some(1.0), end: Some(2.0), text: text.to_owned(), space_after: false });
        result
    }

    struct Rig {
        _dir: tempfile::TempDir,
        source: Arc<Scripted>,
        prefs: Arc<LyricsPrefs>,
        cooldowns: Arc<Cooldowns>,
        chain: Chain,
    }

    fn rig() -> Rig {
        let dir = tempfile::tempdir().unwrap();
        let source = Arc::new(Scripted::default());
        let prefs = Arc::new(LyricsPrefs::new(&Paths::for_tests(dir.path())));
        let cooldowns = Arc::new(Cooldowns::default());
        let chain = Chain::new(source.clone(), prefs.clone(), cooldowns.clone());
        Rig { _dir: dir, source, prefs, cooldowns, chain }
    }

    fn track(title: &str) -> TrackQuery {
        TrackQuery::new("vid", Some(title), Some("Artist"), Some(200))
    }

    #[tokio::test]
    async fn a_word_level_hit_ends_the_walk_at_once() {
        let rig = rig();
        rig.source.answer(Provider::AppleMusic, "Song", word("Apple Music", "la"));
        rig.source.answer(Provider::Lrclib, "Song", synced("LRCLIB", "la"));
        let result = rig.chain.run(&track("Song")).await.unwrap();
        assert_eq!(result.source, "Apple Music");
        assert_eq!(rig.source.called(), [Provider::AppleMusic]);
    }

    #[tokio::test]
    async fn quality_mode_walks_past_a_plain_hit_and_falls_back_to_it() {
        let rig = rig();
        rig.source.answer(Provider::AppleMusic, "Song", plain("Apple Music", "la"));
        rig.source.answer(Provider::Lrclib, "Song", synced("LRCLIB", "la"));
        assert_eq!(rig.chain.run(&track("Song")).await.unwrap().source, "LRCLIB");
        // YouTube Music can only be plain, which is already held, so it is never asked.
        assert!(!rig.source.called().contains(&Provider::YouTubeMusic));

        let lonely = self::rig();
        lonely.source.answer(Provider::BetterLyrics, "Song", plain("BetterLyrics", "la"));
        assert_eq!(lonely.chain.run(&track("Song")).await.unwrap().source, "BetterLyrics");
        assert_eq!(lonely.source.called(), [Provider::AppleMusic, Provider::BetterLyrics, Provider::BiniLyrics, Provider::NetEase, Provider::Lrclib]);
    }

    #[tokio::test]
    async fn strict_mode_takes_the_first_provider_with_anything() {
        let rig = rig();
        rig.prefs.set_match_mode(MATCH_STRICT);
        rig.source.answer(Provider::AppleMusic, "Song", plain("Apple Music", "la"));
        rig.source.answer(Provider::Lrclib, "Song", synced("LRCLIB", "la"));
        assert_eq!(rig.chain.run(&track("Song")).await.unwrap().source, "Apple Music");
        assert_eq!(rig.source.called(), [Provider::AppleMusic]);
    }

    #[tokio::test]
    async fn the_queue_order_and_the_switches_come_from_prefs() {
        let rig = rig();
        rig.prefs.set_provider_order(&["LRCLIB", "Apple Music"]);
        rig.prefs.set_provider_enabled("NetEase", false);
        rig.source.answer(Provider::NetEase, "Song", synced("NetEase", "la"));
        rig.source.answer(Provider::Lrclib, "Song", synced("LRCLIB", "la"));
        rig.source.answer(Provider::AppleMusic, "Song", word("Apple Music", "la"));
        assert_eq!(rig.chain.run(&track("Song")).await.unwrap().source, "LRCLIB");
        assert_eq!(rig.source.called(), [Provider::Lrclib]);
    }

    #[tokio::test]
    async fn title_variants_are_tried_in_order() {
        let rig = rig();
        rig.prefs.set_provider_order(&["LRCLIB"]);
        rig.source.answer(Provider::Lrclib, "Medicine", synced("LRCLIB", "la"));
        assert!(rig.chain.run(&track("イガク - Medicine")).await.is_some());
        let titles: Vec<String> = rig.source.calls.lock().unwrap().iter().map(|c| c.1.clone()).collect();
        assert_eq!(titles, ["イガク - Medicine", "Medicine"]);
    }

    #[tokio::test]
    async fn youtube_music_is_asked_by_video_id_even_without_a_title() {
        let rig = rig();
        rig.source.answer(Provider::YouTubeMusic, "vid", plain("Musixmatch", "la"));
        let result = rig.chain.run(&TrackQuery::new("vid", None, None, None)).await.unwrap();
        assert_eq!(result.source, "Musixmatch");
        assert_eq!(rig.source.called(), [Provider::YouTubeMusic], "no title means no variants for the others");
    }

    #[tokio::test]
    async fn a_provider_backing_off_is_skipped() {
        let rig = rig();
        rig.cooldowns.trip(Provider::AppleMusic, Duration::from_secs(60), "test");
        rig.source.answer(Provider::AppleMusic, "Song", word("Apple Music", "la"));
        rig.source.answer(Provider::NetEase, "Song", synced("NetEase", "la"));
        assert_eq!(rig.chain.run(&track("Song")).await.unwrap().source, "NetEase");
        assert!(!rig.source.called().contains(&Provider::AppleMusic));
        assert!(!rig.chain.provider_ready(Provider::AppleMusic));
    }

    #[tokio::test]
    async fn nothing_anywhere_is_none() {
        let rig = rig();
        assert!(rig.chain.run(&track("Song")).await.is_none());
        assert_eq!(rig.source.called().len(), 6);
    }

    #[tokio::test]
    async fn korean_and_cyrillic_are_romanized_without_a_request() {
        let rig = rig();
        rig.prefs.set_provider_order(&["LRCLIB"]);
        rig.source.answer(Provider::Lrclib, "Song", synced("LRCLIB", "사랑해"));
        let result = rig.chain.run(&track("Song")).await.unwrap();
        assert_eq!(result.lines[0].romanization.as_deref(), Some("saranghae"));
        assert_eq!(rig.source.called(), [Provider::Lrclib]);
    }

    #[tokio::test]
    async fn a_reading_is_merged_from_a_second_provider_with_a_wider_search() {
        let rig = rig();
        rig.prefs.set_provider_order(&["LRCLIB", "NetEase", "Apple Music"]);
        rig.source.answer(Provider::Lrclib, "千本桜", synced("LRCLIB", "千本桜　夜ニ紛レ"));
        let mut netease = synced("NetEase", "千本桜 夜ニ紛レ");
        netease.lines[0].romanization = Some("senbonzakura yoru ni magire".into());
        rig.source.answer(Provider::NetEase, "千本桜", netease);

        let result = rig.chain.run(&track("千本桜")).await.unwrap();
        assert_eq!(result.source, "LRCLIB");
        assert_eq!(result.lines[0].romanization.as_deref(), Some("senbonzakura yoru ni magire"));
        let romanization_calls: Vec<Call> = rig.source.calls.lock().unwrap().iter().filter(|c| c.0 == Provider::NetEase).cloned().collect();
        assert_eq!(romanization_calls, [(Provider::NetEase, "千本桜".to_owned(), "Artist".to_owned(), false)], "the first attempt already merged");

        // The same track again answers from the memo, with no new request.
        let before = rig.source.calls.lock().unwrap().len();
        let again = rig.chain.augment(synced("LRCLIB", "千本桜　夜ニ紛レ"), &track("千本桜")).await;
        assert_eq!(again.lines[0].romanization.as_deref(), Some("senbonzakura yoru ni magire"));
        assert_eq!(rig.source.calls.lock().unwrap().len(), before);
    }

    #[tokio::test]
    async fn the_dictionary_fills_in_when_no_provider_has_the_reading() {
        let rig = rig();
        rig.prefs.set_provider_order(&["LRCLIB"]);
        rig.source.answer(Provider::Lrclib, "Song", synced("LRCLIB", "你好"));
        let result = rig.chain.run(&track("Song")).await.unwrap();
        assert_eq!(result.lines[0].romanization.as_deref(), Some("nǐ hǎo"));
    }

    #[tokio::test]
    async fn no_second_provider_is_asked_when_the_second_line_shows_something_else() {
        let rig = rig();
        rig.prefs.set_second_line_mode("translation");
        rig.prefs.set_provider_order(&["LRCLIB", "NetEase"]);
        rig.source.answer(Provider::Lrclib, "Song", synced("LRCLIB", "千本桜"));
        rig.chain.run(&track("Song")).await.unwrap();
        assert!(!rig.source.calls.lock().unwrap().iter().any(|c| !c.3), "no romanization pass ran");
    }

    #[tokio::test]
    async fn latin_lyrics_get_no_second_line() {
        let rig = rig();
        rig.source.answer(Provider::AppleMusic, "Song", word("Apple Music", "hello"));
        let result = rig.chain.run(&track("Song")).await.unwrap();
        assert_eq!(result.lines[0].romanization, None);
    }

    #[tokio::test]
    async fn fetch_one_stops_at_the_first_variant_with_lines() {
        let rig = rig();
        rig.source.answer(Provider::NetEase, "Song", synced("NetEase", "la"));
        assert!(rig.chain.fetch_one(Provider::NetEase, &track("Song (Remastered)")).await.is_some());
        assert_eq!(rig.source.calls.lock().unwrap().len(), 2);
        assert!(rig.chain.fetch_one(Provider::Lrclib, &track("Song")).await.is_none());
    }

    #[tokio::test]
    async fn the_manual_search_walks_the_enabled_browsers_and_labels_the_source() {
        let rig = rig();
        rig.prefs.set_provider_enabled("NetEase", false);
        let entry = |label: &str| LyricsMatch { label: label.to_owned(), detail: String::new(), result: synced("whatever", "la"), source: None };
        rig.source.matches.lock().unwrap().insert(Provider::AppleMusic, vec![entry("a1"), entry("a2"), entry("a3")]);
        rig.source.matches.lock().unwrap().insert(Provider::NetEase, vec![entry("n1")]);
        rig.source.matches.lock().unwrap().insert(Provider::Lrclib, vec![entry("l1")]);

        let found = rig.chain.search_manually("typed", "", 0, 2).await;
        let labels: Vec<(&str, Option<&str>)> = found.iter().map(|m| (m.label.as_str(), m.source.as_deref())).collect();
        assert_eq!(labels, [("a1", Some("Apple Music")), ("a2", Some("Apple Music")), ("l1", Some("LRCLIB"))]);
        assert!(found.iter().all(|m| m.result.source == m.source.clone().unwrap()));

        assert!(rig.chain.provider_matches(Provider::BiniLyrics, "typed", "", 0, 8).await.is_empty());
        let browsed = rig.chain.provider_matches(Provider::NetEase, "typed", "", 0, 8).await;
        assert_eq!(browsed[0].source, None);
        assert_eq!(browsed[0].result.source, "NetEase");
    }
}

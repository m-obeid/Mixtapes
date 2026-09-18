//! The rows behind a playlist page: order, filter, selection and the sort
//! metrics that only some orders need.
//!
//! `fetched` is every track the page has, in the order it arrived. `rendered`
//! is what the list shows: sorted, and cut to the chunk rendered so far. The
//! page asks this module which rows to show and which are ticked; it never
//! re-derives either.

use std::collections::{HashMap, HashSet};

use crate::model::Track;
use crate::net::cache::SortMetric;
use crate::net::playlists;

pub const SORT_DEFAULT: u32 = 0;
pub const SORT_TITLE: u32 = 1;
pub const SORT_ARTIST: u32 = 2;
pub const SORT_ALBUM: u32 = 3;
pub const SORT_DURATION: u32 = 4;
pub const SORT_VIEWS: u32 = 5;
pub const SORT_ADDED: u32 = 6;

/// The orders that need a number YouTube keeps off the track itself.
pub fn needs_metric(sort_type: u32) -> bool {
    matches!(sort_type, SORT_VIEWS | SORT_ADDED)
}

#[derive(Default)]
pub struct TrackList {
    fetched: Vec<Track>,
    rendered: Vec<Track>,
    filter: String,
    sort_type: u32,
    descending: bool,
    selected: HashSet<String>,
    metrics: HashMap<u32, SortMetric>,
}

impl TrackList {
    /// The list to work from.
    ///
    /// A page renders before the full fetch lands, so early on the only rows
    /// are the ones already rendered. One answer, in one place.
    pub fn source(&self) -> &[Track] {
        if self.fetched.is_empty() { &self.rendered } else { &self.fetched }
    }

    /// Everything fetched so far, in fetch order.
    pub fn fetched(&self) -> &[Track] {
        &self.fetched
    }

    /// The rows the list is showing.
    pub fn rendered(&self) -> &[Track] {
        &self.rendered
    }

    /// Rows the list shows right now: the matches while a search is active,
    /// otherwise what is rendered.
    pub fn visible(&self) -> Vec<Track> {
        if self.filter.is_empty() { self.rendered.clone() } else { self.matches() }
    }

    pub fn is_empty(&self) -> bool {
        self.source().is_empty()
    }

    /// Everything the page has, both views, in fetch order.
    pub fn set(&mut self, tracks: Vec<Track>) {
        self.rendered = tracks.clone();
        self.fetched = tracks;
    }

    /// Rows that arrived after the first page.
    pub fn extend(&mut self, tracks: Vec<Track>) {
        self.rendered.extend(tracks.iter().cloned());
        self.fetched.extend(tracks);
    }

    /// The full list, when the page rendered a prefix first.
    pub fn set_fetched(&mut self, tracks: Vec<Track>) {
        self.fetched = tracks;
    }

    /// What the list shows, after a sort or another chunk.
    pub fn set_rendered(&mut self, tracks: Vec<Track>) {
        self.rendered = tracks;
    }

    /// Render the next `size` rows of the fetched list. Returns what to append.
    pub fn render_chunk(&mut self, size: usize) -> Vec<Track> {
        let start = self.rendered.len();
        let end = (start + size).min(self.fetched.len());
        if start >= end {
            return Vec::new();
        }
        let chunk = self.fetched[start..end].to_vec();
        self.rendered.extend(chunk.iter().cloned());
        chunk
    }

    /// Whether every fetched row is on screen.
    pub fn fully_rendered(&self) -> bool {
        !self.rendered.is_empty() && self.rendered.len() >= self.fetched.len()
    }

    pub fn clear(&mut self) {
        self.fetched.clear();
        self.rendered.clear();
        self.selected.clear();
    }

    /// Drop rows, both views at once. Returns whether anything went.
    pub fn remove(&mut self, drop: impl Fn(&Track) -> bool) -> bool {
        let before = self.fetched.len() + self.rendered.len();
        self.fetched.retain(|t| !drop(t));
        self.rendered.retain(|t| !drop(t));
        before != self.fetched.len() + self.rendered.len()
    }

    // -- search ----------------------------------------------------------

    pub fn filter(&self) -> &str {
        &self.filter
    }

    pub fn set_filter(&mut self, text: String) {
        self.filter = text;
    }

    pub fn filtering(&self) -> bool {
        !self.filter.is_empty()
    }

    /// Rows matching the search, in the current order. Title, artist and album
    /// all count, the way the Python page searched.
    pub fn matches(&self) -> Vec<Track> {
        let text = &self.filter;
        let hits: Vec<Track> = self
            .source()
            .iter()
            .filter(|t| {
                let (title, artist, album) = playlists::track_search_text(t);
                title.contains(text) || artist.contains(text) || album.contains(text)
            })
            .cloned()
            .collect();
        self.sorted(hits)
    }

    // -- order -----------------------------------------------------------

    pub fn sort_type(&self) -> u32 {
        self.sort_type
    }

    pub fn descending(&self) -> bool {
        self.descending
    }

    pub fn set_order(&mut self, sort_type: u32, descending: bool) {
        self.sort_type = sort_type;
        self.descending = descending;
    }

    /// Put tracks in the current order.
    ///
    /// Views and added-date read a metric fetched separately; rows the metric
    /// does not know about keep their order and sit at the end.
    pub fn sorted(&self, tracks: Vec<Track>) -> Vec<Track> {
        let reverse = self.descending;
        let mut result = tracks;
        let lower = |s: &str| s.to_lowercase();
        match self.sort_type {
            SORT_DEFAULT => {
                if reverse {
                    result.reverse();
                }
                return result;
            }
            SORT_TITLE => result.sort_by_cached_key(|t| lower(&t.title)),
            SORT_ARTIST => result.sort_by_cached_key(|t| (t.artists.first().map(|a| lower(&a.name)).unwrap_or_default(), lower(&t.title))),
            SORT_ALBUM => result.sort_by_cached_key(|t| (t.album.as_ref().map(|a| lower(&a.name)).unwrap_or_default(), lower(&t.title))),
            SORT_DURATION => result.sort_by_key(|t| t.duration_seconds.unwrap_or(0)),
            SORT_VIEWS | SORT_ADDED => {
                let metric = self.metric(self.sort_type);
                let (mut ranked, unranked): (Vec<Track>, Vec<Track>) = result.into_iter().partition(|t| metric.contains_key(t.video_id.as_str()));
                ranked.sort_by_key(|t| metric.get(t.video_id.as_str()).copied().unwrap_or(0));
                if !reverse {
                    ranked.reverse();
                }
                ranked.extend(unranked);
                return ranked;
            }
            _ => {}
        }
        if reverse {
            result.reverse();
        }
        result
    }

    /// The source list in the current order, what a sort change renders.
    pub fn sorted_source(&self) -> Vec<Track> {
        self.sorted(self.source().to_vec())
    }

    pub fn metric(&self, sort_type: u32) -> SortMetric {
        self.metrics.get(&sort_type).cloned().unwrap_or_default()
    }

    pub fn has_metric(&self, sort_type: u32) -> bool {
        self.metrics.contains_key(&sort_type)
    }

    pub fn set_metric(&mut self, sort_type: u32, metric: SortMetric) {
        self.metrics.insert(sort_type, metric);
    }

    /// Forget the metrics, for a playlist whose rows changed.
    pub fn drop_metrics(&mut self) {
        self.metrics.clear();
    }

    // -- selection -------------------------------------------------------

    pub fn is_selected(&self, video_id: &str) -> bool {
        self.selected.contains(video_id)
    }

    pub fn selected_count(&self) -> usize {
        self.selected.len()
    }

    pub fn has_selection(&self) -> bool {
        !self.selected.is_empty()
    }

    pub fn select(&mut self, video_id: &str, on: bool) {
        if video_id.is_empty() {
            return;
        }
        if on {
            self.selected.insert(video_id.to_owned());
        } else {
            self.selected.remove(video_id);
        }
    }

    /// Tick every row on screen. A search narrows what that means.
    pub fn select_visible(&mut self) {
        for track in self.visible() {
            self.select(&track.video_id.0.clone(), true);
        }
    }

    pub fn clear_selection(&mut self) {
        self.selected.clear();
    }

    /// Ticked rows, in the order the list shows them.
    pub fn selected_tracks(&self) -> Vec<Track> {
        let picked: Vec<Track> = self.source().iter().filter(|t| self.selected.contains(&t.video_id.0)).cloned().collect();
        self.sorted(picked)
    }

    pub fn selected_ids(&self) -> Vec<String> {
        self.selected_tracks().into_iter().map(|t| t.video_id.0).collect()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::model::{Named, Person, VideoId};

    fn track(id: &str, title: &str, artist: &str, seconds: u32) -> Track {
        Track {
            video_id: VideoId(id.to_owned()),
            title: title.to_owned(),
            artist: artist.to_owned(),
            artists: vec![Person { name: artist.to_owned(), id: None }],
            album: Some(Named { name: "Album".to_owned(), id: None }),
            duration_seconds: Some(seconds),
            ..Track::default()
        }
    }

    fn list() -> TrackList {
        let mut list = TrackList::default();
        list.set(vec![track("a", "Zebra", "Beta", 100), track("b", "Apple", "Alpha", 300), track("c", "Mango", "Gamma", 200)]);
        list
    }

    fn titles(tracks: &[Track]) -> Vec<&str> {
        tracks.iter().map(|t| t.title.as_str()).collect()
    }

    #[test]
    fn the_source_is_what_was_rendered_until_the_fetch_lands() {
        let mut list = TrackList::default();
        list.set_rendered(vec![track("a", "Zebra", "Beta", 100)]);
        assert_eq!(titles(list.source()), ["Zebra"]);
        list.set_fetched(vec![track("b", "Apple", "Alpha", 300)]);
        assert_eq!(titles(list.source()), ["Apple"]);
    }

    #[test]
    fn sorting_covers_every_order_and_reverses() {
        let mut list = list();
        assert_eq!(titles(&list.sorted_source()), ["Zebra", "Apple", "Mango"]);
        list.set_order(SORT_TITLE, false);
        assert_eq!(titles(&list.sorted_source()), ["Apple", "Mango", "Zebra"]);
        list.set_order(SORT_TITLE, true);
        assert_eq!(titles(&list.sorted_source()), ["Zebra", "Mango", "Apple"]);
        list.set_order(SORT_ARTIST, false);
        assert_eq!(titles(&list.sorted_source()), ["Apple", "Zebra", "Mango"]);
        list.set_order(SORT_DURATION, false);
        assert_eq!(titles(&list.sorted_source()), ["Zebra", "Mango", "Apple"]);
        list.set_order(SORT_DEFAULT, true);
        assert_eq!(titles(&list.sorted_source()), ["Mango", "Apple", "Zebra"]);
    }

    #[test]
    fn rows_the_metric_does_not_know_keep_their_order_at_the_end() {
        let mut list = list();
        let mut metric = SortMetric::new();
        metric.insert("a".to_owned(), 5);
        metric.insert("c".to_owned(), 9);
        list.set_metric(SORT_VIEWS, metric);
        list.set_order(SORT_VIEWS, false);
        assert!(list.has_metric(SORT_VIEWS));
        assert_eq!(titles(&list.sorted_source()), ["Mango", "Zebra", "Apple"]);
        list.drop_metrics();
        assert!(!list.has_metric(SORT_VIEWS));
    }

    #[test]
    fn a_search_matches_title_artist_or_album_in_the_current_order() {
        let mut list = list();
        list.set_order(SORT_TITLE, false);
        list.set_filter("a".to_owned());
        assert_eq!(titles(&list.matches()), ["Apple", "Mango", "Zebra"]);
        list.set_filter("alpha".to_owned());
        assert_eq!(titles(&list.matches()), ["Apple"]);
        list.set_filter("nothing".to_owned());
        assert!(list.matches().is_empty());
    }

    #[test]
    fn select_all_only_takes_what_the_search_left() {
        let mut list = list();
        list.set_filter("alpha".to_owned());
        list.select_visible();
        assert_eq!(list.selected_count(), 1);
        assert!(list.is_selected("b"));
        list.clear_selection();
        list.set_filter(String::new());
        list.select_visible();
        assert_eq!(list.selected_count(), 3);
    }

    #[test]
    fn selected_tracks_come_back_in_the_current_order() {
        let mut list = list();
        list.select("a", true);
        list.select("b", true);
        list.set_order(SORT_TITLE, false);
        assert_eq!(titles(&list.selected_tracks()), ["Apple", "Zebra"]);
        list.select("a", false);
        assert_eq!(list.selected_ids(), ["b"]);
    }

    #[test]
    fn chunks_render_the_fetched_list_a_piece_at_a_time() {
        let mut list = TrackList::default();
        list.set_fetched((0..5).map(|i| track(&i.to_string(), "T", "A", 60)).collect());
        assert_eq!(list.render_chunk(2).len(), 2);
        assert!(!list.fully_rendered());
        assert_eq!(list.render_chunk(9).len(), 3);
        assert!(list.fully_rendered());
        assert!(list.render_chunk(9).is_empty());
    }

    #[test]
    fn removing_a_row_takes_it_out_of_both_views() {
        let mut list = list();
        assert!(list.remove(|t| t.video_id.0 == "b"));
        assert_eq!(list.fetched().len(), 2);
        assert_eq!(list.rendered().len(), 2);
        assert!(!list.remove(|t| t.video_id.0 == "b"));
    }
}

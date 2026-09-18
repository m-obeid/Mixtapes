//! One row of the queue list model. GObject so ListView factories can bind
//! to its properties with expressions and survive row recycling.

use std::cell::{Cell, RefCell};

use glib::prelude::*;
use glib::subclass::prelude::*;

use crate::model::Track;

mod imp {
    use super::*;

    #[derive(Default, glib::Properties)]
    #[properties(wrapper_type = super::QueueEntry)]
    pub struct QueueEntry {
        #[property(get, set)]
        pub index: Cell<u32>,
        #[property(get, set)]
        pub title: RefCell<String>,
        #[property(get, set)]
        pub artist: RefCell<String>,
        #[property(get, set)]
        pub video_id: RefCell<String>,
        #[property(get, set)]
        pub playing: Cell<bool>,
        /// True while the playing row's track is paused or stopped.
        #[property(get, set)]
        pub paused: Cell<bool>,
        pub track: RefCell<Track>,
    }

    #[glib::object_subclass]
    impl ObjectSubclass for QueueEntry {
        const NAME: &'static str = "MxQueueEntry";
        type Type = super::QueueEntry;
    }

    #[glib::derived_properties]
    impl ObjectImpl for QueueEntry {}
}

glib::wrapper! {
    pub struct QueueEntry(ObjectSubclass<imp::QueueEntry>);
}

impl QueueEntry {
    pub fn new(index: u32, track: &Track, playing: bool, paused: bool) -> Self {
        let entry: Self = glib::Object::builder()
            .property("index", index)
            .property("title", track.title.as_str())
            .property("artist", track.artist.as_str())
            .property("video-id", track.video_id.as_str())
            .property("playing", playing)
            .property("paused", paused)
            .build();
        entry.imp().track.replace(track.clone());
        entry
    }

    pub fn track(&self) -> Track {
        self.imp().track.borrow().clone()
    }

    /// Bring a row that stays in the list up to date. Only what changed is set,
    /// so a bound row redraws for a reason and not on every sync.
    pub fn update(&self, index: u32, track: &Track, playing: bool, paused: bool) {
        if self.index() != index {
            self.set_index(index);
        }
        if self.title() != track.title {
            self.set_title(track.title.as_str());
        }
        if self.artist() != track.artist {
            self.set_artist(track.artist.as_str());
        }
        if self.playing() != playing {
            self.set_playing(playing);
        }
        if self.paused() != paused {
            self.set_paused(paused);
        }
        if *self.imp().track.borrow() != *track {
            self.imp().track.replace(track.clone());
        }
    }
}

/// How many rows at the start and at the end two id lists share, never overlapping.
/// What lies between is the only part of the list model a sync has to replace.
pub fn shared_ends(old: &[String], new: &[String]) -> (usize, usize) {
    let prefix = old.iter().zip(new).take_while(|(a, b)| a == b).count();
    let room = old.len().min(new.len()) - prefix;
    let suffix = old.iter().rev().zip(new.iter().rev()).take(room).take_while(|(a, b)| a == b).count();
    (prefix, suffix)
}

#[cfg(test)]
mod tests {
    use super::shared_ends;

    fn ids(list: &[&str]) -> Vec<String> {
        list.iter().map(|s| (*s).to_owned()).collect()
    }

    #[test]
    fn an_unchanged_queue_shares_everything() {
        assert_eq!(shared_ends(&ids(&["a", "b", "c"]), &ids(&["a", "b", "c"])), (3, 0));
    }

    #[test]
    fn an_append_a_removal_and_a_move_touch_only_the_middle() {
        assert_eq!(shared_ends(&ids(&["a", "b"]), &ids(&["a", "b", "c"])), (2, 0), "radio extended the queue");
        assert_eq!(shared_ends(&ids(&["a", "b", "c", "d"]), &ids(&["a", "c", "d"])), (1, 2), "b was removed");
        assert_eq!(shared_ends(&ids(&["a", "b", "c", "d"]), &ids(&["a", "c", "b", "d"])), (1, 1), "b and c swapped");
        assert_eq!(shared_ends(&ids(&["a", "a", "a"]), &ids(&["a", "a"])), (2, 0), "repeats never overlap");
        assert_eq!(shared_ends(&[], &ids(&["a"])), (0, 0));
    }
}

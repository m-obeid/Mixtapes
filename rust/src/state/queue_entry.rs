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
}

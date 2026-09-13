//! `PlayerState`: the single UI-facing store.
//!
//! Lives on the GTK thread only. Widgets bind to its properties with
//! `bind_property` / `PropertyExpression` and read the queue through its
//! `gio::ListStore`. Nothing outside the GTK thread ever holds a reference.
//! Only `player::Player` writes to it, in response to events from the audio
//! thread and results from the network runtime.

use std::cell::{Cell, OnceCell, RefCell};
use std::sync::OnceLock;

use glib::prelude::*;
use glib::subclass::prelude::*;
use glib::subclass::Signal;

use crate::model::{PlaybackStatus, RepeatMode};

pub mod media_object;
pub mod track_object;
pub mod queue_entry;
pub use media_object::{MediaObject, sync_store};
pub use queue_entry::QueueEntry;

mod imp {
    use super::*;

    #[derive(Default, glib::Properties)]
    #[properties(wrapper_type = super::PlayerState)]
    pub struct PlayerState {
        #[property(get, set, builder(PlaybackStatus::Stopped))]
        pub status: Cell<PlaybackStatus>,
        #[property(get, set)]
        pub position: Cell<f64>,
        #[property(get, set)]
        pub duration: Cell<f64>,

        #[property(get, set)]
        pub title: RefCell<String>,
        #[property(get, set)]
        pub artist: RefCell<String>,
        #[property(get, set)]
        pub thumbnail_url: RefCell<String>,
        #[property(get, set)]
        pub video_id: RefCell<String>,
        #[property(get, set)]
        pub like_status: RefCell<String>,

        #[property(get, set, minimum = 0.0, maximum = 1.0, default = 1.0)]
        pub volume: Cell<f64>,
        #[property(get, set)]
        pub muted: Cell<bool>,
        #[property(get, set)]
        pub shuffle: Cell<bool>,
        #[property(get, set, builder(RepeatMode::Off))]
        pub repeat: Cell<RepeatMode>,

        /// Index of the playing track in `queue`, or -1.
        #[property(get, set, default = -1)]
        pub current_index: Cell<i32>,
        #[property(get, set)]
        pub queue_length: Cell<u32>,

        #[property(get, set)]
        pub authenticated: Cell<bool>,
        #[property(get, set)]
        pub account_name: RefCell<String>,
        #[property(get, set)]
        pub account_handle: RefCell<String>,
        #[property(get, set)]
        pub account_photo_url: RefCell<String>,

        pub queue: OnceCell<gio::ListStore>,
    }

    #[glib::object_subclass]
    impl ObjectSubclass for PlayerState {
        const NAME: &'static str = "MxPlayerState";
        type Type = super::PlayerState;
    }

    #[glib::derived_properties]
    impl ObjectImpl for PlayerState {
        fn signals() -> &'static [Signal] {
            static SIGNALS: OnceLock<Vec<Signal>> = OnceLock::new();
            SIGNALS.get_or_init(|| {
                vec![
                    // video_id, title, message
                    Signal::builder("track-error")
                        .param_types([String::static_type(), String::static_type(), String::static_type()])
                        .build(),
                    // Structural queue change: the ListStore was rebuilt.
                    Signal::builder("queue-changed").build(),
                    // Short message for a toast (rating failed, and similar).
                    Signal::builder("notice").param_types([String::static_type()]).build(),
                ]
            })
        }

        fn constructed(&self) {
            self.parent_constructed();
            self.like_status.replace("INDIFFERENT".to_owned());
        }
    }
}

glib::wrapper! {
    pub struct PlayerState(ObjectSubclass<imp::PlayerState>);
}

impl Default for PlayerState {
    fn default() -> Self {
        Self::new()
    }
}

impl PlayerState {
    pub fn new() -> Self {
        glib::Object::new()
    }

    /// Queue as a list model of `QueueEntry`. Rows bind to its properties.
    pub fn queue_model(&self) -> gio::ListStore {
        self.imp().queue.get_or_init(gio::ListStore::new::<QueueEntry>).clone()
    }

    pub fn emit_track_error(&self, video_id: &str, title: &str, message: &str) {
        self.emit_by_name::<()>("track-error", &[&video_id, &title, &message]);
    }

    pub fn emit_queue_changed(&self) {
        self.emit_by_name::<()>("queue-changed", &[]);
    }

    pub fn emit_notice(&self, message: &str) {
        self.emit_by_name::<()>("notice", &[&message]);
    }
}

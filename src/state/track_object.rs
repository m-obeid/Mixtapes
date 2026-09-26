//! GObject wrapper for a `Track`, the item type of the playlist page's
//! track store. The header row is a plain `glib::Object`, so the flattened
//! model carries both under `glib::Object`.

use std::cell::RefCell;

use glib::subclass::prelude::*;

use crate::model::Track;

mod imp {
    use super::*;

    #[derive(Default)]
    pub struct TrackObject {
        pub track: RefCell<Track>,
    }

    #[glib::object_subclass]
    impl ObjectSubclass for TrackObject {
        const NAME: &'static str = "MxTrackObject";
        type Type = super::TrackObject;
    }

    impl ObjectImpl for TrackObject {}
}

glib::wrapper! {
    pub struct TrackObject(ObjectSubclass<imp::TrackObject>);
}

impl TrackObject {
    pub fn new(track: Track) -> Self {
        let object: Self = glib::Object::new();
        object.imp().track.replace(track);
        object
    }

    pub fn track(&self) -> Track {
        self.imp().track.borrow().clone()
    }

    pub fn video_id(&self) -> String {
        self.imp().track.borrow().video_id.0.clone()
    }

    pub fn with_track<R>(&self, f: impl FnOnce(&Track) -> R) -> R {
        f(&self.imp().track.borrow())
    }
}

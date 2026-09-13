//! GObject wrapper for a `MediaItem`, so library sections can live in
//! `gio::ListStore` models and feed `ListBox::bind_model`.

use std::cell::RefCell;

use gio::prelude::*;
use glib::subclass::prelude::*;

use crate::model::MediaItem;

mod imp {
    use super::*;

    #[derive(Default)]
    pub struct MediaObject {
        pub item: RefCell<MediaItem>,
    }

    #[glib::object_subclass]
    impl ObjectSubclass for MediaObject {
        const NAME: &'static str = "MxMediaObject";
        type Type = super::MediaObject;
    }

    impl ObjectImpl for MediaObject {}
}

glib::wrapper! {
    pub struct MediaObject(ObjectSubclass<imp::MediaObject>);
}

impl MediaObject {
    pub fn new(item: MediaItem) -> Self {
        let object: Self = glib::Object::new();
        object.imp().item.replace(item);
        object
    }

    pub fn item(&self) -> MediaItem {
        self.imp().item.borrow().clone()
    }

    pub fn id(&self) -> String {
        self.imp().item.borrow().id.clone()
    }
}

/// Replace a store's contents, keeping objects whose id and item are unchanged.
pub fn sync_store(store: &gio::ListStore, items: Vec<MediaItem>) {
    let objects: Vec<MediaObject> = items.into_iter().map(MediaObject::new).collect();
    store.splice(0, store.n_items(), &objects);
}

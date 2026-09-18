//! What every page and view receives: the player, the network handle, paths,
//! the compact flag, and a navigator the window fills in once it exists.

use std::cell::{Cell, RefCell};
use std::rc::Rc;
use std::sync::Arc;

use crate::downloads::{Downloads, Event as DownloadEvent};
use crate::model::{MediaItem, Track};
use crate::net::explore::Category;
use crate::net::NetHandle;
use crate::net::online::Online;
use crate::paths::Paths;
use crate::player::Player;

#[derive(Clone, Debug)]
pub enum NavRequest {
    /// `thumb` is the card art shown while the page loads, what open_playlist's initial_data carried.
    Playlist { id: String, title: String, thumb: Option<String> },
    Album { id: String, title: String, thumb: Option<String> },
    Artist { id: Option<String>, name: String },
    /// A mood or genre pill: the params open its carousels.
    Category { title: String, params: String },
    /// The full pill list behind View All.
    AllMoods { title: String, items: Vec<Category> },
    Search { query: String },
    /// An artist's albums, singles or songs grid.
    Discography { channel_id: String, title: String, browse_id: Option<String>, params: Option<String>, initial: Vec<MediaItem> },
}

type NavSink = Box<dyn Fn(NavRequest)>;

/// Told about one library card whose picture changed.
type CardSink = Box<dyn Fn(&str)>;

/// Opens the file picker for uploads, which the window owns.
type UploadSink = Box<dyn Fn()>;

#[derive(Default)]
pub struct Navigator {
    sink: RefCell<Option<NavSink>>,
    library_refresh: RefCell<Option<Box<dyn Fn()>>>,
    library_card_refresh: RefCell<Option<CardSink>>,
    upload_picker: RefCell<Option<UploadSink>>,
}

impl Navigator {
    pub fn set_sink(&self, sink: impl Fn(NavRequest) + 'static) {
        self.sink.replace(Some(Box::new(sink)));
    }

    /// What pages call after changing the library, like root.library_page.load_library().
    pub fn set_library_refresh(&self, f: impl Fn() + 'static) {
        self.library_refresh.replace(Some(Box::new(f)));
    }

    /// Where a card whose picture changed is reported. Its address does not
    /// say so on its own: YouTube keeps the same one for a new cover.
    pub fn set_library_card_refresh(&self, f: impl Fn(&str) + 'static) {
        self.library_card_refresh.replace(Some(Box::new(f)));
    }

    /// The window fills this in, so the uploads tab can ask for the picker.
    pub fn set_upload_picker(&self, f: impl Fn() + 'static) {
        self.upload_picker.replace(Some(Box::new(f)));
    }

    pub fn pick_uploads(&self) {
        if let Some(f) = self.upload_picker.borrow().as_ref() {
            f();
        }
    }

    pub fn refresh_library_card(&self, playlist_id: &str) {
        if let Some(f) = self.library_card_refresh.borrow().as_ref() {
            f(playlist_id);
        }
    }

    pub fn refresh_library(&self) {
        if let Some(f) = self.library_refresh.borrow().as_ref() {
            f();
        }
    }

    pub fn go(&self, request: NavRequest) {
        match self.sink.borrow().as_ref() {
            Some(sink) => sink(request),
            None => tracing::warn!(?request, "navigation before the window exists"),
        }
    }
}

/// Returns false once its widget is gone, so the list prunes itself.
type CompactListener = Box<dyn Fn(bool) -> bool>;

/// Same contract for download progress: false means the widget is gone.
type DownloadListener = Box<dyn Fn(&DownloadEvent) -> bool>;

/// Tracks, the playlist or album they came from, and its id.
type DownloadSink = Box<dyn Fn(Vec<Track>, String, String)>;

pub struct UiContext {
    pub player: Rc<Player>,
    pub net: NetHandle,
    pub paths: Paths,
    pub nav: Rc<Navigator>,
    pub online: Rc<Online>,
    /// Offline downloads, shared with the tokio side.
    pub downloads: Arc<Downloads>,
    /// Lyrics providers and their cache, shared by both lyrics views.
    pub lyrics: crate::lyrics::Lyrics,
    pub compact: Cell<bool>,
    compact_listeners: RefCell<Vec<CompactListener>>,
    download_listeners: RefCell<Vec<DownloadListener>>,
    download_sink: RefCell<Option<DownloadSink>>,
}

impl UiContext {
    pub fn new(player: Rc<Player>, net: NetHandle, paths: Paths, downloads: Arc<Downloads>, lyrics: crate::lyrics::Lyrics) -> Rc<Self> {
        let online = Online::new(net.clone(), paths.clone());
        Rc::new(Self {
            player,
            net,
            paths,
            nav: Rc::new(Navigator::default()),
            online,
            downloads,
            lyrics,
            compact: Cell::new(false),
            compact_listeners: RefCell::new(Vec::new()),
            download_listeners: RefCell::new(Vec::new()),
            download_sink: RefCell::new(None),
        })
    }

    /// Read download events off the manager's channel and hand them to
    /// whoever is listening. The window starts this once.
    pub fn pump_downloads(self: &Rc<Self>, events: async_channel::Receiver<DownloadEvent>) {
        let weak = Rc::downgrade(self);
        glib::spawn_future_local(async move {
            while let Ok(event) = events.recv().await {
                let Some(ctx) = weak.upgrade() else { return };
                let listeners = std::mem::take(&mut *ctx.download_listeners.borrow_mut());
                let kept: Vec<DownloadListener> = listeners.into_iter().filter(|f| f(&event)).collect();
                ctx.download_listeners.borrow_mut().extend(kept);
            }
        });
    }

    /// The window fills this in so every Download action lands in the same
    /// popover, the way each one walked up to the root window in Python.
    pub fn set_download_sink(&self, sink: impl Fn(Vec<Track>, String, String) + 'static) {
        self.download_sink.replace(Some(Box::new(sink)));
    }

    /// Queue tracks for offline playback. Without a window, which happens in
    /// the demo harness, the manager still takes them.
    pub fn download(&self, tracks: Vec<Track>, album_title: &str, album_id: &str) {
        match self.download_sink.borrow().as_ref() {
            Some(sink) => sink(tracks, album_title.to_owned(), album_id.to_owned()),
            None => self.downloads.queue_tracks(tracks, album_title, album_id),
        }
    }

    /// Register a widget that follows the download queue. Dropped once it
    /// returns false, like the compact listeners.
    pub fn on_download(&self, listener: impl Fn(&DownloadEvent) -> bool + 'static) {
        self.download_listeners.borrow_mut().push(Box::new(listener));
    }

    /// Flip the phone layout flag and tell every registered widget.
    /// Stands in for the `_propagate_compact` tree walks the Python pages did.
    pub fn set_compact(&self, compact: bool) {
        self.compact.set(compact);
        let listeners = std::mem::take(&mut *self.compact_listeners.borrow_mut());
        let kept: Vec<CompactListener> = listeners.into_iter().filter(|f| f(compact)).collect();
        self.compact_listeners.borrow_mut().extend(kept);
    }

    /// Register a widget that resizes for the phone layout. Called at once with the current flag.
    pub fn on_compact(&self, listener: impl Fn(bool) -> bool + 'static) {
        if listener(self.compact.get()) {
            self.compact_listeners.borrow_mut().push(Box::new(listener));
        }
    }
}

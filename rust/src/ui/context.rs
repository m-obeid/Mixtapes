//! What every page and view receives: the player, the network handle, paths,
//! the compact flag, and a navigator the window fills in once it exists.

use std::cell::{Cell, RefCell};
use std::rc::Rc;

use crate::model::MediaItem;
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
    Category { title: String },
    Search { query: String },
    /// An artist's albums, singles or songs grid.
    Discography { channel_id: String, title: String, browse_id: Option<String>, params: Option<String>, initial: Vec<MediaItem> },
}

type NavSink = Box<dyn Fn(NavRequest)>;

#[derive(Default)]
pub struct Navigator {
    sink: RefCell<Option<NavSink>>,
    library_refresh: RefCell<Option<Box<dyn Fn()>>>,
}

impl Navigator {
    pub fn set_sink(&self, sink: impl Fn(NavRequest) + 'static) {
        self.sink.replace(Some(Box::new(sink)));
    }

    /// What pages call after changing the library, like root.library_page.load_library().
    pub fn set_library_refresh(&self, f: impl Fn() + 'static) {
        self.library_refresh.replace(Some(Box::new(f)));
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

pub struct UiContext {
    pub player: Rc<Player>,
    pub net: NetHandle,
    pub paths: Paths,
    pub nav: Rc<Navigator>,
    pub online: Rc<Online>,
    pub compact: Cell<bool>,
    compact_listeners: RefCell<Vec<CompactListener>>,
}

impl UiContext {
    pub fn new(player: Rc<Player>, net: NetHandle, paths: Paths) -> Rc<Self> {
        let online = Online::new(net.clone(), paths.clone());
        Rc::new(Self { player, net, paths, nav: Rc::new(Navigator::default()), online, compact: Cell::new(false), compact_listeners: RefCell::new(Vec::new()) })
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

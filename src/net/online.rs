//! Port of the connectivity probe in ui/utils.py. Gio.NetworkMonitor is fast
//! but noisy, so it only triggers a probe. A TCP connect to music.youtube.com
//! decides, and listeners hear about real transitions once each.
//!
//! GTK-thread only: the probe runs on tokio, the bookkeeping does not.

use std::cell::{Cell, RefCell};
use std::rc::Rc;
use std::time::{Duration, Instant};

use gtk::glib;

use crate::net::NetHandle;
use crate::paths::Paths;

/// How stale the cached answer is allowed to get before is_online re-probes.
const PROBE_INTERVAL: Duration = Duration::from_secs(15);
/// The force_offline pref is read from disk at most this often.
const FORCE_OFFLINE_TTL: Duration = Duration::from_secs(10);
const PROBE_TIMEOUT: Duration = Duration::from_secs(2);
const PROBE_HOST: (&str, u16) = ("music.youtube.com", 443);

type Listener = Rc<dyn Fn(bool)>;
type Waiter = Box<dyn FnOnce(bool)>;

pub struct Online {
    net: NetHandle,
    paths: Paths,
    /// Optimistic until the first probe lands, like the Python default.
    value: Cell<bool>,
    last_probe: Cell<Option<Instant>>,
    in_flight: Cell<bool>,
    /// The last value handed to listeners.
    notified: Cell<bool>,
    listeners: RefCell<Vec<Listener>>,
    waiters: RefCell<Vec<Waiter>>,
    force_offline: Cell<(bool, Option<Instant>)>,
}

impl Online {
    pub fn new(net: NetHandle, paths: Paths) -> Rc<Self> {
        Rc::new(Self {
            net,
            paths,
            value: Cell::new(true),
            last_probe: Cell::new(None),
            in_flight: Cell::new(false),
            notified: Cell::new(true),
            listeners: RefCell::new(Vec::new()),
            waiters: RefCell::new(Vec::new()),
            force_offline: Cell::new((false, None)),
        })
    }

    /// The force_offline pref, cached so bind paths do not hit the disk.
    fn force_offline(&self) -> bool {
        let (value, expires) = self.force_offline.get();
        if expires.is_some_and(|t| Instant::now() < t) {
            return value;
        }
        let value = self.paths.read_prefs().get("force_offline").and_then(|v| v.as_bool()).unwrap_or(false);
        self.force_offline.set((value, Some(Instant::now() + FORCE_OFFLINE_TTL)));
        value
    }

    /// `listener(online)` runs on the GTK thread each time a probe flips the state.
    pub fn add_listener(&self, listener: impl Fn(bool) + 'static) {
        self.listeners.borrow_mut().push(Rc::new(listener));
    }

    /// The last observed state. Never blocks; kicks a background probe when stale.
    pub fn is_online(self: &Rc<Self>) -> bool {
        if self.force_offline() {
            return false;
        }
        self.kick();
        self.value.get()
    }

    fn kick(self: &Rc<Self>) {
        if self.in_flight.get() {
            return;
        }
        if self.last_probe.get().is_some_and(|t| t.elapsed() < PROBE_INTERVAL) {
            return;
        }
        self.start_probe();
    }

    /// Probe at once, ignoring staleness, and hand the fresh answer to `callback`.
    /// A probe already in flight is joined rather than duplicated.
    pub fn probe_now(self: &Rc<Self>, callback: Option<Waiter>) {
        if let Some(cb) = callback {
            self.waiters.borrow_mut().push(cb);
        }
        if self.in_flight.get() {
            return;
        }
        self.start_probe();
    }

    /// Force the next is_online to re-probe and re-read force_offline.
    pub fn invalidate(&self) {
        self.force_offline.set((false, None));
        self.last_probe.set(None);
    }

    fn start_probe(self: &Rc<Self>) {
        self.in_flight.set(true);
        let handle = self.net.spawn(async {
            match tokio::time::timeout(PROBE_TIMEOUT, tokio::net::TcpStream::connect(PROBE_HOST)).await {
                Ok(Ok(_)) => true,
                Ok(Err(_)) | Err(_) => false,
            }
        });
        let weak = Rc::downgrade(self);
        glib::spawn_future_local(async move {
            let reachable = handle.await.unwrap_or(false);
            let Some(this) = weak.upgrade() else { return };
            this.value.set(reachable);
            this.last_probe.set(Some(Instant::now()));
            this.in_flight.set(false);
            let waiters: Vec<Waiter> = std::mem::take(&mut *this.waiters.borrow_mut());
            let online = reachable && !this.force_offline();
            if this.notified.replace(online) != online {
                tracing::info!(online, "connectivity changed");
                let listeners: Vec<Listener> = this.listeners.borrow().clone();
                for listener in listeners {
                    listener(online);
                }
            }
            for waiter in waiters {
                waiter(online);
            }
        });
    }
}

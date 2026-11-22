//! Port of ui/widgets/add_to_playlist.py: the popover for picking a playlist
//! to add tracks to. Cover thumbnails, type-to-search, recently-used first,
//! and a capped height so a user with two hundred playlists scrolls the
//! list, not the window. Given the songs, each playlist shows a tick when it
//! holds them all, and a click adds or removes while the picker stays open,
//! so one song goes to several playlists in one go (#183). Without them, as
//! for a whole queue, a click adds and closes.

use std::cell::{Cell, RefCell};
use std::collections::{HashMap, HashSet};
use std::rc::Rc;
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

use gtk::{glib, prelude::*};

use crate::model::MediaItem;
use crate::net::library;
use crate::net::playlists::editable_playlists;
use crate::net::ytmusic::AuthState;
use crate::paths::Paths;
use crate::ui::context::UiContext;
use crate::ui::cover::CoverImage;

const CSS: &str = "
.add-to-playlist-cover { border-radius: 4px; }
.add-to-playlist-list > row { padding: 4px 6px; }
";

thread_local! {
    static CSS_INSTALLED: std::cell::Cell<bool> = const { std::cell::Cell::new(false) };
    /// Ticks changed in the picker lately, by (playlist, songs). youtube.com takes about ten seconds to report an edit.
    static RECENT_TICKS: RefCell<HashMap<(String, String), (bool, Instant)>> = RefCell::default();
}

/// How long a tick set here outranks youtube.com's answer.
const TICK_MEMORY: Duration = Duration::from_secs(60);

fn songs_key(video_ids: &[String]) -> String {
    video_ids.join(",")
}

fn remember_tick(playlist_id: &str, video_ids: &[String], holds: bool) {
    RECENT_TICKS.with(|recent| {
        let mut recent = recent.borrow_mut();
        recent.retain(|_, (_, at)| at.elapsed() < TICK_MEMORY);
        recent.insert((playlist_id.to_owned(), songs_key(video_ids)), (holds, Instant::now()));
    });
}

/// The ticks set here within the last minute, for these songs.
fn recent_ticks(video_ids: &[String]) -> HashMap<String, bool> {
    let songs = songs_key(video_ids);
    RECENT_TICKS.with(|recent| {
        recent
            .borrow()
            .iter()
            .filter(|((_, key), (_, at))| *key == songs && at.elapsed() < TICK_MEMORY)
            .map(|((playlist, _), (holds, _))| (playlist.clone(), *holds))
            .collect()
    })
}

fn install_css() {
    CSS_INSTALLED.with(|installed| {
        if installed.get() {
            return;
        }
        let provider = gtk::CssProvider::new();
        provider.load_from_string(CSS);
        if let Some(display) = gtk::gdk::Display::default() {
            gtk::style_context_add_provider_for_display(
                &display,
                &provider,
                gtk::STYLE_PROVIDER_PRIORITY_APPLICATION,
            );
            installed.set(true);
        }
    });
}

// -- recents --------------------------------------------------------------

fn recents_path(paths: &Paths) -> std::path::PathBuf {
    paths.cache_dir.join("playlist_recents.json")
}

fn load_recents(paths: &Paths) -> HashMap<String, i64> {
    std::fs::read_to_string(recents_path(paths))
        .ok()
        .and_then(|s| serde_json::from_str(&s).ok())
        .unwrap_or_default()
}

/// Bump a playlist's last-used stamp after a successful add. Best effort.
pub fn mark_playlist_used(paths: &Paths, playlist_id: &str) {
    if playlist_id.is_empty() {
        return;
    }
    let mut data = load_recents(paths);
    let now = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_secs() as i64)
        .unwrap_or(0);
    data.insert(playlist_id.to_owned(), now);
    if data.len() > 200 {
        let mut entries: Vec<(String, i64)> = data.into_iter().collect();
        entries.sort_by_key(|e| std::cmp::Reverse(e.1));
        entries.truncate(200);
        data = entries.into_iter().collect();
    }
    let path = recents_path(paths);
    if let Some(dir) = path.parent() {
        let _ = std::fs::create_dir_all(dir);
    }
    let tmp = path.with_extension("json.tmp");
    if let Ok(text) = serde_json::to_string(&data) {
        if std::fs::write(&tmp, text).is_ok() {
            let _ = std::fs::rename(&tmp, &path);
        }
    }
}

// -- popover --------------------------------------------------------------

pub struct AddToPlaylistPopover {
    popover: gtk::Popover,
    /// Where toasts go, the widget the picker points at.
    anchor: gtk::Widget,
    listbox: gtk::ListBox,
    empty_label: gtk::Label,
    filter_text: Rc<RefCell<String>>,
    covers: RefCell<Vec<Rc<CoverImage>>>,
    on_select: Rc<dyn Fn(String)>,
    ctx: Rc<UiContext>,
    /// The songs the picker ticks playlists for. Empty means no ticks: a click adds and closes.
    video_ids: Vec<String>,
    /// Playlists known to hold all the songs.
    holding: RefCell<HashSet<String>>,
    /// The tick of each playlist row.
    checks: RefCell<HashMap<String, gtk::CheckButton>>,
    /// Playlists the listener ticked or unticked here, which a late answer leaves alone.
    touched: RefCell<HashSet<String>>,
    /// Set while the code moves a tick, so the move is not read as a click.
    syncing: Cell<bool>,
}

impl AddToPlaylistPopover {
    /// Build, anchor on `parent`, fill and pop up. `on_select` gets the playlist id to add to.
    /// With `video_ids`, the playlists holding those songs show ticked, and unticking one removes them.
    pub fn show(
        ctx: &Rc<UiContext>,
        parent: &impl IsA<gtk::Widget>,
        video_ids: Vec<String>,
        on_select: impl Fn(String) + 'static,
    ) -> Rc<Self> {
        install_css();
        let popover = gtk::Popover::builder()
            .has_arrow(true)
            .autohide(true)
            .build();
        popover.set_parent(parent);

        let outer = gtk::Box::builder()
            .orientation(gtk::Orientation::Vertical)
            .spacing(6)
            .width_request(320)
            .build();
        let search_entry = gtk::SearchEntry::builder()
            .placeholder_text(tr!("Search playlists…"))
            .build();
        outer.append(&search_entry);

        let scrolled = gtk::ScrolledWindow::builder()
            .hscrollbar_policy(gtk::PolicyType::Never)
            .vscrollbar_policy(gtk::PolicyType::Automatic)
            .propagate_natural_height(true)
            .min_content_height(120)
            .max_content_height(360)
            .build();
        let listbox = gtk::ListBox::builder()
            .selection_mode(gtk::SelectionMode::None)
            .css_classes(["navigation-sidebar", "add-to-playlist-list"])
            .build();
        scrolled.set_child(Some(&listbox));
        outer.append(&scrolled);

        let empty_label = gtk::Label::builder()
            .label(tr!("No playlists"))
            .css_classes(["dim-label"])
            .margin_top(12)
            .margin_bottom(12)
            .visible(false)
            .build();
        outer.append(&empty_label);
        // Always there, so a listener with no playlist yet, or signed out, can still add.
        let new_button = gtk::Button::builder()
            .child(&adw::ButtonContent::builder().icon_name("list-add-symbolic").label(tr!("New Playlist…")).build())
            .css_classes(["flat"])
            .build();
        outer.append(&new_button);
        popover.set_child(Some(&outer));

        let filter_text: Rc<RefCell<String>> = Rc::new(RefCell::new(String::new()));
        {
            let filter_text = filter_text.clone();
            listbox.set_filter_func(move |row| {
                let text = filter_text.borrow();
                if text.is_empty() {
                    return true;
                }
                let title = unsafe { row.data::<String>("title-lower") }
                    .map(|p| unsafe { p.as_ref() }.clone())
                    .unwrap_or_default();
                title.contains(text.as_str())
            });
        }
        {
            let filter_text = filter_text.clone();
            let listbox = listbox.clone();
            search_entry.connect_search_changed(move |entry| {
                filter_text.replace(entry.text().trim().to_lowercase());
                listbox.invalidate_filter();
            });
        }

        let this = Rc::new(Self {
            popover: popover.clone(),
            anchor: parent.clone().upcast(),
            listbox,
            empty_label,
            filter_text,
            covers: RefCell::new(Vec::new()),
            on_select: Rc::new(on_select),
            ctx: ctx.clone(),
            video_ids: video_ids.into_iter().filter(|id| !id.is_empty()).collect(),
            holding: RefCell::default(),
            checks: RefCell::default(),
            touched: RefCell::default(),
            syncing: Cell::new(false),
        });
        {
            let weak = Rc::downgrade(&this);
            this.listbox.connect_row_activated(move |_, row| {
                let Some(this) = weak.upgrade() else { return };
                let pid = unsafe { row.data::<String>("playlist-id") }
                    .map(|p| unsafe { p.as_ref() }.clone());
                let Some(pid) = pid else { return };
                // With ticks, a click flips the tick, and the tick adds or removes.
                let check = this.checks.borrow().get(&pid).cloned();
                if let Some(check) = check {
                    check.set_active(!check.is_active());
                    return;
                }
                (this.on_select)(pid);
                this.popover.popdown();
            });
        }
        {
            let weak = Rc::downgrade(&this);
            let parent: gtk::Widget = parent.clone().upcast();
            new_button.connect_clicked(move |_| {
                let Some(this) = weak.upgrade() else { return };
                this.popover.popdown();
                let on_select = this.on_select.clone();
                crate::ui::playlist_ops::ask_new_playlist(&this.ctx, &parent, move |id, _title| on_select(id));
            });
        }
        // The popover owns the struct until it closes.
        unsafe { popover.set_data("add-to-playlist", this.clone()) };
        popover.connect_closed(|popover| {
            let popover = popover.clone();
            glib::idle_add_local_once(move || {
                unsafe {
                    let _ = popover.steal_data::<Rc<AddToPlaylistPopover>>("add-to-playlist");
                }
                popover.unparent();
            });
        });
        this.populate();
        this.load_holding();
        popover.popup();
        this
    }

    fn account_name(&self) -> Option<String> {
        match self.ctx.net.client().auth_state() {
            AuthState::Authenticated(info) => Some(info.name),
            _ => None,
        }
    }

    /// Port of _populate: editable playlists, most recently used in-app first,
    /// the rest in the library's own most-recently-modified order.
    fn populate(self: &Rc<Self>) {
        let cached = self.ctx.net.caches().library_playlists();
        if !cached.is_empty() {
            self.fill(cached);
            return;
        }
        if !self.ctx.net.client().is_authenticated() {
            self.fill(Vec::new());
            return;
        }
        let api = self.ctx.net.client().api();
        let handle = self.ctx.net.spawn(library::library_playlists(api));
        let weak = Rc::downgrade(self);
        glib::spawn_future_local(async move {
            let playlists = match handle.await {
                Ok(Ok(p)) => p,
                _ => Vec::new(),
            };
            if let Some(this) = weak.upgrade() {
                this.ctx
                    .net
                    .caches()
                    .set_library_playlists(playlists.clone());
                this.fill(playlists);
            }
        });
    }

    /// Playlists on this device first, then the account's editable ones.
    fn fill(self: &Rc<Self>, playlists: Vec<MediaItem>) {
        let mut all = self.ctx.local.playlist_items();
        all.extend(editable_playlists(&playlists, self.account_name().as_deref()));
        let mut playlists = all;
        if playlists.is_empty() {
            self.empty_label.set_visible(true);
            return;
        }
        let recents = load_recents(&self.ctx.paths);
        let api_order: HashMap<String, usize> = playlists
            .iter()
            .enumerate()
            .map(|(i, p)| (p.id.clone(), i))
            .collect();
        playlists.sort_by_key(|p| {
            (
                -recents.get(&p.id).copied().unwrap_or(0),
                api_order.get(&p.id).copied().unwrap_or(0),
            )
        });

        for p in playlists {
            let row = gtk::ListBoxRow::new();
            unsafe {
                row.set_data("playlist-id", p.id.clone());
                row.set_data("title-lower", p.title.to_lowercase());
            }
            let hbox = gtk::Box::builder()
                .orientation(gtk::Orientation::Horizontal)
                .spacing(10)
                .build();
            let cover = CoverImage::new(self.ctx.net.clone(), 36);
            cover.widget().add_css_class("add-to-playlist-cover");
            if let Some(url) = &p.thumb {
                cover.load(url);
            }
            hbox.append(cover.widget());
            self.covers.borrow_mut().push(cover);
            hbox.append(
                &gtk::Label::builder()
                    .label(&p.title)
                    .halign(gtk::Align::Start)
                    .hexpand(true)
                    .ellipsize(gtk::pango::EllipsizeMode::End)
                    .xalign(0.0)
                    .build(),
            );
            if !self.video_ids.is_empty() {
                let check = gtk::CheckButton::builder()
                    .valign(gtk::Align::Center)
                    .active(self.holding.borrow().contains(&p.id))
                    .build();
                {
                    let (weak, id) = (Rc::downgrade(self), p.id.clone());
                    check.connect_toggled(move |check| {
                        if let Some(this) = weak.upgrade() {
                            this.on_toggled(&id, check);
                        }
                    });
                }
                hbox.append(&check);
                self.checks.borrow_mut().insert(p.id.clone(), check);
            }
            row.set_child(Some(&hbox));
            self.listbox.append(&row);
        }
        let _ = &self.filter_text;
    }

    /// Which playlists hold the songs: the ones on this device at once, the account's from youtube.com.
    fn load_holding(self: &Rc<Self>) {
        if self.video_ids.is_empty() {
            return;
        }
        let local: HashSet<String> = self
            .ctx
            .local
            .playlist_items()
            .into_iter()
            .filter(|p| self.ctx.local.details(&p.id).is_some_and(|d| self.video_ids.iter().all(|id| d.tracks.iter().any(|t| &t.video_id.0 == id))))
            .map(|p| p.id)
            .collect();
        self.mark_holding(local.clone());
        let client = self.ctx.net.client();
        let Some(auth) = client.is_authenticated().then(|| client.media_auth()).flatten() else { return };
        let (http, channel, ids) = (client.http().clone(), client.channel(), self.video_ids.clone());
        let handle = self.ctx.net.spawn(async move { crate::net::playlists::playlists_holding(&http, &auth, channel.as_deref(), &ids).await });
        let weak = Rc::downgrade(self);
        glib::spawn_future_local(async move {
            match handle.await {
                Ok(Ok(mut held)) => {
                    if let Some(this) = weak.upgrade() {
                        held.extend(local);
                        this.mark_holding(held);
                    }
                }
                Ok(Err(err)) => tracing::warn!(%err, "which playlists hold the song"),
                Err(_) => {}
            }
        });
    }

    /// Tick what an answer says holds the songs. A tick set here lately outranks it, and one set in this picker always does.
    fn mark_holding(&self, mut held: HashSet<String>) {
        for (id, holds) in recent_ticks(&self.video_ids) {
            if holds {
                held.insert(id);
            } else {
                held.remove(&id);
            }
        }
        let touched = self.touched.borrow();
        let checks = self.checks.borrow();
        for (id, check) in checks.iter().filter(|(id, _)| !touched.contains(*id)) {
            self.set_tick(check, held.contains(id));
        }
        let mut holding = self.holding.borrow_mut();
        holding.retain(|id| touched.contains(id));
        holding.extend(held.into_iter().filter(|id| !touched.contains(id)));
    }

    /// Move a tick without it counting as a click.
    fn set_tick(&self, check: &gtk::CheckButton, active: bool) {
        self.syncing.set(true);
        check.set_active(active);
        self.syncing.set(false);
    }

    /// The listener ticked or unticked a playlist: add the songs, or take them out.
    fn on_toggled(self: &Rc<Self>, playlist_id: &str, check: &gtk::CheckButton) {
        if self.syncing.get() {
            return;
        }
        self.touched.borrow_mut().insert(playlist_id.to_owned());
        remember_tick(playlist_id, &self.video_ids, check.is_active());
        if check.is_active() {
            self.holding.borrow_mut().insert(playlist_id.to_owned());
            (self.on_select)(playlist_id.to_owned());
            return;
        }
        self.holding.borrow_mut().remove(playlist_id);
        let weak = Rc::downgrade(self);
        let check = check.downgrade();
        let (id, ids) = (playlist_id.to_owned(), self.video_ids.clone());
        crate::ui::playlist_ops::remove_tracks(&self.ctx, &self.anchor, playlist_id.to_owned(), self.video_ids.clone(), move |removed| {
            if removed {
                return;
            }
            // Still in there, so the tick comes back.
            remember_tick(&id, &ids, true);
            if let (Some(this), Some(check)) = (weak.upgrade(), check.upgrade()) {
                this.holding.borrow_mut().insert(id);
                this.set_tick(&check, true);
            }
        });
    }
}

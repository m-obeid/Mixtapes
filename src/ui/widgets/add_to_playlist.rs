//! Port of ui/widgets/add_to_playlist.py: the popover for picking a playlist
//! to add tracks to. Cover thumbnails, type-to-search, recently-used first,
//! and a capped height so a user with two hundred playlists scrolls the
//! list, not the window.

use std::cell::RefCell;
use std::collections::HashMap;
use std::rc::Rc;
use std::time::{SystemTime, UNIX_EPOCH};

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
    listbox: gtk::ListBox,
    empty_label: gtk::Label,
    filter_text: Rc<RefCell<String>>,
    covers: RefCell<Vec<Rc<CoverImage>>>,
    on_select: Rc<dyn Fn(String)>,
    ctx: Rc<UiContext>,
}

impl AddToPlaylistPopover {
    /// Build, anchor on `parent`, fill and pop up. `on_select` gets the playlist id.
    pub fn show(
        ctx: &Rc<UiContext>,
        parent: &impl IsA<gtk::Widget>,
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
            .placeholder_text("Search playlists…")
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
            .label("No playlists")
            .css_classes(["dim-label"])
            .margin_top(12)
            .margin_bottom(12)
            .visible(false)
            .build();
        outer.append(&empty_label);
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
            listbox,
            empty_label,
            filter_text,
            covers: RefCell::new(Vec::new()),
            on_select: Rc::new(on_select),
            ctx: ctx.clone(),
        });
        {
            let weak = Rc::downgrade(&this);
            this.listbox.connect_row_activated(move |_, row| {
                let Some(this) = weak.upgrade() else { return };
                let pid = unsafe { row.data::<String>("playlist-id") }
                    .map(|p| unsafe { p.as_ref() }.clone());
                if let Some(pid) = pid {
                    (this.on_select)(pid);
                }
                this.popover.popdown();
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
            row.set_child(Some(&hbox));
            self.listbox.append(&row);
        }
        let _ = &self.filter_text;
    }
}

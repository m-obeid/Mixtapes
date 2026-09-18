//! Port of ui/pages/library.py on live data: Library and Uploads tabs, a
//! list or grid view per section from the persisted preference, Playlists
//! (with the Downloads entry injected at index one), Albums, Artists, and
//! the overlay loaders. Sections are `gio::ListStore`s of `MediaObject`;
//! the list views bind to them and the grids rebuild on `items-changed`.

use std::cell::{Cell, RefCell};
use std::rc::Rc;

use adw::prelude::*;
use gtk::{gio, glib};

use crate::model::{ItemKind, MediaItem};
use crate::net::library;
use crate::state::{MediaObject, sync_store};
use crate::ui::context::{NavRequest, UiContext};
use crate::ui::cover::CoverImage;
use crate::model::LikeStatus;
use crate::ui::context_menu::{self, MenuAction};
use crate::ui::pages::attach_item_menu;
use crate::ui::toast;
use crate::ui::widgets::card_grid::CardGrid;
use crate::ui::widgets::media_card::{CardOptions, MediaCard};

const VIEW_MODES: [&str; 2] = ["list", "grid"];
const DEFAULT_VIEW_MODE: &str = "grid";
const DOWNLOADS_ID: &str = "DL";
/// A library shown again within this long is fresh enough to leave alone.
const RELOAD_GAP: std::time::Duration = std::time::Duration::from_secs(2);

struct Section {
    root: gtk::Box,
    list: gtk::ListBox,
    grid: CardGrid,
    store: gio::ListStore,
    /// What the list and the grid show: the store with the search applied.
    filtered: gtk::FilterListModel,
    filter: gtk::CustomFilter,
    cards: Rc<RefCell<Vec<Rc<MediaCard>>>>,
}

pub struct LibraryPage {
    root: gtk::Box,
    lib_stack: gtk::Stack,
    content_box: gtk::Box,
    uploads_box: gtk::Box,
    lib_actions: gtk::Box,
    uploads_actions: gtk::Box,
    view_toggle: gtk::Button,
    loading: gtk::Box,
    uploads_loading: gtk::Box,
    empty_uploads: gtk::Label,
    sections: Vec<Rc<Section>>,
    upload_sections: Vec<Rc<Section>>,
    uploads_tab: gtk::ToggleButton,
    ctx: Rc<UiContext>,
    is_loading: Cell<bool>,
    /// When the last load finished, so coming back into view does not refetch
    /// what was just fetched.
    loaded_at: Cell<Option<std::time::Instant>>,
    compact: Rc<Cell<bool>>,
    /// What the search bar last typed, lowercased. Shared with every filter.
    query: Rc<RefCell<String>>,
    on_refresh_done: RefCell<Option<Rc<dyn Fn()>>>,
}

impl LibraryPage {
    pub fn new(ctx: Rc<UiContext>) -> Rc<Self> {
        let root = gtk::Box::builder().orientation(gtk::Orientation::Vertical).build();
        let content_box = gtk::Box::builder().orientation(gtk::Orientation::Vertical).spacing(24).margin_top(12).margin_bottom(24).margin_start(12).margin_end(12).build();

        // Tab row: Library / Uploads toggles, then per-tab actions.
        let tab_row = gtk::Box::builder().orientation(gtk::Orientation::Horizontal).spacing(8).margin_bottom(8).build();
        let lib_tab = gtk::ToggleButton::builder().label("Library").active(true).build();
        let upl_tab = gtk::ToggleButton::builder().label("Uploads").group(&lib_tab).build();
        tab_row.append(&lib_tab);
        tab_row.append(&upl_tab);
        tab_row.append(&gtk::Box::builder().hexpand(true).build());
        let lib_actions = gtk::Box::builder().orientation(gtk::Orientation::Horizontal).spacing(4).build();
        let view_toggle = gtk::Button::builder().css_classes(["flat", "circular"]).valign(gtk::Align::Center).build();
        lib_actions.append(&view_toggle);
        tab_row.append(&lib_actions);
        let uploads_actions = gtk::Box::builder().orientation(gtk::Orientation::Horizontal).spacing(4).visible(false).build();
        let all_songs = gtk::Button::builder().icon_name("audio-x-generic-symbolic").css_classes(["flat", "circular"]).valign(gtk::Align::Center).tooltip_text("All Uploaded Songs").build();
        uploads_actions.append(&all_songs);
        let upload = gtk::Button::builder().icon_name("document-send-symbolic").css_classes(["flat", "circular"]).valign(gtk::Align::Center).tooltip_text("Upload Songs").build();
        uploads_actions.append(&upload);
        tab_row.append(&uploads_actions);
        content_box.append(&tab_row);

        let lib_stack = gtk::Stack::builder().transition_type(gtk::StackTransitionType::SlideLeftRight).vexpand(true).build();
        content_box.append(&lib_stack);

        let lib_content = gtk::Box::builder().orientation(gtk::Orientation::Vertical).spacing(24).build();
        let new_playlist = gtk::Button::builder().icon_name("list-add-symbolic").css_classes(["flat", "circular"]).valign(gtk::Align::Center).tooltip_text("New Playlist").build();
        // One query behind every section, library and uploads alike: the
        // search bar filters whichever sub-tab is showing, like Python's.
        let query: Rc<RefCell<String>> = Rc::new(RefCell::new(String::new()));
        let playlists = Section::new("Playlists", Some(&new_playlist), &query);
        let albums = Section::new("Albums", None, &query);
        let artists = Section::new("Artists", None, &query);
        for s in [&playlists, &albums, &artists] {
            lib_content.append(&s.root);
        }
        lib_stack.add_titled(&lib_content, Some("library"), "Library");

        let uploads_box = gtk::Box::builder().orientation(gtk::Orientation::Vertical).spacing(24).vexpand(true).build();
        let up_albums = Section::new("Albums", None, &query);
        let up_artists = Section::new("Artists", None, &query);
        uploads_box.append(&up_albums.root);
        uploads_box.append(&up_artists.root);
        let empty_uploads = gtk::Label::builder().label("No uploaded music").css_classes(["dim-label"]).visible(false).build();
        uploads_box.append(&empty_uploads);
        lib_stack.add_titled(&uploads_box, Some("uploads"), "Uploads");

        let clamp = adw::Clamp::builder().maximum_size(1124).tightening_threshold(600).child(&content_box).build();
        let scrolled = gtk::ScrolledWindow::builder().hscrollbar_policy(gtk::PolicyType::Never).vexpand(true).child(&clamp).build();
        crate::ui::suppress_hover_while_scrolling(&scrolled);
        let loading = overlay_loader("Refreshing Library...");
        let uploads_loading = overlay_loader("Loading uploads...");
        let overlay = gtk::Overlay::builder().vexpand(true).child(&scrolled).build();
        overlay.add_overlay(&loading);
        overlay.add_overlay(&uploads_loading);
        root.append(&overlay);

        let page = Rc::new(Self {
            root,
            lib_stack,
            content_box,
            uploads_box,
            lib_actions,
            uploads_actions,
            view_toggle,
            loading,
            uploads_loading,
            empty_uploads,
            sections: vec![playlists, albums, artists],
            upload_sections: vec![up_albums, up_artists],
            uploads_tab: upl_tab.clone(),
            ctx,
            is_loading: Cell::new(false),
            loaded_at: Cell::new(None),
            compact: Rc::new(Cell::new(false)),
            query,
            on_refresh_done: RefCell::new(None),
        });
        for section in page.sections.iter().chain(page.upload_sections.iter()) {
            section.grid.set_cards(&section.cards, page.compact.clone());
        }

        {
            let stack = page.lib_stack.clone();
            lib_tab.connect_toggled(move |b| {
                if b.is_active() {
                    stack.set_visible_child_name("library");
                }
            });
            let stack = page.lib_stack.clone();
            upl_tab.connect_toggled(move |b| {
                if b.is_active() {
                    stack.set_visible_child_name("uploads");
                }
            });
            let weak = Rc::downgrade(&page);
            page.lib_stack.connect_visible_child_name_notify(move |stack| {
                if let Some(p) = weak.upgrade() {
                    let uploads = stack.visible_child_name().as_deref() == Some("uploads");
                    p.lib_actions.set_visible(!uploads);
                    p.uploads_actions.set_visible(uploads);
                }
            });
        }
        {
            let weak = Rc::downgrade(&page);
            page.view_toggle.connect_clicked(move |_| {
                if let Some(p) = weak.upgrade() {
                    let next = if p.view_mode() == "grid" { "list" } else { "grid" };
                    p.ctx.paths.update_prefs(|prefs| {
                        prefs.insert("library_view_mode".into(), serde_json::Value::String(next.to_owned()));
                    });
                    p.apply_layout();
                }
            });
            let weak = Rc::downgrade(&page);
            new_playlist.connect_clicked(move |_| {
                if let Some(p) = weak.upgrade() {
                    p.ask_new_playlist();
                }
            });
            let root = page.root.clone();
            all_songs.connect_clicked(move |_| {
                let _ = root.activate_action("win.open-uploads", None);
            });
            let ctx = page.ctx.clone();
            upload.connect_clicked(move |_| ctx.nav.pick_uploads());
        }
        {
            // Coming back from a playlist shows the library again. Reload then,
            // so a rename, a new cover or a deletion is there without reaching
            // for the refresh button. A page edit asks for a reload too, but
            // that runs while YouTube is still serving the old card.
            let weak = Rc::downgrade(&page);
            page.root.connect_map(move |_| {
                let Some(p) = weak.upgrade() else { return };
                let recent = p.loaded_at.get().is_some_and(|at| at.elapsed() < RELOAD_GAP);
                if !recent {
                    p.load_library(true);
                }
            });
        }
        for section in page.sections.iter().chain(page.upload_sections.iter()) {
            page.bind_section(section);
        }
        page.sync_view_toggle();

        // Load once signed in, clear when signed out.
        {
            let weak = Rc::downgrade(&page);
            let state = page.ctx.player.state();
            state.connect_notify_local(Some("authenticated"), move |state, _| {
                if let Some(p) = weak.upgrade() {
                    // Unverified is what a saved session reads as while the network is
                    // down. Only a real sign-out empties the page, or offline it would
                    // wipe the library that was just filled from disk.
                    let signed_out = matches!(p.ctx.net.client().auth_state(), crate::net::ytmusic::AuthState::Anonymous | crate::net::ytmusic::AuthState::Invalid(_));
                    if state.authenticated() {
                        p.load_library(false);
                    } else if signed_out {
                        p.clear();
                    }
                }
            });
            if state.authenticated() {
                let weak = Rc::downgrade(&page);
                glib::idle_add_local_once(move || {
                    if let Some(p) = weak.upgrade() {
                        p.load_library(false);
                    }
                });
            }
        }
        page
    }

    pub fn widget(&self) -> &gtk::Box {
        &self.root
    }

    pub fn set_compact(&self, compact: bool) {
        self.compact.set(compact);
        if compact {
            self.root.add_css_class("compact");
            self.content_box.set_spacing(16);
            self.uploads_box.set_spacing(16);
        } else {
            self.root.remove_css_class("compact");
            self.content_box.set_spacing(24);
            self.uploads_box.set_spacing(24);
        }
        for section in self.sections.iter().chain(self.upload_sections.iter()) {
            for card in section.cards.borrow().iter() {
                card.set_compact(compact);
            }
        }
        self.apply_layout();
    }

    /// Header refresh button target: a silent reload with the inline spinner.
    pub fn refresh(self: &Rc<Self>, on_done: impl Fn() + 'static) {
        self.on_refresh_done.replace(Some(Rc::new(on_done)));
        self.load_library(true);
    }

    /// Port of _apply_offline_state: grey out list rows that are not cached
    /// for offline use. Playlists and albums with a cached copy stay live,
    /// artists never do.
    pub fn apply_offline_state(self: &Rc<Self>) {
        if self.ctx.online.is_online() {
            for section in &self.sections {
                for row in list_rows(&section.list) {
                    row.set_sensitive(true);
                    row.set_opacity(1.0);
                }
            }
            return;
        }
        let ids: Vec<String> = self.sections[..2].iter().flat_map(|s| (0..s.store.n_items()).filter_map(|i| s.store.item(i).and_downcast::<MediaObject>().map(|o| o.id())).collect::<Vec<_>>()).collect();
        let caches = self.ctx.net.caches().clone();
        let handle = self.ctx.net.spawn(async move {
            tokio::task::spawn_blocking(move || ids.into_iter().filter(|id| caches.disk().has_tracks(id)).collect::<std::collections::HashSet<String>>()).await.unwrap_or_default()
        });
        let weak = Rc::downgrade(self);
        glib::spawn_future_local(async move {
            let cached = handle.await.unwrap_or_default();
            let Some(page) = weak.upgrade() else { return };
            if page.ctx.online.is_online() {
                return;
            }
            for (index, section) in page.sections.iter().enumerate() {
                for (i, row) in list_rows(&section.list).into_iter().enumerate() {
                    let id = section.store.item(i as u32).and_downcast::<MediaObject>().map(|o| o.id()).unwrap_or_default();
                    let available = index < 2 && cached.contains(&id);
                    row.set_sensitive(available);
                    row.set_opacity(if available { 1.0 } else { 0.4 });
                }
            }
        });
    }

    pub fn clear(&self) {
        for section in self.sections.iter().chain(self.upload_sections.iter()) {
            section.store.remove_all();
        }
        self.apply_layout();
    }

    /// Fan the library calls out on the runtime; each section renders as its call returns.
    pub fn load_library(self: &Rc<Self>, silent: bool) {
        if self.is_loading.replace(true) {
            return;
        }
        // Port of the _offline_db fallbacks in get_library_*: the last library
        // that loaded is on disk, so the page is never blank at startup, offline
        // or while the session is still being checked.
        let signed_out = matches!(self.ctx.net.client().auth_state(), crate::net::ytmusic::AuthState::Anonymous);
        if signed_out {
            forget_library(&self.ctx.paths);
        } else if self.sections[0].store.n_items() == 0 {
            self.fill_from_disk();
        }
        if !self.ctx.player.state().authenticated() || !self.ctx.online.is_online() {
            self.is_loading.set(false);
            return;
        }
        if !silent && self.sections[0].store.n_items() == 0 {
            self.loading.set_visible(true);
        }
        let api = self.ctx.net.client().api();
        let net = self.ctx.net.clone();
        let playlists = net.spawn(library::library_playlists(api.clone()));
        let albums = net.spawn(library::library_albums(api.clone()));
        let artists = net.spawn(library::library_subscriptions(api.clone()));
        let up_albums = net.spawn(library::upload_albums(api.clone()));
        let up_artists = net.spawn(library::upload_artists(api));

        let weak = Rc::downgrade(self);
        glib::spawn_future_local(async move {
            let results = [playlists.await, albums.await, artists.await, up_albums.await, up_artists.await];
            let Some(page) = weak.upgrade() else { return };
            let targets = [&page.sections[0], &page.sections[1], &page.sections[2], &page.upload_sections[0], &page.upload_sections[1]];
            let mut saved = read_library(&page.ctx.paths);
            for ((result, section), label) in results.into_iter().zip(targets).zip(LIBRARY_LABELS) {
                match result {
                    Ok(Ok(mut items)) => {
                        saved.insert(label.to_owned(), items.clone());
                        if label == "playlists" {
                            items.insert(items.len().min(1), downloads_entry());
                        }
                        if label == "artists" {
                            page.ctx.net.caches().add_subscriptions(items.iter().map(|i| i.id.clone()));
                        }
                        sync_store(&section.store, items);
                    }
                    Ok(Err(err)) => tracing::warn!(%err, label, "library fetch failed"),
                    Err(_) => {}
                }
            }
            write_library(&page.ctx.paths, saved);
            let has_uploads = page.upload_sections.iter().any(|s| s.store.n_items() > 0);
            page.empty_uploads.set_visible(!has_uploads);
            page.loading.set_visible(false);
            page.uploads_loading.set_visible(false);
            page.is_loading.set(false);
            page.loaded_at.set(Some(std::time::Instant::now()));
            page.apply_layout();
            if let Some(done) = page.on_refresh_done.borrow_mut().take() {
                done();
            }
        });
    }

    /// Show the library as it last loaded.
    fn fill_from_disk(self: &Rc<Self>) {
        let mut saved = read_library(&self.ctx.paths);
        if saved.is_empty() {
            return;
        }
        let targets = [&self.sections[0], &self.sections[1], &self.sections[2], &self.upload_sections[0], &self.upload_sections[1]];
        for (section, label) in targets.into_iter().zip(LIBRARY_LABELS) {
            let mut items = saved.remove(label).unwrap_or_default();
            if label == "playlists" {
                items.insert(items.len().min(1), downloads_entry());
            }
            sync_store(&section.store, items);
        }
        self.empty_uploads.set_visible(!self.upload_sections.iter().any(|s| s.store.n_items() > 0));
        self.apply_layout();
        self.apply_offline_state();
    }

    fn view_mode(&self) -> String {
        self.ctx.paths.read_prefs().get("library_view_mode").and_then(|v| v.as_str()).filter(|m| VIEW_MODES.contains(m)).unwrap_or(DEFAULT_VIEW_MODE).to_owned()
    }

    /// Icon shows the mode the next click switches to, Nautilus style.
    fn sync_view_toggle(&self) {
        if self.view_mode() == "grid" {
            self.view_toggle.set_icon_name("view-list-symbolic");
            self.view_toggle.set_tooltip_text(Some("Switch to list view"));
        } else {
            self.view_toggle.set_icon_name("view-grid-symbolic");
            self.view_toggle.set_tooltip_text(Some("Switch to grid view"));
        }
    }

    /// Port of LibraryPage.filter_content: the global search bar filters the
    /// library's own cards instead of searching YouTube, and a section the
    /// query empties goes with it. Uploads filter too, so whichever sub-tab
    /// is showing reacts.
    pub fn filter_content(&self, text: &str) {
        self.query.replace(text.trim().to_lowercase());
        for section in self.sections.iter().chain(self.upload_sections.iter()) {
            section.filter.changed(gtk::FilterChange::Different);
        }
        self.apply_layout();
    }

    fn apply_layout(&self) {
        let show_grid = self.view_mode() == "grid";
        for section in self.sections.iter().chain(self.upload_sections.iter()) {
            section.list.set_visible(!show_grid);
            section.grid.set_visible(show_grid);
            section.root.set_visible(section.filtered.n_items() > 0);
        }
        self.sync_view_toggle();
    }

    /// The list binds to the store; the grid rebuilds from it on every change.
    /// Port of _on_artist_activated for uploads: a page of that artist's
    /// uploaded songs, filled once the fetch lands.
    fn open_upload_artist(&self, item: &MediaItem) {
        let (browse_id, name) = (item.id.clone(), item.title.clone());
        let _ = self.root.activate_action("win.open-upload-artist", Some(&(browse_id, name).to_variant()));
    }

    /// A card whose picture changed, though its address looks the same.
    ///
    /// Clearing the thumbnail makes the next sync see a difference and rebind
    /// that one row with the fresh address, rather than keeping the picture it
    /// already has.
    pub fn invalidate_card(self: &Rc<Self>, playlist_id: &str) {
        let Some(section) = self.sections.first() else { return };
        for index in 0..section.store.n_items() {
            let Some(object) = section.store.item(index).and_downcast::<MediaObject>() else { continue };
            let item = object.item();
            if item.id != playlist_id {
                continue;
            }
            if let Some(url) = &item.thumb {
                crate::ui::cover::forget_texture(url);
            }
            section.store.splice(index, 1, &[MediaObject::new(MediaItem { thumb: None, ..item })]);
            return;
        }
    }

    /// Demo hook: switch to the uploads tab.
    pub fn show_uploads_for_demo(&self) {
        self.uploads_tab.set_active(true);
    }

    /// Demo hook: what each library playlist card offers in its menu.
    pub fn card_menus_for_demo(&self) {
        let Some(section) = self.sections.first() else { return };
        for index in 0..section.store.n_items().min(10) {
            let Some(object) = section.store.item(index).and_downcast::<MediaObject>() else { continue };
            let item = object.item();
            if item.kind != ItemKind::Playlist || item.id == DOWNLOADS_ID {
                continue;
            }
            let labels: Vec<String> = playlist_extras(&self.ctx, self.root.upcast_ref(), &item).into_iter().map(|action| action.label).collect();
            tracing::info!(title = %item.title, id = %item.id, ?labels, "card menu");
        }
    }

    /// Demo hook: open the new playlist dialog.
    pub fn new_playlist_for_demo(self: &Rc<Self>) {
        self.ask_new_playlist();
    }

    /// Port of on_new_playlist_clicked: title, description and visibility,
    /// then create it and open it.
    fn ask_new_playlist(self: &Rc<Self>) {
        if !self.ctx.net.client().is_authenticated() {
            toast(&self.root, "Sign in to create playlists");
            return;
        }
        let dialog = adw::Dialog::builder().title("New Playlist").content_width(500).build();
        let main_box = gtk::Box::builder().orientation(gtk::Orientation::Vertical).build();
        let header = adw::HeaderBar::builder().css_classes(["flat"]).build();
        let create_btn = gtk::Button::builder().label("Create").css_classes(["suggested-action"]).build();
        header.pack_start(&create_btn);
        main_box.append(&header);

        let prefs_page = adw::PreferencesPage::new();
        let group = adw::PreferencesGroup::builder().title("Playlist Details").margin_start(12).margin_end(12).margin_top(12).margin_bottom(12).build();
        let title_row = adw::EntryRow::builder().title("Title").activates_default(true).build();
        let desc_row = adw::EntryRow::builder().title("Description").build();
        let privacy_row = adw::ComboRow::builder().title("Visibility").model(&gtk::StringList::new(&["Public", "Private", "Unlisted"])).selected(1).build();
        group.add(&title_row);
        group.add(&desc_row);
        group.add(&privacy_row);
        prefs_page.add(&group);
        main_box.append(&prefs_page);
        dialog.set_child(Some(&main_box));

        let page = self.clone();
        let dialog_c = dialog.clone();
        let (title_c, desc_c, privacy_c) = (title_row.clone(), desc_row.clone(), privacy_row.clone());
        create_btn.connect_clicked(move |_| {
            let title = title_c.text().trim().to_owned();
            if title.is_empty() {
                return;
            }
            let description = desc_c.text().trim().to_owned();
            let privacy = ["PUBLIC", "PRIVATE", "UNLISTED"][privacy_c.selected().min(2) as usize];
            page.create_playlist(title, description, privacy);
            dialog_c.close();
        });
        dialog.present(Some(&self.root));
        title_row.grab_focus();
    }

    /// Create it on the runtime, then refresh the library and open the page.
    fn create_playlist(self: &Rc<Self>, title: String, description: String, privacy: &'static str) {
        let api = self.ctx.net.client().api();
        let title_c = title.clone();
        let handle = self.ctx.net.spawn(async move {
            let id = crate::net::playlists::create_playlist(&api, &title_c, &description, privacy).await?;
            // The browse endpoint needs a moment before it will serve it.
            crate::net::playlists::await_playlist(&api, &id).await;
            Ok::<String, crate::net::ytmusic::NetError>(id)
        });
        let weak = Rc::downgrade(self);
        glib::spawn_future_local(async move {
            let Some(page) = weak.upgrade() else { return };
            match handle.await {
                Ok(Ok(playlist_id)) => {
                    tracing::info!(playlist_id, title, "playlist created");
                    page.load_library(true);
                    page.ctx.nav.go(NavRequest::Playlist { id: playlist_id, title, thumb: None });
                }
                Ok(Err(err)) => {
                    tracing::warn!(%err, "playlist creation failed");
                    toast(&page.root, "Could not create the playlist");
                }
                Err(_) => {}
            }
        });
    }

    fn bind_section(self: &Rc<Self>, section: &Rc<Section>) {
        let ctx = self.ctx.clone();
        section.list.bind_model(Some(&section.filtered), move |object| {
            let item = object.downcast_ref::<MediaObject>().map(MediaObject::item).unwrap_or_default();
            let row = list_row(&ctx, &item);
            if item.id != DOWNLOADS_ID {
                attach_playlist_menu(&ctx, &row, item.clone());
            }
            row.upcast()
        });
        let weak_page = Rc::downgrade(self);
        let weak_section = Rc::downgrade(section);
        section.list.connect_row_activated(move |list, row| {
            let (Some(page), Some(section)) = (weak_page.upgrade(), weak_section.upgrade()) else { return };
            let _ = list;
            if let Some(item) = section.filtered.item(row.index().max(0) as u32).and_downcast::<MediaObject>() {
                page.activate(&item.item());
            }
        });
        let weak_page = Rc::downgrade(self);
        let weak_section = Rc::downgrade(section);
        section.filtered.connect_items_changed(move |_, _, _, _| {
            if let (Some(page), Some(section)) = (weak_page.upgrade(), weak_section.upgrade()) {
                page.rebuild_grid(&section);
            }
        });
    }

    fn rebuild_grid(self: &Rc<Self>, section: &Rc<Section>) {
        section.grid.remove_all();
        section.cards.borrow_mut().clear();
        for i in 0..section.filtered.n_items() {
            let Some(item) = section.filtered.item(i).and_downcast::<MediaObject>().map(|o| o.item()) else { continue };
            let (subtitle, fallback_icon, custom_icon) = card_style(&item);
            let card = MediaCard::new(&self.ctx, item.clone(), CardOptions { title_lines: 2, subtitle: Some(subtitle), fallback_icon, custom_icon });
            let weak = Rc::downgrade(self);
            card.connect_clicked(move |item| {
                if let Some(page) = weak.upgrade() {
                    page.activate(item);
                }
            });
            if item.id != DOWNLOADS_ID {
                attach_playlist_menu(&self.ctx, card.widget(), item.clone());
            }
            section.grid.append(card.widget());
            section.cards.borrow_mut().push(card);
        }
        self.apply_layout();
    }

    /// Port of the grid and list activation handlers.
    fn activate(&self, item: &MediaItem) {
        match item.kind {
            ItemKind::Playlist if item.id == DOWNLOADS_ID => {
                let _ = self.root.activate_action("win.open-downloads", None);
            }
            ItemKind::Playlist => self.ctx.nav.go(NavRequest::Playlist { id: item.id.clone(), title: item.title.clone(), thumb: item.thumb.clone() }),
            ItemKind::Album => self.ctx.nav.go(NavRequest::Album { id: item.id.clone(), title: item.title.clone(), thumb: item.thumb.clone() }),
            // An uploaded artist has no artist page, only the songs of theirs
            // that were uploaded.
            ItemKind::Artist if item.id.starts_with("FEmusic_library_privately_owned_artist") => {
                self.open_upload_artist(item);
            }
            ItemKind::Artist => self.ctx.nav.go(NavRequest::Artist { id: Some(item.id.clone()), name: item.title.clone() }),
            _ => {}
        }
    }
}

impl Section {
    fn new(title: &str, action: Option<&gtk::Button>, query: &Rc<RefCell<String>>) -> Rc<Self> {
        let root = gtk::Box::builder().orientation(gtk::Orientation::Vertical).spacing(8).visible(false).build();
        let header = gtk::Box::builder().orientation(gtk::Orientation::Horizontal).spacing(12).height_request(34).build();
        header.append(&gtk::Label::builder().label(title).css_classes(["heading"]).halign(gtk::Align::Start).valign(gtk::Align::Center).hexpand(true).build());
        if let Some(action) = action {
            header.append(action);
        }
        root.append(&header);
        let list = gtk::ListBox::builder().selection_mode(gtk::SelectionMode::None).css_classes(["boxed-list", "songs-list"]).build();
        root.append(&list);
        let grid = CardGrid::new();
        grid.set_valign(gtk::Align::Start);
        grid.set_visible(false);
        root.append(&grid);
        let store = gio::ListStore::new::<MediaObject>();
        let query = query.clone();
        let filter = gtk::CustomFilter::new(move |object| {
            let query = query.borrow();
            query.is_empty() || object.downcast_ref::<MediaObject>().is_some_and(|object| matches_query(&object.item(), &query))
        });
        let filtered = gtk::FilterListModel::new(Some(store.clone()), Some(filter.clone()));
        Rc::new(Self { root, list, grid, store, filtered, filter, cards: Rc::new(RefCell::new(Vec::new())) })
    }
}

/// Port of _apply_library_filter's match: the title, and for an album the
/// artists behind it as well.
fn matches_query(item: &MediaItem, query: &str) -> bool {
    if item.title.to_lowercase().contains(query) {
        return true;
    }
    item.kind == ItemKind::Album && item.artists_text().to_lowercase().contains(query)
}

/// The synthetic Downloads playlist library.py inserts at index one.
/// Section names, in the order load_library fetches them. Also the keys of library_cache.json.
const LIBRARY_LABELS: [&str; 5] = ["playlists", "albums", "artists", "upload albums", "upload artists"];

type SavedLibrary = std::collections::HashMap<String, Vec<MediaItem>>;

fn library_file(paths: &crate::paths::Paths) -> std::path::PathBuf {
    paths.data_dir.join("library_cache.json")
}

fn read_library(paths: &crate::paths::Paths) -> SavedLibrary {
    std::fs::read(library_file(paths)).ok().and_then(|bytes| serde_json::from_slice(&bytes).ok()).unwrap_or_default()
}

fn write_library(paths: &crate::paths::Paths, library: SavedLibrary) {
    let path = library_file(paths);
    // Off the GTK thread: a big library is a few hundred kilobytes of JSON.
    std::thread::spawn(move || match serde_json::to_vec(&library) {
        Ok(bytes) => {
            if let Err(err) = std::fs::write(&path, bytes) {
                tracing::debug!(%err, "library cache not saved");
            }
        }
        Err(err) => tracing::debug!(%err, "library cache not encoded"),
    });
}

/// A signed-out app must not show the last account's library.
fn forget_library(paths: &crate::paths::Paths) {
    let _ = std::fs::remove_file(library_file(paths));
}

fn downloads_entry() -> MediaItem {
    MediaItem { kind: ItemKind::Playlist, id: DOWNLOADS_ID.to_owned(), title: "Downloads".to_owned(), description: Some("Downloaded songs".to_owned()), ..MediaItem::default() }
}

/// Subtitle and icons for a grid card, following _rebuild_*_grid.
fn card_style(item: &MediaItem) -> (String, &'static str, Option<&'static str>) {
    match item.kind {
        ItemKind::Playlist if item.id == DOWNLOADS_ID => (item.description.clone().unwrap_or_default(), "media-playlist-audio-symbolic", Some("folder-download-symbolic")),
        ItemKind::Playlist if item.id.len() == 2 => (item.description.clone().unwrap_or_default(), "folder-music-symbolic", None),
        ItemKind::Playlist => (item.count.as_ref().map(|c| format!("{c} songs")).unwrap_or_default(), "folder-music-symbolic", None),
        ItemKind::Album => (album_subtitle(item), "media-optical-symbolic", None),
        ItemKind::Artist => (artist_subtitle(item), "avatar-default-symbolic", None),
        _ => (item.detail(), "folder-music-symbolic", None),
    }
}

/// "Artist • Album • 2024", as update_albums built it.
fn album_subtitle(item: &MediaItem) -> String {
    let mut parts = Vec::new();
    let artists = item.artists_text();
    if !artists.is_empty() {
        parts.push(artists);
    }
    parts.push(item.item_type.clone().unwrap_or_else(|| "Album".to_owned()));
    if let Some(year) = &item.year {
        parts.push(year.clone());
    }
    parts.join(" • ")
}

/// "12M" or "1,204": a number with at most a magnitude letter, no unit of its own.
fn is_bare_count(text: &str) -> bool {
    let text = text.trim();
    let digits = text.trim_end_matches(['K', 'M', 'B', 'k', 'm', 'b']).trim_end();
    !digits.is_empty() && text.len() - digits.len() <= 2 && digits.chars().all(|c| c.is_ascii_digit() || matches!(c, '.' | ','))
}

fn artist_subtitle(item: &MediaItem) -> String {
    match &item.subscribers {
        // A bare count is subscribers. An uploaded artist says "5 songs", which already names its unit.
        Some(s) if is_bare_count(s) => format!("{s} subscribers"),
        Some(s) => s.clone(),
        None => String::new(),
    }
}

/// List-mode row: thumbnail (or the download icon), title, subtitle.
fn list_row(ctx: &Rc<UiContext>, item: &MediaItem) -> gtk::ListBoxRow {
    let row = gtk::ListBoxRow::builder().activatable(true).build();
    let inner = gtk::Box::builder().orientation(gtk::Orientation::Horizontal).spacing(12).css_classes(["song-row"]).build();
    row.set_child(Some(&inner));

    if item.id == DOWNLOADS_ID {
        let icon_box = gtk::Box::builder().orientation(gtk::Orientation::Vertical).css_classes(["song-download-icon", "song-img"]).vexpand(false).build();
        icon_box.append(&gtk::Image::builder().icon_name("folder-download-symbolic").halign(gtk::Align::Center).valign(gtk::Align::Center).vexpand(true).build());
        inner.append(&icon_box);
    } else {
        let cover = CoverImage::in_context(ctx, 56);
        cover.widget().add_css_class("song-img");
        match &item.thumb {
            Some(url) => cover.load(url),
            None => cover.set_placeholder(match item.kind {
                ItemKind::Album => "media-optical-symbolic",
                ItemKind::Artist => "avatar-default-symbolic",
                _ => "folder-music-symbolic",
            }),
        }
        inner.append(cover.widget());
        unsafe { row.set_data("cover", cover) };
    }

    let subtitle = match item.kind {
        ItemKind::Playlist if item.id.len() == 2 => {
            let mut text = "Automatic Playlist".to_owned();
            if let Some(count) = &item.count {
                text.push_str(&format!(" • {count} songs"));
            }
            text
        }
        ItemKind::Playlist => item.count.as_ref().map(|c| format!("{c} songs")).unwrap_or_default(),
        ItemKind::Album => album_subtitle(item),
        ItemKind::Artist => artist_subtitle(item),
        _ => item.detail(),
    };
    let text = gtk::Box::builder().orientation(gtk::Orientation::Vertical).spacing(2).valign(gtk::Align::Center).hexpand(true).build();
    text.append(&gtk::Label::builder().label(&item.title).halign(gtk::Align::Start).xalign(0.0).ellipsize(gtk::pango::EllipsizeMode::End).lines(1).width_chars(1).build());
    let subtitle_label = gtk::Label::builder().label(&subtitle).halign(gtk::Align::Start).xalign(0.0).ellipsize(gtk::pango::EllipsizeMode::End).lines(1).width_chars(1).css_classes(["dim-label", "caption"]).visible(!subtitle.is_empty()).build();
    text.append(&subtitle_label);
    inner.append(&text);
    row
}

fn overlay_loader(text: &str) -> gtk::Box {
    let wrap = gtk::Box::builder().orientation(gtk::Orientation::Vertical).spacing(12).vexpand(true).valign(gtk::Align::Center).halign(gtk::Align::Center).visible(false).build();
    let spinner = adw::Spinner::new();
    spinner.set_size_request(48, 48);
    wrap.append(&spinner);
    wrap.append(&gtk::Label::builder().label(text).css_classes(["caption"]).build());
    wrap
}

fn list_rows(list: &gtk::ListBox) -> Vec<gtk::ListBoxRow> {
    let mut rows = Vec::new();
    let mut child = list.first_child();
    while let Some(c) = child {
        child = c.next_sibling();
        if let Ok(row) = c.downcast::<gtk::ListBoxRow>() {
            rows.push(row);
        }
    }
    rows
}

/// A library card's menu, with the entries only the library offers: a
/// playlist of your own can be deleted, one you saved can be dropped again.
fn attach_playlist_menu(ctx: &Rc<UiContext>, widget: &impl IsA<gtk::Widget>, item: MediaItem) {
    let is_playlist = item.kind == ItemKind::Playlist && item.id != DOWNLOADS_ID;
    if !is_playlist && !is_upload_album(&item) {
        attach_item_menu(ctx, widget, item);
        return;
    }
    let open = {
        let ctx = ctx.clone();
        let widget = widget.clone().upcast::<gtk::Widget>();
        Rc::new(move |x: f64, y: f64| {
            // Built at each open: signing in or out changes what is offered.
            let extras = playlist_extras(&ctx, &widget, &item);
            crate::ui::context_menu::show_item_menu_with(&widget, x, y, &item, &ctx, extras);
        })
    };
    let right = gtk::GestureClick::builder().button(gtk::gdk::BUTTON_SECONDARY).build();
    let o = open.clone();
    right.connect_released(move |_, _, x, y| o(x, y));
    widget.add_controller(right);
    let long = gtk::GestureLongPress::new();
    long.connect_pressed(move |_, x, y| open(x, y));
    widget.add_controller(long);
}

/// An album that lives in the uploaded library rather than on YouTube Music.
fn is_upload_album(item: &MediaItem) -> bool {
    item.kind == ItemKind::Album && item.id.starts_with("FEmusic_library_privately_owned_release")
}

fn playlist_extras(ctx: &Rc<UiContext>, anchor: &gtk::Widget, item: &MediaItem) -> Vec<MenuAction> {
    if !ctx.net.client().is_authenticated() {
        return Vec::new();
    }
    let (id, title) = (item.id.clone(), item.title.clone());
    if is_upload_album(item) {
        let (ctx, anchor) = (ctx.clone(), anchor.clone());
        return vec![MenuAction::new("Delete Album", context_menu::Section::Remove, move || confirm_delete_upload(&ctx, &anchor, &id, &title))];
    }
    if owns_playlist(account_name(ctx).as_deref(), item) {
        let (ctx, anchor) = (ctx.clone(), anchor.clone());
        return vec![MenuAction::new("Delete Playlist", context_menu::Section::Remove, move || confirm_delete(&ctx, &anchor, &id, &title))];
    }
    let (ctx, anchor) = (ctx.clone(), anchor.clone());
    vec![MenuAction::new("Remove from Library", context_menu::Section::Remove, move || {
        let api = ctx.net.client().api();
        let pid = id.clone();
        let handle = ctx.net.spawn(async move { crate::net::playlists::rate_playlist(&api, &pid, LikeStatus::Indifferent).await });
        let (ctx, anchor) = (ctx.clone(), anchor.clone());
        glib::spawn_future_local(async move {
            match handle.await {
                Ok(Ok(())) => {
                    ctx.net.caches().clear_library_ids();
                    toast(&anchor, "Removed from library");
                    ctx.nav.refresh_library();
                }
                Ok(Err(err)) => {
                    tracing::warn!(%err, "remove from library failed");
                    toast(&anchor, "Failed to remove");
                }
                Err(_) => {}
            }
        });
    })]
}

/// Port of is_own_playlist read from a library card: the system lists are
/// never owned, and otherwise the card's author is the account holder.
fn owns_playlist(account: Option<&str>, item: &MediaItem) -> bool {
    let Some(account) = account.filter(|name| !name.is_empty()) else { return false };
    if ["LM", "SE", "SS", "VLLM"].contains(&item.id.as_str()) {
        return false;
    }
    if !(item.id.starts_with("PL") || item.id.starts_with("VL")) {
        return false;
    }
    match item.artists.first() {
        Some(author) => author.name == account,
        None => true,
    }
}

/// The signed in account's name, which is what a card's author is compared to.
fn account_name(ctx: &Rc<UiContext>) -> Option<String> {
    match ctx.net.client().auth_state() {
        crate::net::ytmusic::AuthState::Authenticated(info) => Some(info.name),
        _ => None,
    }
}

/// Port of _confirm_delete_upload: an uploaded album and its songs, gone.
fn confirm_delete_upload(ctx: &Rc<UiContext>, anchor: &gtk::Widget, entity_id: &str, title: &str) {
    let dialog = adw::AlertDialog::builder()
        .heading("Delete Upload?")
        .body(format!("Are you sure you want to delete \"{title}\"?\nThis cannot be undone."))
        .build();
    dialog.add_response("cancel", "Cancel");
    dialog.add_response("delete", "Delete");
    dialog.set_response_appearance("delete", adw::ResponseAppearance::Destructive);
    dialog.set_default_response(Some("cancel"));
    dialog.set_close_response("cancel");
    let parent = anchor.clone();
    let (ctx, anchor, id) = (ctx.clone(), anchor.clone(), entity_id.to_owned());
    dialog.connect_response(None, move |_, response| {
        if response != "delete" {
            return;
        }
        let api = ctx.net.client().api();
        let entity = id.clone();
        let handle = ctx.net.spawn(async move { crate::net::uploads::delete_entity(&api, &entity).await });
        let (ctx, anchor) = (ctx.clone(), anchor.clone());
        glib::spawn_future_local(async move {
            match handle.await {
                Ok(Ok(())) => {
                    toast(&anchor, "Upload deleted");
                    ctx.nav.refresh_library();
                }
                Ok(Err(err)) => {
                    tracing::warn!(%err, "delete upload failed");
                    toast(&anchor, "Failed to delete the upload");
                }
                Err(_) => {}
            }
        });
    });
    dialog.present(Some(&parent));
}

/// Port of _confirm_delete_playlist, the same wording the playlist page uses.
fn confirm_delete(ctx: &Rc<UiContext>, anchor: &gtk::Widget, playlist_id: &str, title: &str) {
    let dialog = adw::AlertDialog::builder()
        .heading("Delete Playlist?")
        .body(format!("Are you sure you want to delete \"{title}\"?\nThis action cannot be undone."))
        .build();
    dialog.add_response("cancel", "Cancel");
    dialog.add_response("delete", "Delete");
    dialog.set_response_appearance("delete", adw::ResponseAppearance::Destructive);
    dialog.set_default_response(Some("cancel"));
    dialog.set_close_response("cancel");
    let parent = anchor.clone();
    let (ctx, anchor, id) = (ctx.clone(), anchor.clone(), playlist_id.to_owned());
    dialog.connect_response(None, move |_, response| {
        if response != "delete" {
            return;
        }
        let api = ctx.net.client().api();
        let pid = id.clone();
        let handle = ctx.net.spawn(async move { crate::net::playlists::delete_playlist(&api, &pid).await });
        let (ctx, anchor) = (ctx.clone(), anchor.clone());
        glib::spawn_future_local(async move {
            match handle.await {
                Ok(Ok(())) => {
                    toast(&anchor, "Playlist deleted");
                    ctx.nav.refresh_library();
                }
                Ok(Err(err)) => {
                    tracing::warn!(%err, "delete playlist failed");
                    toast(&anchor, "Failed to delete the playlist");
                }
                Err(_) => {}
            }
        });
    });
    dialog.present(Some(&parent));
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::model::Person;

    #[test]
    fn the_search_matches_a_title_anywhere_in_it() {
        let playlist = MediaItem { kind: ItemKind::Playlist, title: "Miku Favorites".into(), ..MediaItem::default() };
        assert!(matches_query(&playlist, "miku"));
        assert!(matches_query(&playlist, "favorites"), "not just the start");
        assert!(!matches_query(&playlist, "luka"));
    }

    #[test]
    fn an_album_also_matches_the_artist_behind_it() {
        let album = MediaItem {
            kind: ItemKind::Album,
            title: "Draining Love Story".into(),
            artists: vec![Person { name: "Sewerslvt".into(), id: None }],
            ..MediaItem::default()
        };
        assert!(matches_query(&album, "sewerslvt"));

        // A playlist by the same author does not: only albums carry artists
        // into the match, which is the rule _apply_library_filter used.
        let playlist = MediaItem { kind: ItemKind::Playlist, artists: album.artists.clone(), ..MediaItem::default() };
        assert!(!matches_query(&playlist, "sewerslvt"));
    }

    fn card(id: &str, author: Option<&str>) -> MediaItem {
        MediaItem {
            kind: ItemKind::Playlist,
            id: id.to_owned(),
            title: "A playlist".to_owned(),
            artists: author.into_iter().map(|name| Person { name: name.to_owned(), id: None }).collect(),
            ..MediaItem::default()
        }
    }

    #[test]
    fn a_card_in_your_own_name_is_yours() {
        assert!(owns_playlist(Some("Mohamad Obeid"), &card("PLl4feRsMPyje", Some("Mohamad Obeid"))));
        assert!(owns_playlist(Some("Mohamad Obeid"), &card("PLnoauthor", None)), "your own lists often show no author");
        assert!(!owns_playlist(Some("Mohamad Obeid"), &card("PLsaved", Some("Someone Else"))));
    }

    #[test]
    fn the_lists_youtube_keeps_are_never_yours() {
        for id in ["LM", "SE", "SS", "VLLM"] {
            assert!(!owns_playlist(Some("Mohamad Obeid"), &card(id, Some("Mohamad Obeid"))), "{id}");
        }
        assert!(!owns_playlist(Some("Mohamad Obeid"), &card("MPREb_abc", Some("Mohamad Obeid"))), "an album is not a playlist of yours");
    }

    #[test]
    fn signed_out_nothing_is_yours() {
        assert!(!owns_playlist(None, &card("PLl4feRsMPyje", Some("Mohamad Obeid"))));
        assert!(!owns_playlist(Some(""), &card("PLl4feRsMPyje", None)));
    }
}

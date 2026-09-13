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
use crate::ui::pages::attach_item_menu;
use crate::ui::toast;
use crate::ui::widgets::card_grid::CardGrid;
use crate::ui::widgets::media_card::{CardOptions, MediaCard};

const VIEW_MODES: [&str; 2] = ["list", "grid"];
const DEFAULT_VIEW_MODE: &str = "grid";
const DOWNLOADS_ID: &str = "DL";

struct Section {
    root: gtk::Box,
    list: gtk::ListBox,
    grid: CardGrid,
    store: gio::ListStore,
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
    ctx: Rc<UiContext>,
    is_loading: Cell<bool>,
    compact: Rc<Cell<bool>>,
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
        tab_row.append(&uploads_actions);
        content_box.append(&tab_row);

        let lib_stack = gtk::Stack::builder().transition_type(gtk::StackTransitionType::SlideLeftRight).vexpand(true).build();
        content_box.append(&lib_stack);

        let lib_content = gtk::Box::builder().orientation(gtk::Orientation::Vertical).spacing(24).build();
        let new_playlist = gtk::Button::builder().icon_name("list-add-symbolic").css_classes(["flat", "circular"]).valign(gtk::Align::Center).tooltip_text("New Playlist").build();
        let playlists = Section::new("Playlists", Some(&new_playlist));
        let albums = Section::new("Albums", None);
        let artists = Section::new("Artists", None);
        for s in [&playlists, &albums, &artists] {
            lib_content.append(&s.root);
        }
        lib_stack.add_titled(&lib_content, Some("library"), "Library");

        let uploads_box = gtk::Box::builder().orientation(gtk::Orientation::Vertical).spacing(24).vexpand(true).build();
        let up_albums = Section::new("Albums", None);
        let up_artists = Section::new("Artists", None);
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
            ctx,
            is_loading: Cell::new(false),
            compact: Rc::new(Cell::new(false)),
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
            let root = page.root.clone();
            new_playlist.connect_clicked(move |_| toast(&root, "New playlist dialog not ported yet"));
            let root = page.root.clone();
            all_songs.connect_clicked(move |_| toast(&root, "Uploaded songs page not ported yet"));
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
                    if state.authenticated() {
                        p.load_library(false);
                    } else {
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
        if !self.ctx.player.state().authenticated() {
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
            let labels = ["playlists", "albums", "artists", "upload albums", "upload artists"];
            for ((result, section), label) in results.into_iter().zip(targets).zip(labels) {
                match result {
                    Ok(Ok(mut items)) => {
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
            let has_uploads = page.upload_sections.iter().any(|s| s.store.n_items() > 0);
            page.empty_uploads.set_visible(!has_uploads);
            page.loading.set_visible(false);
            page.uploads_loading.set_visible(false);
            page.is_loading.set(false);
            page.apply_layout();
            if let Some(done) = page.on_refresh_done.borrow_mut().take() {
                done();
            }
        });
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

    fn apply_layout(&self) {
        let show_grid = self.view_mode() == "grid";
        for section in self.sections.iter().chain(self.upload_sections.iter()) {
            section.list.set_visible(!show_grid);
            section.grid.set_visible(show_grid);
            section.root.set_visible(section.store.n_items() > 0);
        }
        self.sync_view_toggle();
    }

    /// The list binds to the store; the grid rebuilds from it on every change.
    fn bind_section(self: &Rc<Self>, section: &Rc<Section>) {
        let ctx = self.ctx.clone();
        section.list.bind_model(Some(&section.store), move |object| {
            let item = object.downcast_ref::<MediaObject>().map(MediaObject::item).unwrap_or_default();
            let row = list_row(&ctx, &item);
            if item.id != DOWNLOADS_ID {
                attach_item_menu(&ctx, &row, item.clone());
            }
            row.upcast()
        });
        let weak_page = Rc::downgrade(self);
        let weak_section = Rc::downgrade(section);
        section.list.connect_row_activated(move |list, row| {
            let (Some(page), Some(section)) = (weak_page.upgrade(), weak_section.upgrade()) else { return };
            let _ = list;
            if let Some(item) = section.store.item(row.index().max(0) as u32).and_downcast::<MediaObject>() {
                page.activate(&item.item());
            }
        });
        let weak_page = Rc::downgrade(self);
        let weak_section = Rc::downgrade(section);
        section.store.connect_items_changed(move |_, _, _, _| {
            if let (Some(page), Some(section)) = (weak_page.upgrade(), weak_section.upgrade()) {
                page.rebuild_grid(&section);
            }
        });
    }

    fn rebuild_grid(self: &Rc<Self>, section: &Rc<Section>) {
        section.grid.remove_all();
        section.cards.borrow_mut().clear();
        for i in 0..section.store.n_items() {
            let Some(item) = section.store.item(i).and_downcast::<MediaObject>().map(|o| o.item()) else { continue };
            let (subtitle, fallback_icon, custom_icon) = card_style(&item);
            let card = MediaCard::new(&self.ctx, item.clone(), CardOptions { title_lines: 2, subtitle: Some(subtitle), fallback_icon, custom_icon });
            let weak = Rc::downgrade(self);
            card.connect_clicked(move |item| {
                if let Some(page) = weak.upgrade() {
                    page.activate(item);
                }
            });
            if item.id != DOWNLOADS_ID {
                attach_item_menu(&self.ctx, card.widget(), item.clone());
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
            ItemKind::Artist => self.ctx.nav.go(NavRequest::Artist { id: Some(item.id.clone()), name: item.title.clone() }),
            _ => {}
        }
    }
}

impl Section {
    fn new(title: &str, action: Option<&gtk::Button>) -> Rc<Self> {
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
        Rc::new(Self { root, list, grid, store: gio::ListStore::new::<MediaObject>(), cards: Rc::new(RefCell::new(Vec::new())) })
    }
}

/// The synthetic Downloads playlist library.py inserts at index one.
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

fn artist_subtitle(item: &MediaItem) -> String {
    match &item.subscribers {
        Some(s) if !s.to_lowercase().contains("subscribers") => format!("{s} subscribers"),
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

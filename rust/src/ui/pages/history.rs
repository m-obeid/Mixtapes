//! Port of ui/pages/history.py: the plays YouTube has recorded, grouped under
//! the headings it filed them under. A row plays from there through the rest
//! of the history, and its menu can forget it.

use std::cell::{Cell, RefCell};
use std::rc::Rc;

use gtk::{glib, prelude::*};

use crate::model::{ItemKind, MediaItem, Track};
use crate::net::history::{self, HistoryEntry};
use crate::ui::context::UiContext;
use crate::ui::context_menu::{MenuAction, Section};
use crate::ui::widgets::song_row::SongRow;

/// How far the page scrolls before the header takes over the title.
const TITLE_HANDOVER: f64 = 50.0;
const EMPTY_TEXT: &str = "Your listening history will appear here after you play something.";
/// The queue a history row plays is the history itself, not a playlist.
const QUEUE_SOURCE: &str = "HISTORY";

type TitleListener = Box<dyn Fn(&str)>;

pub struct HistoryPage {
    root: gtk::Box,
    content_box: gtk::Box,
    sections_box: gtk::Box,
    empty_label: gtk::Label,
    loading_wrap: gtk::Box,
    ctx: Rc<UiContext>,
    /// Every play in one flat list, which is what a row queues.
    entries: RefCell<Vec<HistoryEntry>>,
    rows: RefCell<Vec<Rc<SongRow>>>,
    loading: Cell<bool>,
    on_title: RefCell<Option<TitleListener>>,
}

impl HistoryPage {
    pub fn new(ctx: Rc<UiContext>) -> Rc<Self> {
        let root = gtk::Box::builder().orientation(gtk::Orientation::Vertical).build();
        let scrolled = gtk::ScrolledWindow::builder().hscrollbar_policy(gtk::PolicyType::Never).vscrollbar_policy(gtk::PolicyType::Automatic).vexpand(true).build();
        crate::ui::suppress_hover_while_scrolling(&scrolled);

        let content_box = gtk::Box::builder().orientation(gtk::Orientation::Vertical).spacing(24).margin_top(24).margin_bottom(24).margin_start(24).margin_end(24).build();
        content_box.append(&gtk::Label::builder().label("Listening History").css_classes(["title-1"]).halign(gtk::Align::Start).build());

        let sections_box = gtk::Box::builder().orientation(gtk::Orientation::Vertical).spacing(16).build();
        content_box.append(&sections_box);

        let empty_label = gtk::Label::builder().label(EMPTY_TEXT).css_classes(["dim-label"]).wrap(true).halign(gtk::Align::Center).margin_top(48).visible(false).build();
        content_box.append(&empty_label);

        let clamp = adw::Clamp::builder().maximum_size(1024).tightening_threshold(600).child(&content_box).build();
        scrolled.set_child(Some(&clamp));

        // Overlaid rather than packed: the same centring the library page uses,
        // which the clamp inside a scrolled window would otherwise fight.
        let loading_wrap = gtk::Box::builder().orientation(gtk::Orientation::Vertical).spacing(12).valign(gtk::Align::Center).halign(gtk::Align::Center).build();
        let spinner = adw::Spinner::new();
        spinner.set_size_request(48, 48);
        loading_wrap.append(&spinner);
        loading_wrap.append(&gtk::Label::builder().label("Loading history...").css_classes(["caption"]).build());
        let overlay = gtk::Overlay::builder().vexpand(true).child(&scrolled).build();
        overlay.add_overlay(&loading_wrap);
        root.append(&overlay);

        let page = Rc::new(Self {
            root,
            content_box,
            sections_box,
            empty_label,
            loading_wrap,
            ctx,
            entries: RefCell::new(Vec::new()),
            rows: RefCell::new(Vec::new()),
            loading: Cell::new(false),
            on_title: RefCell::new(None),
        });

        let weak = Rc::downgrade(&page);
        scrolled.vadjustment().connect_value_changed(move |adj| {
            if let Some(p) = weak.upgrade() {
                p.emit_title(if adj.value() > TITLE_HANDOVER { "Listening History" } else { "" });
            }
        });
        let weak = Rc::downgrade(&page);
        page.ctx.on_compact(move |compact| match weak.upgrade() {
            Some(p) => {
                p.set_compact_mode(compact);
                true
            }
            None => false,
        });
        page
    }

    pub fn widget(&self) -> &gtk::Box {
        &self.root
    }

    /// The window sets its title from this, like the header-title-changed signal.
    pub fn set_on_header_title(&self, f: impl Fn(&str) + 'static) {
        self.on_title.replace(Some(Box::new(f)));
    }

    fn emit_title(&self, title: &str) {
        if let Some(f) = self.on_title.borrow().as_ref() {
            f(title);
        }
    }

    pub fn set_compact_mode(&self, compact: bool) {
        if compact {
            self.root.add_css_class("compact");
            self.content_box.set_margin_start(12);
            self.content_box.set_margin_end(12);
        } else {
            self.root.remove_css_class("compact");
            self.content_box.set_margin_start(24);
            self.content_box.set_margin_end(24);
        }
    }

    /// Port of load: the cached plays go up straight away, then the fetch
    /// replaces them. Without a cache the page waits on the spinner.
    pub fn load(self: &Rc<Self>) {
        if !self.ctx.player.state().authenticated() {
            self.show_empty("Sign in to view listening history.");
            return;
        }
        let cached = history::cached_history(self.ctx.downloads.store());
        if !cached.is_empty() {
            self.render(cached);
        }
        self.refresh();
    }

    /// Whether a fetch is in flight, which is what the header's refresh
    /// button waits on before putting its icon back.
    pub fn is_loading(&self) -> bool {
        self.loading.get()
    }

    /// Port of refresh_from_server.
    pub fn refresh(self: &Rc<Self>) {
        if !self.ctx.player.state().authenticated() || self.loading.get() {
            return;
        }
        if !self.ctx.online.is_online() {
            if self.entries.borrow().is_empty() {
                self.show_empty("History requires an internet connection.");
            }
            return;
        }
        self.loading.set(true);
        let api = self.ctx.net.client().api();
        let handle = self.ctx.net.spawn(history::get_history(api));
        let weak = Rc::downgrade(self);
        glib::spawn_future_local(async move {
            let outcome = handle.await;
            let Some(page) = weak.upgrade() else { return };
            page.loading.set(false);
            match outcome {
                Ok(Ok(entries)) => {
                    history::cache_history(page.ctx.downloads.store(), &entries);
                    page.render(entries);
                }
                Ok(Err(err)) => {
                    tracing::warn!(%err, "history fetch failed");
                    if page.entries.borrow().is_empty() {
                        page.show_empty(EMPTY_TEXT);
                    }
                }
                Err(_) => {}
            }
        });
    }

    fn render(self: &Rc<Self>, entries: Vec<HistoryEntry>) {
        self.loading_wrap.set_visible(false);
        clear_children(&self.sections_box);
        self.rows.borrow_mut().clear();
        if entries.is_empty() {
            self.entries.borrow_mut().clear();
            self.show_empty(EMPTY_TEXT);
            return;
        }
        self.empty_label.set_visible(false);

        // One section per heading, in the order YouTube sent them.
        let mut sections: Vec<(String, Vec<usize>)> = Vec::new();
        for (index, entry) in entries.iter().enumerate() {
            match sections.last_mut() {
                Some((title, rows)) if *title == entry.played => rows.push(index),
                _ => sections.push((entry.played.clone(), vec![index])),
            }
        }
        for (title, indexes) in sections {
            self.sections_box.append(&self.build_section(&title, &indexes, &entries));
        }
        self.entries.replace(entries);
    }

    fn build_section(self: &Rc<Self>, title: &str, indexes: &[usize], entries: &[HistoryEntry]) -> gtk::Box {
        let section = gtk::Box::builder().orientation(gtk::Orientation::Vertical).spacing(8).build();
        section.append(&gtk::Label::builder().label(title).css_classes(["title-3"]).halign(gtk::Align::Start).build());
        let list = gtk::ListBox::builder().css_classes(["boxed-list", "songs-list"]).selection_mode(gtk::SelectionMode::None).build();

        for &index in indexes {
            let entry = &entries[index];
            let item = as_item(&entry.track);
            let row = SongRow::new(self.ctx.clone());
            row.set_plain_subtitle(Some(subtitle(&entry.track)));
            row.bind(&item, None);
            let weak = Rc::downgrade(self);
            row.set_on_activate(move |_| {
                if let Some(page) = weak.upgrade() {
                    page.play_from(index);
                }
            });
            let weak = Rc::downgrade(self);
            row.set_menu_extras(move || weak.upgrade().map(|page| page.row_menu_extras(index)).unwrap_or_default());
            list.append(row.widget());
            self.rows.borrow_mut().push(row);
        }

        let weak = Rc::downgrade(self);
        let indexes = indexes.to_vec();
        list.connect_row_activated(move |_, row| {
            if let (Some(page), Some(index)) = (weak.upgrade(), indexes.get(row.index().max(0) as usize)) {
                page.play_from(*index);
            }
        });
        section.append(&list);
        section
    }

    /// Port of _on_row_activated: play from here through the rest of the
    /// history, the way a click inside a flat playlist does.
    fn play_from(&self, index: usize) {
        let tracks: Vec<Track> = self.entries.borrow().iter().map(|entry| entry.track.clone()).collect();
        if index >= tracks.len() {
            return;
        }
        self.ctx.player.play_tracks(tracks, index, false, Some(QUEUE_SOURCE.to_owned()), false);
    }

    /// The two entries history.py adds to a row's song menu.
    fn row_menu_extras(self: &Rc<Self>, index: usize) -> Vec<MenuAction> {
        let token = self.entries.borrow().get(index).and_then(|entry| entry.feedback_token.clone());
        let video_id = self.entries.borrow().get(index).map(|entry| entry.track.video_id.0.clone()).unwrap_or_default();

        let weak = Rc::downgrade(self);
        let mut extras = vec![MenuAction { first: true, ..MenuAction::new("Play", Section::Queue, move || {
            if let Some(page) = weak.upgrade() {
                page.play_from(index);
            }
        })}];
        // No token means a brand account, where YouTube offers no removal.
        if let Some(token) = token {
            let weak = Rc::downgrade(self);
            extras.push(MenuAction::new("Remove from History", Section::Remove, move || {
                if let Some(page) = weak.upgrade() {
                    page.remove_play(&video_id, &token);
                }
            }));
        }
        extras
    }

    /// Demo hook: what the first row's menu adds on top of the song menu.
    pub fn menu_extras_for_demo(self: &Rc<Self>) -> Vec<String> {
        if self.entries.borrow().is_empty() {
            return Vec::new();
        }
        self.row_menu_extras(0).into_iter().map(|action| action.label).collect()
    }

    /// Port of _remove_track_optimistic: the row goes now, the cache is
    /// patched now, and the account is told afterwards.
    fn remove_play(self: &Rc<Self>, video_id: &str, token: &str) {
        self.entries.borrow_mut().retain(|entry| entry.track.video_id.0 != video_id);
        history::forget_cached(self.ctx.downloads.store(), video_id);
        let remaining = self.entries.borrow().clone();
        self.render(remaining);

        let api = self.ctx.net.client().api();
        let token = token.to_owned();
        self.ctx.net.spawn(async move {
            if let Err(err) = history::remove_history_items(&api, vec![token]).await {
                tracing::warn!(%err, "removing a play failed");
            }
        });
    }

    fn show_empty(&self, message: &str) {
        self.loading_wrap.set_visible(false);
        clear_children(&self.sections_box);
        self.rows.borrow_mut().clear();
        self.empty_label.set_label(message);
        self.empty_label.set_visible(true);
    }
}

/// Port of the row's subtitle: the artists, then the album behind a bullet.
fn subtitle(track: &Track) -> String {
    let album = track.album.as_ref().map(|a| a.name.as_str()).unwrap_or_default();
    match (track.artist.is_empty(), album.is_empty()) {
        (_, true) => track.artist.clone(),
        (true, false) => album.to_owned(),
        (false, false) => format!("{} \u{2022} {album}", track.artist),
    }
}

/// A history row as the boxed-list row wants it.
fn as_item(track: &Track) -> MediaItem {
    MediaItem {
        kind: if track.video_type.as_deref().is_some_and(|t| t != "MUSIC_VIDEO_TYPE_ATV") { ItemKind::Video } else { ItemKind::Song },
        id: track.video_id.0.clone(),
        title: track.title.clone(),
        artists: track.artists.clone(),
        album: track.album.clone(),
        thumb: track.thumb.clone(),
        duration_seconds: track.duration_seconds,
        explicit: track.is_explicit,
        like_status: Some(track.like_status),
        ..MediaItem::default()
    }
}

fn clear_children(container: &gtk::Box) {
    while let Some(child) = container.first_child() {
        container.remove(&child);
    }
}

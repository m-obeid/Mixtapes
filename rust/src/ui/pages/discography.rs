//! Port of ui/pages/discography.py: an artist's albums, singles or songs as
//! a justified card grid that loads more as the user scrolls to the bottom.

use std::cell::{Cell, RefCell};
use std::rc::Rc;

use gtk::{glib, prelude::*};

use crate::model::{ItemKind, MediaItem};
use crate::net::playlists;
use crate::ui::context::{NavRequest, UiContext};
use crate::ui::context_menu::{MenuAction, Section, show_item_menu_with};
use crate::ui::copy_to_clipboard;
use crate::ui::widgets::card_grid::{CardGrid, CardList};
use crate::ui::widgets::media_card::{CardOptions, MediaCard};

type TitleListener = Box<dyn Fn(&str)>;

pub struct DiscographyPage {
    root: gtk::Box,
    content_box: gtk::Box,
    grid: CardGrid,
    loading_wrap: gtk::Box,
    cards: CardList,
    compact: Rc<Cell<bool>>,
    items: RefCell<Vec<MediaItem>>,
    is_loading: Cell<bool>,
    has_more: Cell<bool>,
    channel_id: RefCell<String>,
    title: RefCell<String>,
    browse_id: RefCell<Option<String>>,
    params: RefCell<Option<String>>,
    on_title: RefCell<Option<TitleListener>>,
    ctx: Rc<UiContext>,
}

impl DiscographyPage {
    pub fn new(ctx: Rc<UiContext>) -> Rc<Self> {
        let root = gtk::Box::builder().orientation(gtk::Orientation::Vertical).build();
        let scrolled = gtk::ScrolledWindow::builder().hscrollbar_policy(gtk::PolicyType::Never).vscrollbar_policy(gtk::PolicyType::Automatic).vexpand(true).build();
        crate::ui::suppress_hover_while_scrolling(&scrolled);
        let content_box = gtk::Box::builder().orientation(gtk::Orientation::Vertical).spacing(16).margin_top(24).margin_bottom(24).margin_start(24).margin_end(24).build();
        let grid = CardGrid::new();
        grid.set_valign(gtk::Align::Start);
        content_box.append(&grid);
        let loading_wrap = gtk::Box::builder().orientation(gtk::Orientation::Vertical).vexpand(true).valign(gtk::Align::Center).halign(gtk::Align::Center).margin_top(32).margin_bottom(32).visible(false).build();
        let spinner = adw::Spinner::builder().halign(gtk::Align::Center).build();
        spinner.set_size_request(48, 48);
        loading_wrap.append(&spinner);
        content_box.append(&loading_wrap);
        let clamp = adw::Clamp::builder().maximum_size(1024).tightening_threshold(600).child(&content_box).build();
        scrolled.set_child(Some(&clamp));
        root.append(&scrolled);

        let page = Rc::new(Self {
            root,
            content_box,
            grid,
            loading_wrap,
            cards: Rc::new(RefCell::new(Vec::new())),
            compact: Rc::new(Cell::new(false)),
            items: RefCell::new(Vec::new()),
            is_loading: Cell::new(false),
            has_more: Cell::new(true),
            channel_id: RefCell::new(String::new()),
            title: RefCell::new(String::new()),
            browse_id: RefCell::new(None),
            params: RefCell::new(None),
            on_title: RefCell::new(None),
            ctx,
        });
        page.grid.set_cards(&page.cards, page.compact.clone());
        let weak = Rc::downgrade(&page);
        scrolled.vadjustment().connect_value_changed(move |adj| {
            let Some(p) = weak.upgrade() else { return };
            if p.is_loading.get() || !p.has_more.get() {
                return;
            }
            if adj.upper() - (adj.value() + adj.page_size()) < 200.0 {
                p.load_more();
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
        self.compact.set(compact);
        if compact {
            self.root.add_css_class("compact");
            self.content_box.set_spacing(12);
            self.content_box.set_margin_start(12);
            self.content_box.set_margin_end(12);
        } else {
            self.root.remove_css_class("compact");
            self.content_box.set_spacing(16);
            self.content_box.set_margin_start(24);
            self.content_box.set_margin_end(24);
        }
        for card in self.cards.borrow().iter() {
            card.set_compact(compact);
        }
    }

    pub fn load_discography(self: &Rc<Self>, channel_id: &str, title: &str, browse_id: Option<&str>, params: Option<&str>, initial_items: Vec<MediaItem>) {
        self.channel_id.replace(channel_id.to_owned());
        self.title.replace(title.to_owned());
        self.browse_id.replace(browse_id.map(str::to_owned));
        self.params.replace(params.map(str::to_owned));
        self.items.borrow_mut().clear();
        self.grid.remove_all();
        self.cards.borrow_mut().clear();
        self.has_more.set(true);
        if !initial_items.is_empty() {
            self.items.borrow_mut().extend(initial_items.iter().cloned());
            self.render_items(&initial_items);
        }
        self.emit_title(title);
        self.load_more();
    }

    /// Hide cards whose title does not contain the query.
    pub fn filter_content(&self, text: &str) {
        let query = text.trim().to_lowercase();
        for card in self.cards.borrow().iter() {
            let title = card.item().title.to_lowercase();
            card.widget().set_visible(query.is_empty() || title.contains(&query));
        }
    }

    fn load_more(self: &Rc<Self>) {
        if self.is_loading.get() || !self.has_more.get() {
            return;
        }
        self.is_loading.set(true);
        self.loading_wrap.set_visible(true);
        let api = self.ctx.net.client().api();
        let browse_id = self.browse_id.borrow().clone();
        let params = self.params.borrow().clone();
        let title = self.title.borrow().clone();
        let handle = self.ctx.net.spawn(async move {
            let mut has_more = true;
            let items: Vec<MediaItem> = match (browse_id, params) {
                (Some(_), _) if title.contains("Top Songs") => Vec::new(),
                (Some(b), Some(p)) => {
                    has_more = false;
                    playlists::artist_albums(&api, &b, Some(&p), None).await.unwrap_or_default()
                }
                (Some(b), None) => {
                    has_more = false;
                    match playlists::get_playlist(&api, &b, None).await {
                        Ok(pl) => pl.tracks.iter().map(|t| MediaItem { kind: ItemKind::Song, id: t.video_id.0.clone(), title: t.title.clone(), artists: t.artists.clone(), album: t.album.clone(), thumb: t.thumb.clone(), duration_seconds: t.duration_seconds, explicit: t.is_explicit, ..MediaItem::default() }).collect(),
                        Err(_) => playlists::raw_parse_channel_content(&api, &b, None).await.unwrap_or_default(),
                    }
                }
                _ => Vec::new(),
            };
            (items, has_more)
        });
        let weak = Rc::downgrade(self);
        glib::spawn_future_local(async move {
            let Some(page) = weak.upgrade() else { return };
            match handle.await {
                Ok((new_items, has_more)) => {
                    if !new_items.is_empty() {
                        let existing: std::collections::HashSet<String> = page.items.borrow().iter().map(|i| i.id.clone()).collect();
                        let filtered: Vec<MediaItem> = new_items.into_iter().filter(|i| !existing.contains(&i.id)).collect();
                        page.items.borrow_mut().extend(filtered.iter().cloned());
                        page.render_items(&filtered);
                    } else {
                        page.has_more.set(false);
                    }
                    if !has_more {
                        page.has_more.set(false);
                    }
                }
                Err(_) => tracing::warn!("discography load failed"),
            }
            page.is_loading.set(false);
            page.loading_wrap.set_visible(false);
        });
    }

    fn render_items(self: &Rc<Self>, items: &[MediaItem]) {
        for item in items {
            let card = MediaCard::new(&self.ctx, item.clone(), CardOptions { title_lines: 1, ..CardOptions::default() });
            let weak = Rc::downgrade(self);
            card.connect_clicked(move |item| {
                if let Some(p) = weak.upgrade() {
                    p.activate(item);
                }
            });
            let weak = Rc::downgrade(self);
            let widget = card.widget().clone();
            let item_c = item.clone();
            let open = Rc::new(move |x: f64, y: f64| {
                if let Some(p) = weak.upgrade() {
                    p.on_grid_right_click(&widget, x, y, &item_c);
                }
            });
            let right = gtk::GestureClick::builder().button(gtk::gdk::BUTTON_SECONDARY).build();
            let o = open.clone();
            right.connect_pressed(move |_, _, x, y| o(x, y));
            card.widget().add_controller(right);
            let long = gtk::GestureLongPress::new();
            long.connect_pressed(move |_, x, y| open(x, y));
            card.widget().add_controller(long);
            self.grid.append(card.widget());
            self.cards.borrow_mut().push(card);
        }
    }

    /// Port of _activate_item_data: browse ids open a page, videos play.
    fn activate(&self, item: &MediaItem) {
        match item.kind {
            ItemKind::Song | ItemKind::Video => {
                if let Some(track) = item.to_track() {
                    self.ctx.player.play_tracks(vec![track], 0, false, None, false);
                }
            }
            ItemKind::Album => self.ctx.nav.go(NavRequest::Album { id: item.id.clone(), title: item.title.clone(), thumb: item.thumb.clone() }),
            ItemKind::Artist => self.ctx.nav.go(NavRequest::Artist { id: Some(item.id.clone()), name: item.title.clone() }),
            ItemKind::Playlist => self.ctx.nav.go(NavRequest::Playlist { id: item.id.clone(), title: item.title.clone(), thumb: item.thumb.clone() }),
        }
    }

    fn on_grid_right_click(&self, widget: &gtk::Button, x: f64, y: f64, item: &MediaItem) {
        let data = item.clone();
        let extras = vec![MenuAction::new("Copy JSON (Debug)", Section::Clipboard, move || {
            if let Ok(text) = serde_json::to_string_pretty(&data) {
                copy_to_clipboard(&text);
            }
        })];
        show_item_menu_with(widget, x, y, item, &self.ctx, extras);
    }
}

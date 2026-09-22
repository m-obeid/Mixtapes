//! Port of ui/pages/home.py: the quick-picks dial, boxed song lists for
//! song-heavy shelves, card strips for everything else, and the shelf
//! ordering that puts the named rows first.

use std::cell::{Cell, RefCell};
use std::rc::Rc;
use std::time::Duration;

use gtk::{glib, prelude::*};

use crate::model::{ItemKind, MediaItem};
use crate::net::home;
use crate::ui::context::UiContext;
use crate::ui::cover::CoverImage;
use crate::ui::pages::{activate_item_with_radio, attach_item_menu, clear_children, loading_box};
use crate::ui::widgets::media_card::{CardOptions, MediaCard, STRIP_SPACING, STRIP_SPACING_COMPACT};
use crate::ui::widgets::playing::PlayingTracker;
use crate::ui::widgets::scroll_box::HorizontalScrollBox;
use crate::ui::widgets::song_list::{kind_subtitle, song_row};

/// How many shelves of the feed to page in, what get_home_full asked for.
const FEED_SECTIONS: usize = 25;
/// The seed picture beside a "Based on ..." heading.
const STRAPLINE_COVER: i32 = 30;

const SPEED_TILE_COVER: i32 = 56;
const SPEED_TILE_COVER_COMPACT: i32 = 44;
const SPEED_TILE_WIDTH: i32 = 280;
const SPEED_TILE_WIDTH_COMPACT: i32 = 320;
const SPEED_TILE_WIDTH_COMPACT_MIN: i32 = 200;
const SPEED_TILE_TEXT_INSET: i32 = 26;
const SPEED_TILE_TEXT_INSET_COMPACT: i32 = 22;
const SPEED_DIAL_PEEK: i32 = 32;
const SPEED_DIAL_ROWS: i32 = 3;
const SPEED_DIAL_ROWS_COMPACT: i32 = 4;
const LONG_LISTENS_ROWS: i32 = 4;
const SPEED_DIAL_SPACING: i32 = 8;
const LABEL_NATURAL_MAX_CHARS: i32 = 12;

/// A grid of tiles that scrolls sideways, `rows` high.
struct Dial {
    tiles: Vec<SpeedTile>,
    wrap: adw::WrapBox,
    scroll: Rc<HorizontalScrollBox>,
    rows: i32,
    rows_compact: i32,
}

struct SpeedTile {
    tile: gtk::Button,
    text_col: gtk::Box,
    title: gtk::Label,
    cover: Rc<CoverImage>,
}

pub struct HomePage {
    stack: gtk::Stack,
    feed_box: gtk::Box,
    status: adw::StatusPage,
    loaded: Cell<bool>,
    loading: Cell<bool>,
    retry_count: Cell<u32>,
    ctx: Rc<UiContext>,
    playing: Rc<PlayingTracker>,
    /// Each card strip with its cards, so a layout flip resizes one strip at a time.
    strips: RefCell<Vec<(gtk::Box, Vec<Rc<MediaCard>>)>>,
    scrollers: RefCell<Vec<Rc<HorizontalScrollBox>>>,
    /// The tile grids: Quick picks, and Long listens laid out the same way.
    dials: RefCell<Vec<Dial>>,
    /// The widgets whose rows read the `compact` class. Card strips stay out, so a flip restyles no card.
    compact_scopes: RefCell<Vec<gtk::Widget>>,
    /// The items of each rendered shelf, in the order they are drawn.
    shelves: RefCell<Vec<Vec<MediaItem>>>,
}

impl HomePage {
    pub fn new(ctx: Rc<UiContext>) -> Rc<Self> {
        let stack = gtk::Stack::builder().vexpand(true).build();
        stack.add_named(&loading_box("Loading…"), Some("loading"));

        let feed_box = gtk::Box::builder().orientation(gtk::Orientation::Vertical).spacing(28).margin_top(24).margin_bottom(24).margin_start(12).margin_end(12).build();
        let clamp = adw::Clamp::builder().maximum_size(1024).tightening_threshold(600).child(&feed_box).build();
        let scroll = gtk::ScrolledWindow::builder().hscrollbar_policy(gtk::PolicyType::Never).vscrollbar_policy(gtk::PolicyType::Automatic).child(&clamp).build();
        crate::ui::suppress_hover_while_scrolling(&scroll);
        stack.add_named(&scroll, Some("feed"));
        let status = adw::StatusPage::builder().icon_name("user-home-symbolic").title("Home").description("Your music feed will appear here.").build();
        stack.add_named(&status, Some("status"));
        stack.set_visible_child_name("loading");

        let page = Rc::new(Self {
            stack,
            feed_box,
            status,
            loaded: Cell::new(false),
            loading: Cell::new(false),
            retry_count: Cell::new(0),
            playing: PlayingTracker::new(ctx.player.state()),
            ctx,
            strips: RefCell::new(Vec::new()),
            compact_scopes: RefCell::new(Vec::new()),
            scrollers: RefCell::new(Vec::new()),
            dials: RefCell::new(Vec::new()),
            shelves: RefCell::new(Vec::new()),
        });
        let weak = Rc::downgrade(&page);
        glib::idle_add_local_once(move || {
            if let Some(p) = weak.upgrade() {
                p.load_home_data(false);
            }
        });
        page
    }

    /// Port of load_home_data: one fetch at a time, skipped when the feed is
    /// already there unless forced. Offline goes straight to the status page.
    pub fn load_home_data(self: &Rc<Self>, force: bool) {
        if self.loading.get() {
            return;
        }
        if self.loaded.get() && !force {
            return;
        }
        self.loading.set(true);
        if force {
            self.loaded.set(false);
        }
        self.stack.set_visible_child_name("loading");
        if !self.ctx.online.is_online() {
            self.apply_home(Err("offline"));
            return;
        }
        let api = self.ctx.net.client().api();
        let handle = self.ctx.net.spawn(home::get_home(api, FEED_SECTIONS));
        let weak = Rc::downgrade(self);
        glib::spawn_future_local(async move {
            let outcome = handle.await;
            let Some(page) = weak.upgrade() else { return };
            match outcome {
                Ok(Ok(sections)) if !sections.is_empty() => page.apply_home(Ok(sections)),
                Ok(Ok(_)) => page.apply_home(Err("empty")),
                Ok(Err(err)) => {
                    tracing::warn!(%err, "home fetch failed");
                    page.apply_home(Err("error"));
                }
                Err(_) => page.loading.set(false),
            }
        });
    }

    pub fn refresh(self: &Rc<Self>) {
        self.load_home_data(true);
    }

    fn apply_home(self: &Rc<Self>, outcome: Result<Vec<home::HomeSection>, &str>) {
        self.loading.set(false);
        match outcome {
            Ok(sections) => {
                self.retry_count.set(0);
                self.loaded.set(true);
                self.populate(sections);
                self.stack.set_visible_child_name("feed");
            }
            Err("offline") => self.show_status("network-offline-symbolic", "You're offline", "Home requires an internet connection.\nYour downloaded songs are still available.", false),
            Err(_) => {
                let attempt = self.retry_count.get();
                if attempt < 2 {
                    self.retry_count.set(attempt + 1);
                    let weak = Rc::downgrade(self);
                    glib::timeout_add_local_once(Duration::from_millis(1500 * (attempt as u64 + 1)), move || {
                        if let Some(p) = weak.upgrade() {
                            p.load_home_data(true);
                        }
                    });
                    return;
                }
                self.show_status("dialog-warning-symbolic", "Couldn't load Home", "Try refreshing in a moment.", true);
            }
        }
    }

    fn show_status(self: &Rc<Self>, icon: &str, title: &str, description: &str, show_retry: bool) {
        self.status.set_icon_name(Some(icon));
        self.status.set_title(title);
        self.status.set_description(Some(description));
        self.status.set_child(None::<&gtk::Widget>);
        if show_retry {
            let retry = gtk::Button::builder().label("Retry").css_classes(["pill", "suggested-action"]).halign(gtk::Align::Center).build();
            let weak = Rc::downgrade(self);
            retry.connect_clicked(move |_| {
                if let Some(p) = weak.upgrade() {
                    p.retry_count.set(0);
                    p.load_home_data(true);
                }
            });
            self.status.set_child(Some(&retry));
        }
        self.stack.set_visible_child_name("status");
    }

    pub fn widget(&self) -> &gtk::Stack {
        &self.stack
    }

    pub fn set_compact(&self, compact: bool) {
        if compact {
            self.feed_box.set_spacing(20);
            self.feed_box.set_margin_start(6);
            self.feed_box.set_margin_end(6);
        } else {
            self.feed_box.set_spacing(28);
            self.feed_box.set_margin_start(12);
            self.feed_box.set_margin_end(12);
        }
        // What is on screen flips now, the rest one step per frame, so no frame restyles the whole feed.
        let height = self.stack.height() as f32;
        let on_screen = |w: &gtk::Widget| w.compute_bounds(&self.stack).is_some_and(|b| b.y() < height && b.y() + b.height() > 0.0);
        let mut later: Vec<Box<dyn Fn()>> = Vec::new();
        for scope in self.compact_scopes.borrow().iter() {
            if on_screen(scope) {
                set_compact_class(scope, compact);
            } else {
                let scope = scope.clone();
                later.push(Box::new(move || set_compact_class(&scope, compact)));
            }
        }
        for strip in self.strips.borrow().iter().cloned() {
            if on_screen(strip.0.upcast_ref()) {
                resize_strip(&strip, compact);
            } else {
                later.push(Box::new(move || resize_strip(&strip, compact)));
            }
        }
        let later = RefCell::new(later.into_iter());
        let ctx = self.ctx.clone();
        self.stack.add_tick_callback(move |_, _| {
            // A flip back in the meantime queued its own pass.
            if ctx.compact.get() != compact {
                return glib::ControlFlow::Break;
            }
            let step = later.borrow_mut().next();
            match step {
                Some(step) => {
                    step();
                    glib::ControlFlow::Continue
                }
                None => glib::ControlFlow::Break,
            }
        });
        self.apply_speed_tile_style(compact);
        self.sync_speed_dial_height(compact);
    }

    /// Demo hook: play the first song of the first shelf that has one, the
    /// same path a click on that row takes.
    pub fn activate_first_playable(&self) -> bool {
        let shelves = self.shelves.borrow();
        let Some((item, pool)) = shelves.iter().find_map(|items| {
            let pool: Vec<MediaItem> = items.iter().filter(|i| i.kind.is_playable()).cloned().collect();
            pool.first().cloned().map(|first| (first, pool))
        }) else {
            return false;
        };
        tracing::info!(title = %item.title, id = %item.id, pool = pool.len(), "activating the first home row");
        activate_item_with_radio(&self.ctx, &item, &pool);
        true
    }

    /// Port of _populate_feed: the quick-picks dial first, then the four
    /// named rows, then the rest in the order they arrived.
    fn populate(self: &Rc<Self>, sections: Vec<home::HomeSection>) {
        clear_children(&self.feed_box);
        self.playing.clear();
        self.strips.borrow_mut().clear();
        self.compact_scopes.borrow_mut().clear();
        self.scrollers.borrow_mut().clear();
        self.dials.borrow_mut().clear();
        self.shelves.borrow_mut().clear();

        let (dial, ordered) = home::arrange(sections);
        if !dial.is_empty() {
            self.shelves.borrow_mut().push(dial.clone());
            self.add_speed_dial("Quick picks", None, None, &dial, SPEED_DIAL_ROWS, SPEED_DIAL_ROWS_COMPACT);
        }
        for section in ordered {
            self.shelves.borrow_mut().push(section.items.clone());
            // Hour-long mixes, as tiles like the quick picks so the two rows match.
            if home::is_long_listens(&section.title) {
                self.add_speed_dial(&section.title, section.strapline.as_deref(), section.strapline_thumb.as_deref(), &section.items, LONG_LISTENS_ROWS, LONG_LISTENS_ROWS);
                continue;
            }
            let songs = section.items.iter().filter(|i| i.kind == ItemKind::Song).count();
            let section_box = gtk::Box::builder().orientation(gtk::Orientation::Vertical).spacing(10).build();
            section_box.append(&self.section_header(&section.title, section.strapline.as_deref(), section.strapline_thumb.as_deref()));
            // A shelf that is mostly songs reads better as a list than as cards.
            if songs >= 3.max((section.items.len() as f64 * 0.66) as usize) {
                self.add_song_list(&section_box, &section.items);
            } else {
                self.add_card_strip(&section_box, &section.items);
            }
            self.feed_box.append(&section_box);
        }
        self.set_compact(self.ctx.compact.get());
    }

    /// The seed's picture on a "Based on ..." row, the matching icon on the
    /// rows that have one, and nothing in front of the rest.
    fn section_header(&self, title: &str, strapline: Option<&str>, strapline_thumb: Option<&str>) -> gtk::Box {
        let header = gtk::Box::builder().orientation(gtk::Orientation::Horizontal).spacing(10).halign(gtk::Align::Start).css_classes(["home-section-header"]).build();
        match strapline_thumb {
            Some(url) => {
                let cover = CoverImage::in_context(&self.ctx, STRAPLINE_COVER);
                cover.load(url);
                let wrapper = gtk::Box::builder().overflow(gtk::Overflow::Hidden).css_classes(["home-section-cover"]).valign(gtk::Align::Center).build();
                wrapper.append(cover.widget());
                header.append(&wrapper);
                unsafe { header.set_data("cover", cover) };
            }
            None => {
                if let Some(icon_name) = home::section_icon(title) {
                    header.append(&gtk::Image::builder().icon_name(icon_name).pixel_size(22).valign(gtk::Align::Center).css_classes(["home-section-icon"]).build());
                }
            }
        }
        let title_label = gtk::Label::builder().label(title).css_classes(["title-2", "home-section-title"]).halign(gtk::Align::Start).valign(gtk::Align::Center).ellipsize(gtk::pango::EllipsizeMode::End).build();
        match strapline {
            // The web feed's small line above the title, in capitals like there.
            Some(strapline) => {
                let column = gtk::Box::builder().orientation(gtk::Orientation::Vertical).valign(gtk::Align::Center).build();
                column.append(&gtk::Label::builder().label(strapline.to_uppercase()).css_classes(["home-section-strapline"]).halign(gtk::Align::Start).xalign(0.0).build());
                column.append(&title_label);
                header.append(&column);
            }
            None => header.append(&title_label),
        }
        header
    }

    // -- quick picks ------------------------------------------------------

    fn add_speed_dial(self: &Rc<Self>, title: &str, strapline: Option<&str>, strapline_thumb: Option<&str>, items: &[MediaItem], rows: i32, rows_compact: i32) {
        let section_box = gtk::Box::builder().orientation(gtk::Orientation::Vertical).spacing(10).css_classes(["home-speed-dial"]).build();
        section_box.append(&self.section_header(title, strapline, strapline_thumb));

        let scroll_box = HorizontalScrollBox::new();
        let wrap = adw::WrapBox::builder().orientation(gtk::Orientation::Vertical).line_homogeneous(true).line_spacing(SPEED_DIAL_SPACING).child_spacing(SPEED_DIAL_SPACING).valign(gtk::Align::Start).build();
        let heights = gtk::SizeGroup::new(gtk::SizeGroupMode::Vertical);
        let pool: Vec<MediaItem> = items.iter().filter(|i| i.kind.is_playable()).cloned().collect();

        let mut tiles = Vec::with_capacity(items.len());
        for item in items {
            let tile = self.build_speed_tile(item, &pool);
            heights.add_widget(&tile.tile);
            wrap.append(&tile.tile);
            tiles.push(tile);
        }
        scroll_box.set_content(&wrap);
        section_box.append(scroll_box.widget());

        let weak = Rc::downgrade(self);
        scroll_box.hadjustment().connect_changed(move |_| {
            if let Some(p) = weak.upgrade() {
                if p.ctx.compact.get() {
                    p.apply_speed_tile_style(true);
                }
            }
        });
        self.add_compact_scope(wrap.upcast_ref());
        self.dials.borrow_mut().push(Dial { tiles, wrap, scroll: scroll_box, rows, rows_compact });
        self.feed_box.append(&section_box);
        self.apply_speed_tile_style(self.ctx.compact.get());
        self.sync_speed_dial_height(self.ctx.compact.get());
    }

    fn build_speed_tile(self: &Rc<Self>, item: &MediaItem, pool: &[MediaItem]) -> SpeedTile {
        let tile = gtk::Button::builder().css_classes(["home-speed-tile", "card"]).build();
        let inner = gtk::Box::builder().orientation(gtk::Orientation::Horizontal).spacing(10).build();
        tile.set_child(Some(&inner));

        let cover = CoverImage::new(self.ctx.net.clone(), SPEED_TILE_COVER);
        if let Some(url) = &item.thumb {
            cover.load(url);
        }
        let wrapper = gtk::Box::builder().overflow(gtk::Overflow::Hidden).css_classes(["home-speed-cover"]).valign(gtk::Align::Center).build();
        wrapper.append(cover.widget());
        inner.append(&wrapper);

        let text_col = gtk::Box::builder().orientation(gtk::Orientation::Vertical).spacing(2).valign(gtk::Align::Center).hexpand(true).build();
        let title = gtk::Label::builder()
            .label(&item.title)
            .halign(gtk::Align::Fill)
            .xalign(0.0)
            .ellipsize(gtk::pango::EllipsizeMode::End)
            .wrap_mode(gtk::pango::WrapMode::WordChar)
            .width_chars(1)
            .max_width_chars(LABEL_NATURAL_MAX_CHARS)
            .hexpand(true)
            .css_classes(["home-speed-title"])
            .build();
        text_col.append(&title);
        text_col.append(&kind_subtitle(item, false, true));
        inner.append(&text_col);

        let ctx = self.ctx.clone();
        let (item_c, pool_c) = (item.clone(), pool.to_vec());
        tile.connect_clicked(move |_| activate_item_with_radio(&ctx, &item_c, &pool_c));
        attach_item_menu(&self.ctx, &tile, item.clone());
        if item.kind.is_playable() {
            self.playing.track(&tile, &item.id);
        }
        SpeedTile { tile, text_col, title, cover }
    }

    fn speed_tile_width(dial: &Dial, compact: bool) -> i32 {
        if !compact {
            return SPEED_TILE_WIDTH;
        }
        let viewport = dial.scroll.hadjustment().page_size() as i32;
        if viewport <= 0 {
            return SPEED_TILE_WIDTH_COMPACT;
        }
        SPEED_TILE_WIDTH_COMPACT.min(viewport - SPEED_DIAL_PEEK).max(SPEED_TILE_WIDTH_COMPACT_MIN)
    }

    fn apply_speed_tile_style(&self, compact: bool) {
        let cover = if compact { SPEED_TILE_COVER_COMPACT } else { SPEED_TILE_COVER };
        let inset = if compact { SPEED_TILE_TEXT_INSET_COMPACT } else { SPEED_TILE_TEXT_INSET };
        for dial in self.dials.borrow().iter() {
            let width = Self::speed_tile_width(dial, compact);
            for t in &dial.tiles {
                t.tile.set_size_request(width, -1);
                t.text_col.set_size_request(width - cover - inset, -1);
                t.title.set_wrap(!compact);
                t.title.set_lines(if compact { 1 } else { 2 });
                t.cover.set_size(cover);
            }
        }
    }

    fn sync_speed_dial_height(&self, compact: bool) {
        for dial in self.dials.borrow().iter() {
            let Some(first) = dial.tiles.first() else { continue };
            let rows = if compact { dial.rows_compact } else { dial.rows };
            let row = first.tile.measure(gtk::Orientation::Vertical, -1).1;
            dial.wrap.set_size_request(-1, rows * row + (rows - 1) * SPEED_DIAL_SPACING);
        }
    }

    // -- sections ---------------------------------------------------------

    fn add_song_list(self: &Rc<Self>, section_box: &gtk::Box, items: &[MediaItem]) {
        let list = gtk::ListBox::builder().css_classes(["boxed-list", "songs-list"]).selection_mode(gtk::SelectionMode::None).build();
        let pool: Vec<MediaItem> = items.iter().filter(|i| i.kind.is_playable()).cloned().collect();
        for item in items {
            let (row, _inner) = song_row(&self.ctx, item);
            if item.kind.is_playable() {
                self.playing.track(&row, &item.id);
            }
            attach_item_menu(&self.ctx, &row, item.clone());
            list.append(&row);
        }
        let ctx = self.ctx.clone();
        let (items_c, pool_c) = (items.to_vec(), pool);
        list.connect_row_activated(move |_, row| {
            if let Some(item) = items_c.get(row.index().max(0) as usize) {
                activate_item_with_radio(&ctx, item, &pool_c);
            }
        });
        self.add_compact_scope(list.upcast_ref());
        section_box.append(&list);
    }

    fn add_compact_scope(&self, widget: &gtk::Widget) {
        set_compact_class(widget, self.ctx.compact.get());
        self.compact_scopes.borrow_mut().push(widget.clone());
    }

    fn add_card_strip(self: &Rc<Self>, section_box: &gtk::Box, items: &[MediaItem]) {
        let scroll_box = HorizontalScrollBox::new();
        scroll_box.widget().set_margin_bottom(16);
        let compact = self.ctx.compact.get();
        let strip = gtk::Box::builder().orientation(gtk::Orientation::Horizontal).spacing(if compact { STRIP_SPACING_COMPACT } else { STRIP_SPACING }).build();
        let pool: Vec<MediaItem> = items.iter().filter(|i| i.kind.is_playable()).cloned().collect();
        let mut cards = Vec::with_capacity(items.len());
        for item in items {
            let card = MediaCard::new(&self.ctx, item.clone(), CardOptions { title_lines: 2, ..CardOptions::default() });
            let ctx = self.ctx.clone();
            let pool_c = pool.clone();
            card.connect_clicked(move |item| activate_item_with_radio(&ctx, item, &pool_c));
            attach_item_menu(&self.ctx, card.widget(), item.clone());
            if item.kind.is_playable() {
                self.playing.track(card.widget(), &item.id);
            }
            strip.append(card.widget());
            cards.push(card);
        }
        scroll_box.set_content(&strip);
        section_box.append(scroll_box.widget());
        self.strips.borrow_mut().push((strip, cards));
        self.scrollers.borrow_mut().push(scroll_box);
    }
}

fn resize_strip((strip, cards): &(gtk::Box, Vec<Rc<MediaCard>>), compact: bool) {
    strip.set_spacing(if compact { STRIP_SPACING_COMPACT } else { STRIP_SPACING });
    for card in cards {
        card.set_compact(compact);
    }
}

fn set_compact_class(widget: &gtk::Widget, compact: bool) {
    if compact {
        widget.add_css_class("compact");
    } else {
        widget.remove_css_class("compact");
    }
}

//! Port of ui/pages/category.py: the carousels behind one mood or genre pill.
//! Song shelves become boxed lists that show five rows until View All opens
//! the rest; everything else becomes a card strip.

use std::cell::{Cell, RefCell};
use std::collections::HashMap;
use std::rc::Rc;

use gtk::{glib, prelude::*};

use crate::model::MediaItem;
use crate::net::explore::{self, CategorySection};
use crate::ui::context::UiContext;
use crate::ui::context_menu::{MenuAction, Section, show_item_menu_with};
use crate::ui::copy_to_clipboard;
use crate::ui::pages::activate_item;
use crate::ui::widgets::media_card::{CardOptions, MediaCard, STRIP_SPACING, STRIP_SPACING_COMPACT};
use crate::ui::widgets::scroll_box::HorizontalScrollBox;
use crate::ui::widgets::song_row::SongRow;

/// Rows a song shelf shows before View All.
const SONG_PREVIEW: usize = 5;
/// What View All raises that limit to.
const SONG_ALL: usize = 1000;
/// How far the page scrolls before the header takes over the title.
const TITLE_HANDOVER: f64 = 50.0;

type TitleListener = Box<dyn Fn(&str)>;

pub struct CategoryPage {
    root: gtk::Box,
    content_box: gtk::Box,
    title_label: gtk::Label,
    loading_wrap: gtk::Box,
    ctx: Rc<UiContext>,
    params: RefCell<String>,
    title: RefCell<String>,
    is_loading: Cell<bool>,
    /// How many rows each song shelf shows, raised by that shelf's View All.
    section_limits: RefCell<HashMap<String, usize>>,
    sections: RefCell<Vec<CategorySection>>,
    cards: RefCell<Vec<Rc<MediaCard>>>,
    strips: RefCell<Vec<gtk::Box>>,
    scrollers: RefCell<Vec<Rc<HorizontalScrollBox>>>,
    rows: RefCell<Vec<Rc<SongRow>>>,
    on_title: RefCell<Option<TitleListener>>,
}

impl CategoryPage {
    pub fn new(ctx: Rc<UiContext>) -> Rc<Self> {
        let root = gtk::Box::builder().orientation(gtk::Orientation::Vertical).build();
        let scrolled = gtk::ScrolledWindow::builder().hscrollbar_policy(gtk::PolicyType::Never).vscrollbar_policy(gtk::PolicyType::Automatic).vexpand(true).build();
        crate::ui::suppress_hover_while_scrolling(&scrolled);

        let content_box = gtk::Box::builder().orientation(gtk::Orientation::Vertical).spacing(32).margin_top(24).margin_bottom(24).margin_start(16).margin_end(16).build();
        let title_label = gtk::Label::builder().css_classes(["title-1"]).halign(gtk::Align::Start).margin_bottom(16).build();
        content_box.append(&title_label);

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
            title_label,
            loading_wrap,
            ctx,
            params: RefCell::new(String::new()),
            title: RefCell::new(String::new()),
            is_loading: Cell::new(false),
            section_limits: RefCell::new(HashMap::new()),
            sections: RefCell::new(Vec::new()),
            cards: RefCell::new(Vec::new()),
            strips: RefCell::new(Vec::new()),
            scrollers: RefCell::new(Vec::new()),
            rows: RefCell::new(Vec::new()),
            on_title: RefCell::new(None),
        });

        // Past the first scroll the page title moves into the header bar.
        let weak = Rc::downgrade(&page);
        scrolled.vadjustment().connect_value_changed(move |adj| {
            let Some(p) = weak.upgrade() else { return };
            let title = p.title.borrow().clone();
            p.emit_title(if adj.value() > TITLE_HANDOVER { &title } else { "" });
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
            self.content_box.set_spacing(16);
        } else {
            self.root.remove_css_class("compact");
            self.content_box.set_spacing(32);
        }
        for strip in self.strips.borrow().iter() {
            strip.set_spacing(if compact { STRIP_SPACING_COMPACT } else { STRIP_SPACING });
        }
        for card in self.cards.borrow().iter() {
            card.set_compact(compact);
        }
    }

    pub fn load_category(self: &Rc<Self>, params: &str, title: &str) {
        self.params.replace(params.to_owned());
        self.title.replace(title.to_owned());
        self.title_label.set_label(title);
        self.section_limits.borrow_mut().clear();
        self.clear_sections();
        self.emit_title(title);

        if self.is_loading.replace(true) {
            return;
        }
        self.loading_wrap.set_visible(true);
        let api = self.ctx.net.client().api();
        let params = params.to_owned();
        let handle = self.ctx.net.spawn(async move { explore::get_category_page(&api, &params).await });
        let weak = Rc::downgrade(self);
        glib::spawn_future_local(async move {
            let outcome = handle.await;
            let Some(page) = weak.upgrade() else { return };
            page.is_loading.set(false);
            page.loading_wrap.set_visible(false);
            match outcome {
                Ok(Ok(sections)) => {
                    page.sections.replace(sections);
                    page.render_sections();
                }
                Ok(Err(err)) => tracing::warn!(%err, "category page failed"),
                Err(_) => {}
            }
        });
    }

    /// Everything but the title and the spinner, which the page keeps.
    fn clear_sections(&self) {
        let mut child = self.content_box.first_child();
        while let Some(widget) = child {
            child = widget.next_sibling();
            if widget != self.title_label.clone().upcast::<gtk::Widget>() && widget != self.loading_wrap.clone().upcast::<gtk::Widget>() {
                self.content_box.remove(&widget);
            }
        }
        self.cards.borrow_mut().clear();
        self.strips.borrow_mut().clear();
        self.scrollers.borrow_mut().clear();
        self.rows.borrow_mut().clear();
    }

    fn render_sections(self: &Rc<Self>) {
        self.clear_sections();
        for section in self.sections.borrow().iter() {
            if is_song_section(section) {
                self.add_songs_list(&section.title, &section.items);
            } else {
                self.add_carousel(&section.title, &section.items);
            }
        }
        self.set_compact_mode(self.ctx.compact.get());
    }

    fn add_carousel(self: &Rc<Self>, title: &str, items: &[MediaItem]) {
        if items.is_empty() {
            return;
        }
        let section = gtk::Box::builder().orientation(gtk::Orientation::Vertical).spacing(16).build();
        section.append(&heading(title));
        let scroll_box = HorizontalScrollBox::new();
        let compact = self.ctx.compact.get();
        let strip = gtk::Box::builder().orientation(gtk::Orientation::Horizontal).spacing(if compact { STRIP_SPACING_COMPACT } else { STRIP_SPACING }).build();
        let pool: Vec<MediaItem> = items.iter().filter(|i| i.kind.is_playable()).cloned().collect();
        for item in items {
            let card = MediaCard::new(&self.ctx, item.clone(), CardOptions { title_lines: 1, ..CardOptions::default() });
            let ctx = self.ctx.clone();
            let pool_c = pool.clone();
            card.connect_clicked(move |item| activate_item(&ctx, item, &pool_c));
            self.attach_card_menu(card.widget(), item);
            strip.append(card.widget());
            self.cards.borrow_mut().push(card);
        }
        scroll_box.set_content(&strip);
        section.append(scroll_box.widget());
        self.content_box.append(&section);
        self.strips.borrow_mut().push(strip);
        self.scrollers.borrow_mut().push(scroll_box);
    }

    fn add_songs_list(self: &Rc<Self>, title: &str, items: &[MediaItem]) {
        if items.is_empty() {
            return;
        }
        let section = gtk::Box::builder().orientation(gtk::Orientation::Vertical).spacing(8).build();
        let label = heading(title);
        label.set_margin_bottom(8);
        section.append(&label);

        let limit = self.section_limits.borrow().get(title).copied().unwrap_or(SONG_PREVIEW);
        let showing = &items[..items.len().min(limit)];
        let list = gtk::ListBox::builder().css_classes(["boxed-list", "songs-list"]).selection_mode(gtk::SelectionMode::None).build();
        let pool: Vec<MediaItem> = showing.iter().filter(|i| i.kind.is_playable()).cloned().collect();
        for item in showing {
            let row = SongRow::new(self.ctx.clone());
            row.set_search_style(true);
            row.set_plain_subtitle(Some(item.artists_text()));
            row.bind(item, None);
            list.append(row.widget());
            self.rows.borrow_mut().push(row);
        }
        let ctx = self.ctx.clone();
        let shown = showing.to_vec();
        list.connect_row_activated(move |_, row| {
            if let Some(item) = shown.get(row.index().max(0) as usize) {
                activate_item(&ctx, item, &pool);
            }
        });
        section.append(&list);

        if items.len() > limit {
            let button = gtk::Button::builder().label("View All").css_classes(["pill"]).halign(gtk::Align::Center).margin_top(12).build();
            let weak = Rc::downgrade(self);
            let title = title.to_owned();
            button.connect_clicked(move |_| {
                if let Some(page) = weak.upgrade() {
                    page.section_limits.borrow_mut().insert(title.clone(), SONG_ALL);
                    page.render_sections();
                }
            });
            let button_box = gtk::Box::builder().orientation(gtk::Orientation::Horizontal).spacing(8).halign(gtk::Align::Center).build();
            button_box.append(&button);
            section.append(&button_box);
        }
        self.content_box.append(&section);
    }

    /// Card menu with the Copy JSON entry on_grid_right_click added.
    fn attach_card_menu(self: &Rc<Self>, widget: &gtk::Button, item: &MediaItem) {
        let ctx = self.ctx.clone();
        let button = widget.clone();
        let item = item.clone();
        let open = Rc::new(move |x: f64, y: f64| {
            let data = item.clone();
            let extras = vec![MenuAction::new("Copy JSON (Debug)", Section::Clipboard, move || {
                if let Ok(text) = serde_json::to_string_pretty(&data) {
                    copy_to_clipboard(&text);
                }
            })];
            show_item_menu_with(&button, x, y, &item, &ctx, extras);
        });
        let right = gtk::GestureClick::builder().button(gtk::gdk::BUTTON_SECONDARY).build();
        let o = open.clone();
        right.connect_released(move |_, _, x, y| o(x, y));
        widget.add_controller(right);
        let long = gtk::GestureLongPress::new();
        long.connect_pressed(move |_, x, y| open(x, y));
        widget.add_controller(long);
    }
}

/// Port of the shelf test in _render_sections: a shelf named Songs, or one
/// whose first rows are all playable and whose name says nothing about video.
fn is_song_section(section: &CategorySection) -> bool {
    let title = section.title.to_lowercase();
    if title == "songs" {
        return true;
    }
    !title.contains("video") && section.items.iter().take(3).all(|item| item.kind.is_playable())
}

fn heading(title: &str) -> gtk::Label {
    gtk::Label::builder().label(title).css_classes(["heading"]).halign(gtk::Align::Start).build()
}

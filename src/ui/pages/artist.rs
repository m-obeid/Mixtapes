//! Port of ui/pages/artist.py: banner header with the action row, the top
//! songs list, card strips for albums, singles, videos, playlists, featured
//! appearances and related artists, each with its View All or Load More.

use std::cell::{Cell, RefCell};
use std::collections::HashMap;
use std::rc::Rc;

use adw::prelude::*;
use gtk::{gdk, gio, glib};
use regex::Regex;
use std::sync::LazyLock;

use crate::model::{ItemKind, MediaItem, Person, Track, VideoId};
use crate::net::artist::{self, ArtistData, CardSection, SongSection};
use crate::net::playlists;
use crate::ui::context::{NavRequest, UiContext};
use crate::ui::context_menu::{
    MenuAction, Section, SongMenuOptions, show_item_menu_with, show_song_menu,
};
use crate::ui::copy_to_clipboard;
use crate::ui::cover::CoverImage;
use crate::ui::like_button::LikeButton;
use crate::ui::widgets::cover_picture::CoverPicture;
use crate::ui::widgets::fade_bottom_bin::FadeBottomBin;
use crate::ui::widgets::media_card::{
    CardOptions, MediaCard, STRIP_SPACING, STRIP_SPACING_COMPACT,
};
use crate::ui::widgets::playing::PlayingTracker;
use crate::ui::widgets::scroll_box::HorizontalScrollBox;

static WIKIPEDIA_RE: LazyLock<Regex> =
    LazyLock::new(|| Regex::new(r"(?i)\s*From Wikipedia[^\n]*").unwrap());
static SPACES_RE: LazyLock<Regex> = LazyLock::new(|| Regex::new(r"[^\S\n]{2,}").unwrap());
static NEWLINES_RE: LazyLock<Regex> = LazyLock::new(|| Regex::new(r"\n{3,}").unwrap());

const BANNER_HEIGHT: i32 = 260;
const BANNER_HEIGHT_COMPACT: i32 = 200;
const INFO_TOP: i32 = 160;
const INFO_TOP_COMPACT: i32 = 120;
const DESCRIPTION_PREVIEW: usize = 280;

type TitleListener = Box<dyn Fn(&str)>;

pub struct ArtistPage {
    stack: adw::ViewStack,
    content_box: gtk::Box,
    banner_overlay: gtk::Overlay,
    banner_wrapper: FadeBottomBin,
    avatar: Rc<CoverPicture>,
    info_box: gtk::Box,
    name_label: gtk::Label,
    subscribers_label: gtk::Label,
    play_btn: gtk::Button,
    shuffle_btn: gtk::Button,
    radio_btn: gtk::Button,
    subscribe_btn: gtk::Button,
    description_label: gtk::Label,
    description_box: gtk::Box,
    read_more: RefCell<gtk::Label>,
    sections_box: gtk::Box,
    ctx: Rc<UiContext>,
    playing: Rc<PlayingTracker>,

    channel_id: RefCell<String>,
    artist_name: RefCell<String>,
    data: RefCell<Option<ArtistData>>,
    section_limits: RefCell<HashMap<String, usize>>,
    section_widgets: RefCell<HashMap<String, gtk::Box>>,
    card_strips: RefCell<Vec<gtk::Box>>,
    cards: RefCell<Vec<Rc<MediaCard>>>,
    covers: RefCell<Vec<Rc<CoverImage>>>,
    likes: RefCell<Vec<Rc<LikeButton>>>,
    scrollers: RefCell<Vec<Rc<HorizontalScrollBox>>>,
    description_clean: RefCell<String>,
    description_expanded: Cell<bool>,
    is_subscribed: Cell<bool>,
    ui_init: Cell<bool>,
    compact: Cell<bool>,
    on_title: RefCell<Option<TitleListener>>,
}

impl ArtistPage {
    pub fn new(ctx: Rc<UiContext>) -> Rc<Self> {
        let scrolled = gtk::ScrolledWindow::builder()
            .hscrollbar_policy(gtk::PolicyType::Never)
            .vscrollbar_policy(gtk::PolicyType::Automatic)
            .vexpand(true)
            .build();
        crate::ui::suppress_hover_while_scrolling(&scrolled);
        let clamp = adw::Clamp::builder()
            .maximum_size(1024)
            .tightening_threshold(600)
            .build();
        let content_box = gtk::Box::builder()
            .orientation(gtk::Orientation::Vertical)
            .spacing(0)
            .margin_bottom(24)
            .margin_start(12)
            .margin_end(12)
            .build();

        // -- header grid: banner and info share one cell -----------------
        let header_grid = gtk::Grid::builder().column_homogeneous(true).build();
        content_box.append(&header_grid);
        let banner_overlay = gtk::Overlay::builder()
            .vexpand(false)
            .hexpand(true)
            .valign(gtk::Align::Start)
            .build();
        banner_overlay.set_size_request(-1, BANNER_HEIGHT);
        let avatar = CoverPicture::new(ctx.net.clone());
        avatar.widget().set_hexpand(true);
        avatar.widget().set_vexpand(true);
        avatar.widget().set_halign(gtk::Align::Fill);
        avatar.widget().set_valign(gtk::Align::Fill);
        let banner_wrapper = FadeBottomBin::new(0.55);
        banner_wrapper.set_overflow(gtk::Overflow::Hidden);
        banner_wrapper.add_css_class("banner-top-rounded");
        banner_wrapper.set_hexpand(true);
        banner_wrapper.set_vexpand(false);
        banner_wrapper.set_size_request(-1, BANNER_HEIGHT);
        banner_wrapper.append(avatar.widget());
        banner_overlay.set_child(Some(&banner_wrapper));
        // The fade follows the window's blur mode, read off the root's classes.
        {
            let wrapper = banner_wrapper.clone();
            banner_wrapper.connect_map(move |w| {
                let active = w.root().is_some_and(|r| r.has_css_class("cover-bg-active"));
                wrapper.set_fade_active(active);
            });
        }
        let scrim = gtk::Box::builder()
            .vexpand(true)
            .hexpand(true)
            .css_classes(["banner-scrim"])
            .build();
        banner_overlay.add_overlay(&scrim);
        header_grid.attach(&banner_overlay, 0, 0, 1, 1);

        let info_box = gtk::Box::builder()
            .orientation(gtk::Orientation::Vertical)
            .spacing(4)
            .margin_top(INFO_TOP)
            .margin_start(16)
            .margin_end(16)
            .margin_bottom(24)
            .vexpand(false)
            .valign(gtk::Align::Start)
            .build();
        header_grid.attach(&info_box, 0, 0, 1, 1);
        let name_label = gtk::Label::builder()
            .label("Artist Name")
            .css_classes(["title-1", "banner-text"])
            .halign(gtk::Align::Start)
            .wrap(true)
            .wrap_mode(gtk::pango::WrapMode::WordChar)
            .xalign(0.0)
            .build();
        info_box.append(&name_label);
        let subscribers_label = gtk::Label::builder()
            .css_classes(["caption", "banner-text"])
            .opacity(0.85)
            .halign(gtk::Align::Start)
            .build();
        info_box.append(&subscribers_label);

        let actions = gtk::Box::builder()
            .orientation(gtk::Orientation::Horizontal)
            .spacing(12)
            .margin_top(8)
            .valign(gtk::Align::Center)
            .build();
        info_box.append(&actions);
        let play_btn = gtk::Button::builder()
            .label("Play")
            .css_classes(["suggested-action", "pill"])
            .build();
        actions.append(&play_btn);
        let shuffle_btn = gtk::Button::builder()
            .icon_name("media-playlist-shuffle-symbolic")
            .css_classes(["circular"])
            .valign(gtk::Align::Center)
            .tooltip_text("Shuffle")
            .build();
        shuffle_btn.set_size_request(48, 48);
        actions.append(&shuffle_btn);
        let radio_btn = gtk::Button::builder()
            .icon_name("triangular-antenna-symbolic")
            .css_classes(["circular"])
            .valign(gtk::Align::Center)
            .tooltip_text("Start Radio")
            .build();
        radio_btn.set_size_request(48, 48);
        actions.append(&radio_btn);
        let subscribe_btn = gtk::Button::builder()
            .icon_name("non-starred-symbolic")
            .css_classes(["circular", "flat"])
            .valign(gtk::Align::Center)
            .tooltip_text("Subscribe")
            .build();
        subscribe_btn.set_size_request(48, 48);
        actions.append(&subscribe_btn);

        let description_label = gtk::Label::builder()
            .css_classes(["body"])
            .wrap(true)
            .wrap_mode(gtk::pango::WrapMode::WordChar)
            .halign(gtk::Align::Start)
            .xalign(0.0)
            .lines(0)
            .ellipsize(gtk::pango::EllipsizeMode::None)
            .margin_top(12)
            .build();
        let read_more = read_more_label("Read more");
        read_more.set_visible(false);
        let description_box = gtk::Box::builder()
            .orientation(gtk::Orientation::Vertical)
            .spacing(4)
            .margin_start(16)
            .margin_end(16)
            .margin_top(0)
            .margin_bottom(16)
            .build();
        description_box.append(&description_label);
        description_box.append(&read_more);
        content_box.append(&description_box);

        let sections_box = gtk::Box::builder()
            .orientation(gtk::Orientation::Vertical)
            .spacing(32)
            .margin_top(12)
            .build();
        content_box.append(&sections_box);
        clamp.set_child(Some(&content_box));
        scrolled.set_child(Some(&clamp));
        let main_box = gtk::Box::builder()
            .orientation(gtk::Orientation::Vertical)
            .build();
        main_box.append(&scrolled);

        let stack = adw::ViewStack::new();
        let loading_box = gtk::Box::builder()
            .orientation(gtk::Orientation::Vertical)
            .spacing(12)
            .valign(gtk::Align::Center)
            .halign(gtk::Align::Center)
            .build();
        let spinner = adw::Spinner::new();
        spinner.set_size_request(32, 32);
        loading_box.append(&spinner);
        stack.add_named(&loading_box, Some("loading"));
        stack.add_named(&main_box, Some("content"));

        let page = Rc::new(Self {
            stack,
            content_box,
            banner_overlay,
            banner_wrapper,
            avatar,
            info_box,
            name_label,
            subscribers_label,
            play_btn,
            shuffle_btn,
            radio_btn,
            subscribe_btn,
            description_label,
            description_box,
            read_more: RefCell::new(read_more),
            sections_box,
            playing: PlayingTracker::new(ctx.player.state()),
            ctx,
            channel_id: RefCell::new(String::new()),
            artist_name: RefCell::new(String::new()),
            data: RefCell::new(None),
            section_limits: RefCell::new(HashMap::from([
                ("Top Songs".to_owned(), 5),
                ("Albums".to_owned(), 10),
                ("Singles & EPs".to_owned(), 10),
                ("Videos".to_owned(), 10),
            ])),
            section_widgets: RefCell::new(HashMap::new()),
            card_strips: RefCell::new(Vec::new()),
            cards: RefCell::new(Vec::new()),
            covers: RefCell::new(Vec::new()),
            likes: RefCell::new(Vec::new()),
            scrollers: RefCell::new(Vec::new()),
            description_clean: RefCell::new(String::new()),
            description_expanded: Cell::new(false),
            is_subscribed: Cell::new(false),
            ui_init: Cell::new(false),
            compact: Cell::new(false),
            on_title: RefCell::new(None),
        });
        page.wire(&scrolled);
        page
    }

    pub fn widget(&self) -> &adw::ViewStack {
        &self.stack
    }

    /// Demo hook: what the radio button does.
    pub fn press_radio(&self) {
        self.on_radio_clicked();
    }

    pub fn set_on_header_title(&self, f: impl Fn(&str) + 'static) {
        self.on_title.replace(Some(Box::new(f)));
    }

    fn emit_title(&self, title: &str) {
        if let Some(f) = self.on_title.borrow().as_ref() {
            f(title);
        }
    }

    fn wire(self: &Rc<Self>, scrolled: &gtk::ScrolledWindow) {
        let weak = Rc::downgrade(self);
        scrolled.vadjustment().connect_value_changed(move |adj| {
            if let Some(p) = weak.upgrade() {
                if adj.value() > 100.0 {
                    p.emit_title(&p.artist_name.borrow());
                } else {
                    p.emit_title("");
                }
            }
        });
        let weak = Rc::downgrade(self);
        self.play_btn.connect_clicked(move |_| {
            if let Some(p) = weak.upgrade() {
                let tracks = p.build_queue_tracks();
                if !tracks.is_empty() {
                    p.ctx.player.play_tracks(tracks, 0, false, None, false);
                }
            }
        });
        let weak = Rc::downgrade(self);
        self.shuffle_btn.connect_clicked(move |_| {
            if let Some(p) = weak.upgrade() {
                let tracks = p.build_queue_tracks();
                if !tracks.is_empty() {
                    p.ctx
                        .player
                        .play_tracks(tracks, usize::MAX, true, None, false);
                }
            }
        });
        let weak = Rc::downgrade(self);
        self.radio_btn.connect_clicked(move |_| {
            if let Some(p) = weak.upgrade() {
                p.on_radio_clicked();
            }
        });
        let weak = Rc::downgrade(self);
        self.subscribe_btn.connect_clicked(move |_| {
            if let Some(p) = weak.upgrade() {
                p.on_subscribe_clicked();
            }
        });
        self.connect_read_more();
        let open_menu = {
            let weak = Rc::downgrade(self);
            Rc::new(move |x: f64, y: f64| {
                if let Some(p) = weak.upgrade() {
                    p.on_banner_right_click(x, y);
                }
            })
        };
        let right = gtk::GestureClick::builder()
            .button(gdk::BUTTON_SECONDARY)
            .build();
        let open = open_menu.clone();
        right.connect_pressed(move |_, _, x, y| open(x, y));
        self.banner_overlay.add_controller(right);
        let long = gtk::GestureLongPress::new();
        long.connect_pressed(move |_, x, y| open_menu(x, y));
        self.banner_overlay.add_controller(long);
        let weak = Rc::downgrade(self);
        self.ctx.on_compact(move |compact| match weak.upgrade() {
            Some(p) => {
                p.set_compact_mode(compact);
                true
            }
            None => false,
        });
    }

    fn connect_read_more(self: &Rc<Self>) {
        let weak = Rc::downgrade(self);
        self.read_more.borrow().connect_activate_link(move |_, _| {
            if let Some(p) = weak.upgrade() {
                glib::idle_add_local_once(move || p.toggle_description());
            }
            glib::Propagation::Stop
        });
    }

    // -- loading ----------------------------------------------------------

    pub fn load_artist(self: &Rc<Self>, channel_id: &str, initial_name: Option<&str>) {
        self.channel_id.replace(channel_id.to_owned());
        if let Some(name) = initial_name.filter(|n| !n.is_empty()) {
            self.artist_name.replace(name.to_owned());
            self.name_label.set_label(name);
        }
        self.stack.set_visible_child_name("loading");
        let api = self.ctx.net.client().api();
        let id = channel_id.to_owned();
        let handle = self
            .ctx
            .net
            .spawn(async move { artist::get_artist(api, &id).await });
        let weak = Rc::downgrade(self);
        let id = channel_id.to_owned();
        glib::spawn_future_local(async move {
            let outcome = handle.await;
            let Some(page) = weak.upgrade() else { return };
            if *page.channel_id.borrow() != id {
                return;
            }
            match outcome {
                Ok(Ok(data)) => {
                    page.ui_init.set(false);
                    page.data.replace(Some(data));
                    page.update_ui();
                }
                Ok(Err(err)) => tracing::warn!(%err, id, "artist fetch failed"),
                Err(_) => {}
            }
        });
    }

    /// Port of update_ui: header from the data, then every section in order.
    fn update_ui(self: &Rc<Self>) {
        let Some(data) = self.data.borrow().clone() else {
            return;
        };
        self.stack.set_visible_child_name("content");
        let name = if data.name.is_empty() {
            "Unknown Artist".to_owned()
        } else {
            data.name.clone()
        };
        self.artist_name.replace(name.clone());
        self.name_label.set_label(&name);

        self.description_expanded.set(false);
        match data.description.as_deref().filter(|d| !d.is_empty()) {
            Some(description) => {
                // Strip the Wikipedia attribution, collapse runs of spaces and blank lines.
                let clean = WIKIPEDIA_RE.replace_all(description, "").trim().to_owned();
                let clean = SPACES_RE.replace_all(&clean, " ").into_owned();
                let clean = NEWLINES_RE.replace_all(&clean, "\n\n").into_owned();
                if clean.chars().count() > DESCRIPTION_PREVIEW {
                    self.description_label.set_label(&preview_of(&clean));
                    self.read_more
                        .borrow()
                        .set_markup("<a href='toggle'>Read more</a>");
                    self.read_more.borrow().set_visible(true);
                } else {
                    self.description_label.set_label(&clean);
                    self.read_more.borrow().set_visible(false);
                }
                self.description_clean.replace(clean);
                self.description_label.set_visible(true);
            }
            None => {
                self.description_clean.replace(String::new());
                self.description_label.set_label("");
                self.description_label.set_visible(false);
                self.read_more.borrow().set_visible(false);
            }
        }

        let mut subs = data.subscribers.clone().unwrap_or_default();
        if !subs.is_empty() {
            subs.push_str(" subscribers");
        }
        if let Some(views) = data.views.as_ref().filter(|v| !v.is_empty()) {
            if subs.is_empty() {
                subs = views.clone()
            } else {
                subs = format!("{subs} • {views}")
            }
        }
        self.subscribers_label.set_label(&subs);

        let mut subscribed = data.subscribed;
        let channel_id = self.channel_id.borrow().clone();
        if !subscribed && !channel_id.is_empty() && self.ctx.net.caches().is_subscribed(&channel_id)
        {
            subscribed = true;
        }
        self.is_subscribed.set(subscribed);
        self.update_subscribe_button();

        let is_channel = data.is_channel;
        self.play_btn.set_visible(!is_channel);
        self.shuffle_btn.set_visible(!is_channel);
        self.radio_btn
            .set_visible(!is_channel && data.radio_id.is_some());

        if let Some(url) = data.banner.last().or_else(|| data.thumbnails.last()) {
            self.avatar.load(url);
        }

        if !self.ui_init.get() {
            self.section_widgets.borrow_mut().clear();
            self.cards.borrow_mut().clear();
            self.covers.borrow_mut().clear();
            self.likes.borrow_mut().clear();
            self.scrollers.borrow_mut().clear();
            self.card_strips.borrow_mut().clear();
            self.playing.clear();
            while let Some(child) = self.sections_box.first_child() {
                self.sections_box.remove(&child);
            }
            self.ui_init.set(true);
        }

        if let Some(songs) = &data.songs {
            self.add_songs_section("Top Songs", songs);
        }
        if let Some(s) = &data.albums {
            self.add_grid_section("Albums", s);
        }
        if let Some(s) = &data.singles {
            self.add_grid_section("Singles & EPs", s);
        }
        if let Some(s) = &data.videos {
            self.add_grid_section("Videos", s);
        }
        if let Some(s) = &data.playlists {
            self.add_grid_section("Playlists", s);
        }
        if let Some(s) = &data.featured_on {
            self.add_grid_section("Featured On", s);
        }
        if let Some(s) = &data.related {
            self.add_grid_section("Fans Might Also Like", s);
        }
    }

    /// The section box for a title: cleared when it exists, created otherwise.
    fn section_container(&self, title: &str, inner: bool) -> gtk::Box {
        if let Some(existing) = self.section_widgets.borrow().get(title).cloned() {
            let target = if inner {
                existing
                    .first_child()
                    .and_downcast::<gtk::Box>()
                    .unwrap_or(existing.clone())
            } else {
                existing.clone()
            };
            while let Some(child) = target.first_child() {
                target.remove(&child);
            }
            return target;
        }
        let outer = gtk::Box::builder()
            .orientation(gtk::Orientation::Vertical)
            .spacing(16)
            .build();
        self.section_widgets
            .borrow_mut()
            .insert(title.to_owned(), outer.clone());
        self.sections_box.append(&outer);
        if inner {
            let section_box = gtk::Box::builder()
                .orientation(gtk::Orientation::Vertical)
                .spacing(8)
                .build();
            outer.append(&section_box);
            section_box
        } else {
            outer
        }
    }

    fn limit_for(&self, title: &str, default: usize) -> usize {
        self.section_limits
            .borrow()
            .get(title)
            .copied()
            .unwrap_or(default)
    }

    // -- top songs --------------------------------------------------------

    fn add_songs_section(self: &Rc<Self>, title: &str, section: &SongSection) {
        if section.results.is_empty() {
            return;
        }
        let section_box = self.section_container(title, true);
        section_box.append(
            &gtk::Label::builder()
                .label(title)
                .css_classes(["heading"])
                .halign(gtk::Align::Start)
                .build(),
        );
        let list = gtk::ListBox::builder()
            .css_classes(["boxed-list", "songs-list"])
            .selection_mode(gtk::SelectionMode::None)
            .build();
        let weak = Rc::downgrade(self);
        list.connect_row_activated(move |_, row| {
            if let Some(p) = weak.upgrade() {
                let track =
                    unsafe { row.data::<Track>("track") }.map(|t| unsafe { t.as_ref() }.clone());
                if let Some(track) = track {
                    p.on_song_activated(&track);
                }
            }
        });
        let limit = self.limit_for(title, 5);
        for track in section.results.iter().take(limit) {
            list.append(&self.build_song_row(track));
        }
        section_box.append(&list);

        let has_more_online = section.browse_id.is_some();
        if section.results.len() > limit || has_more_online {
            let load_more = gtk::Button::builder()
                .label("Load More")
                .css_classes(["pill"])
                .halign(gtk::Align::Center)
                .margin_top(12)
                .build();
            let btn_box = gtk::Box::builder()
                .orientation(gtk::Orientation::Horizontal)
                .spacing(8)
                .halign(gtk::Align::Center)
                .build();
            btn_box.append(&load_more);
            let spinner = adw::Spinner::builder().visible(false).build();
            btn_box.append(&spinner);
            let weak = Rc::downgrade(self);
            let title_c = title.to_owned();
            let section_c = section.clone();
            load_more.connect_clicked(move |btn| {
                if let Some(p) = weak.upgrade() {
                    p.on_load_more_songs(&title_c, &section_c, &spinner, btn);
                }
            });
            section_box.append(&btn_box);
        }
    }

    /// The inline row the Python page built: art, title with badge, artists and album, duration, like.
    fn build_song_row(self: &Rc<Self>, track: &Track) -> gtk::ListBoxRow {
        let row = gtk::ListBoxRow::new();
        let inner = gtk::Box::builder()
            .orientation(gtk::Orientation::Horizontal)
            .spacing(12)
            .css_classes(["song-row"])
            .build();
        row.set_child(Some(&inner));
        unsafe { row.set_data("track", track.clone()) };
        let vid = track.video_id.0.clone();
        if !vid.is_empty() {
            self.playing.track(&row, &vid);
        }
        let img = CoverImage::in_context(&self.ctx, 56);
        img.widget().add_css_class("song-img");
        match &track.thumb {
            Some(url) => img.load(url),
            None => img.set_placeholder("media-optical-symbolic"),
        }
        inner.append(img.widget());
        self.covers.borrow_mut().push(img);

        let mut album_name = track
            .album
            .as_ref()
            .map(|a| a.name.clone())
            .unwrap_or_default();
        if album_name == track.title {
            album_name = "Single".to_owned();
        }
        let mut subtitle = track.artist.clone();
        if !album_name.is_empty() {
            subtitle = if subtitle.is_empty() {
                album_name
            } else {
                format!("{subtitle} • {album_name}")
            };
        }
        let vbox = gtk::Box::builder()
            .orientation(gtk::Orientation::Vertical)
            .spacing(2)
            .valign(gtk::Align::Center)
            .hexpand(true)
            .build();
        let title_box = gtk::Box::builder()
            .orientation(gtk::Orientation::Horizontal)
            .spacing(6)
            .build();
        title_box.append(
            &gtk::Label::builder()
                .label(&track.title)
                .halign(gtk::Align::Start)
                .ellipsize(gtk::pango::EllipsizeMode::End)
                .lines(1)
                .width_chars(1)
                .xalign(0.0)
                .build(),
        );
        if track.is_explicit {
            title_box.append(
                &gtk::Label::builder()
                    .label("E")
                    .css_classes(["explicit-badge"])
                    .valign(gtk::Align::Center)
                    .build(),
            );
        }
        vbox.append(&title_box);
        vbox.append(
            &gtk::Label::builder()
                .label(&subtitle)
                .halign(gtk::Align::Start)
                .ellipsize(gtk::pango::EllipsizeMode::End)
                .lines(1)
                .width_chars(1)
                .xalign(0.0)
                .css_classes(["dim-label", "caption"])
                .build(),
        );
        inner.append(&vbox);

        if let Some(d) = track.duration_seconds {
            inner.append(
                &gtk::Label::builder()
                    .label(format!("{}:{:02}", d / 60, d % 60))
                    .css_classes(["caption"])
                    .opacity(0.7)
                    .valign(gtk::Align::Center)
                    .margin_end(6)
                    .build(),
            );
        }
        if !vid.is_empty() {
            let like = LikeButton::new(self.ctx.player.clone());
            like.widget().set_valign(gtk::Align::Center);
            like.set_data(Some(VideoId(vid.clone())), Some(track.like_status));
            inner.append(like.widget());
            self.likes.borrow_mut().push(like);
        }

        let open_menu = {
            let weak = Rc::downgrade(self);
            let row = row.clone();
            let track = track.clone();
            Rc::new(move |x: f64, y: f64| {
                if let Some(p) = weak.upgrade() {
                    let opts = SongMenuOptions {
                        prefix: "row",
                        nav: Some(p.ctx.nav.clone()),
                        ctx: Some(p.ctx.clone()),
                        ..SongMenuOptions::default()
                    };
                    show_song_menu(&row, x, y, &track, &p.ctx.player, opts);
                }
            })
        };
        let right = gtk::GestureClick::builder()
            .button(gdk::BUTTON_SECONDARY)
            .build();
        let open = open_menu.clone();
        right.connect_pressed(move |_, _, x, y| open(x, y));
        row.add_controller(right);
        let long = gtk::GestureLongPress::new();
        long.connect_pressed(move |_, x, y| open_menu(x, y));
        row.add_controller(long);
        row
    }

    fn on_song_activated(self: &Rc<Self>, track: &Track) {
        if track.video_id.0.is_empty() {
            return;
        }
        let queue = self.build_queue_tracks();
        let start = queue
            .iter()
            .position(|t| t.video_id == track.video_id)
            .unwrap_or(0);
        self.ctx
            .player
            .play_tracks(queue, start, false, None, false);
    }

    /// Port of _build_queue_tracks: the top songs with the page's artist
    /// filled in wherever a row lacks one.
    fn build_queue_tracks(&self) -> Vec<Track> {
        let data = self.data.borrow();
        let Some(songs) = data.as_ref().and_then(|d| d.songs.as_ref()) else {
            return Vec::new();
        };
        let artist_name = self.artist_name.borrow().clone();
        let channel_id = self.channel_id.borrow().clone();
        songs
            .results
            .iter()
            .map(|song| {
                let mut track = song.clone();
                if track.artists.is_empty() {
                    track.artists = vec![Person {
                        name: if artist_name.is_empty() {
                            "Unknown Artist".to_owned()
                        } else {
                            artist_name.clone()
                        },
                        id: Some(channel_id.clone()),
                    }];
                } else {
                    for a in &mut track.artists {
                        if a.name.is_empty() {
                            a.name = artist_name.clone();
                        }
                        if a.id.is_none() && a.name == artist_name {
                            a.id = Some(channel_id.clone());
                        }
                    }
                }
                track.artist = track
                    .artists
                    .iter()
                    .map(|a| a.name.as_str())
                    .filter(|n| !n.is_empty())
                    .collect::<Vec<_>>()
                    .join(", ");
                if track.artist.is_empty() {
                    track.artist = artist_name.clone();
                }
                track
            })
            .collect()
    }

    /// Port of the Top Songs branch of on_load_more_clicked: more rows inline,
    /// then the full playlist behind the shelf.
    fn on_load_more_songs(
        self: &Rc<Self>,
        title: &str,
        section: &SongSection,
        spinner: &adw::Spinner,
        btn: &gtk::Button,
    ) {
        let limit = self.limit_for(title, 10);
        if section.results.len() > limit {
            self.section_limits
                .borrow_mut()
                .insert(title.to_owned(), limit + 20);
            self.update_ui();
            return;
        }
        let Some(browse_id) = section.browse_id.clone() else {
            return;
        };
        spinner.set_visible(true);
        btn.set_sensitive(false);
        let api = self.ctx.net.client().api();
        let browse = browse_id.clone();
        let handle = self
            .ctx
            .net
            .spawn(async move { playlists::get_playlist(&api, &browse, None).await });
        let weak = Rc::downgrade(self);
        let (spinner, btn, title) = (spinner.clone(), btn.clone(), title.to_owned());
        glib::spawn_future_local(async move {
            let Some(page) = weak.upgrade() else { return };
            let tracks = match handle.await {
                Ok(Ok(p)) => p.tracks,
                Ok(Err(err)) => {
                    tracing::warn!(%err, "load more failed");
                    Vec::new()
                }
                Err(_) => Vec::new(),
            };
            if tracks.is_empty() {
                spinner.set_visible(false);
                btn.set_sensitive(true);
                return;
            }
            let count = tracks.len();
            if let Some(data) = page.data.borrow_mut().as_mut() {
                if let Some(songs) = data.songs.as_mut() {
                    songs.results = tracks;
                    songs.browse_id = None;
                }
            }
            page.section_limits.borrow_mut().insert(title, count);
            page.update_ui();
        });
    }

    // -- card sections ----------------------------------------------------

    fn add_grid_section(self: &Rc<Self>, title: &str, section: &CardSection) {
        if section.results.is_empty() {
            return;
        }
        let container = self.section_container(title, false);
        container.append(
            &gtk::Label::builder()
                .label(title)
                .css_classes(["heading"])
                .halign(gtk::Align::Start)
                .build(),
        );
        let scroll_box = HorizontalScrollBox::new();
        let compact = self.ctx.compact.get();
        let strip = gtk::Box::builder()
            .orientation(gtk::Orientation::Horizontal)
            .spacing(if compact {
                STRIP_SPACING_COMPACT
            } else {
                STRIP_SPACING
            })
            .build();
        self.card_strips.borrow_mut().push(strip.clone());
        scroll_box.set_content(&strip);
        container.append(scroll_box.widget());
        self.scrollers.borrow_mut().push(scroll_box);

        let limit = self.limit_for(title, 10);
        for item in section.results.iter().take(limit) {
            let card = self.make_grid_card(item);
            strip.append(card.widget());
            self.cards.borrow_mut().push(card);
        }
        if section.results.len() > limit || section.params.is_some() {
            let cell = gtk::Box::builder()
                .orientation(gtk::Orientation::Vertical)
                .valign(gtk::Align::Center)
                .halign(gtk::Align::Center)
                .margin_start(16)
                .margin_end(16)
                .build();
            let more = gtk::Button::builder()
                .label("View All")
                .css_classes(["pill"])
                .build();
            more.set_cursor_from_name(Some("pointer"));
            let weak = Rc::downgrade(self);
            let (title_c, section_c) = (title.to_owned(), section.clone());
            more.connect_clicked(move |_| {
                if let Some(p) = weak.upgrade() {
                    p.on_view_all(&title_c, &section_c);
                }
            });
            cell.append(&more);
            strip.append(&cell);
        }
    }

    fn make_grid_card(self: &Rc<Self>, item: &MediaItem) -> Rc<MediaCard> {
        let card = MediaCard::new(
            &self.ctx,
            item.clone(),
            CardOptions {
                title_lines: 1,
                ..CardOptions::default()
            },
        );
        let weak = Rc::downgrade(self);
        card.connect_clicked(move |item| {
            if let Some(p) = weak.upgrade() {
                p.on_grid_child_activated(item);
            }
        });
        let weak = Rc::downgrade(self);
        let widget = card.widget().clone();
        let item_c = item.clone();
        let open = Rc::new(move |x: f64, y: f64| {
            if let Some(p) = weak.upgrade() {
                let data = item_c.clone();
                let extras = vec![MenuAction::new(
                    "Copy JSON (Debug)",
                    Section::Clipboard,
                    move || {
                        if let Ok(text) = serde_json::to_string_pretty(&data) {
                            copy_to_clipboard(&text);
                        }
                    },
                )];
                show_item_menu_with(&widget, x, y, &item_c, &p.ctx, extras);
            }
        });
        let right = gtk::GestureClick::builder()
            .button(gdk::BUTTON_SECONDARY)
            .build();
        let o = open.clone();
        right.connect_released(move |_, _, x, y| o(x, y));
        card.widget().add_controller(right);
        let long = gtk::GestureLongPress::new();
        long.connect_pressed(move |_, x, y| open(x, y));
        card.widget().add_controller(long);
        card
    }

    /// Port of the grid branch of on_load_more_clicked: the discography page
    /// when the shelf has a browse id, everything inline otherwise.
    fn on_view_all(self: &Rc<Self>, title: &str, section: &CardSection) {
        match &section.browse_id {
            None => {
                self.section_limits
                    .borrow_mut()
                    .insert(title.to_owned(), section.results.len());
                self.update_ui();
            }
            Some(browse_id) => {
                let page_title = format!("{} - {title}", self.artist_name.borrow());
                self.ctx.nav.go(NavRequest::Discography {
                    channel_id: self.channel_id.borrow().clone(),
                    title: page_title,
                    browse_id: Some(browse_id.clone()),
                    params: section.params.clone(),
                    initial: Vec::new(),
                });
            }
        }
    }

    /// Port of on_grid_child_activated: videos play, artists open, the rest opens as a playlist.
    fn on_grid_child_activated(&self, item: &MediaItem) {
        match item.kind {
            ItemKind::Song | ItemKind::Video => {
                if let Some(track) = item.to_track() {
                    self.ctx
                        .player
                        .play_tracks(vec![track], 0, false, None, false);
                }
            }
            ItemKind::Artist => self.ctx.nav.go(NavRequest::Artist {
                id: Some(item.id.clone()),
                name: item.title.clone(),
            }),
            ItemKind::Album => self.ctx.nav.go(NavRequest::Album {
                id: item.id.clone(),
                title: item.title.clone(),
                thumb: item.thumb.clone(),
            }),
            ItemKind::Playlist => self.ctx.nav.go(NavRequest::Playlist {
                id: item.id.clone(),
                title: item.title.clone(),
                thumb: item.thumb.clone(),
            }),
        }
    }

    // -- actions ----------------------------------------------------------

    fn on_radio_clicked(&self) {
        let radio_id = self.data.borrow().as_ref().and_then(|d| d.radio_id.clone());
        match radio_id {
            Some(id) => self.ctx.player.start_radio(None, Some(id)),
            None => {
                // Fall back to a radio from the first top song.
                let first = self
                    .data
                    .borrow()
                    .as_ref()
                    .and_then(|d| d.songs.as_ref())
                    .and_then(|s| s.results.first())
                    .map(|t| t.video_id.0.clone())
                    .filter(|v| !v.is_empty());
                if let Some(vid) = first {
                    self.ctx.player.start_radio(Some(vid), None);
                }
            }
        }
    }

    fn on_subscribe_clicked(self: &Rc<Self>) {
        let channel_id = self.channel_id.borrow().clone();
        if channel_id.is_empty() {
            return;
        }
        let old = self.is_subscribed.get();
        let new = !old;
        self.is_subscribed.set(new);
        self.update_subscribe_button();
        let api = self.ctx.net.client().api();
        let id = channel_id.clone();
        let handle = self.ctx.net.spawn(async move {
            if new {
                artist::subscribe(&api, &id).await
            } else {
                artist::unsubscribe(&api, &id).await
            }
        });
        let weak = Rc::downgrade(self);
        glib::spawn_future_local(async move {
            let Some(page) = weak.upgrade() else { return };
            match handle.await {
                Ok(Ok(())) => page.ctx.net.caches().set_subscribed(&channel_id, new),
                Ok(Err(err)) => {
                    tracing::warn!(%err, channel_id, "failed to toggle subscription");
                    page.is_subscribed.set(old);
                    page.update_subscribe_button();
                }
                Err(_) => {}
            }
        });
    }

    fn update_subscribe_button(&self) {
        if self.is_subscribed.get() {
            self.subscribe_btn.set_icon_name("starred-symbolic");
            self.subscribe_btn.set_tooltip_text(Some("Unsubscribe"));
            self.subscribe_btn.add_css_class("liked-button");
        } else {
            self.subscribe_btn.set_icon_name("non-starred-symbolic");
            self.subscribe_btn.set_tooltip_text(Some("Subscribe"));
            self.subscribe_btn.remove_css_class("liked-button");
        }
    }

    fn toggle_description(self: &Rc<Self>) {
        let expanded = !self.description_expanded.get();
        self.description_expanded.set(expanded);
        let clean = self.description_clean.borrow().clone();
        let text = if expanded {
            self.description_label.set_label(&clean);
            self.description_label.set_lines(0);
            "Show less"
        } else {
            self.description_label.set_label(&preview_of(&clean));
            self.description_label.set_lines(3);
            "Read more"
        };
        let old = self.read_more.borrow().clone();
        self.description_box.remove(&old);
        let label = read_more_label(text);
        self.description_box.append(&label);
        self.read_more.replace(label);
        self.connect_read_more();
    }

    fn on_banner_right_click(&self, x: f64, y: f64) {
        let Some(url) = self.avatar.url() else { return };
        let menu = gio::Menu::new();
        menu.append(Some("Copy Banner URL"), Some("banner.copy_url"));
        let action = gio::SimpleAction::new("copy_url", None);
        action.connect_activate(move |_, _| copy_to_clipboard(&url));
        let group = gio::SimpleActionGroup::new();
        group.add_action(&action);
        self.banner_overlay
            .insert_action_group("banner", Some(&group));
        let popover = gtk::PopoverMenu::from_model(Some(&menu));
        popover.set_parent(&self.banner_overlay);
        popover.set_pointing_to(Some(&gdk::Rectangle::new(x as i32, y as i32, 1, 1)));
        popover.connect_closed(|p| {
            let p = p.clone();
            glib::idle_add_local_once(move || p.unparent());
        });
        popover.popup();
    }

    pub fn set_compact_mode(&self, compact: bool) {
        self.compact.set(compact);
        for strip in self.card_strips.borrow().iter() {
            strip.set_spacing(if compact {
                STRIP_SPACING_COMPACT
            } else {
                STRIP_SPACING
            });
        }
        for card in self.cards.borrow().iter() {
            card.set_compact(compact);
        }
        if compact {
            self.stack.add_css_class("compact");
            self.banner_overlay
                .set_size_request(-1, BANNER_HEIGHT_COMPACT);
            self.banner_wrapper
                .set_size_request(-1, BANNER_HEIGHT_COMPACT);
            self.info_box.set_margin_top(INFO_TOP_COMPACT);
        } else {
            self.stack.remove_css_class("compact");
            self.banner_overlay.set_size_request(-1, BANNER_HEIGHT);
            self.banner_wrapper.set_size_request(-1, BANNER_HEIGHT);
            self.info_box.set_margin_top(INFO_TOP);
        }
        self.info_box.set_halign(gtk::Align::Start);
        self.banner_wrapper.set_halign(gtk::Align::Fill);
        self.avatar.widget().set_halign(gtk::Align::Fill);
        self.avatar.widget().set_hexpand(true);
        self.name_label.set_halign(gtk::Align::Start);
        self.subscribers_label.set_halign(gtk::Align::Start);
        self.description_label.set_halign(gtk::Align::Start);
        self.read_more.borrow().set_halign(gtk::Align::Start);
        let _ = &self.content_box;
    }
}

fn read_more_label(text: &str) -> gtk::Label {
    let label = gtk::Label::builder()
        .use_markup(true)
        .css_classes(["caption"])
        .halign(gtk::Align::Start)
        .build();
    label.set_markup(&format!("<a href='toggle'>{text}</a>"));
    label
}

/// The first 280 characters cut at a word, with an ellipsis.
fn preview_of(text: &str) -> String {
    let head: String = text.chars().take(DESCRIPTION_PREVIEW).collect();
    match head.rfind(' ') {
        Some(i) => format!("{}…", &head[..i]),
        None => format!("{head}…"),
    }
}

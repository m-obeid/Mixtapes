//! Port of ui/pages/playlist.py, which album.py only subclassed to force the
//! album view. One page serves user playlists, Liked Music, albums (MPRE and
//! OLAK ids), uploaded albums, radios and the virtual Downloads list.
//!
//! The track list is a ListView over a flattened model: a one-item header
//! store carrying the whole header widget, then the filtered track store, so
//! the header scrolls with the rows and the view stays virtualized.

use std::cell::{Cell, RefCell};
use std::collections::{HashMap, HashSet};
use std::path::PathBuf;
use std::rc::{Rc, Weak};
use std::time::Duration;

use adw::prelude::*;
use gtk::{gdk, gio, glib};

use crate::model::{LikeStatus, Named, Person, Track};
use crate::net::cache::{CachedMeta, CachedPlaylist, SortMetric};
use crate::net::covers::save_playlist_cover;
use crate::net::playlists::{self, PlaylistDetails};
use crate::net::ytmusic::{AuthState, NetError};
use crate::state::track_object::TrackObject;
use crate::ui::context::{NavRequest, UiContext};
use crate::ui::context_menu::{MenuAction, Section, SongMenuOptions, show_song_menu};
use crate::ui::cover::CoverImage;
use crate::ui::pages::track_list::{needs_metric, TrackList, SORT_ADDED, SORT_DEFAULT, SORT_VIEWS};
use crate::ui::widgets::add_to_playlist::{AddToPlaylistPopover, mark_playlist_used};
use crate::ui::widgets::track_row::{TrackRow, TrackRowHost};
use crate::ui::{copy_to_clipboard, toast};

// Sort dropdown positions. The first five come straight off the track data;
// the last two need a side fetch, so they are appended rather than slotted in.
const SORT_LABELS: [&str; 7] = ["Default", "Title (A-Z)", "Artist (A-Z)", "Album (A-Z)", "Duration", "Most viewed", "Recently added"];

/// Delay the live refresh when something is on screen already, so the
/// chunked re-render does not fight the page-open animation.
const AUTO_REFRESH_DELAY: Duration = Duration::from_millis(2000);
/// Long enough for AdwNavigationView's page transition to finish.
const TRANSITION_GATE: Duration = Duration::from_millis(350);
const FILTER_DEBOUNCE: Duration = Duration::from_millis(150);
/// Longest side of an uploaded playlist cover.
const COVER_MAX_PIXELS: i32 = 1024;
const INITIAL_LIMIT: usize = 200;
const PLACEHOLDER_ICON: &str = "media-playlist-audio-symbolic";

/// What a card knew about the playlist before the page fetched it.
#[derive(Clone, Debug, Default)]
pub struct InitialData {
    pub title: String,
    pub thumb: Option<String>,
    pub author: Option<String>,
}

type TitleListener = Box<dyn Fn(&str)>;
type PageAction = Box<dyn Fn(&Rc<PlaylistPage>)>;

/// The header strings _fetch_playlist_details built for update_ui.
struct HeaderText {
    title: String,
    description: String,
    meta1: String,
    meta2: String,
}

enum Fetched {
    Details(Box<PlaylistDetails>),
    /// An OLAK chart list read raw: title and rows, nothing else.
    Raw { title: Option<String>, tracks: Vec<Track> },
}

pub struct PlaylistPage {
    stack: adw::ViewStack,
    songs_list: gtk::ListView,
    header_container: gtk::Box,
    header_info_box: gtk::Box,
    cover_wrapper: gtk::Box,
    cover: Rc<CoverImage>,
    details_col: gtk::Box,
    name_label: gtk::Label,
    description_label: gtk::Label,
    read_more: RefCell<gtk::Label>,
    desc_box: gtk::Box,
    meta_label: gtk::Label,
    stats_label: gtk::Label,
    actions_box: gtk::Box,
    more_btn: gtk::MenuButton,
    more_menu: gio::Menu,
    sort_dropdown: gtk::DropDown,
    sort_dir_btn: gtk::Button,
    sort_row: gtk::Box,
    select_btn: gtk::ToggleButton,
    content_spinner: adw::Spinner,
    selection_bar: gtk::Box,
    selection_count_label: gtk::Label,
    sel_add_btn: gtk::Button,
    sel_remove_btn: gtk::Button,
    empty_label: gtk::Label,
    load_more_spinner: adw::Spinner,
    track_store: gio::ListStore,
    filter_model: gtk::FilterListModel,
    track_filter: gtk::CustomFilter,
    flatten: gtk::FlattenListModel,
    ctx: Rc<UiContext>,

    playlist_id: RefCell<Option<String>>,
    audio_playlist_id: RefCell<Option<String>>,
    title_text: RefCell<String>,
    description_text: RefCell<String>,
    privacy_text: RefCell<Option<String>>,
    full_description: RefCell<String>,
    description_expanded: Cell<bool>,
    /// The rows: order, search, selection and sort metrics in one place.
    tracks: RefCell<TrackList>,
    current_limit: Cell<usize>,
    is_loading_more: Cell<bool>,
    is_fully_loaded: Cell<bool>,
    is_fully_fetched: Cell<bool>,
    is_background_fetching: Cell<bool>,
    pending_queue_append: Cell<bool>,
    pending_filter_text: RefCell<String>,
    filter_debounce: RefCell<Option<glib::SourceId>>,
    populate_token: Cell<u64>,
    multi_select: Cell<bool>,
    is_owned: Cell<bool>,
    is_editable: Cell<bool>,
    is_saved_to_library: Cell<bool>,
    is_album_view: Cell<bool>,
    is_previewing_cover: Cell<bool>,
    more_menu_dirty: Cell<bool>,
    more_menu_pending_owned: Cell<bool>,
    selected_cover_path: RefCell<Option<PathBuf>>,
    on_title: RefCell<Option<TitleListener>>,
    compact: Cell<bool>,
    me: RefCell<Weak<PlaylistPage>>,
    /// The deferred write of the last fetch, latest wins.
    cache_write: RefCell<Option<glib::SourceId>>,
    /// Header fields of the last fetch, reused when the full track list lands.
    cache_entry: RefCell<Option<CachedPlaylist>>,
}

impl PlaylistPage {
    pub fn new(ctx: Rc<UiContext>) -> Rc<Self> {
        // -- header ------------------------------------------------------
        let header_container = gtk::Box::builder().orientation(gtk::Orientation::Vertical).margin_top(24).margin_bottom(12).build();
        let header_info_box = gtk::Box::builder().orientation(gtk::Orientation::Horizontal).spacing(24).valign(gtk::Align::Start).build();
        let cover = CoverImage::new(ctx.net.clone(), 200);
        cover.widget().set_valign(gtk::Align::Start);
        cover.set_placeholder(PLACEHOLDER_ICON);
        let cover_wrapper = gtk::Box::builder().overflow(gtk::Overflow::Hidden).css_classes(["rounded"]).valign(gtk::Align::Start).build();
        cover_wrapper.set_size_request(200, 200);
        cover_wrapper.append(cover.widget());
        header_info_box.append(&cover_wrapper);

        let details_col = gtk::Box::builder().orientation(gtk::Orientation::Vertical).spacing(6).valign(gtk::Align::Center).hexpand(true).build();
        let name_label = gtk::Label::builder().label("Playlist Title").css_classes(["title-1"]).wrap(true).wrap_mode(gtk::pango::WrapMode::WordChar).justify(gtk::Justification::Left).halign(gtk::Align::Start).vexpand(false).hexpand(true).ellipsize(gtk::pango::EllipsizeMode::End).lines(3).build();
        details_col.append(&name_label);
        let description_label = gtk::Label::builder().css_classes(["body"]).wrap(true).wrap_mode(gtk::pango::WrapMode::WordChar).justify(gtk::Justification::Left).halign(gtk::Align::Start).vexpand(false).hexpand(true).build();
        let read_more = read_more_label("Read more");
        read_more.set_visible(false);
        let desc_box = gtk::Box::builder().orientation(gtk::Orientation::Vertical).spacing(0).visible(false).build();
        desc_box.append(&description_label);
        desc_box.append(&read_more);
        details_col.append(&desc_box);
        let meta_label = gtk::Label::builder().css_classes(["caption"]).wrap(true).wrap_mode(gtk::pango::WrapMode::WordChar).justify(gtk::Justification::Left).halign(gtk::Align::Start).hexpand(true).use_markup(true).build();
        details_col.append(&meta_label);
        let stats_label = gtk::Label::builder().css_classes(["caption"]).wrap(true).wrap_mode(gtk::pango::WrapMode::WordChar).justify(gtk::Justification::Left).halign(gtk::Align::Start).hexpand(true).build();
        details_col.append(&stats_label);

        let actions_box = gtk::Box::builder().orientation(gtk::Orientation::Horizontal).spacing(12).margin_top(12).build();
        let play_btn = gtk::Button::builder().label("Play").css_classes(["suggested-action", "pill"]).build();
        actions_box.append(&play_btn);
        let shuffle_btn = gtk::Button::builder().icon_name("media-playlist-shuffle-symbolic").css_classes(["circular"]).valign(gtk::Align::Center).halign(gtk::Align::Center).build();
        shuffle_btn.set_size_request(48, 48);
        actions_box.append(&shuffle_btn);
        let more_btn = gtk::MenuButton::builder().icon_name("view-more-symbolic").css_classes(["circular"]).tooltip_text("More Options").build();
        more_btn.set_size_request(48, 48);
        let more_menu = gio::Menu::new();
        more_btn.set_menu_model(Some(&more_menu));
        actions_box.append(&more_btn);
        details_col.append(&actions_box);
        header_info_box.append(&details_col);
        header_container.append(&header_info_box);

        // -- sort row ----------------------------------------------------
        let sort_dropdown = gtk::DropDown::from_strings(&SORT_LABELS);
        sort_dropdown.set_valign(gtk::Align::Center);
        sort_dropdown.add_css_class("pill");
        sort_dropdown.add_css_class("sort-dropdown");
        let sort_dir_btn = gtk::Button::builder().icon_name("view-sort-ascending-symbolic").css_classes(["flat", "circular"]).tooltip_text("Toggle sort direction").build();
        let sort_row = gtk::Box::builder().orientation(gtk::Orientation::Horizontal).spacing(6).margin_top(12).css_classes(["playlist-sort-row"]).visible(false).build();
        sort_row.append(&sort_dropdown);
        sort_row.append(&sort_dir_btn);
        let select_btn = gtk::ToggleButton::builder().icon_name("selection-mode-symbolic").css_classes(["flat", "circular"]).tooltip_text("Select multiple songs").build();
        let spacer = gtk::Box::builder().hexpand(true).build();
        // Small inline spinner just left of the multi-select button while tracks load.
        let content_spinner = adw::Spinner::builder().valign(gtk::Align::Center).margin_end(4).visible(false).build();
        content_spinner.set_size_request(18, 18);
        sort_row.append(&spacer);
        sort_row.append(&content_spinner);
        sort_row.append(&select_btn);
        header_container.append(&sort_row);

        // -- selection bar -----------------------------------------------
        let selection_bar = gtk::Box::builder().orientation(gtk::Orientation::Horizontal).spacing(6).margin_top(8).margin_bottom(4).visible(false).build();
        let selection_count_label = gtk::Label::builder().label("0 selected").css_classes(["caption"]).ellipsize(gtk::pango::EllipsizeMode::End).hexpand(true).xalign(0.0).build();
        selection_bar.append(&selection_count_label);
        let sel_play_btn = gtk::Button::builder().icon_name("media-playback-start-symbolic").css_classes(["flat"]).tooltip_text("Play selected").build();
        selection_bar.append(&sel_play_btn);
        let sel_add_btn = gtk::Button::builder().icon_name("list-add-symbolic").css_classes(["flat"]).tooltip_text("Add selected to playlist").build();
        selection_bar.append(&sel_add_btn);
        let sel_remove_btn = gtk::Button::builder().icon_name("user-trash-symbolic").css_classes(["flat", "destructive-action"]).tooltip_text("Remove selected from playlist").visible(false).build();
        selection_bar.append(&sel_remove_btn);
        let sel_overflow_btn = gtk::MenuButton::builder().icon_name("view-more-symbolic").css_classes(["flat"]).tooltip_text("More").build();
        let sel_overflow_menu = gio::Menu::new();
        sel_overflow_menu.append(Some("Select All"), Some("page.sel_all"));
        sel_overflow_menu.append(Some("Deselect All"), Some("page.sel_none"));
        sel_overflow_btn.set_menu_model(Some(&sel_overflow_menu));
        selection_bar.append(&sel_overflow_btn);
        // Cancel is an X icon so it fits next to the others on narrow viewports.
        let sel_cancel_btn = gtk::Button::builder().icon_name("window-close-symbolic").css_classes(["flat"]).tooltip_text("Cancel selection").build();
        selection_bar.append(&sel_cancel_btn);
        header_container.append(&selection_bar);

        let empty_label = gtk::Label::builder().label("This playlist has no songs").css_classes(["dim-label"]).margin_top(24).halign(gtk::Align::Center).visible(false).build();
        header_container.append(&empty_label);

        // -- models and list view ----------------------------------------
        let header_store = gio::ListStore::new::<glib::Object>();
        header_store.append(&glib::Object::new::<glib::Object>());
        let track_store = gio::ListStore::new::<TrackObject>();
        // No filter attached until a search runs: a filter callback per item on
        // every items-changed is wasted work while the search bar is empty.
        let filter_model = gtk::FilterListModel::new(Some(track_store.clone()), None::<gtk::CustomFilter>);
        let master = gio::ListStore::new::<gio::ListModel>();
        master.append(&header_store);
        master.append(&filter_model);
        let flatten = gtk::FlattenListModel::new(Some(master));
        let selection = gtk::NoSelection::new(Some(flatten.clone()));
        let factory = gtk::SignalListItemFactory::new();
        let songs_list = gtk::ListView::new(Some(selection), Some(factory.clone()));
        songs_list.add_css_class("playlist-view");
        songs_list.set_margin_start(12);
        songs_list.set_margin_end(12);
        songs_list.set_margin_bottom(0);

        let scrolled = gtk::ScrolledWindow::builder().hscrollbar_policy(gtk::PolicyType::Never).vscrollbar_policy(gtk::PolicyType::Automatic).vexpand(true).build();

        crate::ui::suppress_hover_while_scrolling(&scrolled);
        let clamp = adw::ClampScrollable::builder().maximum_size(1024).tightening_threshold(600).child(&songs_list).build();
        scrolled.set_child(Some(&clamp));
        let main_box = gtk::Box::builder().orientation(gtk::Orientation::Vertical).build();
        main_box.append(&scrolled);
        let load_more_spinner = adw::Spinner::builder().halign(gtk::Align::Center).margin_top(12).margin_bottom(12).visible(false).build();
        load_more_spinner.set_size_request(24, 24);
        main_box.append(&load_more_spinner);

        let stack = adw::ViewStack::new();
        let loading_box = gtk::Box::builder().orientation(gtk::Orientation::Vertical).spacing(12).valign(gtk::Align::Center).halign(gtk::Align::Center).build();
        let spinner = adw::Spinner::new();
        spinner.set_size_request(32, 32);
        loading_box.append(&spinner);
        stack.add_named(&loading_box, Some("loading"));
        stack.add_named(&main_box, Some("content"));

        let track_filter = gtk::CustomFilter::new(|_| true);

        let page = Rc::new(Self {
            stack,
            songs_list,
            header_container,
            header_info_box,
            cover_wrapper,
            cover,
            details_col,
            name_label,
            description_label,
            read_more: RefCell::new(read_more),
            desc_box,
            meta_label,
            stats_label,
            actions_box,
            more_btn,
            more_menu,
            sort_dropdown,
            sort_dir_btn,
            sort_row,
            select_btn,
            content_spinner,
            selection_bar,
            selection_count_label,
            sel_add_btn,
            sel_remove_btn,
            empty_label,
            load_more_spinner,
            track_store,
            filter_model,
            track_filter,
            flatten,
            ctx,
            playlist_id: RefCell::new(None),
            audio_playlist_id: RefCell::new(None),
            title_text: RefCell::new(String::new()),
            description_text: RefCell::new(String::new()),
            privacy_text: RefCell::new(None),
            full_description: RefCell::new(String::new()),
            description_expanded: Cell::new(false),
            tracks: RefCell::new(TrackList::default()),
            current_limit: Cell::new(INITIAL_LIMIT),
            is_loading_more: Cell::new(false),
            is_fully_loaded: Cell::new(false),
            is_fully_fetched: Cell::new(false),
            is_background_fetching: Cell::new(false),
            pending_queue_append: Cell::new(false),
            pending_filter_text: RefCell::new(String::new()),
            filter_debounce: RefCell::new(None),
            populate_token: Cell::new(0),
            multi_select: Cell::new(false),
            is_owned: Cell::new(false),
            is_editable: Cell::new(false),
            is_saved_to_library: Cell::new(false),
            is_album_view: Cell::new(false),
            is_previewing_cover: Cell::new(false),
            more_menu_dirty: Cell::new(true),
            more_menu_pending_owned: Cell::new(false),
            selected_cover_path: RefCell::new(None),
            on_title: RefCell::new(None),
            compact: Cell::new(false),
            me: RefCell::new(Weak::new()),
            cache_write: RefCell::new(None),
            cache_entry: RefCell::new(None),
        });
        page.me.replace(Rc::downgrade(&page));

        // The filter reads the page's search text, like _track_filter_func.
        {
            let weak = Rc::downgrade(&page);
            page.track_filter.set_filter_func(move |obj| {
                let Some(p) = weak.upgrade() else { return true };
                let list = p.tracks.borrow();
                let text = list.filter();
                if text.is_empty() {
                    return true;
                }
                obj.downcast_ref::<TrackObject>().is_some_and(|t| t.with_track(|track| track.title.to_lowercase().contains(text) || track.artist.to_lowercase().contains(text)))
            });
        }
        page.wire_factory(&factory);
        page.wire_header(&play_btn, &shuffle_btn, &sel_play_btn, &sel_cancel_btn);
        page.install_actions();
        page.wire_downloads();
        {
            let weak = Rc::downgrade(&page);
            scrolled.vadjustment().connect_value_changed(move |adj| {
                if let Some(p) = weak.upgrade() {
                    p.on_scroll(adj);
                }
            });
            let weak = Rc::downgrade(&page);
            page.stack.connect_map(move |_| {
                if let Some(p) = weak.upgrade() {
                    p.on_map();
                }
            });
            let weak = Rc::downgrade(&page);
            page.stack.connect_unmap(move |_| {
                if let Some(p) = weak.upgrade() {
                    p.emit_title("");
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
        }
        page
    }

    pub fn widget(&self) -> &adw::ViewStack {
        &self.stack
    }

    pub fn playlist_id(&self) -> Option<String> {
        self.playlist_id.borrow().clone()
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

    /// Demo hook: what the Play button does.
    /// Demo hook: set the playlist cover from a file, what the edit dialog
    /// does once a crop has been chosen.
    pub fn set_cover_for_demo(self: &Rc<Self>, image: PathBuf) {
        let old_cover = self.cover.url().unwrap_or_default();
        let title = self.title_text.borrow().clone();
        let desc = self.description_text.borrow().clone();
        let privacy = self.privacy_text.borrow().clone().unwrap_or_else(|| "PUBLIC".to_owned());
        self.is_previewing_cover.set(true);
        self.cover.load(&image.to_string_lossy());
        self.save_edits(title.clone(), desc.clone(), privacy.clone(), title, desc, privacy, Some(image), old_cover);
    }

    /// Demo hook: type into the search box and pick a sort order.
    pub fn sift_for_demo(self: &Rc<Self>, filter: Option<&str>, sort: Option<u32>) {
        if let Some(sort) = sort {
            self.sort_dropdown.set_selected(sort);
            self.on_sort_changed(sort);
        }
        if let Some(text) = filter {
            self.filter_content(text);
        }
    }

    pub fn press_play(self: &Rc<Self>) {
        self.on_play_clicked();
    }

    /// The inline spinner the header-bar refresh polls, like Python's content_spinner.
    pub fn content_spinner_visible(&self) -> bool {
        self.content_spinner.is_visible()
    }

    /// Whether the header-bar refresh button applies: user playlists only,
    /// not albums, uploads or derived content.
    pub fn is_refreshable(&self) -> bool {
        let Some(pid) = self.playlist_id() else { return false };
        !(pid.starts_with("MPRE") || pid.starts_with("OLAK") || pid.starts_with("FEmusic_library_privately_owned") || pid.is_empty())
    }

    // -- wiring -----------------------------------------------------------

    fn wire_factory(self: &Rc<Self>, factory: &gtk::SignalListItemFactory) {
        let weak = Rc::downgrade(self);
        factory.connect_setup(move |_, item| {
            let Some(item) = item.downcast_ref::<gtk::ListItem>() else { return };
            let Some(page) = weak.upgrade() else { return };
            let weak_page: Weak<PlaylistPage> = Rc::downgrade(&page);
            let host: Weak<dyn TrackRowHost> = weak_page;
            let row = TrackRow::new(&page.ctx, host);
            item.set_child(Some(row.bin()));
            item.set_selectable(false);
            item.set_activatable(false);
            unsafe { item.set_data("track-row", row) };
        });
        let weak = Rc::downgrade(self);
        factory.connect_bind(move |_, item| {
            let Some(item) = item.downcast_ref::<gtk::ListItem>() else { return };
            let Some(page) = weak.upgrade() else { return };
            let Some(row) = row_of(item) else { return };
            match item.item().and_downcast::<TrackObject>() {
                Some(track) => row.bind(&track.track(), item.position()),
                None => row.bind_header(page.header_container.upcast_ref()),
            }
        });
        factory.connect_unbind(move |_, item| {
            let Some(item) = item.downcast_ref::<gtk::ListItem>() else { return };
            let Some(row) = row_of(item) else { return };
            if item.item().and_downcast::<TrackObject>().is_some() {
                row.unbind();
            } else {
                row.bin().set_child(gtk::Widget::NONE);
            }
        });
        factory.connect_teardown(|_, item| {
            if let Some(item) = item.downcast_ref::<gtk::ListItem>() {
                item.set_child(gtk::Widget::NONE);
                unsafe {
                    let _ = item.steal_data::<Rc<TrackRow>>("track-row");
                }
            }
        });
        let weak = Rc::downgrade(self);
        self.songs_list.connect_activate(move |_, position| {
            if let Some(p) = weak.upgrade() {
                p.on_list_activate(position);
            }
        });
    }

    fn wire_header(self: &Rc<Self>, play_btn: &gtk::Button, shuffle_btn: &gtk::Button, sel_play_btn: &gtk::Button, sel_cancel_btn: &gtk::Button) {
        let weak = Rc::downgrade(self);
        play_btn.connect_clicked(move |_| {
            if let Some(p) = weak.upgrade() {
                p.on_play_clicked();
            }
        });
        let weak = Rc::downgrade(self);
        shuffle_btn.connect_clicked(move |_| {
            if let Some(p) = weak.upgrade() {
                p.on_shuffle_clicked();
            }
        });
        let weak = Rc::downgrade(self);
        self.more_btn.connect_active_notify(move |btn| {
            if let Some(p) = weak.upgrade() {
                if btn.is_active() && p.more_menu_dirty.get() {
                    p.rebuild_more_menu(p.more_menu_pending_owned.get());
                    p.more_menu_dirty.set(false);
                }
            }
        });
        let weak = Rc::downgrade(self);
        self.sort_dropdown.connect_selected_notify(move |dd| {
            if let Some(p) = weak.upgrade() {
                p.on_sort_changed(dd.selected());
            }
        });
        let weak = Rc::downgrade(self);
        self.sort_dir_btn.connect_clicked(move |_| {
            if let Some(p) = weak.upgrade() {
                {
                    let mut list = p.tracks.borrow_mut();
                    let flipped = !list.descending();
                    list.set_order(p.sort_dropdown.selected(), flipped);
                }
                p.refresh_sort_dir_icon();
                p.reorder_playlist(p.sort_dropdown.selected());
            }
        });
        let weak = Rc::downgrade(self);
        self.select_btn.connect_toggled(move |btn| {
            if let Some(p) = weak.upgrade() {
                p.on_select_toggled(btn.is_active());
            }
        });
        let weak = Rc::downgrade(self);
        sel_play_btn.connect_clicked(move |_| {
            if let Some(p) = weak.upgrade() {
                let tracks = p.selected_tracks();
                if !tracks.is_empty() {
                    p.ctx.player.play_tracks(tracks, 0, false, None, false);
                }
            }
        });
        let weak = Rc::downgrade(self);
        self.sel_add_btn.connect_clicked(move |btn| {
            if let Some(p) = weak.upgrade() {
                let page = p.clone();
                AddToPlaylistPopover::show(&p.ctx, btn, move |pid| page.do_sel_add_to_playlist(&pid));
            }
        });
        let weak = Rc::downgrade(self);
        self.sel_remove_btn.connect_clicked(move |_| {
            if let Some(p) = weak.upgrade() {
                p.on_sel_remove();
            }
        });
        let select_btn = self.select_btn.clone();
        sel_cancel_btn.connect_clicked(move |_| select_btn.set_active(false));

        let weak = Rc::downgrade(self);
        self.meta_label.connect_activate_link(move |_, uri| {
            if let (Some(p), Some(aid)) = (weak.upgrade(), uri.strip_prefix("artist:")) {
                p.ctx.nav.go(NavRequest::Artist { id: Some(aid.to_owned()), name: "Artist".to_owned() });
                return glib::Propagation::Stop;
            }
            glib::Propagation::Proceed
        });
        self.connect_read_more();

        let open_cover_menu = {
            let weak = Rc::downgrade(self);
            Rc::new(move |x: f64, y: f64| {
                if let Some(p) = weak.upgrade() {
                    p.on_cover_right_click(x, y);
                }
            })
        };
        let right = gtk::GestureClick::builder().button(gdk::BUTTON_SECONDARY).build();
        let open = open_cover_menu.clone();
        right.connect_pressed(move |_, _, x, y| open(x, y));
        self.cover_wrapper.add_controller(right);
        let long = gtk::GestureLongPress::new();
        long.connect_pressed(move |_, x, y| open_cover_menu(x, y));
        self.cover_wrapper.add_controller(long);
    }

    fn connect_read_more(self: &Rc<Self>) {
        let weak = Rc::downgrade(self);
        self.read_more.borrow().connect_activate_link(move |_, _| {
            if let Some(p) = weak.upgrade() {
                // Deferred: swapping the label during the signal is unsafe.
                glib::idle_add_local_once(move || p.toggle_description());
            }
            glib::Propagation::Stop
        });
    }

    /// The "page." actions the more menu and selection overflow use.
    /// Repaint row badges as downloads are queued, finish, or are removed.
    fn wire_downloads(self: &Rc<Self>) {
        let weak = Rc::downgrade(self);
        self.ctx.on_download(move |event| {
            let Some(page) = weak.upgrade() else { return false };
            let video_id = match event {
                crate::downloads::Event::Queued { video_id } | crate::downloads::Event::Removed { video_id } => video_id.clone(),
                crate::downloads::Event::Item { video_id, ok: true, .. } => video_id.clone(),
                _ => return true,
            };
            for row in page.live_rows() {
                if row.video_id().as_deref() == Some(video_id.as_str()) {
                    row.show_download_state(&video_id);
                }
            }
            true
        });
    }

    fn install_actions(self: &Rc<Self>) {
        let group = gio::SimpleActionGroup::new();
        let add = |name: &str, f: PageAction| {
            let action = gio::SimpleAction::new(name, None);
            let weak = Rc::downgrade(self);
            action.connect_activate(move |_, _| {
                if let Some(p) = weak.upgrade() {
                    f(&p);
                }
            });
            group.add_action(&action);
        };
        add("show_add_all_to_playlist", Box::new(|p| p.on_show_add_all_to_playlist()));
        add("sel_all", Box::new(|p| p.select_all()));
        add("sel_none", Box::new(|p| p.deselect_all()));
        add("copy_link", Box::new(|p| p.on_copy_link_clicked()));
        add("edit", Box::new(|p| p.show_edit_dialog()));
        add("delete", Box::new(|p| p.on_delete_clicked()));
        add("save_to_library", Box::new(|p| p.rate_library(LikeStatus::Like)));
        add("remove_from_library", Box::new(|p| p.rate_library(LikeStatus::Indifferent)));
        add("download_all", Box::new(|p| p.on_download_all()));
        add("start_radio", Box::new(|p| p.on_start_radio()));
        add("play_all_next", Box::new(|p| p.on_play_all_next()));
        add("add_all_to_queue", Box::new(|p| p.on_add_all_to_queue()));
        self.stack.insert_action_group("page", Some(&group));
    }

    // -- header title -----------------------------------------------------

    fn on_scroll(self: &Rc<Self>, adj: &gtk::Adjustment) {
        let value = adj.value();
        if value <= 50.0 {
            self.emit_title("");
        } else {
            self.emit_title(&self.title_text.borrow());
        }
        let max = adj.upper() - adj.page_size();
        if max > 0.0 && value >= max - 200.0 && !self.is_loading_more.get() && self.playlist_id.borrow().is_some() && !self.is_fully_loaded.get() {
            self.load_more();
        }
    }

    fn on_map(&self) {
        self.refresh_more_menu(self.is_editable.get());
    }

    // -- loading ----------------------------------------------------------

    /// Port of load_playlist. `initial` is what the card knew, shown while the fetch runs.
    pub fn load_playlist(self: &Rc<Self>, playlist_id: &str, initial: Option<InitialData>) {
        if self.playlist_id.borrow().as_deref() != Some(playlist_id) {
            self.playlist_id.replace(Some(playlist_id.to_owned()));
            self.audio_playlist_id.replace(None);
            self.title_text.replace(String::new());
            self.current_limit.set(INITIAL_LIMIT);
            self.emit_title("");
            self.tracks.borrow_mut().clear();
            self.is_previewing_cover.set(false);
            self.clear_track_store();
        }

        if !self.ctx.online.is_online() {
            self.load_playlist_offline(playlist_id, initial);
            return;
        }

        if let Some(initial) = &initial {
            self.title_text.replace(initial.title.clone());
            self.name_label.set_label(&initial.title);
            self.description_label.set_label("");
            match initial.author.as_deref().filter(|a| *a != "Unknown") {
                Some(author) => self.meta_label.set_label(&format!("{author} • Loading tracks...")),
                None => self.meta_label.set_label("Loading tracks..."),
            }
            match &initial.thumb {
                Some(thumb) => {
                    // Prefer the on-disk cover: same image, no round trip.
                    let local = self.ctx.paths.local_playlist_cover(&initial.title).map(|p| p.to_string_lossy().into_owned());
                    let cover_url = local.unwrap_or_else(|| thumb.clone());
                    if self.cover.url().as_deref() != Some(cover_url.as_str()) {
                        self.cover.set_placeholder(PLACEHOLDER_ICON);
                        self.cover.load(&cover_url);
                    }
                }
                None => self.cover.clear(),
            }
            self.stack.set_visible_child_name("content");
            // Optimistic render from the disk cache while the fresh fetch runs.
            // The delayed fetch only applies when rows are showing, so the
            // cache read decides the schedule once it lands.
            let mut rendered = false;
            if let Some(cached) = self.ctx.net.caches().cached_tracks(playlist_id) {
                self.apply_cached_tracks(cached);
                rendered = true;
            }
            self.populate_from_disk_cache(playlist_id, Some(initial.clone()), rendered);
            return;
        } else if let Some(cached) = self.ctx.net.caches().cached_tracks(playlist_id) {
            tracing::info!(playlist_id, tracks = cached.len(), "loading playlist from cache");
            self.is_fully_loaded.set(true);
            self.stack.set_visible_child_name("content");
            self.apply_cached_tracks(cached);
            self.schedule_details_fetch(playlist_id, true);
            return;
        } else if self.stack.visible_child_name().as_deref() != Some("content") {
            self.stack.set_visible_child_name("loading");
            self.name_label.set_label("Loading...");
            self.description_label.set_label("");
            self.meta_label.set_label("");
            self.cover.clear();
            self.content_spinner.set_visible(true);
        } else {
            self.content_spinner.set_visible(false);
        }
        self.populate_from_disk_cache(playlist_id, None, false);
    }

    /// Port of _populate_from_disk_cache: read the cached copy on the
    /// runtime, render its header and rows, then let the live fetch follow.
    /// `rendered` says rows are already showing from the memory cache.
    fn populate_from_disk_cache(self: &Rc<Self>, playlist_id: &str, initial: Option<InitialData>, rendered: bool) {
        if Self::is_virtual(playlist_id) {
            self.schedule_details_fetch(playlist_id, rendered);
            return;
        }
        let token = self.populate_token.get();
        let caches = self.ctx.net.caches().clone();
        let id = playlist_id.to_owned();
        let id_read = id.clone();
        let handle = self.ctx.net.spawn(async move { tokio::task::spawn_blocking(move || caches.disk().get(&id_read)).await.ok().flatten() });
        let weak = Rc::downgrade(self);
        glib::spawn_future_local(async move {
            let cached = handle.await.unwrap_or(None);
            let Some(page) = weak.upgrade() else { return };
            if page.playlist_id.borrow().as_deref() != Some(id.as_str()) {
                return;
            }
            match cached.filter(|c| !c.tracks.is_empty()) {
                Some(cached) if token == page.populate_token.get() => {
                    page.apply_disk_cache_header(&cached, initial.as_ref());
                    page.apply_cached_tracks(cached.tracks);
                    page.schedule_details_fetch(&id, true);
                }
                _ => page.schedule_details_fetch(&id, rendered),
            }
        });
    }

    /// Port of _apply_disk_cache_header: the full header from the cached copy.
    fn apply_disk_cache_header(self: &Rc<Self>, cached: &CachedPlaylist, initial: Option<&InitialData>) {
        let pid = self.playlist_id().unwrap_or_default();
        let title = if !cached.title.is_empty() { cached.title.clone() } else { initial.map(|i| i.title.clone()).unwrap_or_default() };
        let author_markup = if !cached.meta.author_raw.is_empty() {
            artist_markup(&cached.meta.author_raw)
        } else {
            let plain = cached.author.trim();
            if plain.starts_with('{') || plain.starts_with('[') { String::new() } else { glib::markup_escape_text(plain).to_string() }
        };
        let is_album = pid.starts_with("MPRE") || pid.starts_with("OLAK") || pid.starts_with("FEmusic_library_privately_owned");
        let n = cached.tracks.len();
        let mut meta1_parts = Vec::new();
        if pid.starts_with("MPRE") || pid.starts_with("OLAK") {
            meta1_parts.push(if n == 1 { "Single" } else if (2..=6).contains(&n) { "EP" } else { "Album" }.to_owned());
        } else {
            let privacy = cached.meta.privacy.clone().map(|p| p.trim().to_owned()).filter(|p| !p.is_empty());
            meta1_parts.push(privacy.map(capitalize).unwrap_or_else(|| "Playlist".to_owned()));
        }
        if let Some(year) = cached.meta.year.as_ref().filter(|y| !y.is_empty()) {
            meta1_parts.push(year.clone());
        }
        if !author_markup.is_empty() {
            meta1_parts.push(author_markup);
        }
        let total = cached.meta.duration_seconds.filter(|s| *s > 0).unwrap_or_else(|| cached.tracks.iter().filter_map(|t| t.duration_seconds).sum());
        let song_text = if n == 1 { "song" } else { "songs" };
        let meta2 = if total > 0 { format!("{n} {song_text} • {}", short_duration(total)) } else { format!("{n} {song_text}") };
        let mut thumbnails = cached.meta.thumbnails.clone();
        if thumbnails.is_empty() {
            if let Some(thumb) = initial.and_then(|i| i.thumb.clone()) {
                thumbnails.push(thumb);
            }
        }

        self.stack.set_visible_child_name("content");
        self.title_text.replace(title.clone());
        self.description_text.replace(cached.meta.description.clone());
        self.name_label.set_label(&title);
        self.meta_label.set_markup(&meta1_parts.join(" • "));
        self.stats_label.set_label(&meta2);
        self.audio_playlist_id.replace(cached.meta.audio_playlist_id.clone());
        if let Some(privacy) = &cached.meta.privacy {
            self.privacy_text.replace(Some(privacy.clone()));
        }
        self.is_album_view.set(is_album);
        self.sort_row.set_visible(n > 0 && !is_album);
        // Ownership is in the cached header too, so the menu offers Edit and
        // Delete right away rather than only once the live fetch lands.
        let owned = playlists::owns_playlist(&cached.playlist_id, cached.meta.author_raw.first().map(|a| a.name.as_str()), cached.meta.collaborators.as_deref(), self.account_name().as_deref());
        self.is_owned.set(owned);
        self.is_editable.set(self.ctx.net.client().is_authenticated() && !is_album && owned);
        self.refresh_more_menu(owned);
        self.set_description(&cached.meta.description);
        if let Some(url) = thumbnails.last() {
            if self.cover.url().as_deref() != Some(url.as_str()) {
                self.is_previewing_cover.set(false);
                let local = self.ctx.paths.local_playlist_cover(&title).map(|p| p.to_string_lossy().into_owned());
                self.cover.load(&local.unwrap_or_else(|| url.clone()));
            }
        }
    }

    /// Port of _write_disk_cache with the deferred write of
    /// _schedule_playlist_cache_write: the serialization waits until the
    /// page has rendered, and back-to-back writes collapse into the last.
    fn schedule_disk_cache_write(self: &Rc<Self>, entry: CachedPlaylist) {
        if Self::is_virtual(&entry.playlist_id) || entry.tracks.is_empty() || !self.ctx.online.is_online() {
            return;
        }
        self.cache_entry.replace(Some(entry.clone()));
        if let Some(id) = self.cache_write.borrow_mut().take() {
            id.remove();
        }
        let caches = self.ctx.net.caches().clone();
        let net = self.ctx.net.clone();
        let weak = Rc::downgrade(self);
        let id = glib::timeout_add_local_once(Duration::from_millis(1500), move || {
            if let Some(page) = weak.upgrade() {
                page.cache_write.borrow_mut().take();
            }
            net.spawn(async move {
                let _ = tokio::task::spawn_blocking(move || caches.disk().put(&entry)).await;
            });
        });
        self.cache_write.replace(Some(id));
    }

    fn cache_entry_from(&self, details: &PlaylistDetails, tracks: &[Track]) -> CachedPlaylist {
        CachedPlaylist {
            playlist_id: self.playlist_id().unwrap_or_default(),
            title: details.title.clone(),
            author: details.author.iter().map(|a| a.name.as_str()).collect::<Vec<_>>().join(", "),
            track_count: details.track_count,
            last_synced: glib::DateTime::now_local().ok().and_then(|d| d.format("%Y-%m-%dT%H:%M:%S").ok()).map(|s| s.to_string()).unwrap_or_default(),
            tracks: tracks.to_vec(),
            meta: CachedMeta {
                description: details.description.clone(),
                year: details.year.clone(),
                privacy: details.privacy.clone(),
                author_raw: details.author.clone(),
                collaborators: details.collaborators.clone(),
                thumbnails: details.thumbnails.clone(),
                duration_seconds: details.duration_seconds,
                audio_playlist_id: details.audio_playlist_id.clone(),
                album_type: details.album_type.clone(),
            },
        }
    }

    /// Render rows from the in-memory cache while the live fetch runs, what
    /// _apply_disk_cache_tracks did with the disk cache.
    fn apply_cached_tracks(self: &Rc<Self>, cached: Vec<Track>) {
        self.tracks.borrow_mut().set(cached.clone());
        self.empty_label.set_visible(cached.is_empty());
        self.is_fully_fetched.set(false);
        self.populate_tracks_chunked(cached);
    }

    fn schedule_details_fetch(self: &Rc<Self>, playlist_id: &str, delay: bool) {
        let weak = Rc::downgrade(self);
        let id = playlist_id.to_owned();
        let start = move || {
            let Some(p) = weak.upgrade() else { return };
            if p.playlist_id.borrow().as_deref() != Some(id.as_str()) {
                return;
            }
            p.content_spinner.set_visible(true);
            p.fetch_playlist_details(&id, false);
        };
        if delay {
            glib::timeout_add_local_once(AUTO_REFRESH_DELAY, start);
        } else {
            start();
        }
    }

    /// Port of _load_playlist_offline: the cached copy is the whole page.
    fn load_playlist_offline(self: &Rc<Self>, playlist_id: &str, initial: Option<InitialData>) {
        let caches = self.ctx.net.caches().clone();
        let id = playlist_id.to_owned();
        let id_read = id.clone();
        let handle = self.ctx.net.spawn(async move { tokio::task::spawn_blocking(move || caches.disk().get(&id_read)).await.ok().flatten() });
        let weak = Rc::downgrade(self);
        glib::spawn_future_local(async move {
            let cached = handle.await.unwrap_or(None);
            let Some(page) = weak.upgrade() else { return };
            if page.playlist_id.borrow().as_deref() != Some(id.as_str()) {
                return;
            }
            match cached.filter(|c| !c.tracks.is_empty()) {
                Some(cached) => {
                    let title = if !cached.title.is_empty() { cached.title.clone() } else { initial.as_ref().map(|i| i.title.clone()).filter(|t| !t.is_empty()).unwrap_or_else(|| "Playlist".to_owned()) };
                    let tracks = cached.tracks;
                    page.title_text.replace(title.clone());
                    page.tracks.borrow_mut().set(tracks.clone());
                    page.is_fully_loaded.set(true);
                    page.is_fully_fetched.set(true);
                    let total: u32 = tracks.iter().filter_map(|t| t.duration_seconds).sum();
                    let n = tracks.len();
                    let thumbnails: Vec<String> = tracks.first().and_then(|t| t.thumb.clone()).into_iter().collect();
                    page.update_ui(HeaderText { title, description: String::new(), meta1: "Offline".to_owned(), meta2: format!("{n} songs • {}", short_duration(total)) }, thumbnails, tracks, false, Some(n as u32), false);
                }
                None => page.show_offline_empty(initial),
            }
        });
    }

    fn show_offline_empty(&self, initial: Option<InitialData>) {
        match initial {
            Some(initial) => {
                let title = if initial.title.is_empty() { "Playlist".to_owned() } else { initial.title };
                self.title_text.replace(title.clone());
                self.name_label.set_label(&title);
                self.meta_label.set_label("Offline - no cached data");
                self.stack.set_visible_child_name("content");
                self.empty_label.set_label("This playlist hasn't been cached for offline use");
                self.empty_label.set_visible(true);
                self.is_fully_loaded.set(true);
            }
            None => {
                self.stack.set_visible_child_name("content");
                self.empty_label.set_label("Offline - no cached data");
                self.empty_label.set_visible(true);
                self.is_fully_loaded.set(true);
            }
        }
    }

    fn is_inf(&self) -> bool {
        self.playlist_id.borrow().as_deref().is_some_and(|p| p.starts_with("RD") || p.starts_with("VLRD"))
    }

    fn is_virtual(id: &str) -> bool {
        id.starts_with("UPLOAD") || id == "DOWNLOADS" || id == "HISTORY"
    }

    fn account_name(&self) -> Option<String> {
        match self.ctx.net.client().auth_state() {
            AuthState::Authenticated(info) => Some(info.name),
            _ => None,
        }
    }

    /// Port of _fetch_playlist_details: pick the endpoint by id shape, fetch
    /// on the runtime, then build the header on the GTK thread.
    fn fetch_playlist_details(self: &Rc<Self>, playlist_id: &str, incremental: bool) {
        if Self::is_virtual(playlist_id) {
            self.is_fully_loaded.set(true);
            self.is_fully_fetched.set(true);
            return;
        }
        if !self.ctx.online.is_online() {
            self.is_fully_loaded.set(true);
            self.is_fully_fetched.set(true);
            return;
        }
        let api = self.ctx.net.client().api();
        let http = self.ctx.net.client().http().clone();
        let auth = self.ctx.net.client().media_auth();
        let id = playlist_id.to_owned();
        let limit = self.current_limit.get();
        let handle = self.ctx.net.spawn(async move { fetch_details(&api, &http, auth, id, limit).await });
        let weak = Rc::downgrade(self);
        let id = playlist_id.to_owned();
        glib::spawn_future_local(async move {
            let outcome = handle.await;
            let Some(page) = weak.upgrade() else { return };
            if page.playlist_id.borrow().as_deref() != Some(id.as_str()) {
                return;
            }
            match outcome {
                Ok(Ok(fetched)) => page.apply_fetch(&id, fetched, incremental),
                Ok(Err(err)) => {
                    tracing::warn!(%err, id, "playlist fetch failed");
                    if !incremental && page.tracks.borrow().rendered().is_empty() {
                        page.update_ui(HeaderText { title: "Error Loading Playlist".into(), description: err.to_string(), meta1: "Playlist • Error".into(), meta2: "0 songs".into() }, Vec::new(), Vec::new(), false, Some(0), false);
                    }
                    page.is_loading_more.set(false);
                    page.load_more_spinner.set_visible(false);
                    page.content_spinner.set_visible(false);
                }
                Err(_) => {}
            }
        });
    }

    /// The tail of _fetch_playlist_details: strings for the header, then update_ui.
    fn apply_fetch(self: &Rc<Self>, playlist_id: &str, fetched: Fetched, incremental: bool) {
        let mut details = match fetched {
            Fetched::Raw { title, tracks } => {
                let title = title.or_else(|| Some(self.title_text.borrow().clone()).filter(|t| !t.is_empty())).unwrap_or_else(|| "Chart Playlist".to_owned());
                self.tracks.borrow_mut().set(tracks.clone());
                self.is_fully_fetched.set(true);
                self.is_fully_loaded.set(true);
                let total: u32 = tracks.iter().filter_map(|t| t.duration_seconds).sum();
                let count = tracks.len();
                let meta2 = format!("{count} songs • {}", short_duration(total));
                self.update_ui(HeaderText { title, description: String::new(), meta1: "Playlist".into(), meta2 }, Vec::new(), tracks, false, Some(count as u32), false);
                return;
            }
            Fetched::Details(d) => *d,
        };
        let is_upload = playlist_id.starts_with("FEmusic_library_privately_owned");
        let is_album = playlist_id.starts_with("MPRE") || playlist_id.starts_with("OLAK");
        let track_len = details.tracks.len();
        let song_text = if track_len == 1 { "song" } else { "songs" };

        let (count_str, is_owned, author, album_type) = if is_upload {
            let author = details.author.iter().map(|a| glib::markup_escape_text(&a.name).to_string()).collect::<Vec<_>>().join(", ");
            (format!("{track_len} {song_text}"), false, author, Some("Upload".to_owned()))
        } else if playlist_id == "LM" {
            // Liked Music shows no year, as the Python branch set year = None.
            details.year = None;
            (format!("{track_len} {song_text}"), false, "You".to_owned(), None)
        } else if is_album {
            self.audio_playlist_id.replace(details.audio_playlist_id.clone());
            let track_count = details.track_count.unwrap_or(track_len as u32);
            let album_type = if track_count == 1 { "Single" } else if (2..=6).contains(&track_count) { "EP" } else { "Album" };
            let author = artist_markup(&details.author);
            let owned = playlists::is_own_playlist(&details, playlist_id, self.account_name().as_deref());
            (format!("{track_len} {song_text}"), owned, author, Some(album_type.to_owned()))
        } else {
            let count_str = if details.track_count.is_none() && self.is_inf() { "Infinite".to_owned() } else { format!("{track_len} {song_text}") };
            let owned = playlists::is_own_playlist(&details, playlist_id, self.account_name().as_deref());
            self.privacy_text.replace(Some(details.privacy.clone().unwrap_or_else(|| "PUBLIC".to_owned())));
            let mut author = if details.author.is_empty() { "Unknown".to_owned() } else { artist_markup(&details.author) };
            if author.contains("Unknown") && !author.starts_with("<a") {
                if let Some(text) = &details.collaborators {
                    let clean = text.strip_prefix("by ").unwrap_or(text);
                    author = glib::markup_escape_text(clean).to_string();
                }
            }
            (count_str, owned, author, None)
        };

        let total_seconds = details.duration_seconds.filter(|s| *s > 0);
        let duration_str = match total_seconds {
            Some(s) => long_duration(s),
            None => details.duration.clone().unwrap_or_default(),
        };
        let mut meta1_parts: Vec<String> = Vec::new();
        if is_album {
            meta1_parts.push(album_type.clone().unwrap_or_else(|| "Album".to_owned()));
        } else if is_upload {
            meta1_parts.push("Upload".to_owned());
        } else {
            let privacy = self.privacy_text.borrow().clone().or_else(|| details.privacy.clone());
            meta1_parts.push(privacy.map(capitalize).unwrap_or_else(|| "Playlist".to_owned()));
        }
        if let Some(year) = details.year.as_ref().filter(|y| !y.is_empty()) {
            meta1_parts.push(year.clone());
        }
        if !author.is_empty() {
            meta1_parts.push(author);
        }
        let mut meta2_parts = vec![count_str];
        if !duration_str.is_empty() {
            meta2_parts.push(duration_str);
        }
        let title = if details.title.is_empty() { let t = self.title_text.borrow().clone(); if t.is_empty() { "Unknown Playlist".to_owned() } else { t } } else { details.title.clone() };
        let track_count = details.track_count;
        let thumbnails = details.thumbnails.clone();
        let mut tracks = std::mem::take(&mut details.tracks);
        if is_album || is_upload {
            if let Some(album_thumb) = thumbnails.last() {
                for t in &mut tracks {
                    if t.thumb.is_none() {
                        t.thumb = Some(album_thumb.clone());
                    }
                }
            }
        }
        // Python started this from the fetch thread before update_ui ran on the
        // main loop. update_ui marks a first render fully fetched, so it must
        // start first here as well or a long playlist stops at the first pages.
        if !incremental && !is_album && track_count.is_some_and(|count| (track_len as u32) < count) {
            self.start_background_full_fetch();
        }
        if !incremental {
            let mut for_cache = details.clone();
            for_cache.title = title.clone();
            self.schedule_disk_cache_write(self.cache_entry_from(&for_cache, &tracks));
        }
        self.update_ui(HeaderText { title, description: details.description, meta1: meta1_parts.join(" • "), meta2: meta2_parts.join(" • ") }, thumbnails, tracks, incremental, track_count, is_owned);
    }

    // -- update UI --------------------------------------------------------

    #[allow(clippy::too_many_arguments)]
    fn update_ui(self: &Rc<Self>, text: HeaderText, thumbnails: Vec<String>, tracks: Vec<Track>, append: bool, total_tracks: Option<u32>, is_owned: bool) {
        self.stack.set_visible_child_name("content");
        self.content_spinner.set_visible(false);
        self.title_text.replace(text.title.clone());
        self.description_text.replace(text.description.clone());
        self.name_label.set_label(&text.title);

        // A partial live fetch must not regress a richer view already rendered.
        let existing_count = self.tracks.borrow().fetched().len();
        let keep_richer = !append && existing_count > tracks.len();

        self.set_description(&text.description);
        self.meta_label.set_markup(&text.meta1);
        if !keep_richer {
            self.stats_label.set_label(&text.meta2);
        }

        let pid = self.playlist_id.borrow().clone().unwrap_or_default();
        let is_album = pid.starts_with("MPRE") || pid.starts_with("OLAK") || pid.starts_with("FEmusic_library_privately_owned");
        self.is_album_view.set(is_album);
        let has_tracks = !tracks.is_empty();
        self.empty_label.set_visible(!has_tracks);
        self.sort_row.set_visible(has_tracks && !is_album);

        self.is_owned.set(is_owned);
        let editable = self.ctx.net.client().is_authenticated() && !is_album && is_owned;
        self.is_editable.set(editable);
        let check_id = self.audio_playlist_id.borrow().clone().unwrap_or_else(|| pid.clone());
        self.is_saved_to_library.set(is_owned || self.is_in_library(&check_id));
        self.refresh_more_menu(editable);

        if let Some(url) = thumbnails.last().filter(|_| !append) {
            if self.cover.url().as_deref() != Some(url.as_str()) {
                self.is_previewing_cover.set(false);
                let local = self.ctx.paths.local_playlist_cover(&text.title).map(|p| p.to_string_lossy().into_owned());
                self.cover.load(&local.unwrap_or_else(|| url.clone()));
                self.save_playlist_cover_async(&text.title, url);
            }
        } else if thumbnails.is_empty() && self.cover.url().is_none() && !self.is_previewing_cover.get() {
            self.cover.clear();
        }

        if append {
            let start = self.tracks.borrow().rendered().len();
            let new_tracks: Vec<Track> = tracks.get(start..).map(|s| s.to_vec()).unwrap_or_default();
            if new_tracks.is_empty() {
                tracing::info!("no new tracks, playlist fully loaded");
                self.is_fully_loaded.set(true);
                self.load_more_spinner.set_visible(false);
                self.is_loading_more.set(false);
                return;
            }
            self.tracks.borrow_mut().extend(new_tracks.clone());
            if self.sort_dropdown.selected() != SORT_DEFAULT {
                self.reorder_playlist(self.sort_dropdown.selected());
            } else {
                for t in &new_tracks {
                    self.track_store.append(&TrackObject::new(t.clone()));
                }
            }
            self.load_more_spinner.set_visible(false);
            self.is_loading_more.set(false);
            if tracks.len() < self.current_limit.get() || total_tracks.is_some_and(|t| tracks.len() as u32 >= t) {
                self.is_fully_loaded.set(true);
            }
        } else {
            if total_tracks.is_some_and(|t| tracks.len() as u32 >= t) {
                self.is_fully_loaded.set(true);
                self.is_fully_fetched.set(true);
                self.ctx.net.caches().set_cached_tracks(&pid, tracks.clone());
            }
            if !keep_richer {
                let mut list = self.tracks.borrow_mut();
                list.set_rendered(tracks.clone());
                if list.fetched().is_empty() {
                    list.set_fetched(tracks.clone());
                }
                drop(list);
                self.sort_dropdown.set_selected(SORT_DEFAULT);
                self.populate_tracks_chunked(tracks);
            } else {
                tracing::info!(existing_count, fetched = tracks.len(), "keeping cached render over partial fetch");
            }
        }
        if self.tracks.borrow().fully_rendered() {
            self.is_fully_fetched.set(true);
        }
    }

    fn set_description(&self, description: &str) {
        if description.trim().is_empty() {
            self.desc_box.set_visible(false);
            return;
        }
        self.full_description.replace(description.to_owned());
        self.description_expanded.set(false);
        self.read_more.borrow().set_markup("<a href='toggle'>Read more</a>");
        if description.chars().count() > 200 {
            self.description_label.set_label(&truncate_description(description));
            self.read_more.borrow().set_visible(true);
        } else {
            self.description_label.set_label(description);
            self.read_more.borrow().set_visible(false);
        }
        self.desc_box.set_visible(true);
    }

    /// Port of _toggle_description: the link label is replaced to dodge GTK's visited colour.
    fn toggle_description(self: &Rc<Self>) {
        let expanded = !self.description_expanded.get();
        self.description_expanded.set(expanded);
        let full = self.full_description.borrow().clone();
        let text = if expanded {
            self.description_label.set_label(&full);
            "Show less"
        } else {
            self.description_label.set_label(&truncate_description(&full));
            "Read more"
        };
        let old = self.read_more.borrow().clone();
        self.desc_box.remove(&old);
        let label = read_more_label(text);
        self.desc_box.append(&label);
        self.read_more.replace(label);
        self.connect_read_more();
    }

    // -- store helpers ----------------------------------------------------

    fn clear_track_store(&self) {
        self.track_store.remove_all();
        // A new token cancels any chunker still pumping from an older render.
        self.populate_token.set(self.populate_token.get() + 1);
    }

    /// Port of _populate_tracks_chunked: the first batch at once so the page
    /// transition has rows to paint, the rest pumped on idle after the
    /// transition, eighty at a time.
    fn populate_tracks_chunked(self: &Rc<Self>, tracks: Vec<Track>) {
        const FIRST: usize = 40;
        const BATCH: usize = 80;
        self.clear_track_store();
        if tracks.is_empty() {
            return;
        }
        let token = self.populate_token.get();
        let head: Vec<TrackObject> = tracks.iter().take(FIRST).map(|t| TrackObject::new(t.clone())).collect();
        self.track_store.splice(0, 0, &head);
        if tracks.len() <= FIRST {
            return;
        }
        let tracks = Rc::new(tracks);
        let weak = Rc::downgrade(self);
        glib::timeout_add_local_once(TRANSITION_GATE, move || pump(weak, tracks, FIRST, token, BATCH));

        fn pump(weak: Weak<PlaylistPage>, tracks: Rc<Vec<Track>>, cursor: usize, token: u64, batch: usize) {
            let Some(page) = weak.upgrade() else { return };
            if token != page.populate_token.get() {
                return;
            }
            let end = (cursor + batch).min(tracks.len());
            let chunk: Vec<TrackObject> = tracks[cursor..end].iter().map(|t| TrackObject::new(t.clone())).collect();
            if !chunk.is_empty() {
                page.track_store.splice(page.track_store.n_items(), 0, &chunk);
            }
            if end < tracks.len() {
                glib::idle_add_local_once(move || pump(weak, tracks, end, token, batch));
            }
        }
    }

    // -- scroll / lazy load -----------------------------------------------

    /// Port of load_more: slice from the fetched list when it is complete,
    /// hit the network only for infinite lists.
    fn load_more(self: &Rc<Self>) {
        if self.is_fully_fetched.get() {
            let new_tracks = self.tracks.borrow_mut().render_chunk(50);
            if !new_tracks.is_empty() {
                self.is_loading_more.set(true);
                self.load_more_spinner.set_visible(true);
                if self.sort_dropdown.selected() != SORT_DEFAULT {
                    self.reorder_playlist(self.sort_dropdown.selected());
                } else {
                    for t in &new_tracks {
                        self.track_store.append(&TrackObject::new(t.clone()));
                    }
                }
                self.load_more_spinner.set_visible(false);
                self.is_loading_more.set(false);
                return;
            }
        }
        if self.is_fully_loaded.get() || !self.is_inf() {
            return;
        }
        self.is_loading_more.set(true);
        self.load_more_spinner.set_visible(true);
        let limit = self.tracks.borrow().rendered().len() + 50;
        self.current_limit.set(limit);
        tracing::info!(limit, "loading more");
        if let Some(id) = self.playlist_id() {
            self.fetch_playlist_details(&id, true);
        }
    }

    /// Port of _start_background_full_fetch: get every row, then refresh the
    /// list, the queue and the duration once they land.
    fn start_background_full_fetch(self: &Rc<Self>) {
        if self.is_fully_fetched.get() {
            return;
        }
        let Some(id) = self.playlist_id() else { return };
        tracing::info!(id, "starting background fetch for full playlist");
        self.is_background_fetching.set(true);
        self.pending_queue_append.set(false);
        let api = self.ctx.net.client().api();
        let id_c = id.clone();
        let handle = self.ctx.net.spawn(async move { get_playlist_full(&api, &id_c).await });
        let weak = Rc::downgrade(self);
        glib::spawn_future_local(async move {
            let outcome = handle.await;
            let Some(page) = weak.upgrade() else { return };
            if page.playlist_id.borrow().as_deref() != Some(id.as_str()) {
                return;
            }
            match outcome {
                Ok(Ok(tracks)) if !tracks.is_empty() => {
                    tracing::info!(count = tracks.len(), "background fetch complete");
                    page.ctx.net.caches().set_cached_tracks(&id, tracks.clone());
                    // The full list replaces the partial one in the disk cache, header fields kept.
                    // The borrow must end before schedule_disk_cache_write replaces the cell.
                    let entry = page.cache_entry.borrow().clone();
                    if let Some(mut entry) = entry {
                        entry.tracks = tracks.clone();
                        entry.track_count = Some(tracks.len() as u32);
                        page.schedule_disk_cache_write(entry);
                    }
                    page.on_background_fetch_complete(Some(tracks));
                }
                Ok(Ok(_)) => page.on_background_fetch_complete(None),
                Ok(Err(err)) => {
                    tracing::warn!(%err, "background fetch failed");
                    page.on_background_fetch_complete(None);
                }
                Err(_) => {}
            }
        });
    }

    fn on_background_fetch_complete(self: &Rc<Self>, tracks: Option<Vec<Track>>) {
        if let Some(tracks) = &tracks {
            self.tracks.borrow_mut().set_fetched(tracks.clone());
        }
        self.is_fully_fetched.set(true);
        self.is_background_fetching.set(false);
        let sort_type = self.sort_dropdown.selected();
        if sort_type != SORT_DEFAULT || self.tracks.borrow().descending() {
            self.reorder_playlist(sort_type);
        } else if let Some(tracks) = &tracks {
            // The live list may differ from the rendered one: refresh without a manual reload.
            let new_ids: Vec<&str> = tracks.iter().map(|t| t.video_id.as_str()).collect();
            let cur_ids: Vec<String> = self.tracks.borrow().rendered().iter().map(|t| t.video_id.0.clone()).collect();
            if !new_ids.is_empty() && new_ids != cur_ids.iter().map(String::as_str).collect::<Vec<_>>() && !self.tracks.borrow().filtering() {
                tracing::info!(cached = cur_ids.len(), live = new_ids.len(), "external edits detected, refreshing");
                self.tracks.borrow_mut().set_rendered(tracks.clone());
                self.populate_tracks_chunked(tracks.clone());
            }
        }
        self.content_spinner.set_visible(false);

        // Complete the snapshot the queue took when Play beat the fetch.
        if let Some(pid) = self.playlist_id() {
            if self.ctx.player.queue_source_id().as_deref() == Some(pid.as_str()) {
                let queue_len = self.ctx.player.queue_tracks().len();
                let fetched = self.tracks.borrow().fetched().to_vec();
                if queue_len > 0 && queue_len < fetched.len() {
                    tracing::info!(queue_len, total = fetched.len(), "extending player queue after background fetch");
                    self.ctx.player.extend_queue(fetched[queue_len..].to_vec());
                }
            }
        }
        self.pending_queue_append.set(false);
        self.update_duration_from_all_tracks();
    }

    fn update_duration_from_all_tracks(&self) {
        let list = self.tracks.borrow();
        let tracks = list.source();
        let total: u32 = tracks.iter().filter_map(|t| t.duration_seconds).sum();
        let count = tracks.len();
        let mut parts = vec![format!("{count} {}", if count == 1 { "song" } else { "songs" })];
        if total > 0 {
            parts.push(long_duration(total));
        }
        self.stats_label.set_label(&parts.join(" • "));
    }

    // -- filter -----------------------------------------------------------

    /// Port of filter_content: debounced, immediate when clearing.
    pub fn filter_content(self: &Rc<Self>, text: &str) {
        let pending = text.trim().to_lowercase();
        self.pending_filter_text.replace(pending.clone());
        if let Some(id) = self.filter_debounce.borrow_mut().take() {
            id.remove();
        }
        let delay = if pending.is_empty() { Duration::ZERO } else { FILTER_DEBOUNCE };
        let weak = Rc::downgrade(self);
        let id = glib::timeout_add_local_once(delay, move || {
            if let Some(p) = weak.upgrade() {
                p.filter_debounce.borrow_mut().take();
                p.filter_content_apply();
            }
        });
        self.filter_debounce.replace(Some(id));
    }

    fn filter_content_apply(self: &Rc<Self>) {
        let text = self.pending_filter_text.borrow().clone();
        self.tracks.borrow_mut().set_filter(text.clone());
        if !self.tracks.borrow().fetched().is_empty() {
            self.filter_model.set_filter(None::<&gtk::CustomFilter>);
            let rows = self.tracks.borrow().visible();
            let objects: Vec<TrackObject> = rows.into_iter().map(TrackObject::new).collect();
            self.track_store.splice(0, self.track_store.n_items(), &objects);
            // Keep the filter attached only while a search is active.
            if !text.is_empty() {
                self.filter_model.set_filter(Some(&self.track_filter));
            } else {
                self.filter_model.set_filter(None::<&gtk::CustomFilter>);
            }
            if self.multi_select.get() {
                self.update_selection_count();
            }
            return;
        }
        if self.filter_model.filter().is_none() {
            self.filter_model.set_filter(Some(&self.track_filter));
        }
        self.track_filter.changed(gtk::FilterChange::Different);
        if self.multi_select.get() {
            self.update_selection_count();
        }
    }

    // -- playback ---------------------------------------------------------

    fn on_list_activate(self: &Rc<Self>, position: u32) {
        let Some(track) = self.flatten.item(position).and_downcast::<TrackObject>().map(|t| t.track()) else { return };
        if self.multi_select.get() {
            self.toggle_track_selection(&track.video_id.0, None);
            self.refresh_all_row_visuals();
            return;
        }
        self.play_track(&track);
    }

    fn page_cover_url(&self) -> Option<String> {
        self.cover.url()
    }

    /// Port of _play_track: jump inside the live queue when it came from this
    /// page, otherwise queue the page's rows with covers filled in.
    fn play_track(self: &Rc<Self>, track: &Track) {
        let video_id = track.video_id.0.clone();
        if video_id.is_empty() || !self.ctx.online.is_online() {
            return;
        }
        let pid = self.playlist_id();
        if pid.is_some() && self.ctx.player.queue_source_id() == pid {
            let queue = self.ctx.player.queue_tracks();
            if let Some(i) = queue.iter().position(|t| t.video_id.0 == video_id) {
                self.ctx.player.play_queue_index(i);
            }
            return;
        }
        let page_cover = self.page_cover_url();
        let mut start_index = None;
        let queue: Vec<Track> = self.best_queue().into_iter().enumerate().map(|(i, mut t)| {
            if t.thumb.is_none() {
                t.thumb = page_cover.clone();
            }
            if t.video_id.0 == video_id {
                start_index = Some(i);
            }
            t
        }).collect();
        let Some(start) = start_index else { return };
        self.ctx.player.play_tracks(queue, start, false, pid, self.is_inf());
        if self.is_background_fetching.get() {
            self.pending_queue_append.set(true);
        }
    }

    /// Port of _best_queue: the full list only under the forward default sort.
    fn best_queue(&self) -> Vec<Track> {
        let list = self.tracks.borrow();
        if self.is_fully_fetched.get() && !list.fetched().is_empty() && list.sort_type() == SORT_DEFAULT && !list.descending() {
            return list.fetched().to_vec();
        }
        list.rendered().to_vec()
    }

    /// Offline the queue keeps downloaded songs only. None are known, so it empties.
    /// Port of _filter_queue_offline: with no connection only downloaded songs play.
    fn offline_filter_queue(&self, tracks: Vec<Track>) -> Vec<Track> {
        if self.ctx.online.is_online() {
            return tracks;
        }
        tracks.into_iter().filter(|t| self.ctx.downloads.is_downloaded(&t.video_id.0)).collect()
    }

    fn on_play_clicked(self: &Rc<Self>) {
        if self.tracks.borrow().rendered().is_empty() {
            return;
        }
        let queue = self.offline_filter_queue(self.best_queue());
        if queue.is_empty() {
            toast(&self.stack, "No downloaded songs to play");
            return;
        }
        self.ctx.player.play_tracks(queue, 0, false, self.playlist_id(), self.is_inf());
        if self.is_background_fetching.get() {
            self.pending_queue_append.set(true);
        }
    }

    fn on_shuffle_clicked(self: &Rc<Self>) {
        if self.tracks.borrow().rendered().is_empty() {
            return;
        }
        let queue = self.offline_filter_queue(self.best_queue());
        if queue.is_empty() {
            toast(&self.stack, "No downloaded songs to shuffle");
            return;
        }
        self.ctx.player.play_tracks(queue, usize::MAX, true, self.playlist_id(), self.is_inf());
        if self.is_background_fetching.get() {
            self.pending_queue_append.set(true);
        }
    }

    // -- more menu --------------------------------------------------------

    /// Mark the menu for a rebuild the next time its popover opens.
    fn refresh_more_menu(&self, is_owned: bool) {
        self.more_menu_pending_owned.set(is_owned);
        self.more_menu_dirty.set(true);
    }

    fn rebuild_more_menu(&self, is_owned: bool) {
        self.more_menu.remove_all();
        let queue_section = gio::Menu::new();
        queue_section.append(Some("Play Next"), Some("page.play_all_next"));
        queue_section.append(Some("Add to Queue"), Some("page.add_all_to_queue"));
        self.more_menu.append_section(None, &queue_section);
        let authed = self.ctx.net.client().is_authenticated();
        if authed {
            self.more_menu.append(Some("Add all to Playlist…"), Some("page.show_add_all_to_playlist"));
        }
        if self.ctx.online.is_online() && (self.audio_playlist_id.borrow().is_some() || self.playlist_id.borrow().is_some()) {
            self.more_menu.append(Some("Start Radio"), Some("page.start_radio"));
        }
        self.more_menu.append(Some("Copy Link"), Some("page.copy_link"));
        if !is_owned && authed {
            if self.is_saved_to_library.get() {
                self.more_menu.append(Some("Remove from Library"), Some("page.remove_from_library"));
            } else {
                self.more_menu.append(Some("Add to Library"), Some("page.save_to_library"));
            }
        }
        self.more_menu.append(Some("Download All"), Some("page.download_all"));
        if is_owned {
            self.more_menu.append(Some("Edit Playlist"), Some("page.edit"));
            self.more_menu.append(Some("Delete Playlist"), Some("page.delete"));
        }
    }

    fn all_tracks(&self) -> Vec<Track> {
        self.tracks.borrow().source().iter().filter(|t| !t.video_id.0.is_empty()).cloned().collect()
    }

    fn on_play_all_next(&self) {
        let tracks = self.all_tracks();
        if !tracks.is_empty() {
            let n = tracks.len();
            self.ctx.player.add_to_queue(tracks, true);
            toast(&self.stack, &format!("Playing {n} tracks next"));
        }
    }

    fn on_add_all_to_queue(&self) {
        let tracks = self.all_tracks();
        if !tracks.is_empty() {
            let n = tracks.len();
            self.ctx.player.add_to_queue(tracks, false);
            toast(&self.stack, &format!("Added {n} tracks to queue"));
        }
    }

    fn on_start_radio(&self) {
        let pid = self.audio_playlist_id.borrow().clone().or_else(|| self.playlist_id());
        let Some(pid) = pid else { return };
        let radio_id = if pid.starts_with("RDAMPL") { pid } else { format!("RDAMPL{pid}") };
        self.ctx.player.start_radio(None, Some(radio_id));
        toast(&self.stack, "Starting radio...");
    }

    /// Port of _on_download_all: every track the page knows, tagged with the
    /// playlist so the .m3u8 mirror follows.
    fn on_download_all(&self) {
        let tracks = self.all_tracks();
        if tracks.is_empty() {
            return;
        }
        if !self.ctx.online.is_online() {
            toast(&self.stack, "Downloads need an internet connection");
            return;
        }
        let title = self.title_text.borrow().clone();
        self.ctx.download(tracks, &title, &self.playlist_id().unwrap_or_default());
    }

    fn on_show_add_all_to_playlist(self: &Rc<Self>) {
        let page = self.clone();
        AddToPlaylistPopover::show(&self.ctx, &self.more_btn, move |pid| page.do_add_all_to_playlist(&pid));
    }

    fn do_add_all_to_playlist(self: &Rc<Self>, playlist_id: &str) {
        let video_ids: Vec<String> = self.tracks.borrow().rendered().iter().map(|t| t.video_id.0.clone()).filter(|v| !v.is_empty()).collect();
        if playlist_id.is_empty() || video_ids.is_empty() {
            return;
        }
        mark_playlist_used(&self.ctx.paths, playlist_id);
        self.add_to_playlist(playlist_id.to_owned(), video_ids, false);
    }

    fn add_to_playlist(self: &Rc<Self>, playlist_id: String, video_ids: Vec<String>, _selection: bool) {
        let api = self.ctx.net.client().api();
        let count = video_ids.len();
        let handle = self.ctx.net.spawn(async move { playlists::add_playlist_items(&api, &playlist_id, video_ids, None).await });
        let weak = Rc::downgrade(self);
        glib::spawn_future_local(async move {
            let Some(page) = weak.upgrade() else { return };
            match handle.await {
                Ok(Ok(())) => toast(&page.stack, &format!("Added {count} tracks to playlist")),
                Ok(Err(err)) => {
                    tracing::warn!(%err, "add to playlist failed");
                    toast(&page.stack, "Failed to add tracks");
                }
                Err(_) => {}
            }
        });
    }

    fn on_copy_link_clicked(&self) {
        let Some(pid) = self.playlist_id() else { return };
        let is_album = pid.starts_with("MPRE") || pid.starts_with("OLAK");
        let link = yt_music_link(&pid, is_album, self.audio_playlist_id.borrow().as_deref());
        if !link.is_empty() {
            copy_to_clipboard(&link);
            toast(&self.stack, "Link copied to clipboard");
            tracing::info!(link, "copied link");
        }
    }

    // -- library membership -----------------------------------------------

    /// Port of is_in_library: never blocks. A cold cache answers false and
    /// warms in the background, then the menu is corrected.
    fn is_in_library(self: &Rc<Self>, check_id: &str) -> bool {
        if check_id.is_empty() || !self.ctx.net.client().is_authenticated() {
            return false;
        }
        match self.ctx.net.caches().library_ids() {
            Some(ids) => ids.contains(check_id),
            None => {
                self.warm_library_ids();
                false
            }
        }
    }

    fn warm_library_ids(self: &Rc<Self>) {
        let api = self.ctx.net.client().api();
        let handle = self.ctx.net.spawn(playlists::fetch_library_ids(api));
        let weak = Rc::downgrade(self);
        glib::spawn_future_local(async move {
            let Some(page) = weak.upgrade() else { return };
            if let Ok(Ok((ids, playlists))) = handle.await {
                page.ctx.net.caches().set_library_ids(ids);
                page.ctx.net.caches().set_library_playlists(playlists);
                page.recheck_library_status();
            }
        });
    }

    fn recheck_library_status(self: &Rc<Self>) {
        let check_id = self.audio_playlist_id.borrow().clone().or_else(|| self.playlist_id());
        let Some(check_id) = check_id else { return };
        let saved = self.is_owned.get() || self.ctx.net.caches().library_ids().is_some_and(|ids| ids.contains(&check_id));
        self.is_saved_to_library.set(saved);
        self.refresh_more_menu(self.is_editable.get());
    }

    fn rate_library(self: &Rc<Self>, rating: LikeStatus) {
        let pid = self.audio_playlist_id.borrow().clone().or_else(|| self.playlist_id());
        let Some(pid) = pid else { return };
        let api = self.ctx.net.client().api();
        let handle = self.ctx.net.spawn(async move { playlists::rate_playlist(&api, &pid, rating).await });
        let weak = Rc::downgrade(self);
        glib::spawn_future_local(async move {
            let Some(page) = weak.upgrade() else { return };
            let saving = rating == LikeStatus::Like;
            match handle.await {
                Ok(Ok(())) => {
                    page.is_saved_to_library.set(saving);
                    page.ctx.net.caches().clear_library_ids();
                    toast(&page.stack, if saving { "Saved to library" } else { "Removed from library" });
                    page.refresh_more_menu(page.is_owned.get());
                    page.ctx.nav.refresh_library();
                }
                Ok(Err(err)) => {
                    tracing::warn!(%err, "rate playlist failed");
                    toast(&page.stack, if saving { "Failed to save" } else { "Failed to remove" });
                }
                Err(_) => {}
            }
        });
    }

    // -- multi-select -----------------------------------------------------

    fn on_select_toggled(self: &Rc<Self>, active: bool) {
        self.multi_select.set(active);
        self.tracks.borrow_mut().clear_selection();
        if active {
            self.selection_bar.set_visible(true);
            self.sel_remove_btn.set_visible(self.is_owned.get());
            self.update_selection_count();
        } else {
            self.selection_bar.set_visible(false);
        }
        self.refresh_all_row_visuals();
    }

    fn toggle_track_selection(self: &Rc<Self>, video_id: &str, row: Option<&Rc<TrackRow>>) {
        if video_id.is_empty() {
            return;
        }
        let selected = {
            let mut list = self.tracks.borrow_mut();
            let on = !list.is_selected(video_id);
            list.select(video_id, on);
            on
        };
        self.update_selection_count();
        if let Some(row) = row {
            row.apply_selection(selected);
        }
    }

    /// Every realized row, found the way Python walked the ListView's children.
    fn live_rows(&self) -> Vec<Rc<TrackRow>> {
        let mut rows = Vec::new();
        let mut child = self.songs_list.first_child();
        while let Some(c) = child {
            child = c.next_sibling();
            if let Some(bin) = c.first_child().and_downcast::<adw::Bin>() {
                if let Some(row) = TrackRow::from_bin(&bin) {
                    rows.push(row);
                }
            }
        }
        rows
    }

    fn refresh_all_row_visuals(&self) {
        let multi = self.multi_select.get();
        for row in self.live_rows() {
            if row.track().is_none() {
                continue;
            }
            let selected = row.video_id().is_some_and(|v| self.tracks.borrow().is_selected(&v));
            row.refresh_visuals(multi, selected);
        }
    }

    /// Port of _get_visible_tracks: the filtered rows under a search, else everything known.
    fn visible_tracks(&self) -> Vec<Track> {
        let list = self.tracks.borrow();
        if list.filtering() { list.matches() } else { list.source().to_vec() }
    }

    fn select_all(self: &Rc<Self>) {
        self.tracks.borrow_mut().select_visible();
        self.update_selection_count();
        self.refresh_all_row_visuals();
    }

    fn deselect_all(self: &Rc<Self>) {
        self.tracks.borrow_mut().clear_selection();
        self.update_selection_count();
        self.refresh_all_row_visuals();
    }

    fn update_selection_count(&self) {
        let count = self.tracks.borrow().selected_count();
        let total = self.visible_tracks().len();
        self.selection_count_label.set_label(&format!("{count} of {total} selected"));
    }

    /// Selected tracks in the current sort order.
    fn selected_tracks(&self) -> Vec<Track> {
        self.tracks.borrow().selected_tracks()
    }

    fn do_sel_add_to_playlist(self: &Rc<Self>, target: &str) {
        if target.is_empty() {
            return;
        }
        let video_ids: Vec<String> = self.selected_tracks().into_iter().map(|t| t.video_id.0).filter(|v| !v.is_empty()).collect();
        if video_ids.is_empty() {
            return;
        }
        mark_playlist_used(&self.ctx.paths, target);
        self.add_to_playlist(target.to_owned(), video_ids, true);
    }

    fn on_sel_remove(self: &Rc<Self>) {
        let to_remove: Vec<(String, String)> = self.selected_tracks().into_iter().filter_map(|t| Some((t.video_id.0.clone(), t.set_video_id?))).filter(|(v, _)| !v.is_empty()).collect();
        if to_remove.is_empty() {
            return;
        }
        self.remove_items(to_remove, true);
    }

    fn remove_items(self: &Rc<Self>, items: Vec<(String, String)>, announce: bool) {
        let Some(pid) = self.playlist_id() else { return };
        let api = self.ctx.net.client().api();
        let count = items.len();
        let pid_c = pid.clone();
        let handle = self.ctx.net.spawn(async move { playlists::remove_playlist_items(&api, &pid_c, &items).await });
        let weak = Rc::downgrade(self);
        glib::spawn_future_local(async move {
            let Some(page) = weak.upgrade() else { return };
            match handle.await {
                Ok(Ok(())) => {
                    page.invalidate_disk_cache();
                    if announce {
                        toast(&page.stack, &format!("Removed {count} tracks"));
                    }
                    page.load_playlist(&pid, None);
                }
                Ok(Err(err)) => {
                    tracing::warn!(%err, "remove failed");
                    if announce {
                        toast(&page.stack, "Failed to remove tracks");
                    }
                }
                Err(_) => {}
            }
        });
    }

    fn copy_selection_debug(&self) {
        let tracks = self.selected_tracks();
        let mut ids: Vec<String> = self.tracks.borrow().selected_ids();
        ids.sort();
        let debug = serde_json::json!({
            "selected_count": tracks.len(),
            "selected_video_ids": ids,
            "current_tracks_count": self.tracks.borrow().rendered().len(),
            "original_tracks_count": self.tracks.borrow().fetched().len(),
            "tracks": tracks.iter().map(|t| serde_json::json!({
                "videoId": t.video_id.0, "title": t.title, "artists": t.artists.iter().map(|a| a.name.clone()).collect::<Vec<_>>(), "setVideoId": t.set_video_id, "duration_seconds": t.duration_seconds,
            })).collect::<Vec<_>>(),
        });
        copy_to_clipboard(&serde_json::to_string_pretty(&debug).unwrap_or_default());
        toast(&self.stack, &format!("Copied debug data for {} tracks", tracks.len()));
    }

    // -- sort -------------------------------------------------------------

    /// Point the arrow at the order on screen. Most viewed and recently added
    /// run biggest first untoggled, so their icon is the flip of the toggle.
    fn refresh_sort_dir_icon(&self) {
        let mut descending = self.tracks.borrow().descending();
        if matches!(self.sort_dropdown.selected(), SORT_VIEWS | SORT_ADDED) {
            descending = !descending;
        }
        self.sort_dir_btn.set_icon_name(if descending { "view-sort-descending-symbolic" } else { "view-sort-ascending-symbolic" });
    }

    fn on_sort_changed(self: &Rc<Self>, sort_type: u32) {
        self.refresh_sort_dir_icon();
        if needs_metric(sort_type) && !self.tracks.borrow().has_metric(sort_type) {
            self.fetch_sort_metric(sort_type);
            return;
        }
        self.reorder_playlist(sort_type);
    }

    fn drop_sort_metrics(&self, playlist_id: &str) {
        self.tracks.borrow_mut().drop_metrics();
        let browse_id = if playlist_id.starts_with("VL") { playlist_id.to_owned() } else { format!("VL{playlist_id}") };
        self.ctx.net.caches().drop_sort_metrics(&browse_id);
    }

    /// Neither number rides along on a track: fetch it once per playlist,
    /// then reorder when it lands.
    fn fetch_sort_metric(self: &Rc<Self>, sort_type: u32) {
        let Some(pid) = self.playlist_id().filter(|p| p != "DOWNLOADS" && p != "HISTORY") else {
            self.sort_metric_unavailable(sort_type, false);
            return;
        };
        if !self.ctx.online.is_online() {
            self.sort_metric_unavailable(sort_type, true);
            return;
        }
        self.content_spinner.set_visible(true);
        self.sort_dropdown.set_sensitive(false);
        let api = self.ctx.net.client().api();
        let http = self.ctx.net.client().http().clone();
        let auth = self.ctx.net.client().media_auth();
        let caches = self.ctx.net.caches().clone();
        let pid_c = pid.clone();
        let handle = self.ctx.net.spawn(async move {
            let browse_id = if pid_c.starts_with("VL") { pid_c.clone() } else { format!("VL{pid_c}") };
            let kind = if sort_type == SORT_VIEWS { "views" } else { "added" };
            if let Some(cached) = caches.sort_metric(kind, &browse_id) {
                return cached;
            }
            let metric = if sort_type == SORT_VIEWS { playlists::playlist_view_counts(&http, auth.as_ref(), &pid_c).await } else { playlists::playlist_added_dates(&api, &pid_c).await };
            let metric = metric.unwrap_or_default();
            if !metric.is_empty() {
                caches.set_sort_metric(kind, &browse_id, metric.clone());
            }
            metric
        });
        let weak = Rc::downgrade(self);
        glib::spawn_future_local(async move {
            let Some(page) = weak.upgrade() else { return };
            let metric = handle.await.unwrap_or_default();
            page.sort_dropdown.set_sensitive(true);
            if page.playlist_id().as_deref() != Some(pid.as_str()) {
                return;
            }
            page.content_spinner.set_visible(false);
            if metric.is_empty() {
                page.sort_metric_unavailable(sort_type, false);
                return;
            }
            page.tracks.borrow_mut().set_metric(sort_type, metric.clone());
            page.reorder_playlist(sort_type);
            page.warn_partial_sort_metric(sort_type, &metric);
        });
    }

    fn warn_partial_sort_metric(&self, sort_type: u32, metric: &SortMetric) {
        let list = self.tracks.borrow();
        let ids: HashSet<&str> = list.source().iter().map(|t| t.video_id.as_str()).filter(|v| !v.is_empty()).collect();
        let total = ids.len();
        let ranked = ids.iter().filter(|v| metric.contains_key(**v)).count();
        if total == 0 || ranked as f64 >= total as f64 * 0.9 {
            return;
        }
        let label = if sort_type == SORT_VIEWS { "view counts" } else { "add dates" };
        toast(&self.stack, &format!("Only {ranked} of {total} songs have {label} — the rest stay at the end"));
    }

    fn sort_metric_unavailable(&self, sort_type: u32, offline: bool) {
        let label = if sort_type == SORT_VIEWS { "View counts" } else { "Add dates" };
        toast(&self.stack, &if offline { format!("{label} need an internet connection") } else { format!("{label} aren't available for this list") });
        self.content_spinner.set_visible(false);
        self.sort_dropdown.set_sensitive(true);
        if self.sort_dropdown.selected() != SORT_DEFAULT {
            self.sort_dropdown.set_selected(SORT_DEFAULT);
        }
    }

    fn reorder_playlist(self: &Rc<Self>, sort_type: u32) {
        let sorted = {
            let mut list = self.tracks.borrow_mut();
            if list.is_empty() {
                return;
            }
            let descending = list.descending();
            list.set_order(sort_type, descending);
            let sorted = list.sorted_source();
            list.set_rendered(sorted.clone());
            sorted
        };
        let filter = self.tracks.borrow().filter().to_owned();
        if !filter.is_empty() {
            self.filter_content(&filter);
        } else {
            self.populate_tracks_chunked(sorted);
        }
    }

    // -- row context menu -------------------------------------------------

    fn open_row_menu(self: &Rc<Self>, row: &Rc<TrackRow>, x: f64, y: f64) {
        let Some(track) = row.track() else { return };
        let vid = track.video_id.0.clone();
        let has_selection = self.multi_select.get() && self.tracks.borrow().has_selection();
        let selection = if has_selection { self.selected_tracks() } else { Vec::new() };
        let mut extras: Vec<MenuAction> = Vec::new();

        if self.is_owned.get() {
            if has_selection {
                let n = self.tracks.borrow().selected_count();
                let page = self.clone();
                extras.push(MenuAction::new(&format!("Remove {n} from Playlist"), Section::Remove, move || page.remove_selected_from_playlist()));
            } else if let (Some(set_id), false) = (track.set_video_id.clone(), vid.is_empty()) {
                let page = self.clone();
                let vid_c = vid.clone();
                extras.push(MenuAction::new("Remove from Playlist", Section::Remove, move || page.remove_items(vec![(vid_c.clone(), set_id.clone())], false)));
            }
        }
        let is_upload = self.playlist_id().is_some_and(|p| p.starts_with("UPLOAD"));
        if let (true, Some(entity_id)) = (is_upload, track.entity_id.clone()) {
            let page = self.clone();
            let title = if track.title.is_empty() { "this song".to_owned() } else { track.title.clone() };
            extras.push(MenuAction::new("Delete Upload", Section::Remove, move || page.confirm_delete_upload_track(&entity_id, &title)));
        }
        if self.multi_select.get() {
            let is_selected = !vid.is_empty() && self.tracks.borrow().is_selected(&vid);
            let page = self.clone();
            let row_c = row.clone();
            let vid_c = vid.clone();
            extras.push(MenuAction::new(if is_selected { "Deselect This" } else { "Select This" }, Section::Remove, move || page.toggle_track_selection(&vid_c, Some(&row_c))));
            let page = self.clone();
            extras.push(MenuAction::new("Select All", Section::Remove, move || page.select_all()));
            let page = self.clone();
            extras.push(MenuAction::new("Deselect All", Section::Remove, move || page.deselect_all()));
            if self.tracks.borrow().has_selection() {
                let page = self.clone();
                extras.push(MenuAction::new("Copy Selection Data (Debug)", Section::Clipboard, move || page.copy_selection_debug()));
            }
        }
        let album = Some((self.title_text.borrow().clone(), self.playlist_id().unwrap_or_default()));
        let opts = SongMenuOptions { prefix: "ctx", selection, extras, nav: Some(self.ctx.nav.clone()), ctx: Some(self.ctx.clone()), album, ..SongMenuOptions::default() };
        show_song_menu(row.widget(), x, y, &track, &self.ctx.player, opts);
    }

    fn remove_selected_from_playlist(self: &Rc<Self>) {
        let to_remove: Vec<(String, String)> = self.selected_tracks().into_iter().filter_map(|t| Some((t.video_id.0.clone(), t.set_video_id?))).filter(|(v, _)| !v.is_empty()).collect();
        if to_remove.is_empty() {
            return;
        }
        self.remove_items(to_remove, true);
    }

    fn confirm_delete_upload_track(self: &Rc<Self>, entity_id: &str, title: &str) {
        let dialog = adw::AlertDialog::builder().heading("Delete Upload?").body(format!("Are you sure you want to delete \"{title}\"?\nThis cannot be undone.")).build();
        dialog.add_response("cancel", "Cancel");
        dialog.add_response("delete", "Delete");
        dialog.set_response_appearance("delete", adw::ResponseAppearance::Destructive);
        dialog.set_default_response(Some("cancel"));
        dialog.set_close_response("cancel");
        let weak = Rc::downgrade(self);
        let entity_id = entity_id.to_owned();
        let title = title.to_owned();
        dialog.connect_response(None, move |_, response| {
            if response != "delete" {
                return;
            }
            let Some(page) = weak.upgrade() else { return };
            let api = page.ctx.net.client().api();
            let eid = entity_id.clone();
            let handle = page.ctx.net.spawn(async move { api.send_request("music/delete_privately_owned_entity", serde_json::json!({ "entityId": eid })).await });
            let weak = Rc::downgrade(&page);
            let (eid, title) = (entity_id.clone(), title.clone());
            glib::spawn_future_local(async move {
                let Some(page) = weak.upgrade() else { return };
                match handle.await {
                    Ok(Ok(_)) => {
                        toast(&page.stack, &format!("Deleted {title}"));
                        page.remove_track_by_entity_id(&eid);
                    }
                    Ok(Err(err)) => tracing::warn!(%err, "delete upload failed"),
                    Err(_) => {}
                }
            });
        });
        dialog.present(Some(&self.stack));
    }

    fn remove_track_by_entity_id(self: &Rc<Self>, entity_id: &str) {
        self.tracks.borrow_mut().remove(|t| t.entity_id.as_deref() == Some(entity_id));
        self.clear_track_store();
        for t in self.tracks.borrow().rendered() {
            self.track_store.append(&TrackObject::new(t.clone()));
        }
        self.update_duration_from_all_tracks();
    }

    // -- refresh, virtual lists -------------------------------------------

    /// Port of _invalidate_disk_cache: drop cached state so a smaller fetch
    /// after a deletion replaces the old render.
    fn invalidate_disk_cache(&self) {
        if let Some(pid) = self.playlist_id() {
            self.ctx.net.caches().drop_cached_tracks(&pid);
            if let Some(id) = self.cache_write.borrow_mut().take() {
                id.remove();
            }
            let caches = self.ctx.net.caches().clone();
            self.ctx.net.spawn(async move {
                let _ = tokio::task::spawn_blocking(move || caches.disk().invalidate(&pid)).await;
            });
        }
        self.tracks.borrow_mut().clear();
        self.is_fully_fetched.set(false);
        self.is_fully_loaded.set(false);
    }

    /// Port of refresh_in_place, what the header-bar refresh button calls.
    pub fn refresh_in_place(self: &Rc<Self>) {
        let Some(pid) = self.playlist_id() else { return };
        if pid == "DOWNLOADS" {
            self.clear_track_store();
            self.tracks.borrow_mut().clear();
            self.stack.set_visible_child_name("loading");
            // No downloads database yet: the list is empty.
            let weak = Rc::downgrade(self);
            glib::idle_add_local_once(move || {
                if let Some(p) = weak.upgrade() {
                    p.show_virtual("Downloaded Songs", Vec::new(), "0 songs available offline");
                }
            });
            return;
        }
        if pid == "HISTORY" {
            toast(&self.stack, "History requires an internet connection");
            return;
        }
        if !self.ctx.online.is_online() {
            toast(&self.stack, "Refresh requires an internet connection");
            return;
        }
        self.invalidate_disk_cache();
        self.ctx.net.caches().drop_cached_tracks(&pid);
        self.drop_sort_metrics(&pid);
        self.clear_track_store();
        self.current_limit.set(INITIAL_LIMIT);
        self.stack.set_visible_child_name("loading");
        self.content_spinner.set_visible(true);
        self.load_playlist(&pid, None);
    }

    /// Set up the page as the Downloads or History list before its rows arrive.
    pub fn prepare_virtual(&self, id: &str) {
        self.playlist_id.replace(Some(id.to_owned()));
        self.is_fully_loaded.set(true);
        self.is_fully_fetched.set(true);
        self.stack.set_visible_child_name("loading");
    }

    /// Port of _reshow_virtual and _fill_downloads_page.
    pub fn show_virtual(self: &Rc<Self>, title: &str, tracks: Vec<Track>, meta1: &str) {
        self.tracks.borrow_mut().set(tracks.clone());
        let total: u32 = tracks.iter().filter_map(|t| t.duration_seconds).sum();
        let thumbnails: Vec<String> = tracks.first().and_then(|t| t.thumb.clone()).into_iter().collect();
        self.update_ui(HeaderText { title: title.to_owned(), description: String::new(), meta1: meta1.to_owned(), meta2: short_duration(total) }, thumbnails, tracks, false, None, false);
    }

    fn save_playlist_cover_async(self: &Rc<Self>, title: &str, url: &str) {
        let Some(path) = self.ctx.paths.playlist_cover_path(title) else { return };
        let http = self.ctx.net.client().http().clone();
        let auth = self.ctx.net.client().media_auth();
        let shown = path.to_string_lossy().into_owned();
        let handle = self.ctx.net.spawn(save_playlist_cover(http, auth, path, url.to_owned()));
        let weak = Rc::downgrade(self);
        glib::spawn_future_local(async move {
            // A replaced file keeps its path, and the texture cache is keyed by
            // path, so the page would keep drawing the old picture.
            if handle.await.unwrap_or(false) {
                if let Some(page) = weak.upgrade() {
                    crate::ui::cover::forget_texture(&shown);
                    if page.cover.url().as_deref() == Some(shown.as_str()) {
                        page.cover.reload();
                    }
                }
            }
        });
    }

    // -- cover menu, edit, delete -----------------------------------------

    fn on_cover_right_click(self: &Rc<Self>, x: f64, y: f64) {
        let url = self.cover.url();
        let can_edit = self.is_editable.get();
        if url.is_none() && !can_edit {
            return;
        }
        let menu = gio::Menu::new();
        let group = gio::SimpleActionGroup::new();
        if let Some(url) = url {
            menu.append(Some("Copy Cover URL"), Some("cover.copy_url"));
            let action = gio::SimpleAction::new("copy_url", None);
            action.connect_activate(move |_, _| copy_to_clipboard(&url));
            group.add_action(&action);
        }
        if can_edit {
            menu.append(Some("Edit Playlist"), Some("cover.edit_playlist"));
            let action = gio::SimpleAction::new("edit_playlist", None);
            let weak = Rc::downgrade(self);
            action.connect_activate(move |_, _| {
                if let Some(p) = weak.upgrade() {
                    p.show_edit_dialog();
                }
            });
            group.add_action(&action);
        }
        self.cover_wrapper.insert_action_group("cover", Some(&group));
        let popover = gtk::PopoverMenu::from_model(Some(&menu));
        popover.set_parent(&self.cover_wrapper);
        popover.set_pointing_to(Some(&gdk::Rectangle::new(x as i32, y as i32, 1, 1)));
        popover.connect_closed(|p| {
            let p = p.clone();
            glib::idle_add_local_once(move || p.unparent());
        });
        popover.popup();
    }

    fn on_delete_clicked(self: &Rc<Self>) {
        let title = self.title_text.borrow().clone();
        let dialog = adw::AlertDialog::builder().heading("Delete Playlist?").body(format!("Are you sure you want to delete \"{title}\"?\nThis action cannot be undone.")).build();
        dialog.add_response("cancel", "Cancel");
        dialog.add_response("delete", "Delete");
        dialog.set_response_appearance("delete", adw::ResponseAppearance::Destructive);
        dialog.set_default_response(Some("cancel"));
        dialog.set_close_response("cancel");
        let weak = Rc::downgrade(self);
        dialog.connect_response(None, move |_, response| {
            if response == "delete" {
                if let Some(p) = weak.upgrade() {
                    p.delete_playlist_confirmed();
                }
            }
        });
        dialog.present(Some(&self.stack));
    }

    fn delete_playlist_confirmed(self: &Rc<Self>) {
        let Some(pid) = self.playlist_id() else { return };
        self.content_spinner.set_visible(true);
        self.stack.set_visible_child_name("loading");
        let api = self.ctx.net.client().api();
        let pid_c = pid.clone();
        let handle = self.ctx.net.spawn(async move { playlists::delete_playlist(&api, &pid_c).await });
        let weak = Rc::downgrade(self);
        glib::spawn_future_local(async move {
            let Some(page) = weak.upgrade() else { return };
            match handle.await {
                Ok(Ok(())) => {
                    tracing::info!(pid, "playlist deleted");
                    page.invalidate_disk_cache();
                    page.ctx.net.caches().clear_library_ids();
                    if let Some(nav) = page.stack.ancestor(adw::NavigationView::static_type()).and_downcast::<adw::NavigationView>() {
                        nav.pop();
                    }
                }
                Ok(Err(err)) => {
                    tracing::warn!(%err, pid, "failed to delete playlist");
                    page.stack.set_visible_child_name("content");
                    page.content_spinner.set_visible(false);
                }
                Err(_) => {}
            }
        });
    }

    /// Port of _show_edit_dialog. The Python save job only mirrored the new
    /// cover locally and reloaded; this one also sends the changed title,
    /// description and privacy to the edit endpoint.
    fn show_edit_dialog(self: &Rc<Self>) {
        let dialog = adw::Dialog::builder().title("Edit Playlist").content_width(500).build();
        let main_box = gtk::Box::builder().orientation(gtk::Orientation::Vertical).build();
        dialog.set_child(Some(&main_box));
        let header = adw::HeaderBar::builder().css_classes(["flat"]).build();
        main_box.append(&header);
        let save_btn = gtk::Button::builder().label("Save").css_classes(["suggested-action"]).build();
        header.pack_start(&save_btn);
        let page = adw::PreferencesPage::new();
        main_box.append(&page);
        let group = adw::PreferencesGroup::builder().title("Playlist Details").margin_start(12).margin_end(12).margin_top(12).margin_bottom(12).build();
        page.add(&group);
        let title_row = adw::EntryRow::builder().title("Title").text(self.title_text.borrow().as_str()).build();
        group.add(&title_row);
        let desc_row = adw::EntryRow::builder().title("Description").text(self.description_text.borrow().as_str()).build();
        group.add(&desc_row);
        let privacy_row = adw::ComboRow::builder().title("Visibility").build();
        privacy_row.set_model(Some(&gtk::StringList::new(&["Public", "Private", "Unlisted"])));
        let current_privacy = self.privacy_text.borrow().clone().unwrap_or_else(|| "PUBLIC".to_owned()).to_uppercase();
        privacy_row.set_selected(match current_privacy.as_str() { "PRIVATE" => 1, "UNLISTED" => 2, _ => 0 });
        group.add(&privacy_row);
        let cover_row = adw::ActionRow::builder().title("Playlist Cover").subtitle("No file selected").build();
        group.add(&cover_row);
        self.selected_cover_path.replace(None);

        let choose_btn = gtk::Button::builder().label("Choose File...").valign(gtk::Align::Center).build();
        {
            let weak = Rc::downgrade(self);
            let cover_row = cover_row.clone();
            choose_btn.connect_clicked(move |_| {
                let Some(p) = weak.upgrade() else { return };
                let filter = gtk::FileFilter::new();
                filter.set_name(Some("Images"));
                filter.add_mime_type("image/jpeg");
                filter.add_mime_type("image/png");
                let filters = gio::ListStore::new::<gtk::FileFilter>();
                filters.append(&filter);
                let file_dialog = gtk::FileDialog::builder().title("Select Cover Image").filters(&filters).build();
                let parent = p.stack.root().and_downcast::<gtk::Window>();
                let weak = Rc::downgrade(&p);
                let cover_row = cover_row.clone();
                file_dialog.open(parent.as_ref(), None::<&gio::Cancellable>, move |result| {
                    let Ok(file) = result else { return };
                    let Some(path) = file.path() else { return };
                    tracing::info!(?path, "local cover file selected");
                    let Ok(pixbuf) = gtk::gdk_pixbuf::Pixbuf::from_file(&path) else { return };
                    let Some(p) = weak.upgrade() else { return };
                    let Some(window) = p.stack.root().and_downcast::<gtk::Window>() else { return };
                    let basename = file.basename().map(|b| b.to_string_lossy().into_owned()).unwrap_or_default();
                    let weak = Rc::downgrade(&p);
                    let cover_row = cover_row.clone();
                    crate::ui::crop_dialog::show(&window, pixbuf, move |cropped| {
                        let temp = std::env::temp_dir().join(format!("mixtape_crop_{}.png", std::process::id()));
                        // YouTube shows the cover at 1024 at most, and a phone
                        // photo would otherwise be a several megabyte upload.
                        let cropped = match cropped.width().max(cropped.height()) > COVER_MAX_PIXELS {
                            true => cropped.scale_simple(COVER_MAX_PIXELS, COVER_MAX_PIXELS, gtk::gdk_pixbuf::InterpType::Bilinear).unwrap_or(cropped),
                            false => cropped,
                        };
                        if cropped.savev(&temp, "png", &[]).is_ok() {
                            if let Some(p) = weak.upgrade() {
                                p.selected_cover_path.replace(Some(temp));
                            }
                            cover_row.set_subtitle(&format!("Cropped PNG: {basename}"));
                        }
                    });
                });
            });
        }
        cover_row.add_suffix(&choose_btn);

        {
            let weak = Rc::downgrade(self);
            let dialog = dialog.clone();
            save_btn.connect_clicked(move |_| {
                let Some(p) = weak.upgrade() else { return };
                let new_title = title_row.text().to_string();
                let new_desc = desc_row.text().to_string();
                let new_privacy = ["PUBLIC", "PRIVATE", "UNLISTED"][privacy_row.selected().min(2) as usize].to_owned();
                let img_path = p.selected_cover_path.borrow().clone();
                let old_cover = p.cover.url().unwrap_or_default();
                let old_title = p.title_text.borrow().clone();
                let old_desc = p.description_text.borrow().clone();
                let old_privacy = p.privacy_text.borrow().clone().unwrap_or_else(|| "PUBLIC".to_owned()).to_uppercase();

                // Optimistic update.
                p.name_label.set_label(&new_title);
                p.title_text.replace(new_title.clone());
                if !new_desc.trim().is_empty() {
                    p.description_label.set_label(&new_desc);
                    p.description_label.set_visible(true);
                } else {
                    p.description_label.set_visible(false);
                }
                p.description_text.replace(new_desc.clone());
                if let Some(path) = &img_path {
                    tracing::info!(?path, "optimistically showing local image");
                    p.is_previewing_cover.set(true);
                    p.cover.load(&path.to_string_lossy());
                }
                p.save_edits(new_title, new_desc, new_privacy, old_title, old_desc, old_privacy, img_path, old_cover);
                dialog.close();
            });
        }
        dialog.present(Some(&self.stack));
    }

    #[allow(clippy::too_many_arguments)]
    fn save_edits(self: &Rc<Self>, new_title: String, new_desc: String, new_privacy: String, old_title: String, old_desc: String, old_privacy: String, img_path: Option<PathBuf>, old_cover: String) {
        let Some(pid) = self.playlist_id() else { return };
        let clean_title = new_title.trim().to_owned();
        let clean_desc = new_desc.trim().to_owned();
        let changed = clean_title != old_title.trim() || clean_desc != old_desc.trim() || new_privacy != old_privacy;
        let api = self.ctx.net.client().api();
        let http = self.ctx.net.client().http().clone();
        let headers = self.ctx.net.client().browser_headers();
        let cover_dst = self.ctx.paths.playlist_cover_path(if clean_title.is_empty() { &old_title } else { &clean_title });
        let mirrored = cover_dst.clone();
        let had_cover = img_path.is_some();
        let pid_c = pid.clone();
        let handle = self.ctx.net.spawn(async move {
            if changed {
                tracing::info!(title = %clean_title, privacy = %new_privacy, "updating playlist metadata");
                let title = (clean_title != old_title.trim()).then_some(clean_title.as_str());
                let desc = (clean_desc != old_desc.trim()).then_some(clean_desc.as_str());
                let privacy = (new_privacy != old_privacy).then_some(new_privacy.as_str());
                if let Err(err) = playlists::edit_playlist(&api, &pid_c, title, desc, privacy).await {
                    tracing::warn!(%err, "edit playlist failed");
                }
            }
            let Some(src) = img_path else { return };
            match headers {
                Some(headers) => match playlists::set_playlist_thumbnail(&api, &http, &headers, &pid_c, &src).await {
                    Ok(()) => tracing::info!(playlist_id = %pid_c, "playlist cover uploaded"),
                    Err(err) => tracing::warn!(%err, "playlist cover upload failed"),
                },
                None => tracing::warn!("no session for the cover upload"),
            }
            // Keep a local copy so the page shows the new cover at once. The
            // sidecar stays on the address of the cover being replaced: the
            // mirror then leaves this file alone until YouTube serves a
            // different one, which is the uploaded image.
            if let Some(dst) = cover_dst {
                if let Some(dir) = dst.parent() {
                    let _ = tokio::fs::create_dir_all(dir).await;
                }
                if let Err(err) = tokio::fs::copy(&src, &dst).await {
                    tracing::warn!(%err, "local cover mirror failed");
                }
                crate::net::covers::mark_mirror(&dst, &old_cover).await;
            }
        });
        let weak = Rc::downgrade(self);
        glib::spawn_future_local(async move {
            let _ = handle.await;
            let Some(page) = weak.upgrade() else { return };
            // The cover behind that path is a different picture now.
            if let Some(path) = mirrored {
                crate::ui::cover::forget_texture(&path.to_string_lossy());
            }
            // A new cover keeps the address it had, so the library is told
            // outright rather than left to spot a difference.
            if had_cover {
                page.ctx.nav.refresh_library_card(&pid);
            }
            page.ctx.net.caches().drop_cached_tracks(&pid);
            page.load_playlist(&pid, None);
            page.ctx.nav.refresh_library();
        });
    }

    // -- compact mode -----------------------------------------------------

    pub fn set_compact_mode(&self, compact: bool) {
        self.compact.set(compact);
        if compact {
            self.stack.add_css_class("compact");
            self.header_info_box.set_orientation(gtk::Orientation::Vertical);
            self.header_info_box.set_halign(gtk::Align::Center);
            self.cover_wrapper.set_halign(gtk::Align::Center);
            self.details_col.set_halign(gtk::Align::Center);
            self.name_label.set_halign(gtk::Align::Center);
            self.name_label.set_justify(gtk::Justification::Center);
            self.description_label.set_halign(gtk::Align::Center);
            self.description_label.set_justify(gtk::Justification::Center);
            self.meta_label.set_halign(gtk::Align::Center);
            self.stats_label.set_halign(gtk::Align::Center);
            self.actions_box.set_halign(gtk::Align::Center);
        } else {
            self.stack.remove_css_class("compact");
            self.header_info_box.set_orientation(gtk::Orientation::Horizontal);
            self.header_info_box.set_halign(gtk::Align::Start);
            self.cover_wrapper.set_halign(gtk::Align::Start);
            self.details_col.set_halign(gtk::Align::Fill);
            self.name_label.set_halign(gtk::Align::Start);
            self.name_label.set_justify(gtk::Justification::Left);
            self.description_label.set_halign(gtk::Align::Start);
            self.description_label.set_justify(gtk::Justification::Left);
            self.meta_label.set_halign(gtk::Align::Start);
            self.stats_label.set_halign(gtk::Align::Start);
            self.actions_box.set_halign(gtk::Align::Start);
        }
    }
}

impl TrackRowHost for PlaylistPage {
    fn multi_select(&self) -> bool {
        self.multi_select.get()
    }

    fn is_selected(&self, video_id: &str) -> bool {
        self.tracks.borrow().is_selected(video_id)
    }

    fn toggle_selection(&self, video_id: &str, row: Option<&Rc<TrackRow>>) {
        let Some(page) = self.me.borrow().upgrade() else { return };
        page.toggle_track_selection(video_id, row);
    }

    fn row_clicked(&self, row: &Rc<TrackRow>) {
        let Some(page) = self.me.borrow().upgrade() else { return };
        if page.multi_select.get() {
            if let Some(vid) = row.video_id() {
                page.toggle_track_selection(&vid, Some(row));
                row.set_check_active(page.tracks.borrow().is_selected(&vid));
            }
            return;
        }
        if let Some(track) = row.track() {
            page.play_track(&track);
        }
    }

    fn row_menu(&self, row: &Rc<TrackRow>, x: f64, y: f64) {
        let Some(page) = self.me.borrow().upgrade() else { return };
        page.open_row_menu(row, x, y);
    }

    fn is_album_view(&self) -> bool {
        if self.is_album_view.get() {
            return true;
        }
        self.playlist_id.borrow().as_deref().is_some_and(|p| p.starts_with("MPRE") || p.starts_with("OLAK") || p.starts_with("FEmusic_library_privately_owned"))
    }

    fn page_cover(&self) -> Option<String> {
        self.cover.url()
    }
}

// -- free helpers ---------------------------------------------------------

fn row_of(item: &gtk::ListItem) -> Option<Rc<TrackRow>> {
    unsafe { item.data::<Rc<TrackRow>>("track-row") }.map(|p| unsafe { p.as_ref() }.clone())
}

fn read_more_label(text: &str) -> gtk::Label {
    let label = gtk::Label::builder().use_markup(true).css_classes(["caption"]).halign(gtk::Align::Start).build();
    label.set_markup(&format!("<a href='toggle'>{text}</a>"));
    label
}

/// The first 200 characters, cut at a word, with an ellipsis.
fn truncate_description(text: &str) -> String {
    let head: String = text.chars().take(200).collect();
    match head.rfind(' ') {
        Some(i) => format!("{}...", &head[..i]),
        None => format!("{head}..."),
    }
}

fn capitalize(text: String) -> String {
    let mut chars = text.chars();
    match chars.next() {
        Some(first) => first.to_uppercase().collect::<String>() + &chars.as_str().to_lowercase(),
        None => String::new(),
    }
}

/// "1 hr 5 min" or "4 min 12 sec", what the header stats show.
fn long_duration(total: u32) -> String {
    let hours = total / 3600;
    let minutes = (total % 3600) / 60;
    let seconds = total % 60;
    if hours > 0 { format!("{hours} hr {minutes} min") } else { format!("{minutes} min {seconds} sec") }
}

/// "1 hr 5 min" or "4 min", what the virtual lists show.
fn short_duration(total: u32) -> String {
    let hours = total / 3600;
    let minutes = (total % 3600) / 60;
    if hours > 0 { format!("{hours} hr {minutes} min") } else { format!("{minutes} min") }
}

/// `<a href='artist:ID'>Name</a>` per artist, plain when there is no id.
fn artist_markup(artists: &[Person]) -> String {
    artists
        .iter()
        .map(|a| {
            let name = glib::markup_escape_text(if a.name.is_empty() { "Unknown" } else { &a.name });
            match &a.id {
                Some(id) => format!("<a href='artist:{id}'>{name}</a>"),
                None => name.to_string(),
            }
        })
        .collect::<Vec<_>>()
        .join(", ")
}

/// Port of get_yt_music_link: albums share their OLAK playlist, MPRE ids are internal.
fn yt_music_link(item_id: &str, is_album: bool, audio_playlist_id: Option<&str>) -> String {
    if item_id.is_empty() {
        return String::new();
    }
    if item_id.starts_with("OLAK") {
        return format!("https://music.youtube.com/playlist?list={item_id}");
    }
    if is_album || item_id.starts_with("MPRE") {
        return match audio_playlist_id {
            Some(pid) => format!("https://music.youtube.com/playlist?list={pid}"),
            None => format!("https://music.youtube.com/browse/{item_id}"),
        };
    }
    format!("https://music.youtube.com/playlist?list={item_id}")
}

/// The endpoint side of _fetch_playlist_details, on the runtime.
async fn fetch_details(api: &ytmusicapi::YTMusicClient, http: &reqwest::Client, auth: Option<crate::model::HttpAuth>, id: String, limit: usize) -> Result<Fetched, NetError> {
    let mut playlist_id = id;
    if playlist_id.starts_with("OLAK") {
        match playlists::get_album_browse_id(http, auth.as_ref(), &playlist_id).await {
            Ok(Some(new_id)) if new_id.starts_with("MPRE") => {
                tracing::info!(from = %playlist_id, to = %new_id, "converted album id");
                playlist_id = new_id;
            }
            _ => {}
        }
        if playlist_id.starts_with("OLAK") {
            let (title, items) = playlists::raw_parse_playlist(api, &format!("VL{playlist_id}")).await?;
            let tracks: Vec<Track> = items.iter().filter_map(|i| i.to_track()).collect();
            if !tracks.is_empty() {
                return Ok(Fetched::Raw { title, tracks });
            }
        }
    }
    if playlist_id.starts_with("FEmusic_library_privately_owned") {
        let mut album = playlists::get_upload_album(api, &playlist_id).await?;
        // Cross-reference every uploaded song to fill missing artists and art.
        let all_songs = playlists::get_upload_songs(api).await.unwrap_or_default();
        let by_id: HashMap<&str, &Track> = all_songs.iter().map(|s| (s.video_id.as_str(), s)).collect();
        let album_thumb = album.thumbnails.last().cloned();
        let album_artists = album.author.clone();
        for track in &mut album.tracks {
            let reference = by_id.get(track.video_id.as_str()).copied();
            if track.thumb.is_none() {
                track.thumb = reference.and_then(|r| r.thumb.clone()).or_else(|| album_thumb.clone());
            }
            if let Some(r) = reference.filter(|r| !r.artists.is_empty()) {
                track.artists = r.artists.clone();
            } else if track.artists.is_empty() && !album_artists.is_empty() {
                track.artists = album_artists.clone();
            }
            if !track.artists.is_empty() {
                track.artist = track.artists.iter().map(|a| a.name.as_str()).collect::<Vec<_>>().join(", ");
            } else if let Some(r) = reference {
                track.artist = r.artist.clone();
            }
            if track.album.is_none() {
                track.album = Some(Named { name: album.title.clone(), id: Some(playlist_id.clone()) });
            }
        }
        return Ok(Fetched::Details(Box::new(album)));
    }
    if playlist_id == "LM" {
        let details = playlists::get_playlist(api, "LM", Some(limit)).await?;
        return Ok(Fetched::Details(Box::new(details)));
    }
    if playlist_id.starts_with("MPRE") {
        let details = playlists::get_album(api, &playlist_id).await?;
        return Ok(Fetched::Details(Box::new(details)));
    }
    // Brand new playlists take a moment to appear: retry like the Python page.
    let mut last_err = None;
    for attempt in 0..3 {
        match playlists::get_playlist(api, &playlist_id, Some(limit)).await {
            Ok(details) if !details.title.is_empty() => return Ok(Fetched::Details(Box::new(details))),
            Ok(details) => {
                if attempt == 2 {
                    return Ok(Fetched::Details(Box::new(details)));
                }
            }
            Err(err) => {
                tracing::warn!(%err, attempt = attempt + 1, "playlist fetch attempt failed");
                last_err = Some(err);
            }
        }
        if attempt < 2 {
            tokio::time::sleep(Duration::from_millis(1500)).await;
        }
    }
    Err(last_err.unwrap_or_else(|| NetError::Message("Failed to fetch playlist after retries".into())))
}

/// Port of get_playlist_full's first two stages: ytmusicapi, then the raw
/// continuation walk when the count says rows are missing. The yt-dlp flat
/// enumeration stage is not ported.
async fn get_playlist_full(api: &ytmusicapi::YTMusicClient, playlist_id: &str) -> Result<Vec<Track>, NetError> {
    let details = playlists::get_playlist(api, playlist_id, None).await?;
    let mut tracks = details.tracks;
    let track_count = details.track_count.unwrap_or(0) as usize;
    tracing::info!(got = tracks.len(), track_count, playlist_id, "get_playlist_full");
    if track_count > 0 && tracks.len() + 5 < track_count {
        let browse_id = if playlist_id.starts_with("VL") { playlist_id.to_owned() } else { format!("VL{playlist_id}") };
        match playlists::raw_parse_playlist(api, &browse_id).await {
            Ok((_, items)) => {
                let mut seen: HashSet<String> = tracks.iter().map(|t| t.video_id.0.clone()).collect();
                let mut added = 0;
                for item in items {
                    if let Some(track) = item.to_track() {
                        if seen.insert(track.video_id.0.clone()) {
                            tracks.push(track);
                            added += 1;
                        }
                    }
                }
                tracing::info!(added, total = tracks.len(), "raw-continuation fallback");
            }
            Err(err) => tracing::warn!(%err, "raw-continuation fallback failed"),
        }
    }
    Ok(tracks)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn links_follow_the_python_rules() {
        assert_eq!(yt_music_link("OLAK5uy_x", false, None), "https://music.youtube.com/playlist?list=OLAK5uy_x");
        assert_eq!(yt_music_link("MPREb_1", true, Some("OLAK5uy_y")), "https://music.youtube.com/playlist?list=OLAK5uy_y");
        assert_eq!(yt_music_link("MPREb_1", true, None), "https://music.youtube.com/browse/MPREb_1");
        assert_eq!(yt_music_link("PLabc", false, None), "https://music.youtube.com/playlist?list=PLabc");
    }

    #[test]
    fn durations_format_like_the_page() {
        assert_eq!(long_duration(3900), "1 hr 5 min");
        assert_eq!(long_duration(252), "4 min 12 sec");
        assert_eq!(short_duration(252), "4 min");
    }

    #[test]
    fn description_truncates_at_a_word() {
        let text = "word ".repeat(60);
        let cut = truncate_description(&text);
        assert!(cut.ends_with("..."));
        assert!(cut.len() <= 204);
    }
}

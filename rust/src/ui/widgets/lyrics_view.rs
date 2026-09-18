//! The lyrics column. Port of LyricsView in ui/widgets/lyrics_view.py: follows
//! the playing track, fetches its lyrics on tokio, renders them as tap-to-seek
//! rows with the active line centred, and offers a picker for the source, the
//! second line, other matches and a manual search.
//!
//! Two instances exist, one in the expanded player and one in the desktop
//! cover view. Only the mapped one scrolls, so they never race each other.

use std::cell::{Cell, RefCell};
use std::collections::HashMap;
use std::rc::{Rc, Weak};
use std::time::{Duration, Instant};

use adw::prelude::*;
use gtk::{glib, pango};

use crate::lyrics::model::{Alternative, LyricLine, LyricsMatch, LyricsResult};
use crate::lyrics::{DEFAULT_MANUAL_SEARCH_LIMIT, DEFAULT_MATCH_LIMIT, Lyrics, TrackQuery, prefs};
use crate::model::PlaybackStatus;
use crate::ui::context::UiContext;
use crate::ui::widgets::fade_edges_bin::FadeEdgesBin;
use crate::ui::widgets::lyric_rows::{Effects, InterludeRow, LyricRow, RowOptions, find_interludes};

/// Autoscroll stands down this long after the listener scrolls by hand.
const USER_SCROLL_PAUSE: Duration = Duration::from_secs(4);
/// After a tap-to-seek, position ticks from before the seek are ignored this long.
const SEEK_SETTLE: Duration = Duration::from_millis(600);
const SCROLL_ANIMATION_MS: f64 = 500.0;
/// Resting sizes in style.css, which the size preference multiplies.
const BASE_FONT_EM: f64 = 1.82;
const SUB_FONT_EM: f64 = 1.16;

const SECOND_LINE_LABELS: [(&str, &str); 5] = [("off", "Off"), ("auto", "Auto"), ("romanization", "Romanization"), ("translation", "Translation"), ("background", "Background vocals")];

thread_local! {
    /// Every live view, so a display pref change reaches both.
    static VIEWS: RefCell<Vec<Weak<LyricsView>>> = const { RefCell::new(Vec::new()) };
    /// One provider for the whole display: both views show the same size.
    static FONT_CSS: RefCell<Option<(gtk::CssProvider, f64)>> = const { RefCell::new(None) };
}

/// Both live views, the expanded player's and the desktop cover view's.
pub fn live_views() -> Vec<Rc<LyricsView>> {
    VIEWS.with(|views| {
        views.borrow_mut().retain(|v| v.strong_count() > 0);
        views.borrow().iter().filter_map(Weak::upgrade).collect()
    })
}

/// Push the type-size preference into the display's CSS.
fn apply_font_scale(scale: f64) {
    FONT_CSS.with(|slot| {
        let mut slot = slot.borrow_mut();
        if slot.as_ref().is_some_and(|(_, current)| *current == scale) {
            return;
        }
        let Some(display) = gtk::gdk::Display::default() else { return };
        let provider = slot.take().map(|(p, _)| p).unwrap_or_else(|| {
            let provider = gtk::CssProvider::new();
            // One step above the app stylesheet, still below user CSS.
            gtk::style_context_add_provider_for_display(&display, &provider, gtk::STYLE_PROVIDER_PRIORITY_APPLICATION + 1);
            provider
        });
        provider.load_from_string(&format!(
            ".lyrics-line .lyrics-line-label {{ font-size: {:.3}em; }}\n.lyrics-line .lyrics-line-sub {{ font-size: {:.3}em; }}\n",
            BASE_FONT_EM * scale,
            SUB_FONT_EM * scale
        ));
        *slot = Some((provider, scale));
    });
}

/// What the scroll request is aimed at. Interlude rows share no line index, so they key off the widget.
#[derive(Clone, PartialEq)]
enum ScrollTarget {
    Line(usize),
    Interlude(InterludeRow),
}

/// What activating a row of the source list does.
enum SourceAction {
    Switch(String),
    Search,
    Reset,
    Nothing,
}

#[derive(Clone, PartialEq)]
struct DisplayPrefs {
    second_line_mode: String,
    effects: String,
    sweep: bool,
    active_scale: f64,
}

pub struct LyricsView {
    root: gtk::Box,
    ctx: Rc<UiContext>,
    lyrics: Lyrics,
    stack: gtk::Stack,
    status_page: adw::StatusPage,
    scroller: gtk::ScrolledWindow,
    list: gtk::ListBox,
    picker_btn: gtk::MenuButton,
    popover: gtk::Popover,
    picker_stack: gtk::Stack,
    source_list: gtk::ListBox,
    spinner_row: gtk::ListBoxRow,
    second_line_section: gtk::Box,
    second_line_list: gtk::ListBox,
    matches_title: gtk::Label,
    matches_list: gtk::ListBox,
    search_entry: gtk::SearchEntry,
    search_list: gtk::ListBox,

    display: RefCell<DisplayPrefs>,
    /// Invalidates in-flight fetches when the track changes.
    fetch_gen: Cell<u64>,
    video_id: RefCell<Option<String>>,
    lines: RefCell<Vec<LyricLine>>,
    synced: Cell<bool>,
    source: RefCell<Option<String>>,
    active_idx: Cell<Option<usize>>,
    /// The row carrying a live cursor. Not always `active_idx`: while unmapped
    /// the index moves without lighting anything, and a rebuild drops every row.
    lit_idx: Cell<Option<usize>>,
    rows: RefCell<HashMap<usize, LyricRow>>,
    interludes: RefCell<Vec<InterludeRow>>,
    lit_interlude: RefCell<Option<InterludeRow>>,
    last_pos: Cell<f64>,
    user_scrolled_at: Cell<Option<Instant>>,
    suppress_activate: Cell<bool>,
    scroll_target: RefCell<Option<ScrollTarget>>,
    scroll_anim: RefCell<Option<gtk::TickCallbackId>>,
    seek_pending: Cell<Option<(f64, Instant)>>,
    source_actions: RefCell<Vec<SourceAction>>,
    second_line_keys: RefCell<Vec<&'static str>>,
    /// Results behind the rows of the matches and search lists.
    match_rows: RefCell<Vec<LyricsMatch>>,
    search_rows: RefCell<Vec<LyricsMatch>>,
    matches_source: RefCell<Option<String>>,
}

fn heading(text: &str) -> gtk::Label {
    gtk::Label::builder().label(text).halign(gtk::Align::Start).css_classes(["heading"]).build()
}

fn picker_list() -> gtk::ListBox {
    // navigation-sidebar gives the row hover without the boxed-list card look.
    gtk::ListBox::builder().selection_mode(gtk::SelectionMode::None).css_classes(["navigation-sidebar", "lyrics-source-list"]).build()
}

fn clear_list(list: &gtk::ListBox) {
    while let Some(child) = list.first_child() {
        list.remove(&child);
    }
}

fn loading_row(text: &str) -> gtk::ListBoxRow {
    let content = gtk::Box::builder().orientation(gtk::Orientation::Horizontal).spacing(8).halign(gtk::Align::Center).margin_top(8).margin_bottom(8).build();
    content.append(&adw::Spinner::builder().width_request(16).height_request(16).build());
    content.append(&gtk::Label::builder().label(text).css_classes(["dim-label"]).build());
    gtk::ListBoxRow::builder().activatable(false).child(&content).build()
}

fn message_row(text: &str) -> gtk::ListBoxRow {
    let label = gtk::Label::builder().label(text).margin_top(8).margin_bottom(8).css_classes(["dim-label"]).build();
    gtk::ListBoxRow::builder().activatable(false).child(&label).build()
}

fn timing_words(result: &LyricsResult, capitalized: bool) -> &'static str {
    match (result.is_word_level(), result.synced, capitalized) {
        (true, _, true) => "Word by word",
        (true, _, false) => "word by word",
        (false, true, true) => "Line by line",
        (false, true, false) => "line by line",
        (false, false, true) => "No timing",
        (false, false, false) => "no timing",
    }
}

fn match_row(found: &LyricsMatch, source: Option<&str>) -> gtk::ListBoxRow {
    let text = gtk::Box::new(gtk::Orientation::Vertical, 0);
    text.append(&gtk::Label::builder().label(&found.label).halign(gtk::Align::Start).ellipsize(pango::EllipsizeMode::End).build());
    let mut bits: Vec<&str> = Vec::new();
    bits.extend(source);
    if !found.detail.is_empty() {
        bits.push(&found.detail);
    }
    bits.push(timing_words(&found.result, false));
    text.append(&gtk::Label::builder().label(bits.join(" \u{b7} ")).halign(gtk::Align::Start).ellipsize(pango::EllipsizeMode::End).css_classes(["dim-label", "caption"]).build());
    gtk::ListBoxRow::builder().activatable(true).child(&text).build()
}

/// A back button and a title, the header of the picker's second pages.
fn sub_page(title: &gtk::Label, list: &gtk::ListBox, max_height: i32, extra: Option<&gtk::Widget>) -> (gtk::Box, gtk::Button) {
    let page = gtk::Box::new(gtk::Orientation::Vertical, 6);
    let header = gtk::Box::new(gtk::Orientation::Horizontal, 4);
    let back = gtk::Button::builder().icon_name("go-previous-symbolic").tooltip_text("Back to sources").build();
    // The builder would replace the image-button class the icon brings.
    back.add_css_class("flat");
    header.append(&back);
    header.append(title);
    page.append(&header);
    if let Some(extra) = extra {
        page.append(extra);
    }
    // Several matches per provider is normal, so this scrolls instead of growing past the window.
    let scroller = gtk::ScrolledWindow::builder().hscrollbar_policy(gtk::PolicyType::Never).propagate_natural_height(true).max_content_height(max_height).child(list).build();
    crate::ui::suppress_hover_while_scrolling(&scroller);
    page.append(&scroller);
    (page, back)
}

impl LyricsView {
    pub fn new(ctx: Rc<UiContext>) -> Rc<Self> {
        let lyrics = ctx.lyrics.clone();
        let lyrics_prefs = lyrics.prefs();
        let display = DisplayPrefs {
            second_line_mode: lyrics_prefs.ensure_second_line_mode(),
            effects: lyrics_prefs.effects_level(),
            sweep: lyrics_prefs.line_sweep(),
            active_scale: lyrics_prefs.active_scale(),
        };
        apply_font_scale(lyrics_prefs.font_scale());

        let root = gtk::Box::builder().orientation(gtk::Orientation::Vertical).hexpand(true).vexpand(true).css_classes(["lyrics-view"]).build();
        let stack = gtk::Stack::builder().transition_type(gtk::StackTransitionType::Crossfade).transition_duration(150).hexpand(true).vexpand(true).build();
        let stack_overlay = gtk::Overlay::builder().child(&stack).build();
        root.append(&stack_overlay);

        // -- loading page ------------------------------------------------
        let loading = gtk::Box::builder().orientation(gtk::Orientation::Vertical).spacing(12).valign(gtk::Align::Center).halign(gtk::Align::Center).vexpand(true).build();
        loading.append(&adw::Spinner::builder().width_request(36).height_request(36).build());
        stack.add_named(&loading, Some("loading"));

        // -- empty page ----------------------------------------------------
        let status_page = adw::StatusPage::builder().icon_name("format-justify-fill-symbolic").title("No lyrics").description("No lyrics found for this track.").vexpand(true).build();
        stack.add_named(&status_page, Some("empty"));

        // -- lyrics page -----------------------------------------------------
        let scroller = gtk::ScrolledWindow::builder().hscrollbar_policy(gtk::PolicyType::Never).hexpand(true).vexpand(true).css_classes(["lyrics-scroller"]).build();
        crate::ui::suppress_hover_while_scrolling(&scroller);
        // A small top margin keeps the first line out of the fade band. The
        // large bottom one lets the last lines reach the viewport centre.
        let list = gtk::ListBox::builder().selection_mode(gtk::SelectionMode::Single).margin_top(32).margin_bottom(400).margin_start(16).margin_end(16).css_classes(["lyrics-list"]).build();
        let clamp = adw::Clamp::builder().maximum_size(820).tightening_threshold(640).child(&list).build();
        scroller.set_child(Some(&clamp));
        let fade = FadeEdgesBin::new(20.0, 80.0);
        fade.set_orientation(gtk::Orientation::Vertical);
        fade.set_hexpand(true);
        fade.set_vexpand(true);
        fade.append(&scroller);
        let lyrics_page = gtk::Box::new(gtk::Orientation::Horizontal, 0);
        lyrics_page.append(&fade);
        stack.add_named(&lyrics_page, Some("lyrics"));

        // -- source picker -----------------------------------------------------
        let picker_btn = gtk::MenuButton::builder()
            .icon_name("view-more-symbolic")
            .tooltip_text("Choose lyrics source")
            .halign(gtk::Align::End)
            .valign(gtk::Align::Start)
            .margin_end(20)
            .visible(false)
            .build();
        picker_btn.add_css_class("circular");
        picker_btn.add_css_class("lyrics-osd-btn");
        let popover = gtk::Popover::builder().position(gtk::PositionType::Bottom).width_request(200).build();

        let sources_page = gtk::Box::new(gtk::Orientation::Vertical, 6);
        let header = heading("Lyrics source");
        header.set_margin_start(10);
        header.set_margin_top(4);
        header.set_margin_bottom(2);
        sources_page.append(&header);
        let source_list = picker_list();
        sources_page.append(&source_list);

        let spinner_content = gtk::Box::builder().orientation(gtk::Orientation::Horizontal).spacing(8).margin_top(6).margin_bottom(6).margin_start(8).margin_end(8).halign(gtk::Align::Center).build();
        spinner_content.append(&adw::Spinner::builder().width_request(16).height_request(16).build());
        spinner_content.append(&gtk::Label::builder().label("Searching\u{2026}").css_classes(["dim-label"]).build());
        let spinner_row = gtk::ListBoxRow::builder().selectable(false).activatable(false).child(&spinner_content).build();

        // Which second lines exist is a property of the track, so the choice sits next to the lyrics too.
        let second_line_section = gtk::Box::builder().orientation(gtk::Orientation::Vertical).spacing(6).visible(false).build();
        second_line_section.append(&gtk::Separator::new(gtk::Orientation::Horizontal));
        let second_header = heading("Second line");
        second_header.set_margin_start(10);
        second_header.set_margin_bottom(2);
        second_line_section.append(&second_header);
        let second_line_list = picker_list();
        second_line_section.append(&second_line_list);
        sources_page.append(&second_line_section);

        let matches_title = heading("");
        matches_title.set_ellipsize(pango::EllipsizeMode::End);
        let matches_list = picker_list();
        let (matches_page, matches_back) = sub_page(&matches_title, &matches_list, 300, None);

        let search_entry = gtk::SearchEntry::builder().placeholder_text("Song title").margin_start(4).margin_end(4).build();
        let search_list = picker_list();
        let (search_page, search_back) = sub_page(&heading("Search by name"), &search_list, 280, Some(search_entry.upcast_ref()));

        let picker_stack = gtk::Stack::builder().transition_type(gtk::StackTransitionType::SlideLeftRight).transition_duration(150).build();
        picker_stack.add_named(&sources_page, Some("sources"));
        picker_stack.add_named(&matches_page, Some("matches"));
        picker_stack.add_named(&search_page, Some("search"));
        popover.set_child(Some(&picker_stack));
        picker_btn.set_popover(Some(&popover));
        // Over the whole stack, so it stays reachable when a track has no lyrics.
        stack_overlay.add_overlay(&picker_btn);
        stack.set_visible_child_name("empty");

        let this = Rc::new(Self {
            root,
            lyrics,
            stack,
            status_page,
            scroller,
            list,
            picker_btn,
            popover,
            picker_stack,
            source_list,
            spinner_row,
            second_line_section,
            second_line_list,
            matches_title,
            matches_list,
            search_entry,
            search_list,
            display: RefCell::new(display),
            fetch_gen: Cell::new(0),
            video_id: RefCell::new(None),
            lines: RefCell::new(Vec::new()),
            synced: Cell::new(false),
            source: RefCell::new(None),
            active_idx: Cell::new(None),
            lit_idx: Cell::new(None),
            rows: RefCell::new(HashMap::new()),
            interludes: RefCell::new(Vec::new()),
            lit_interlude: RefCell::new(None),
            last_pos: Cell::new(0.0),
            user_scrolled_at: Cell::new(None),
            suppress_activate: Cell::new(false),
            scroll_target: RefCell::new(None),
            scroll_anim: RefCell::new(None),
            seek_pending: Cell::new(None),
            source_actions: RefCell::new(Vec::new()),
            second_line_keys: RefCell::new(Vec::new()),
            match_rows: RefCell::new(Vec::new()),
            search_rows: RefCell::new(Vec::new()),
            matches_source: RefCell::new(None),
            ctx,
        });
        VIEWS.with(|views| views.borrow_mut().push(Rc::downgrade(&this)));
        this.wire(&matches_back, &search_back);

        let video_id = this.ctx.player.state().video_id();
        if !video_id.is_empty() {
            this.video_id.replace(Some(video_id));
            this.refresh_for_current_track();
        }
        this
    }

    pub fn widget(&self) -> &gtk::Box {
        &self.root
    }

    fn wire(self: &Rc<Self>, matches_back: &gtk::Button, search_back: &gtk::Button) {
        // A weak handle per closure keeps the view free to drop with its window.
        macro_rules! weak {
            (|$this:ident $(, $arg:pat_param)*| $body:expr) => {{
                let weak = Rc::downgrade(self);
                move |$($arg),*| {
                    if let Some($this) = weak.upgrade() {
                        $body
                    }
                }
            }};
        }
        self.root.connect_map(weak!(|v, _| v.on_map()));
        self.list.connect_row_selected(weak!(|v, _, row| {
            if let Some(row) = row {
                v.scroll_to_row(row);
            }
        }));
        self.list.connect_row_activated(weak!(|v, _, row| v.on_row_activated(row)));
        // Only the scroll controller: a drag gesture fired on incidental pointer movement.
        let scroll = gtk::EventControllerScroll::new(gtk::EventControllerScrollFlags::VERTICAL);
        let weak = Rc::downgrade(self);
        scroll.connect_scroll(move |_, _, _| {
            if let Some(v) = weak.upgrade() {
                v.user_scrolled_at.set(Some(Instant::now()));
            }
            glib::Propagation::Proceed
        });
        self.scroller.add_controller(scroll);

        self.popover.connect_closed(weak!(|v, _| v.show_picker_page("sources")));
        self.picker_btn.connect_active_notify(weak!(|v, btn| {
            if btn.is_active() {
                v.on_picker_opened();
            }
        }));
        self.source_list.connect_row_activated(weak!(|v, _, row| v.on_source_row_activated(row.index())));
        self.second_line_list.connect_row_activated(weak!(|v, _, row| v.on_second_line_row_activated(row.index())));
        self.matches_list.connect_row_activated(weak!(|v, _, row| v.on_match_row_activated(row.index(), false)));
        self.search_list.connect_row_activated(weak!(|v, _, row| v.on_match_row_activated(row.index(), true)));
        matches_back.connect_clicked(weak!(|v, _| v.show_picker_page("sources")));
        search_back.connect_clicked(weak!(|v, _| v.show_picker_page("sources")));
        self.search_entry.connect_activate(weak!(|v, _| v.on_manual_search()));

        let state = self.ctx.player.state().clone();
        state.connect_video_id_notify(weak!(|v, state| v.on_metadata_changed(&state.video_id())));
        state.connect_position_notify(weak!(|v, state| v.on_progression(state.position())));
        state.connect_status_notify(weak!(|v, state| v.on_state_changed(state.status())));
    }

    // -- track metadata ---------------------------------------------------------

    /// Title, first artist and duration of the playing track, what the providers match on.
    fn track_query(&self) -> Option<TrackQuery> {
        let video_id = self.video_id.borrow().clone()?;
        let track = self.ctx.player.current_track().filter(|t| t.video_id.0 == video_id);
        let title = track.as_ref().map(|t| t.title.clone());
        let artist = track.as_ref().map(|t| t.artists.iter().map(|a| a.name.as_str()).find(|n| !n.is_empty()).unwrap_or(t.artist.as_str()).to_owned()).filter(|a| !a.is_empty());
        let duration = track.as_ref().and_then(|t| t.duration_seconds).filter(|d| *d > 0).or_else(|| {
            let live = self.ctx.player.state().duration();
            (live > 0.0).then_some(live as u32)
        });
        Some(TrackQuery::new(&video_id, title.as_deref(), artist.as_deref(), duration))
    }

    // -- player signals -----------------------------------------------------------

    fn on_metadata_changed(self: &Rc<Self>, video_id: &str) {
        if self.video_id.borrow().as_deref() == Some(video_id) && !self.lines.borrow().is_empty() {
            return;
        }
        self.video_id.replace((!video_id.is_empty()).then(|| video_id.to_owned()));
        self.refresh_for_current_track();
    }

    fn on_state_changed(&self, status: PlaybackStatus) {
        let paused = status == PlaybackStatus::Paused;
        for row in self.rows.borrow().values() {
            row.set_paused(paused);
        }
        if status == PlaybackStatus::Stopped && self.ctx.player.state().video_id().is_empty() {
            self.video_id.replace(None);
            self.lines.borrow_mut().clear();
            self.render_status("Not playing", None);
        }
    }

    fn on_progression(self: &Rc<Self>, pos: f64) {
        self.last_pos.set(pos);
        if !self.synced.get() || self.lines.borrow().is_empty() {
            return;
        }
        if !self.root.is_mapped() {
            // Keep the index current so becoming visible does not replay a stale activation.
            self.active_idx.set(self.index_for_position(pos));
            return;
        }
        if let Some((start, at)) = self.seek_pending.get() {
            if (pos - start).abs() > 1.5 && at.elapsed() < SEEK_SETTLE {
                return;
            }
            self.seek_pending.set(None);
        }

        let ms = (pos * 1000.0) as i64;
        // An instrumental stretch takes over: the dots fill and the last sung line dims.
        if let Some(interlude) = self.interlude_at(ms) {
            self.enter_interlude(&interlude, ms);
            return;
        }
        if let Some(lit) = self.lit_interlude.borrow_mut().take() {
            lit.set_cursor_ms(-1);
            // Force the next activation to scroll back onto the vocal line.
            self.active_idx.set(None);
        }

        let idx = self.index_for_position(pos).filter(|i| self.lines.borrow()[*i].end.is_none_or(|end| pos < end));
        if idx != self.active_idx.get() {
            self.activate_row(idx, ms);
        } else if let Some(row) = idx.and_then(|i| self.rows.borrow().get(&i).cloned()) {
            row.set_cursor_ms(ms);
        }
    }

    fn on_map(self: &Rc<Self>) {
        if !self.synced.get() || self.lines.borrow().is_empty() {
            return;
        }
        let pos = self.last_pos.get();
        let ms = (pos * 1000.0) as i64;
        // Re-activate even when the index did not move while hidden: the row was never scrolled to.
        self.active_idx.set(None);
        match self.interlude_at(ms) {
            Some(interlude) => {
                self.lit_interlude.replace(None);
                self.enter_interlude(&interlude, ms);
            }
            None => self.activate_row(self.index_for_position(pos), ms),
        }
    }

    // -- fetch pipeline --------------------------------------------------------------

    /// Drop this track's cached lyrics and fetch again from scratch.
    pub fn refresh(self: &Rc<Self>) {
        if let Some(video_id) = self.video_id.borrow().clone() {
            self.lyrics.cache().invalidate(&video_id);
            self.refresh_for_current_track();
        }
    }

    fn refresh_for_current_track(self: &Rc<Self>) {
        // Wipe everything first so a late position tick cannot index stale lines.
        self.fetch_gen.set(self.fetch_gen.get() + 1);
        self.lines.borrow_mut().clear();
        self.synced.set(false);
        self.active_idx.set(None);
        self.source.replace(None);
        self.clear_rows();
        let Some(query) = self.track_query() else {
            self.picker_btn.set_visible(false);
            self.render_status("Not playing", Some("Play a song to see lyrics."));
            return;
        };
        self.picker_btn.set_visible(true);
        self.stack.set_visible_child_name("loading");

        let request = self.fetch_gen.get();
        let lyrics = self.lyrics.clone();
        let handle = self.ctx.net.spawn(async move { lyrics.get_lyrics(&query).await });
        let weak = Rc::downgrade(self);
        glib::spawn_future_local(async move {
            let result = handle.await.ok().flatten();
            if let Some(view) = weak.upgrade().filter(|v| v.fetch_gen.get() == request) {
                view.apply_fetch_result(result);
            }
        });
    }

    fn apply_fetch_result(self: &Rc<Self>, result: Option<LyricsResult>) {
        let Some(result) = result.filter(|r| !r.lines.is_empty()) else {
            self.lines.borrow_mut().clear();
            self.synced.set(false);
            self.render_status("No lyrics", Some("Nothing matched automatically. Try another source, or search by name."));
            self.picker_btn.set_visible(true);
            return;
        };
        tracing::debug!(source = %result.source, synced = result.synced, lines = result.lines.len(), "lyrics fetched");
        let source = result.source.clone();
        self.show_result(&source, result, false);
    }

    /// Render one provider's lyrics for the current track.
    fn show_result(self: &Rc<Self>, source: &str, result: LyricsResult, switched: bool) {
        self.synced.set(result.synced);
        self.lines.replace(result.lines);
        self.source.replace(Some(source.to_owned()));
        self.active_idx.set(None);
        self.build_rows();
        self.stack.set_visible_child_name("lyrics");
        self.picker_btn.set_visible(true);
        self.picker_btn.set_tooltip_text(Some(&format!("Lyrics source: {source}")));
        if switched {
            self.refresh_second_line_rows();
        }
        if self.synced.get() && self.ctx.player.state().duration() > 0.0 {
            let pos = self.last_pos.get();
            self.activate_row(self.index_for_position(pos), (pos * 1000.0) as i64);
        } else if !switched {
            let adj = self.scroller.vadjustment();
            adj.set_value(adj.lower());
        }
    }

    fn render_status(&self, title: &str, description: Option<&str>) {
        self.status_page.set_title(title);
        if let Some(description) = description {
            self.status_page.set_description(Some(description));
        }
        self.stack.set_visible_child_name("empty");
    }

    // -- display prefs -------------------------------------------------------------------

    /// Re-read the display prefs and rebuild the rows in place. No refetch:
    /// the lines already carry every second line the provider had.
    pub fn apply_display_prefs(self: &Rc<Self>) {
        let lyrics_prefs = self.lyrics.prefs();
        lyrics_prefs.invalidate();
        apply_font_scale(lyrics_prefs.font_scale());
        let display = DisplayPrefs {
            second_line_mode: lyrics_prefs.second_line_mode(),
            effects: lyrics_prefs.effects_level(),
            sweep: lyrics_prefs.line_sweep(),
            active_scale: lyrics_prefs.active_scale(),
        };
        if *self.display.borrow() == display {
            return;
        }
        self.display.replace(display);
        if self.lines.borrow().is_empty() {
            return;
        }
        let active = self.active_idx.replace(None);
        self.build_rows();
        if self.synced.get() {
            let pos = self.last_pos.get();
            self.activate_row(active.or_else(|| self.index_for_position(pos)), (pos * 1000.0) as i64);
        }
    }

    /// The second-line modes this track's lyrics can fill. Off and Auto come along once anything else does.
    fn available_second_lines(&self) -> Vec<&'static str> {
        let has = |text: &Option<String>| text.as_deref().is_some_and(|t| !t.trim().is_empty());
        let lines = self.lines.borrow();
        let have = [("romanization", lines.iter().any(|l| has(&l.romanization))), ("translation", lines.iter().any(|l| has(&l.translation))), ("background", lines.iter().any(|l| has(&l.bg_text)))];
        if !have.iter().any(|(_, present)| *present) {
            return Vec::new();
        }
        SECOND_LINE_LABELS.iter().map(|(key, _)| *key).filter(|key| matches!(*key, "off" | "auto") || have.iter().any(|(k, present)| k == key && *present)).collect()
    }

    /// The saved mode, or the default when these lyrics have nothing for it.
    fn effective_second_line_mode(&self) -> String {
        let mode = self.display.borrow().second_line_mode.clone();
        let available = self.available_second_lines();
        if mode == "off" || available.is_empty() || available.contains(&mode.as_str()) { mode } else { prefs::SECOND_LINE_DEFAULT.to_owned() }
    }

    // -- rows ------------------------------------------------------------------------------

    fn clear_rows(&self) {
        // No row is lit once they are all gone. A stale index would suppress the next clear.
        self.lit_idx.set(None);
        self.rows.borrow_mut().clear();
        self.interludes.borrow_mut().clear();
        self.lit_interlude.replace(None);
        clear_list(&self.list);
    }

    fn reset_all_rows(&self) {
        self.lit_interlude.replace(None);
        for row in self.rows.borrow().values() {
            row.reset_state();
        }
        for row in self.interludes.borrow().iter() {
            row.reset_state();
        }
        self.lit_idx.set(None);
        self.active_idx.set(None);
    }

    fn build_rows(&self) {
        self.clear_rows();
        let display = self.display.borrow().clone();
        let effects = Effects::from_pref(&display.effects);
        let options = RowOptions { second_line_mode: self.effective_second_line_mode(), effects, sweep: display.sweep, active_scale: display.active_scale };
        let lines = self.lines.borrow();
        let synced = self.synced.get();
        // Interlude markers take rows of their own, so list position stops matching the line index.
        let mut pending: std::collections::VecDeque<(f64, f64)> = if synced { find_interludes(&lines).into() } else { Default::default() };
        let add_interlude = |(start, end): (f64, f64)| {
            let row = InterludeRow::new(start, end, effects);
            self.list.append(&row);
            self.interludes.borrow_mut().push(row);
        };
        for (i, line) in lines.iter().enumerate() {
            while let Some(gap) = pending.front().copied().filter(|gap| line.start.is_some_and(|start| gap.1 <= start)) {
                pending.pop_front();
                add_interlude(gap);
            }
            // A provider's empty marker line has no words. The interlude row stands in for it.
            if line.text.trim().is_empty() {
                continue;
            }
            let row = LyricRow::new(line, i, sweep_end_ms(&lines, i, synced), &options);
            // Unsynced text has no cursor, so every row is lit: full brightness reads as "no sync info".
            if !synced {
                row.set_cursor_ms(0);
            }
            self.list.append(&row);
            self.rows.borrow_mut().insert(i, row);
        }
        pending.into_iter().for_each(add_interlude);
    }

    fn index_for_position(&self, pos: f64) -> Option<usize> {
        let mut active = None;
        for (i, line) in self.lines.borrow().iter().enumerate() {
            match line.start {
                Some(start) if start <= pos => active = Some(i),
                Some(_) => break,
                None => {}
            }
        }
        active
    }

    fn interlude_at(&self, ms: i64) -> Option<InterludeRow> {
        self.interludes.borrow().iter().find(|row| row.start_ms() <= ms && ms < row.end_ms()).cloned()
    }

    fn select_quietly(&self, row: &impl IsA<gtk::ListBoxRow>) {
        self.suppress_activate.set(true);
        self.list.select_row(Some(row));
        self.suppress_activate.set(false);
    }

    fn enter_interlude(self: &Rc<Self>, row: &InterludeRow, ms: i64) {
        if self.lit_interlude.borrow().as_ref() != Some(row) {
            // Dim whatever lyric line was lit before the break.
            if let Some(prev) = self.lit_idx.take().and_then(|i| self.rows.borrow().get(&i).cloned()) {
                prev.set_cursor_ms(-1);
            }
            if let Some(prev) = self.lit_interlude.replace(Some(row.clone())) {
                prev.set_cursor_ms(-1);
            }
            self.select_quietly(row);
            self.scroll_to_row(row.upcast_ref());
        }
        row.set_cursor_ms(ms);
    }

    fn activate_row(self: &Rc<Self>, idx: Option<usize>, cursor_ms: i64) {
        // A lyric line becoming current means no instrumental break is.
        if let Some(lit) = self.lit_interlude.borrow_mut().take() {
            lit.set_cursor_ms(-1);
        }
        if let Some(prev) = self.lit_idx.get().filter(|lit| Some(*lit) != idx) {
            if let Some(row) = self.rows.borrow().get(&prev) {
                row.set_cursor_ms(-1);
            }
            self.lit_idx.set(None);
        }
        self.active_idx.set(idx);
        let Some(idx) = idx else { return };
        let Some(row) = self.rows.borrow().get(&idx).cloned() else { return };
        row.set_cursor_ms(cursor_ms);
        self.lit_idx.set(Some(idx));
        // Only the full level blurs by distance, and this walk runs on every line change.
        if self.display.borrow().effects == "full" {
            for (i, other) in self.rows.borrow().iter() {
                other.set_distance(i.abs_diff(idx) as i32);
            }
        }
        // Selection drives the :selected style and, through row-selected, the autoscroll.
        self.select_quietly(&row);
    }

    fn on_row_activated(self: &Rc<Self>, row: &gtk::ListBoxRow) {
        if self.suppress_activate.get() || !self.synced.get() || self.ctx.player.state().duration() <= 0.0 {
            return;
        }
        let lyric = row.downcast_ref::<LyricRow>();
        let interlude = row.downcast_ref::<InterludeRow>();
        let Some(start_ms) = lyric.map(LyricRow::start_ms).or_else(|| interlude.map(InterludeRow::start_ms)) else { return };
        let start = start_ms as f64 / 1000.0;
        self.user_scrolled_at.set(None);
        self.seek_pending.set(Some((start, Instant::now())));
        self.reset_all_rows();
        self.last_pos.set(start);
        if let Some(row) = lyric {
            self.activate_row(usize::try_from(row.line_idx()).ok(), start_ms);
        } else if let Some(row) = interlude {
            self.enter_interlude(row, start_ms);
        }
        self.ctx.player.seek(start);
    }

    // -- autoscroll ----------------------------------------------------------------------------

    fn scroll_to_row(self: &Rc<Self>, row: &gtk::ListBoxRow) {
        if self.user_scrolled_at.get().is_some_and(|at| at.elapsed() < USER_SCROLL_PAUSE) {
            return;
        }
        let target = match row.downcast_ref::<LyricRow>() {
            Some(lyric) => ScrollTarget::Line(lyric.line_idx().max(0) as usize),
            None => match row.downcast_ref::<InterludeRow>() {
                Some(interlude) => ScrollTarget::Interlude(interlude.clone()),
                None => return,
            },
        };
        // A newer request supersedes this one before its deferred scroll fires.
        self.scroll_target.replace(Some(target.clone()));
        let weak = Rc::downgrade(self);
        let row = row.clone();
        let retries = Cell::new(8);
        self.root.add_tick_callback(move |_, _| {
            let Some(view) = weak.upgrade() else { return glib::ControlFlow::Break };
            if view.scroll_target.borrow().as_ref() != Some(&target) {
                return glib::ControlFlow::Break;
            }
            // The row has no size until the list has been laid out.
            let Some(bounds) = row.compute_bounds(&view.list).filter(|b| b.height() > 0.0) else {
                retries.set(retries.get() - 1);
                return if retries.get() > 0 { glib::ControlFlow::Continue } else { glib::ControlFlow::Break };
            };
            let viewport = f64::from(view.scroller.height());
            if viewport <= 0.0 {
                return glib::ControlFlow::Break;
            }
            let adj = view.scroller.vadjustment();
            let centred = f64::from(bounds.y()) - viewport / 2.0 + f64::from(bounds.height()) / 2.0;
            view.animate_to(&adj, centred.clamp(adj.lower(), (adj.upper() - adj.page_size()).max(adj.lower())));
            glib::ControlFlow::Break
        });
    }

    /// Ease the adjustment to `target`. An animation in flight is replaced, so calls retarget smoothly.
    fn animate_to(self: &Rc<Self>, adj: &gtk::Adjustment, target: f64) {
        if let Some(id) = self.scroll_anim.borrow_mut().take() {
            id.remove();
        }
        let start_value = adj.value();
        if (start_value - target).abs() < 1.0 {
            adj.set_value(target);
            return;
        }
        let start_time = glib::monotonic_time();
        let adj = adj.clone();
        let weak = Rc::downgrade(self);
        let id = self.root.add_tick_callback(move |_, clock| {
            let elapsed_ms = (clock.frame_time() - start_time) as f64 / 1000.0;
            let t = (elapsed_ms / SCROLL_ANIMATION_MS).clamp(0.0, 1.0);
            adj.set_value(start_value + (target - start_value) * (1.0 - (1.0 - t).powi(3)));
            if t < 1.0 {
                return glib::ControlFlow::Continue;
            }
            if let Some(view) = weak.upgrade() {
                // Returning Break removes the callback, so only forget the id.
                std::mem::forget(view.scroll_anim.borrow_mut().take());
            }
            glib::ControlFlow::Break
        });
        self.scroll_anim.replace(Some(id));
    }

    // -- source picker -------------------------------------------------------------------------------

    /// The source list is a few short names. A match list has to keep "[A Cappella]" apart from "[Slowed]".
    fn show_picker_page(&self, name: &str) {
        self.popover.set_size_request(if name == "sources" { 200 } else { 340 }, -1);
        self.picker_stack.set_visible_child_name(name);
    }

    fn on_picker_opened(self: &Rc<Self>) {
        // Rebuilt on every open so the rows reflect the cache as it stands.
        self.refresh_source_rows(true);
        self.refresh_second_line_rows();
        let Some(query) = self.track_query() else { return };
        // Providers with nothing cached yet run in the background and report as they finish.
        let (tx, rx) = async_channel::unbounded::<Alternative>();
        let lyrics = self.lyrics.clone();
        self.ctx.net.spawn(async move { lyrics.fetch_alternatives(&query, tx).await });
        let weak = Rc::downgrade(self);
        let request = self.fetch_gen.get();
        glib::spawn_future_local(async move {
            let mut reported: Vec<String> = Vec::new();
            while let Ok(alternative) = rx.recv().await {
                let Some(view) = weak.upgrade().filter(|v| v.fetch_gen.get() == request) else { return };
                reported.push(alternative.source);
                if !view.picker_btn.is_active() {
                    continue;
                }
                let cached = view.cached_alternatives();
                let all_done = Lyrics::provider_names().iter().all(|name| reported.iter().any(|r| r == name) || cached.iter().any(|(s, _)| s == name));
                view.refresh_source_rows(!all_done);
            }
            // The channel closed: every provider has answered.
            if let Some(view) = weak.upgrade().filter(|v| v.fetch_gen.get() == request && v.picker_btn.is_active()) {
                view.refresh_source_rows(false);
            }
        });
    }

    fn cached_alternatives(&self) -> Vec<(String, LyricsResult)> {
        self.video_id.borrow().as_deref().map(|id| self.lyrics.cache().get_alternatives(id)).unwrap_or_default()
    }

    fn refresh_source_rows(self: &Rc<Self>, include_spinner: bool) {
        clear_list(&self.source_list);
        let mut actions = Vec::new();
        let Some(video_id) = self.video_id.borrow().clone() else {
            self.source_actions.replace(actions);
            return;
        };
        let preferred = self.lyrics.cache().get_preferred(&video_id);
        let active = self.source.borrow().clone();
        // In the order of the queue in Settings. Disabled providers still appear, at the bottom.
        let queue = self.lyrics.prefs().full_provider_order();
        let disabled = self.lyrics.prefs().disabled_providers();
        let mut alternatives = self.cached_alternatives();
        alternatives.sort_by_key(|(name, _)| (disabled.contains(name), queue.iter().position(|q| q == name).unwrap_or(queue.len()), name.clone()));

        for (source, result) in &alternatives {
            self.source_list.append(&self.source_row(source, result, active.as_deref() == Some(source), preferred.as_deref() == Some(source)));
            actions.push(SourceAction::Switch(source.clone()));
        }
        if include_spinner {
            self.source_list.append(&self.spinner_row);
            actions.push(SourceAction::Nothing);
        }

        // Always offered, and the only way in when nothing matched at all.
        let search = gtk::Box::new(gtk::Orientation::Horizontal, 8);
        search.append(&gtk::Image::from_icon_name("system-search-symbolic"));
        search.append(&gtk::Label::builder().label("Search by name\u{2026}").halign(gtk::Align::Start).hexpand(true).build());
        self.source_list.append(&gtk::ListBoxRow::builder().activatable(true).child(&search).build());
        actions.push(SourceAction::Search);

        // Only once something is pinned, or it would be an undo for a choice nobody made.
        if let Some(preferred) = preferred {
            let reset = gtk::Box::new(gtk::Orientation::Vertical, 0);
            reset.append(&gtk::Label::builder().label("Use automatic choice").halign(gtk::Align::Start).build());
            reset.append(&gtk::Label::builder().label(format!("Pinned to {preferred}")).halign(gtk::Align::Start).ellipsize(pango::EllipsizeMode::End).css_classes(["dim-label", "caption"]).build());
            self.source_list.append(&gtk::ListBoxRow::builder().activatable(true).child(&reset).build());
            actions.push(SourceAction::Reset);
        }
        self.source_actions.replace(actions);
    }

    /// Provider name, what its timing is worth, and a checkmark on the one being shown.
    fn source_row(self: &Rc<Self>, source: &str, result: &LyricsResult, is_active: bool, is_preferred: bool) -> gtk::ListBoxRow {
        let content = gtk::Box::new(gtk::Orientation::Horizontal, 12);
        let text = gtk::Box::builder().orientation(gtk::Orientation::Vertical).hexpand(true).valign(gtk::Align::Center).build();
        text.append(&gtk::Label::builder().label(source).halign(gtk::Align::Start).ellipsize(pango::EllipsizeMode::End).build());
        text.append(&gtk::Label::builder().label(timing_words(result, true)).halign(gtk::Align::Start).css_classes(["dim-label", "caption"]).build());
        content.append(&text);
        // Always present, transparent when inactive, so switching never shifts the other rows.
        content.append(&gtk::Image::builder().icon_name("object-select-symbolic").valign(gtk::Align::Center).opacity(if is_active { 1.0 } else { 0.0 }).build());
        if Lyrics::provider_supports_matches(source) {
            let more = gtk::Button::builder().icon_name("go-next-symbolic").valign(gtk::Align::Center).tooltip_text(format!("Other matches from {source}")).build();
            more.add_css_class("flat");
            let weak = Rc::downgrade(self);
            let source = source.to_owned();
            more.connect_clicked(move |_| {
                if let Some(view) = weak.upgrade() {
                    view.open_matches(&source);
                }
            });
            content.append(&more);
        }
        let row = gtk::ListBoxRow::builder().activatable(true).child(&content).build();
        if is_preferred {
            row.set_tooltip_text(Some(&format!("Pinned to {source} for this track")));
        }
        row
    }

    fn on_source_row_activated(self: &Rc<Self>, index: i32) {
        let Some(video_id) = self.video_id.borrow().clone() else { return };
        let actions = self.source_actions.borrow();
        match usize::try_from(index).ok().and_then(|i| actions.get(i)) {
            Some(SourceAction::Search) => {
                drop(actions);
                self.open_search();
            }
            Some(SourceAction::Reset) => {
                drop(actions);
                self.lyrics.cache().clear_user_choice(&video_id);
                self.refresh_for_current_track();
                self.picker_btn.set_active(false);
            }
            Some(SourceAction::Switch(source)) => {
                let source = source.clone();
                drop(actions);
                // Pinned, so later plays of this track come back to the same source.
                self.lyrics.cache().set_preferred(&video_id, Some(&source));
                if let Some((_, result)) = self.cached_alternatives().into_iter().find(|(name, _)| *name == source) {
                    self.show_result(&source, result, true);
                }
                self.picker_btn.set_active(false);
            }
            Some(SourceAction::Nothing) | None => {}
        }
    }

    fn refresh_second_line_rows(&self) {
        clear_list(&self.second_line_list);
        let available = self.available_second_lines();
        self.second_line_section.set_visible(!available.is_empty());
        let current = self.effective_second_line_mode();
        for key in &available {
            let label = SECOND_LINE_LABELS.iter().find(|(k, _)| k == key).map_or(*key, |(_, label)| *label);
            let content = gtk::Box::new(gtk::Orientation::Horizontal, 12);
            content.append(&gtk::Label::builder().label(label).halign(gtk::Align::Start).hexpand(true).build());
            content.append(&gtk::Image::builder().icon_name("object-select-symbolic").valign(gtk::Align::Center).opacity(if *key == current { 1.0 } else { 0.0 }).build());
            self.second_line_list.append(&gtk::ListBoxRow::builder().activatable(true).child(&content).build());
        }
        self.second_line_keys.replace(available);
    }

    fn on_second_line_row_activated(&self, index: i32) {
        let mode = usize::try_from(index).ok().and_then(|i| self.second_line_keys.borrow().get(i).copied());
        if let Some(mode) = mode.filter(|m| *m != self.display.borrow().second_line_mode) {
            self.lyrics.prefs().set_second_line_mode(mode);
            // Both views read the same pref.
            for view in live_views() {
                view.apply_display_prefs();
            }
        }
        self.picker_btn.set_active(false);
    }

    // -- other matches and manual search -------------------------------------------------------------------

    /// Show every match `source` has for the current track.
    fn open_matches(self: &Rc<Self>, source: &str) {
        let Some(query) = self.track_query() else { return };
        self.matches_title.set_label(source);
        self.matches_source.replace(Some(source.to_owned()));
        self.match_rows.borrow_mut().clear();
        clear_list(&self.matches_list);
        self.matches_list.append(&loading_row("Searching\u{2026}"));
        self.show_picker_page("matches");

        let request = self.fetch_gen.get();
        let lyrics = self.lyrics.clone();
        let source = source.to_owned();
        let asked = source.clone();
        let handle = self.ctx.net.spawn(async move { lyrics.fetch_provider_matches(&asked, &query, DEFAULT_MATCH_LIMIT).await });
        let weak = Rc::downgrade(self);
        glib::spawn_future_local(async move {
            let matches = handle.await.unwrap_or_default();
            let Some(view) = weak.upgrade() else { return };
            // A track change while searching invalidates the list.
            if view.fetch_gen.get() != request || view.matches_source.borrow().as_deref() != Some(source.as_str()) {
                return;
            }
            clear_list(&view.matches_list);
            if matches.is_empty() {
                view.matches_list.append(&message_row(&format!("No other matches from {source}")));
            }
            for found in &matches {
                view.matches_list.append(&match_row(found, None));
            }
            view.match_rows.replace(matches);
        });
    }

    fn open_search(&self) {
        // Seeded with the title, so the common case is deleting the bracketed credits.
        if self.search_entry.text().is_empty() {
            if let Some(query) = self.track_query().filter(|q| !q.title.is_empty()) {
                self.search_entry.set_text(&query.title);
            }
        }
        self.show_picker_page("search");
        self.search_entry.grab_focus();
    }

    fn on_manual_search(self: &Rc<Self>) {
        let text = self.search_entry.text().trim().to_owned();
        let Some(query) = self.track_query().filter(|_| !text.is_empty()) else { return };
        self.search_rows.borrow_mut().clear();
        clear_list(&self.search_list);
        self.search_list.append(&loading_row("Searching\u{2026}"));
        self.matches_source.replace(None);

        let request = self.fetch_gen.get();
        let lyrics = self.lyrics.clone();
        let handle = self.ctx.net.spawn(async move {
            let artist = (!query.artist.is_empty()).then_some(query.artist.as_str());
            lyrics.search_manually(&text, artist, (query.duration > 0).then_some(query.duration), DEFAULT_MANUAL_SEARCH_LIMIT).await
        });
        let weak = Rc::downgrade(self);
        glib::spawn_future_local(async move {
            let matches = handle.await.unwrap_or_default();
            let Some(view) = weak.upgrade().filter(|v| v.fetch_gen.get() == request) else { return };
            clear_list(&view.search_list);
            if matches.is_empty() {
                view.search_list.append(&message_row("Nothing found"));
            }
            for found in &matches {
                view.search_list.append(&match_row(found, found.source.as_deref()));
            }
            view.search_rows.replace(matches);
        });
    }

    fn on_match_row_activated(self: &Rc<Self>, index: i32, from_search: bool) {
        let rows = if from_search { &self.search_rows } else { &self.match_rows };
        let Some(found) = usize::try_from(index).ok().and_then(|i| rows.borrow().get(i).cloned()) else { return };
        let Some(source) = found.source.clone().or_else(|| self.matches_source.borrow().clone()) else { return };
        let Some(query) = self.track_query() else { return };
        self.picker_btn.set_active(false);

        // Stored as this provider's result and pinned, so the choice survives the next play.
        let request = self.fetch_gen.get();
        let lyrics = self.lyrics.clone();
        let chosen = source.clone();
        let handle = self.ctx.net.spawn(async move { lyrics.choose_match(&query, &chosen, found.result).await });
        let weak = Rc::downgrade(self);
        glib::spawn_future_local(async move {
            let Ok(result) = handle.await else { return };
            if let Some(view) = weak.upgrade().filter(|v| v.fetch_gen.get() == request) {
                view.show_result(&source, result, true);
            }
        });
    }
}

/// When line `idx` gives way to the next, in ms: its own end, else the next
/// timed line's start. A blank marker line counts, since singing stops there.
fn sweep_end_ms(lines: &[LyricLine], idx: usize, synced: bool) -> Option<i64> {
    if !synced {
        return None;
    }
    let end = lines[idx].end.or_else(|| lines[idx + 1..].iter().find_map(|l| l.start));
    end.map(|seconds| (seconds * 1000.0) as i64)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_sweep_ends_at_the_line_end_or_the_next_start() {
        let mut first = LyricLine::new(Some(1.0), "a");
        first.end = Some(2.5);
        let lines = vec![first, LyricLine::new(Some(4.0), "b"), LyricLine::new(None, "c"), LyricLine::new(Some(9.0), "d")];
        assert_eq!(sweep_end_ms(&lines, 0, true), Some(2500));
        assert_eq!(sweep_end_ms(&lines, 1, true), Some(9000));
        assert_eq!(sweep_end_ms(&lines, 3, true), None);
        assert_eq!(sweep_end_ms(&lines, 0, false), None);
    }
}

//! Port of ui/pages/search.py: the Explore feed (mood and genre pills, new
//! releases, videos, trending and the charts) and the search results view
//! with its toggle tabs.

use std::cell::{Cell, RefCell};
use std::rc::Rc;
use std::time::Duration;

use adw::prelude::*;
use gtk::glib;

use crate::model::{ItemKind, MediaItem};
use crate::net::explore::{self, Category, ChartArtist, Charts, ExploreData, Trend};
use crate::net::search::{SearchResults, search_all};
use crate::ui::context::{NavRequest, UiContext};
use crate::ui::cover::CoverImage;
use crate::ui::pages::{activate_item, attach_item_menu, clear_children, loading_box};
use crate::ui::toast;
use crate::ui::widgets::media_card::{CardOptions, MediaCard};
use crate::ui::widgets::scroll_box::HorizontalScrollBox;
use crate::ui::widgets::song_list::{SONG_THUMB_SIZE, search_subtitle, song_row_with_subtitle};
use crate::ui::widgets::song_row::SongRow;

/// How many pills a category row shows before it offers View All.
const PILL_LIMIT: usize = 20;
/// Gap between chart cards, which search.py sets on the strip itself.
const CHART_STRIP_SPACING: i32 = 12;
/// Gap between sections, tightened for the phone layout by set_compact_mode.
const SECTION_SPACING: i32 = 24;
const SECTION_SPACING_COMPACT: i32 = 16;
/// Section caps, straight from update_explore_ui.
const NEW_RELEASE_LIMIT: usize = 10;
const VIDEO_LIMIT: usize = 5;
const TRENDING_LIMIT: usize = 5;
const CHART_ARTIST_LIMIT: usize = 20;

pub struct ExplorePage {
    stack: gtk::Stack,
    explore_box: gtk::Box,
    results_stack: gtk::Stack,
    toggle_container: gtk::Box,
    ctx: Rc<UiContext>,
    cards: RefCell<Vec<Rc<MediaCard>>>,
    scrollers: RefCell<Vec<Rc<HorizontalScrollBox>>>,
    toggle_group: RefCell<Option<adw::ToggleGroup>>,
    song_rows: RefCell<Vec<Rc<SongRow>>>,
    explore_rows: RefCell<Vec<Rc<SongRow>>>,
    last_results: RefCell<Vec<MediaItem>>,
    current_query: RefCell<Option<String>>,
    inflight: RefCell<Option<tokio::task::AbortHandle>>,
    explore_inflight: RefCell<Option<tokio::task::AbortHandle>>,
    /// ISO country code the charts are drawn for, shared with the Python app's prefs.
    charts_country: RefCell<String>,
    /// The feed as it was last drawn, so a pill can be followed without refetching.
    data: RefCell<Option<ExploreData>>,
    /// The chart country menu and the codes behind its rows.
    country_menu: RefCell<Option<(gtk::DropDown, Vec<String>)>>,
    explore_loaded: Cell<bool>,
    explore_loading: Cell<bool>,
    explore_retry: Cell<u32>,
}

impl ExplorePage {
    pub fn new(ctx: Rc<UiContext>) -> Rc<Self> {
        let stack = gtk::Stack::builder().vexpand(true).build();

        // Results: tab strip plus a stack of result pages.
        let results_page = gtk::Box::builder().orientation(gtk::Orientation::Vertical).build();
        let toggle_container = gtk::Box::builder().orientation(gtk::Orientation::Horizontal).halign(gtk::Align::Center).margin_start(12).margin_end(12).build();
        let toggle_viewport = gtk::Viewport::builder().hscroll_policy(gtk::ScrollablePolicy::Natural).child(&toggle_container).build();
        let toggle_scroller = gtk::ScrolledWindow::builder().hscrollbar_policy(gtk::PolicyType::External).vscrollbar_policy(gtk::PolicyType::Never).child(&toggle_viewport).margin_top(16).margin_bottom(8).build();
        results_page.append(&toggle_scroller);
        let results_stack = gtk::Stack::builder().transition_type(gtk::StackTransitionType::Crossfade).vhomogeneous(false).hhomogeneous(false).valign(gtk::Align::Start).build();
        let results_clamp = adw::Clamp::builder().maximum_size(1024).tightening_threshold(600).child(&results_stack).build();
        let results_scrolled = gtk::ScrolledWindow::builder().hscrollbar_policy(gtk::PolicyType::Never).vexpand(true).child(&results_clamp).build();
        crate::ui::suppress_hover_while_scrolling(&results_scrolled);
        results_page.append(&results_scrolled);
        stack.add_named(&results_page, Some("results"));

        stack.add_named(&loading_box("Searching..."), Some("loading"));

        let explore_box = gtk::Box::builder().orientation(gtk::Orientation::Vertical).spacing(SECTION_SPACING).margin_top(24).margin_bottom(24).margin_start(12).margin_end(12).build();
        let explore_clamp = adw::Clamp::builder().maximum_size(1024).tightening_threshold(600).child(&explore_box).build();
        let explore_scrolled = gtk::ScrolledWindow::builder().hscrollbar_policy(gtk::PolicyType::Never).child(&explore_clamp).build();
        crate::ui::suppress_hover_while_scrolling(&explore_scrolled);
        stack.add_named(&explore_scrolled, Some("explore"));
        stack.set_visible_child_name("explore");

        let country = ctx.paths.read_prefs().get("charts_country").and_then(|v| v.as_str()).unwrap_or("ZZ").to_owned();
        let page = Rc::new(Self {
            stack,
            explore_box,
            results_stack,
            toggle_container,
            ctx,
            cards: RefCell::new(Vec::new()),
            scrollers: RefCell::new(Vec::new()),
            toggle_group: RefCell::new(None),
            song_rows: RefCell::new(Vec::new()),
            explore_rows: RefCell::new(Vec::new()),
            last_results: RefCell::new(Vec::new()),
            current_query: RefCell::new(None),
            inflight: RefCell::new(None),
            explore_inflight: RefCell::new(None),
            charts_country: RefCell::new(country),
            data: RefCell::new(None),
            country_menu: RefCell::new(None),
            explore_loaded: Cell::new(false),
            explore_loading: Cell::new(false),
            explore_retry: Cell::new(0),
        });
        let weak = Rc::downgrade(&page);
        glib::idle_add_local_once(move || {
            if let Some(p) = weak.upgrade() {
                p.load_explore_data(false);
            }
        });
        // Port of _check_and_reset_if_empty: coming back to the tab with no
        // search running shows the feed, and loads it if it never arrived.
        let weak = Rc::downgrade(&page);
        page.stack.connect_map(move |_| {
            let Some(p) = weak.upgrade() else { return };
            if p.current_query.borrow().is_some() {
                return;
            }
            p.stack.set_visible_child_name("explore");
            p.load_explore_data(false);
        });
        page
    }

    pub fn widget(&self) -> &gtk::Stack {
        &self.stack
    }

    pub fn set_compact(&self, compact: bool) {
        if compact {
            self.stack.add_css_class("compact");
        } else {
            self.stack.remove_css_class("compact");
        }
        // Sections sit closer together on a phone, feed and result tabs alike.
        let spacing = if compact { SECTION_SPACING_COMPACT } else { SECTION_SPACING };
        self.explore_box.set_spacing(spacing);
        let mut child = self.results_stack.first_child();
        while let Some(page) = child {
            child = page.next_sibling();
            if let Some(page) = page.downcast_ref::<gtk::Box>() {
                page.set_spacing(spacing);
            }
        }
        for card in self.cards.borrow().iter() {
            card.set_compact(compact);
        }
    }

    /// Back to the feed, after the search bar closes. Cancels a running search.
    pub fn show_explore(&self) {
        self.current_query.replace(None);
        if let Some(handle) = self.inflight.borrow_mut().take() {
            handle.abort();
        }
        self.stack.set_visible_child_name("explore");
    }

    // -- explore feed -----------------------------------------------------

    /// Port of load_explore_data: one fetch at a time, skipped once the feed
    /// is there unless forced. A forced load cancels the one in flight, so a
    /// country change is never swallowed by the fetch it replaces.
    pub fn load_explore_data(self: &Rc<Self>, force: bool) {
        if (self.explore_loading.get() || self.explore_loaded.get()) && !force {
            return;
        }
        if let Some(handle) = self.explore_inflight.borrow_mut().take() {
            handle.abort();
        }
        if force {
            self.explore_loaded.set(false);
        }
        self.explore_loading.set(true);

        if !self.ctx.online.is_online() {
            self.update_explore_ui(None);
            return;
        }
        if self.explore_box.first_child().is_none() {
            self.explore_box.append(&loading_box("Loading…"));
        }

        let api = self.ctx.net.client().api();
        let country = self.charts_country.borrow().clone();
        let handle = self.ctx.net.spawn(explore::load_explore(api, country));
        self.explore_inflight.replace(Some(handle.abort_handle()));
        let weak = Rc::downgrade(self);
        glib::spawn_future_local(async move {
            let outcome = handle.await;
            let Some(page) = weak.upgrade() else { return };
            // An aborted fetch was replaced; the load that replaced it owns the state.
            let Ok(result) = outcome else { return };
            page.explore_inflight.borrow_mut().take();
            match result {
                Ok(data) => page.update_explore_ui(Some(data)),
                Err(err) => {
                    tracing::warn!(%err, "explore fetch failed");
                    page.update_explore_ui(None);
                }
            }
        });
    }

    /// Port of update_explore_ui. No data means offline, or a failure that is
    /// retried three times with growing delays before the Retry button.
    fn update_explore_ui(self: &Rc<Self>, data: Option<ExploreData>) {
        self.explore_loading.set(false);
        let Some(data) = data else {
            if !self.ctx.online.is_online() {
                self.clear_explore();
                self.explore_box.append(&status_box("network-offline-symbolic", "You're offline", Some("Explore requires an internet connection.\nYour downloaded songs are still available.")));
                return;
            }
            let attempt = self.explore_retry.get();
            if attempt < 3 {
                self.explore_retry.set(attempt + 1);
                let weak = Rc::downgrade(self);
                glib::timeout_add_local_once(Duration::from_millis(1500 * (attempt as u64 + 1)), move || {
                    if let Some(p) = weak.upgrade() {
                        if !p.explore_loaded.get() {
                            p.load_explore_data(true);
                        }
                    }
                });
            } else {
                self.show_explore_retry_placeholder();
            }
            return;
        };
        self.clear_explore();
        self.explore_loaded.set(true);
        self.explore_retry.set(0);
        self.populate_explore(&data);
        self.data.replace(Some(data));
    }

    fn show_explore_retry_placeholder(self: &Rc<Self>) {
        self.clear_explore();
        let status = status_box("dialog-warning-symbolic", "Couldn't load Explore", None);
        let retry = gtk::Button::builder().label("Retry").css_classes(["pill", "suggested-action"]).halign(gtk::Align::Center).build();
        let weak = Rc::downgrade(self);
        retry.connect_clicked(move |_| {
            if let Some(p) = weak.upgrade() {
                p.explore_retry.set(0);
                p.load_explore_data(true);
            }
        });
        status.append(&retry);
        self.explore_box.append(&status);
    }

    fn clear_explore(&self) {
        clear_children(&self.explore_box);
        self.cards.borrow_mut().clear();
        self.scrollers.borrow_mut().clear();
        self.explore_rows.borrow_mut().clear();
        self.country_menu.replace(None);
    }

    fn populate_explore(self: &Rc<Self>, data: &ExploreData) {
        // The separated grids when the categories call answered, the feed's
        // own single row when it did not. "For you" is picked for the account
        // and leads, the way the Moods & Genres page orders them.
        if !data.for_you.is_empty() || !data.moods.is_empty() || !data.genres.is_empty() {
            self.add_pill_section("For You", &data.for_you);
            self.add_pill_section("Moods & Moments", &data.moods);
            self.add_pill_section("Genres", &data.genres);
        } else {
            self.add_pill_section("Moods & Genres", &data.feed.moods_and_genres);
        }

        self.add_row_section("New Albums & Singles", capped(&data.feed.new_releases, NEW_RELEASE_LIMIT));
        self.add_row_section("New Music Videos", capped(&data.feed.new_videos, VIDEO_LIMIT));
        self.add_row_section("Trending", capped(&data.feed.trending, TRENDING_LIMIT));
        if let Some(charts) = &data.charts {
            self.add_charts(charts);
        }
        self.set_compact(self.ctx.compact.get());
    }

    /// A scrolling row of pills. Past twenty it ends with View All, which
    /// opens the full list on its own page.
    fn add_pill_section(self: &Rc<Self>, title: &str, categories: &[Category]) {
        if categories.is_empty() {
            return;
        }
        let section = gtk::Box::builder().orientation(gtk::Orientation::Vertical).spacing(8).build();
        section.append(&heading(title));
        let scroll_box = HorizontalScrollBox::new();
        // Outside the scrolled window: a margin within it is empty space for
        // the overlay scrollbar to draw a line in.
        scroll_box.widget().set_margin_bottom(12);
        let strip = gtk::Box::builder().orientation(gtk::Orientation::Horizontal).spacing(12).build();
        for category in categories.iter().take(PILL_LIMIT) {
            let button = gtk::Button::builder().label(&category.title).css_classes(["pill"]).build();
            let ctx = self.ctx.clone();
            let category = category.clone();
            button.connect_clicked(move |_| ctx.nav.go(NavRequest::Category { title: category.title.clone(), params: category.params.clone() }));
            strip.append(&button);
        }
        if categories.len() > PILL_LIMIT {
            let view_all = gtk::Button::builder().label("View All").css_classes(["pill", "flat"]).build();
            let ctx = self.ctx.clone();
            let (title, items) = (title.to_owned(), categories.to_vec());
            view_all.connect_clicked(move |_| ctx.nav.go(NavRequest::AllMoods { title: title.clone(), items: items.clone() }));
            strip.append(&view_all);
        }
        scroll_box.set_content(&strip);
        section.append(scroll_box.widget());
        self.explore_box.append(&section);
        self.scrollers.borrow_mut().push(scroll_box);
    }

    /// One boxed list of rows, what add_section built for every feed shelf.
    fn add_row_section(self: &Rc<Self>, title: &str, items: &[MediaItem]) {
        if items.is_empty() {
            return;
        }
        self.add_song_list(&self.explore_box, title, items, &self.explore_rows);
    }

    /// Demo hook: pick a chart country through the menu itself, so the
    /// reload runs the same way a click on it does.
    pub fn pick_chart_country_for_demo(&self, code: &str) -> bool {
        let menu = self.country_menu.borrow();
        let Some((dropdown, codes)) = menu.as_ref() else { return false };
        let Some(index) = codes.iter().position(|c| c == code) else { return false };
        dropdown.set_selected(index as u32);
        true
    }

    /// Demo hook: follow the first genre pill, the way a click on it does.
    pub fn open_first_category_for_demo(&self) -> bool {
        let data = self.data.borrow();
        let Some(category) = data.as_ref().and_then(|d| d.genres.first().or_else(|| d.moods.first())) else { return false };
        self.ctx.nav.go(NavRequest::Category { title: category.title.clone(), params: category.params.clone() });
        true
    }

    /// Demo hook: the View All at the end of the genre row.
    pub fn open_all_moods_for_demo(&self) -> bool {
        let data = self.data.borrow();
        let Some(items) = data.as_ref().map(|d| if d.genres.is_empty() { d.moods.clone() } else { d.genres.clone() }) else { return false };
        if items.is_empty() {
            return false;
        }
        self.ctx.nav.go(NavRequest::AllMoods { title: "Genres".into(), items });
        true
    }

    // -- charts -----------------------------------------------------------

    /// Port of _add_charts_sections: the heading with its country menu, the
    /// chart playlists as cards, then the ranked artists.
    fn add_charts(self: &Rc<Self>, charts: &Charts) {
        let header = gtk::Box::builder().orientation(gtk::Orientation::Horizontal).spacing(8).build();
        header.append(&gtk::Label::builder().label("Charts").css_classes(["title-3"]).halign(gtk::Align::Start).hexpand(true).build());
        if let Some(dropdown) = self.country_dropdown(&charts.countries) {
            header.append(&dropdown);
        }
        self.explore_box.append(&header);

        // A premium account gets daily and weekly rows where everyone else
        // gets one trending row.
        self.add_chart_playlists("Trending", &charts.videos);
        self.add_chart_playlists("Daily", &charts.daily);
        self.add_chart_playlists("Weekly", &charts.weekly);
        self.add_chart_playlists("Genre Charts", &charts.genres);
        self.add_chart_artists("Top Artists", &charts.artists);
    }

    /// The country menu, built only once YouTube has told us the choices.
    /// Selecting one saves the code both apps read and reloads the feed.
    fn country_dropdown(self: &Rc<Self>, codes: &[String]) -> Option<gtk::DropDown> {
        if codes.is_empty() {
            return None;
        }
        let options = explore::country_options(codes);
        let names: Vec<&str> = options.iter().map(|(_, name)| name.as_str()).collect();
        let dropdown = gtk::DropDown::from_strings(&names);
        dropdown.add_css_class("flat");
        // Select before connecting, so restoring the saved country is not a change.
        if let Some(index) = options.iter().position(|(code, _)| *code == *self.charts_country.borrow()) {
            dropdown.set_selected(index as u32);
        }
        let codes: Vec<String> = options.into_iter().map(|(code, _)| code).collect();
        self.country_menu.replace(Some((dropdown.clone(), codes.clone())));
        let weak = Rc::downgrade(self);
        dropdown.connect_selected_notify(move |dd| {
            let Some(page) = weak.upgrade() else { return };
            let Some(code) = codes.get(dd.selected() as usize) else { return };
            if *code == *page.charts_country.borrow() {
                return;
            }
            page.charts_country.replace(code.clone());
            let code = code.clone();
            page.ctx.paths.update_prefs(|prefs| {
                prefs.insert("charts_country".into(), serde_json::Value::String(code));
            });
            page.load_explore_data(true);
        });
        Some(dropdown)
    }

    /// Port of _add_chart_playlists: a card strip of chart playlists.
    fn add_chart_playlists(self: &Rc<Self>, title: &str, items: &[MediaItem]) {
        if items.is_empty() {
            return;
        }
        let section = gtk::Box::builder().orientation(gtk::Orientation::Vertical).spacing(8).build();
        section.append(&heading(title));
        let scroll_box = HorizontalScrollBox::new();
        scroll_box.widget().set_margin_bottom(8);
        // A chart strip keeps its own spacing at every width, like the Python page.
        let strip = gtk::Box::builder().orientation(gtk::Orientation::Horizontal).spacing(CHART_STRIP_SPACING).build();
        for item in items {
            let card = MediaCard::new(&self.ctx, item.clone(), CardOptions { title_lines: 2, ..CardOptions::default() });
            let ctx = self.ctx.clone();
            card.connect_clicked(move |item| activate_item(&ctx, item, &[]));
            attach_item_menu(&self.ctx, card.widget(), item.clone());
            strip.append(card.widget());
            self.cards.borrow_mut().push(card);
        }
        scroll_box.set_content(&strip);
        section.append(scroll_box.widget());
        self.explore_box.append(&section);
        self.scrollers.borrow_mut().push(scroll_box);
    }

    /// Port of _add_chart_artists: rank, trend arrow, picture, name and
    /// subscriber count, opening the artist page.
    fn add_chart_artists(self: &Rc<Self>, title: &str, artists: &[ChartArtist]) {
        if artists.is_empty() {
            return;
        }
        let section = gtk::Box::builder().orientation(gtk::Orientation::Vertical).spacing(8).build();
        section.append(&heading(title));
        let list = gtk::ListBox::builder().css_classes(["boxed-list", "songs-list"]).selection_mode(gtk::SelectionMode::None).build();
        for artist in artists.iter().take(CHART_ARTIST_LIMIT) {
            let row = gtk::ListBoxRow::builder().activatable(true).build();
            let inner = gtk::Box::builder().orientation(gtk::Orientation::Horizontal).spacing(12).css_classes(["song-row"]).build();
            row.set_child(Some(&inner));

            inner.append(&gtk::Label::builder().label(artist.rank.clone().unwrap_or_default()).width_chars(3).css_classes(["heading"]).valign(gtk::Align::Center).build());
            let (icon, css) = match artist.trend {
                Trend::Up => ("go-up-symbolic", "success"),
                Trend::Down => ("go-down-symbolic", "error"),
                Trend::Neutral => ("go-next-symbolic", "dim-label"),
            };
            inner.append(&gtk::Image::builder().icon_name(icon).pixel_size(12).valign(gtk::Align::Center).css_classes([css]).build());

            let cover = CoverImage::in_context(&self.ctx, SONG_THUMB_SIZE);
            cover.widget().add_css_class("song-img");
            match &artist.item.thumb {
                Some(url) => cover.load(url),
                None => cover.set_placeholder("avatar-default-symbolic"),
            }
            inner.append(cover.widget());
            unsafe { row.set_data("cover", cover) };

            let text = gtk::Box::builder().orientation(gtk::Orientation::Vertical).spacing(2).valign(gtk::Align::Center).hexpand(true).build();
            text.append(&gtk::Label::builder().label(&artist.item.title).halign(gtk::Align::Start).xalign(0.0).ellipsize(gtk::pango::EllipsizeMode::End).lines(1).width_chars(1).build());
            if let Some(subscribers) = artist.item.subscribers.as_deref().filter(|s| !s.is_empty()) {
                text.append(&gtk::Label::builder().label(subscribers).halign(gtk::Align::Start).xalign(0.0).ellipsize(gtk::pango::EllipsizeMode::End).lines(1).width_chars(1).css_classes(["dim-label", "caption"]).build());
            }
            inner.append(&text);

            attach_item_menu(&self.ctx, &row, artist.item.clone());
            list.append(&row);
        }
        let ctx = self.ctx.clone();
        let items: Vec<MediaItem> = artists.iter().take(CHART_ARTIST_LIMIT).map(|a| a.item.clone()).collect();
        list.connect_row_activated(move |_, row| {
            if let Some(item) = items.get(row.index().max(0) as usize) {
                ctx.nav.go(NavRequest::Artist { id: Some(item.id.clone()), name: item.title.clone() });
            }
        });
        section.append(&list);
        self.explore_box.append(&section);
    }

    // -- search results ---------------------------------------------------

    /// Search YouTube Music through the client on the tokio runtime and lay
    /// the results out in tabs like update_results. A newer query cancels the
    /// older request, and a stale reply is dropped when it lands.
    pub fn show_results(self: &Rc<Self>, query: &str) {
        let query = query.trim().to_owned();
        if query.is_empty() {
            self.show_explore();
            return;
        }
        if self.current_query.borrow().as_deref() == Some(query.as_str()) {
            return;
        }
        self.current_query.replace(Some(query.clone()));
        if let Some(handle) = self.inflight.borrow_mut().take() {
            handle.abort();
        }
        // Port of _search_local: without a network, search what is on disk.
        if !self.ctx.online.is_online() {
            let items = local_results(&self.ctx.downloads.all(), &query);
            self.render_results(&query, SearchResults { top_result: None, items });
            return;
        }
        self.stack.set_visible_child_name("loading");

        let client = self.ctx.net.client().api();
        let handle = self.ctx.net.spawn(search_all(client, query.clone()));
        self.inflight.replace(Some(handle.abort_handle()));
        let weak = Rc::downgrade(self);
        glib::spawn_future_local(async move {
            let outcome = handle.await;
            let Some(page) = weak.upgrade() else { return };
            if page.current_query.borrow().as_deref() != Some(query.as_str()) {
                return;
            }
            page.inflight.borrow_mut().take();
            match outcome {
                Ok(Ok(results)) => page.render_results(&query, results),
                Ok(Err(err)) => {
                    tracing::warn!(%err, %query, "search failed");
                    toast(&page.stack, &format!("Search failed: {err}"));
                    page.render_results(&query, SearchResults::default());
                }
                Err(_) => {}
            }
        });
    }

    fn render_results(self: &Rc<Self>, query: &str, results: SearchResults) {
        self.stack.set_visible_child_name("results");
        clear_children(&self.results_stack);
        clear_children(&self.toggle_container);
        self.toggle_group.replace(None);
        self.song_rows.borrow_mut().clear();

        let mut all: Vec<MediaItem> = Vec::new();
        let has_top = results.top_result.is_some();
        if let Some(top) = &results.top_result {
            all.push(top.clone());
        }
        all.extend(results.items);
        self.last_results.replace(all.clone());
        if all.is_empty() {
            self.results_stack.add_named(&adw::StatusPage::builder().icon_name("system-search-symbolic").title("No results").description(format!("Nothing found for \"{query}\"")).build(), Some("empty"));
            self.results_stack.set_visible_child_name("empty");
            return;
        }

        let group = adw::ToggleGroup::builder().css_classes(["round"]).build();
        self.toggle_container.append(&group);
        let compact = self.ctx.compact.get();
        let mut first_id: Option<&str> = None;
        let mut make_tab = |name: &str, id: &'static str, compact_name: &str| -> gtk::Box {
            let page_box = gtk::Box::builder().orientation(gtk::Orientation::Vertical).spacing(if compact { SECTION_SPACING_COMPACT } else { SECTION_SPACING }).margin_top(16).margin_bottom(24).margin_start(12).margin_end(12).build();
            self.results_stack.add_named(&page_box, Some(id));
            group.add(adw::Toggle::builder().name(id).label(if compact { compact_name } else { name }).build());
            if first_id.is_none() {
                first_id = Some(id);
            }
            page_box
        };

        let songs: Vec<MediaItem> = all.iter().filter(|r| r.kind == ItemKind::Song).cloned().collect();
        let artists: Vec<MediaItem> = all.iter().filter(|r| r.kind == ItemKind::Artist).cloned().collect();
        let playlists: Vec<MediaItem> = all.iter().filter(|r| r.kind == ItemKind::Playlist).cloned().collect();
        let albums: Vec<MediaItem> = all.iter().filter(|r| r.kind == ItemKind::Album).cloned().collect();
        let videos: Vec<MediaItem> = all.iter().filter(|r| r.kind == ItemKind::Video).cloned().collect();

        let main = make_tab("Main", "main", "Main");
        // Only a card YouTube sent as the top result earns the heading.
        let rest = if has_top {
            self.add_result_section(&main, "Top Result", &all[..1]);
            &all[1..]
        } else {
            &all[..]
        };
        if !rest.is_empty() {
            self.add_result_section(&main, "Relevant Results", rest);
        }
        if !songs.is_empty() {
            let tab = make_tab("Songs", "songs", "Songs");
            self.add_result_section(&tab, "Songs", &songs);
        }
        if !artists.is_empty() {
            let tab = make_tab("Artists", "artists", "Artists");
            self.add_result_section(&tab, "Artists", &artists);
        }
        if !playlists.is_empty() {
            let tab = make_tab("Community Playlists", "playlists", "Playlists");
            self.add_result_section(&tab, "Playlists", &playlists);
        }
        if !albums.is_empty() || !videos.is_empty() {
            let tab = make_tab("Other results", "others", "Other");
            if !albums.is_empty() {
                self.add_result_section(&tab, "Albums", &albums);
            }
            // Episodes sit apart from music videos, under the heading Python gave them.
            let (episodes, videos): (Vec<MediaItem>, Vec<MediaItem>) = videos.into_iter().partition(|v| v.item_type.as_deref() == Some("Episode"));
            if !videos.is_empty() {
                self.add_result_section(&tab, "Videos", &videos);
            }
            if !episodes.is_empty() {
                self.add_result_section(&tab, "More results", &episodes);
            }
        }

        let results_stack = self.results_stack.clone();
        group.connect_active_name_notify(move |g| {
            if let Some(name) = g.active_name() {
                results_stack.set_visible_child_name(&name);
            }
        });
        if let Some(id) = first_id {
            group.set_active_name(Some(id));
            self.results_stack.set_visible_child_name(id);
        }
        self.toggle_group.replace(Some(group));
    }

    /// Same path a click on the first song row takes. Used by the demo.
    pub fn activate_first_playable(&self) -> bool {
        let results = self.last_results.borrow();
        let pool: Vec<MediaItem> = results.iter().filter(|i| i.kind.is_playable()).cloned().collect();
        match pool.first() {
            Some(item) => {
                tracing::info!(title = %item.title, id = %item.id, "activating first search result");
                activate_item(&self.ctx, item, &pool);
                true
            }
            None => false,
        }
    }

    fn add_result_section(self: &Rc<Self>, parent: &gtk::Box, title: &str, items: &[MediaItem]) {
        self.add_song_list(parent, title, items, &self.song_rows);
    }

    /// A boxed list: SongRow for songs and videos, the simple row for
    /// collections. Activating a song queues every playable row of the
    /// section, like on_row_activated. The feed and the result tabs keep
    /// their rows in separate sinks, so reloading one leaves the other alone.
    fn add_song_list(self: &Rc<Self>, parent: &gtk::Box, title: &str, items: &[MediaItem], rows: &RefCell<Vec<Rc<SongRow>>>) {
        let section = gtk::Box::builder().orientation(gtk::Orientation::Vertical).spacing(8).build();
        section.append(&heading(title));
        let list = gtk::ListBox::builder().css_classes(["boxed-list", "songs-list"]).selection_mode(gtk::SelectionMode::None).build();
        let pool: Vec<MediaItem> = items.iter().filter(|i| i.kind.is_playable()).cloned().collect();
        for item in items {
            if item.kind.is_playable() {
                let row = SongRow::new(self.ctx.clone());
                row.set_search_style(true);
                row.bind(item, None);
                list.append(row.widget());
                rows.borrow_mut().push(row);
            } else {
                let subtitle = search_subtitle(item);
                let (row, _) = song_row_with_subtitle(&self.ctx, item, Some(&subtitle));
                attach_item_menu(&self.ctx, &row, item.clone());
                list.append(&row);
            }
        }
        let ctx = self.ctx.clone();
        let items_c = items.to_vec();
        list.connect_row_activated(move |_, row| {
            if let Some(item) = items_c.get(row.index().max(0) as usize) {
                activate_item(&ctx, item, &pool);
            }
        });
        section.append(&list);
        parent.append(&section);
    }
}

fn heading(title: &str) -> gtk::Label {
    gtk::Label::builder().label(title).css_classes(["heading"]).halign(gtk::Align::Start).build()
}

/// The first `limit` items of a shelf, what update_explore_ui sliced off.
fn capped(items: &[MediaItem], limit: usize) -> &[MediaItem] {
    &items[..items.len().min(limit)]
}

/// The centred icon, title and caption the Python explore page builds inline
/// for its offline and retry states.
fn status_box(icon: &str, title: &str, subtitle: Option<&str>) -> gtk::Box {
    let status = gtk::Box::builder().orientation(gtk::Orientation::Vertical).spacing(12).valign(gtk::Align::Center).halign(gtk::Align::Center).vexpand(true).build();
    status.append(&gtk::Image::builder().icon_name(icon).pixel_size(48).css_classes(["dim-label"]).build());
    status.append(&gtk::Label::builder().label(title).css_classes(["title-3"]).build());
    if let Some(subtitle) = subtitle {
        status.append(&gtk::Label::builder().label(subtitle).css_classes(["dim-label"]).justify(gtk::Justification::Center).build());
    }
    status
}

/// Downloads whose title, artist or album contains the query, as song results.
fn local_results(downloads: &[crate::downloads::store::Entry], query: &str) -> Vec<MediaItem> {
    let needle = query.to_lowercase();
    downloads
        .iter()
        .filter(|d| [&d.title, &d.artist, &d.album].iter().any(|field| field.to_lowercase().contains(&needle)))
        .map(|d| MediaItem {
            kind: ItemKind::Song,
            id: d.video_id.clone(),
            title: d.title.clone(),
            artists: vec![crate::model::Person { name: d.artist.clone(), id: (!d.artist_id.is_empty()).then(|| d.artist_id.clone()) }],
            album: (!d.album.is_empty()).then(|| crate::model::Named { name: d.album.clone(), id: (!d.album_id.is_empty()).then(|| d.album_id.clone()) }),
            thumb: (!d.thumbnail_url.is_empty()).then(|| d.thumbnail_url.clone()),
            duration_seconds: d.duration_seconds,
            like_status: Some(d.like_status),
            ..MediaItem::default()
        })
        .collect()
}

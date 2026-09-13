//! Port of ui/pages/search.py on mock data: the Explore feed (mood and
//! genre pills, new releases, videos, trending, charts) and the search
//! results view with its toggle tabs.

use std::cell::{Cell, RefCell};
use std::rc::Rc;
use std::time::Duration;

use adw::prelude::*;
use gtk::glib;

use crate::mock;
use crate::model::{ItemKind, MediaItem};
use crate::net::search::{SearchResults, search_all};
use crate::ui::context::{NavRequest, UiContext};
use crate::ui::cover::CoverImage;
use crate::ui::pages::{activate_item, attach_item_menu, clear_children, loading_box};
use crate::ui::toast;
use crate::ui::widgets::media_card::{CardOptions, MediaCard, STRIP_SPACING, STRIP_SPACING_COMPACT};
use crate::ui::widgets::playing::PlayingTracker;
use crate::ui::widgets::scroll_box::HorizontalScrollBox;
use crate::ui::widgets::song_list::{search_subtitle, song_row_with_subtitle};
use crate::ui::widgets::song_row::SongRow;

pub struct ExplorePage {
    stack: gtk::Stack,
    explore_box: gtk::Box,
    results_stack: gtk::Stack,
    toggle_container: gtk::Box,
    ctx: Rc<UiContext>,
    playing: Rc<PlayingTracker>,
    cards: RefCell<Vec<Rc<MediaCard>>>,
    strips: RefCell<Vec<gtk::Box>>,
    scrollers: RefCell<Vec<Rc<HorizontalScrollBox>>>,
    toggle_group: RefCell<Option<adw::ToggleGroup>>,
    song_rows: RefCell<Vec<Rc<SongRow>>>,
    last_results: RefCell<Vec<MediaItem>>,
    current_query: RefCell<Option<String>>,
    inflight: RefCell<Option<tokio::task::AbortHandle>>,
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

        let explore_box = gtk::Box::builder().orientation(gtk::Orientation::Vertical).spacing(24).margin_top(24).margin_bottom(24).margin_start(12).margin_end(12).build();
        let explore_clamp = adw::Clamp::builder().maximum_size(1024).tightening_threshold(600).child(&explore_box).build();
        let explore_scrolled = gtk::ScrolledWindow::builder().hscrollbar_policy(gtk::PolicyType::Never).child(&explore_clamp).build();
        crate::ui::suppress_hover_while_scrolling(&explore_scrolled);
        stack.add_named(&explore_scrolled, Some("explore"));
        stack.set_visible_child_name("explore");

        let page = Rc::new(Self {
            stack,
            explore_box,
            results_stack,
            toggle_container,
            playing: PlayingTracker::new(ctx.player.state()),
            ctx,
            cards: RefCell::new(Vec::new()),
            strips: RefCell::new(Vec::new()),
            scrollers: RefCell::new(Vec::new()),
            toggle_group: RefCell::new(None),
            song_rows: RefCell::new(Vec::new()),
            last_results: RefCell::new(Vec::new()),
            current_query: RefCell::new(None),
            inflight: RefCell::new(None),
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
        page
    }

    /// Port of load_explore_data: one fetch at a time, skipped once loaded unless forced.
    pub fn load_explore_data(self: &Rc<Self>, force: bool) {
        if self.explore_loading.get() {
            return;
        }
        if self.explore_loaded.get() && !force {
            return;
        }
        if force {
            self.explore_loaded.set(false);
        }
        self.explore_loading.set(true);
        let online = self.ctx.online.is_online();
        let weak = Rc::downgrade(self);
        glib::idle_add_local_once(move || {
            if let Some(page) = weak.upgrade() {
                // Explore endpoints are not ported yet: mock data stands in for get_explore.
                page.update_explore_ui(online);
            }
        });
    }

    fn update_explore_ui(self: &Rc<Self>, has_data: bool) {
        self.explore_loading.set(false);
        if !has_data {
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
        }
        self.explore_loaded.set(true);
        self.explore_retry.set(0);
        self.populate_explore();
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
        self.strips.borrow_mut().clear();
        self.scrollers.borrow_mut().clear();
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
        for strip in self.strips.borrow().iter() {
            strip.set_spacing(if compact { STRIP_SPACING_COMPACT } else { STRIP_SPACING });
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
            let page_box = gtk::Box::builder().orientation(gtk::Orientation::Vertical).spacing(if compact { 16 } else { 24 }).margin_top(16).margin_bottom(24).margin_start(12).margin_end(12).build();
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
        self.add_result_section(&main, "Top Result", &all[..1]);
        if all.len() > 1 {
            self.add_result_section(&main, "Relevant Results", &all[1..]);
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
            if !videos.is_empty() {
                self.add_result_section(&tab, "Videos", &videos);
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

    /// A boxed list: SongRow for songs and videos, the simple row for collections.
    /// Activating a song queues every playable row of the section, like on_row_activated.
    fn add_result_section(self: &Rc<Self>, parent: &gtk::Box, title: &str, items: &[MediaItem]) {
        let section = gtk::Box::builder().orientation(gtk::Orientation::Vertical).spacing(8).build();
        section.append(&gtk::Label::builder().label(title).css_classes(["heading"]).halign(gtk::Align::Start).build());
        let list = gtk::ListBox::builder().css_classes(["boxed-list", "songs-list"]).selection_mode(gtk::SelectionMode::None).build();
        let pool: Vec<MediaItem> = items.iter().filter(|i| i.kind.is_playable()).cloned().collect();
        for item in items {
            if item.kind.is_playable() {
                let row = SongRow::new(self.ctx.clone());
                row.set_search_style(true);
                row.bind(item, None);
                list.append(row.widget());
                self.song_rows.borrow_mut().push(row);
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

    // -- explore feed -----------------------------------------------------

    fn populate_explore(self: &Rc<Self>) {
        self.clear_explore();
        let data = mock::explore();

        self.add_pill_section("Moods & Moments", &data.moods);
        self.add_pill_section("Genres", &data.genres);
        self.add_card_section("New Albums & Singles", &data.new_releases);
        self.add_card_section("New Music Videos", &data.new_videos);
        self.add_card_section("Trending", &data.trending);
        self.add_charts(&data);
        self.set_compact(self.ctx.compact.get());
    }

    fn add_pill_section(self: &Rc<Self>, title: &str, names: &[String]) {
        let section = gtk::Box::builder().orientation(gtk::Orientation::Vertical).spacing(8).build();
        section.append(&gtk::Label::builder().label(title).css_classes(["heading"]).halign(gtk::Align::Start).build());
        let scroll_box = HorizontalScrollBox::new();
        scroll_box.widget().set_margin_bottom(12);
        let strip = gtk::Box::builder().orientation(gtk::Orientation::Horizontal).spacing(12).build();
        for name in names {
            let button = gtk::Button::builder().label(name).css_classes(["pill"]).build();
            let ctx = self.ctx.clone();
            let title = name.clone();
            button.connect_clicked(move |_| ctx.nav.go(NavRequest::Category { title: title.clone() }));
            strip.append(&button);
        }
        scroll_box.set_content(&strip);
        section.append(scroll_box.widget());
        self.explore_box.append(&section);
        self.scrollers.borrow_mut().push(scroll_box);
    }

    fn add_card_section(self: &Rc<Self>, title: &str, items: &[MediaItem]) {
        if items.is_empty() {
            return;
        }
        let section = gtk::Box::builder().orientation(gtk::Orientation::Vertical).spacing(8).build();
        section.append(&gtk::Label::builder().label(title).css_classes(["heading"]).halign(gtk::Align::Start).build());
        let scroll_box = HorizontalScrollBox::new();
        scroll_box.widget().set_margin_bottom(8);
        let compact = self.ctx.compact.get();
        let strip = gtk::Box::builder().orientation(gtk::Orientation::Horizontal).spacing(if compact { STRIP_SPACING_COMPACT } else { STRIP_SPACING }).build();
        let pool: Vec<MediaItem> = items.iter().filter(|i| i.kind.is_playable()).cloned().collect();
        for item in items {
            let card = MediaCard::new(&self.ctx, item.clone(), CardOptions { title_lines: 2, ..CardOptions::default() });
            let ctx = self.ctx.clone();
            let pool_c = pool.clone();
            card.connect_clicked(move |item| activate_item(&ctx, item, &pool_c));
            attach_item_menu(&self.ctx, card.widget(), item.clone());
            if item.kind.is_playable() {
                self.playing.track(card.widget(), &item.id);
            }
            strip.append(card.widget());
            self.cards.borrow_mut().push(card);
        }
        scroll_box.set_content(&strip);
        section.append(scroll_box.widget());
        self.explore_box.append(&section);
        self.strips.borrow_mut().push(strip);
        self.scrollers.borrow_mut().push(scroll_box);
    }

    fn add_charts(self: &Rc<Self>, data: &mock::ExploreData) {
        let header = gtk::Box::builder().orientation(gtk::Orientation::Horizontal).spacing(8).build();
        header.append(&gtk::Label::builder().label("Charts").css_classes(["title-3"]).halign(gtk::Align::Start).hexpand(true).build());
        let names: Vec<&str> = data.countries.iter().map(|(_, name)| *name).collect();
        let dropdown = gtk::DropDown::from_strings(&names);
        dropdown.add_css_class("flat");
        let saved = self.ctx.paths.read_prefs().get("charts_country").and_then(|v| v.as_str()).unwrap_or("ZZ").to_owned();
        if let Some(i) = data.countries.iter().position(|(code, _)| *code == saved) {
            dropdown.set_selected(i as u32);
        }
        let codes: Vec<String> = data.countries.iter().map(|(code, _)| code.to_string()).collect();
        let paths = self.ctx.paths.clone();
        dropdown.connect_selected_notify(move |dd| {
            if let Some(code) = codes.get(dd.selected() as usize) {
                let code = code.clone();
                paths.update_prefs(|p| {
                    p.insert("charts_country".into(), serde_json::Value::String(code));
                });
            }
        });
        header.append(&dropdown);
        self.explore_box.append(&header);

        self.add_card_section("Trending", &data.chart_videos);
        self.add_card_section("Genre Charts", &data.chart_genres);

        let section = gtk::Box::builder().orientation(gtk::Orientation::Vertical).spacing(8).build();
        section.append(&gtk::Label::builder().label("Top Artists").css_classes(["heading"]).halign(gtk::Align::Start).build());
        let list = gtk::ListBox::builder().css_classes(["boxed-list"]).selection_mode(gtk::SelectionMode::None).build();
        for (i, artist) in data.chart_artists.iter().enumerate() {
            let row = adw::ActionRow::builder().title(&artist.title).subtitle(artist.subscribers.clone().unwrap_or_default()).activatable(true).build();
            row.add_prefix(&gtk::Label::builder().label(format!("{}", i + 1)).css_classes(["dim-label", "numeric"]).width_chars(2).build());
            let avatar = CoverImage::new(self.ctx.net.clone(), 40);
            avatar.widget().add_css_class("avatar");
            if let Some(url) = &artist.thumb {
                avatar.load(url);
            }
            let wrapper = gtk::Box::builder().overflow(gtk::Overflow::Hidden).css_classes(["avatar"]).valign(gtk::Align::Center).build();
            wrapper.append(avatar.widget());
            unsafe { row.set_data("cover", avatar) };
            row.add_prefix(&wrapper);
            let ctx = self.ctx.clone();
            let item = artist.clone();
            row.connect_activated(move |_| ctx.nav.go(NavRequest::Artist { id: Some(item.id.clone()), name: item.title.clone() }));
            list.append(&row);
        }
        section.append(&list);
        self.explore_box.append(&section);
    }
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

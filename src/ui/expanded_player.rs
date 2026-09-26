//! Port of ui/expanded_player.py: the mobile sheet with a Player / Queue /
//! Lyrics toggle, a cover carousel over the queue, metadata with like,
//! seek bar, transport row over the visualizer.

use std::cell::{Cell, RefCell};
use std::rc::Rc;
use std::time::{Duration, Instant};

use adw::prelude::*;
use gtk::{gdk, glib};

use crate::model::{LikeStatus, PlaybackStatus, VideoId};
use crate::ui::context::{NavRequest, UiContext};
use crate::ui::like_button::LikeButton;
use crate::ui::marquee::MarqueeLabel;
use crate::ui::queue_panel::QueuePanel;
use crate::ui::widgets::cover_picture::CoverPicture;
use crate::ui::widgets::lyrics_view::LyricsView;
use crate::ui::widgets::transport::Transport;
use crate::ui::widgets::visualizer::Visualizer;

const MAX_CAROUSEL_COVERS: usize = 31;
const CAROUSEL_PRELOAD_RADIUS: usize = 5;
const USER_INPUT_WINDOW: Duration = Duration::from_millis(800);
/// Scroll events arrive as a stream with no beginning of their own. A pause
/// longer than this starts a new gesture, and with it a new baseline.
const GESTURE_GAP: Duration = Duration::from_millis(600);

pub struct ExpandedPlayer {
    root: gtk::Box,
    view_stack: adw::ViewStack,
    toggle_nav: adw::ToggleGroup,
    carousel: adw::Carousel,
    covers: RefCell<Vec<Rc<CoverPicture>>>,
    cover_offset: Cell<usize>,
    ignore_page_change: Cell<bool>,
    more_btn: gtk::MenuButton,
    user_input_at: Cell<Option<Instant>>,
    /// Where the carousel sat when the current gesture began. A settle that
    /// did not move from here is a tap, not a swipe.
    gesture_start: Cell<Option<f64>>,
    /// Set while a centre is waiting for the carousel to be laid out.
    center_pending: Cell<bool>,
    title: Rc<MarqueeLabel>,
    artists_box: gtk::Box,
    like: Rc<LikeButton>,
    /// Held so the transport's state bindings stay alive with the view.
    #[allow(dead_code)]
    transport: Rc<Transport>,
    visualizer: Rc<Visualizer>,
    /// A metadata update is already queued for the next idle.
    metadata_pending: Cell<bool>,
    /// Kept alive here. The widget tree only holds its root box.
    #[allow(dead_code)]
    lyrics_view: Rc<LyricsView>,
    height_probe: gtk::ScrolledWindow,
    #[allow(dead_code)]
    queue_panel: Rc<QueuePanel>,
    ctx: Rc<UiContext>,
    on_dismiss: RefCell<Option<Rc<dyn Fn()>>>,
}

impl ExpandedPlayer {
    pub fn new(ctx: Rc<UiContext>) -> Rc<Self> {
        let player = ctx.player.clone();
        let root = gtk::Box::builder()
            .orientation(gtk::Orientation::Vertical)
            .margin_top(32)
            .build();
        let view_stack = adw::ViewStack::builder().vexpand(true).build();
        root.append(&view_stack);

        let toggle_nav = adw::ToggleGroup::builder()
            .css_classes(["round"])
            .halign(gtk::Align::Center)
            .margin_top(8)
            .margin_bottom(8)
            .build();
        for (name, label, icon) in [
            ("player", "Player", "folder-music-symbolic"),
            ("queue", "Queue", "music-queue-symbolic"),
            ("lyrics", "Lyrics", "format-justify-fill-symbolic"),
        ] {
            toggle_nav.add(
                adw::Toggle::builder()
                    .name(name)
                    .label(label)
                    .icon_name(icon)
                    .build(),
            );
        }
        root.append(&toggle_nav);

        // -- player view -------------------------------------------------
        let main_box = gtk::Box::builder()
            .orientation(gtk::Orientation::Vertical)
            .spacing(12)
            .margin_top(12)
            .margin_bottom(24)
            .build();
        let carousel = adw::Carousel::builder()
            .spacing(16)
            .interactive(true)
            .build();
        let cover_frame = gtk::AspectFrame::builder()
            .ratio(1.0)
            .obey_child(false)
            .halign(gtk::Align::Center)
            .valign(gtk::Align::Center)
            .vexpand(true)
            .hexpand(true)
            .overflow(gtk::Overflow::Hidden)
            .margin_start(24)
            .margin_end(24)
            .child(&carousel)
            .build();
        main_box.append(&cover_frame);

        let meta_row = gtk::Box::builder()
            .orientation(gtk::Orientation::Horizontal)
            .spacing(12)
            .hexpand(true)
            .valign(gtk::Align::Center)
            .margin_start(24)
            .margin_end(24)
            .margin_bottom(8)
            .build();
        let text_box = gtk::Box::builder()
            .orientation(gtk::Orientation::Vertical)
            .spacing(2)
            .hexpand(true)
            .valign(gtk::Align::Center)
            .build();
        let title = MarqueeLabel::new();
        title.set_label("Not Playing");
        title.add_css_class("title-3");
        let artists_box = gtk::Box::builder()
            .orientation(gtk::Orientation::Horizontal)
            .spacing(2)
            .halign(gtk::Align::Start)
            .build();
        text_box.append(title.widget());
        text_box.append(&artists_box);
        let like = LikeButton::new(player.clone());
        like.widget().set_visible(false);
        meta_row.append(&text_box);
        meta_row.append(like.widget());
        main_box.append(&meta_row);

        let transport = Transport::new(player.clone(), 64, 24, 48);
        let progress_box = gtk::Box::builder()
            .orientation(gtk::Orientation::Vertical)
            .margin_start(24)
            .margin_end(24)
            .build();
        progress_box.append(&transport.scale);
        let timings = gtk::Box::builder()
            .orientation(gtk::Orientation::Horizontal)
            .build();
        timings.append(&transport.pos_label);
        timings.append(&gtk::Box::builder().hexpand(true).build());
        timings.append(&transport.dur_label);
        progress_box.append(&timings);

        let controls = gtk::Box::builder()
            .orientation(gtk::Orientation::Horizontal)
            .spacing(12)
            .halign(gtk::Align::Center)
            .margin_top(20)
            .margin_start(24)
            .margin_end(24)
            .build();
        let more_btn = gtk::MenuButton::builder()
            .icon_name("view-more-symbolic")
            .css_classes(["flat", "circular"])
            .valign(gtk::Align::Center)
            .build();
        controls.append(&transport.vol_btn);
        controls.append(&transport.prev_btn);
        controls.append(&transport.play_btn);
        controls.append(&transport.next_btn);
        controls.append(&more_btn);

        let visualizer = Visualizer::new(&ctx, 85);
        visualizer.widget().set_hexpand(true);
        visualizer.widget().set_valign(gtk::Align::End);
        visualizer.widget().set_can_target(false);
        visualizer.widget().add_css_class("player-visualizer");
        let controls_content = gtk::Box::builder()
            .orientation(gtk::Orientation::Vertical)
            .hexpand(true)
            .build();
        controls_content.append(&progress_box);
        controls_content.append(&controls);
        let controls_overlay = gtk::Overlay::builder()
            .hexpand(true)
            .child(visualizer.widget())
            .build();
        controls_overlay.add_overlay(&controls_content);
        controls_overlay.set_measure_overlay(&controls_content, true);
        main_box.append(&controls_overlay);

        // The sheet sizes to natural height: the probe claims a tall natural, near-zero minimum.
        let height_probe = gtk::ScrolledWindow::builder()
            .hscrollbar_policy(gtk::PolicyType::Never)
            .vscrollbar_policy(gtk::PolicyType::External)
            .propagate_natural_height(true)
            .can_target(false)
            .visible(false)
            .build();
        let filler = gtk::Box::new(gtk::Orientation::Vertical, 0);
        filler.set_size_request(-1, 3000);
        height_probe.set_child(Some(&filler));
        let page_overlay = gtk::Overlay::builder().child(&main_box).build();
        page_overlay.add_overlay(&height_probe);
        page_overlay.set_measure_overlay(&height_probe, true);

        let player_scroll = gtk::ScrolledWindow::builder()
            .hscrollbar_policy(gtk::PolicyType::Never)
            .propagate_natural_height(true)
            .child(&page_overlay)
            .build();
        view_stack.add_titled_with_icon(
            &player_scroll,
            Some("player"),
            "Player",
            "folder-music-symbolic",
        );

        // -- queue and lyrics views --------------------------------------
        let queue_panel = QueuePanel::new(ctx.clone());
        queue_panel.widget().set_vexpand(true);
        let lyrics_view = LyricsView::new(ctx.clone());
        view_stack.add_titled_with_icon(
            queue_panel.widget(),
            Some("queue"),
            "Queue",
            "music-queue-symbolic",
        );
        view_stack.add_titled_with_icon(
            lyrics_view.widget(),
            Some("lyrics"),
            "Lyrics",
            "format-justify-fill-symbolic",
        );

        let this = Rc::new(Self {
            root,
            view_stack,
            toggle_nav,
            carousel,
            covers: RefCell::new(Vec::new()),
            cover_offset: Cell::new(0),
            ignore_page_change: Cell::new(false),
            more_btn: more_btn.clone(),
            user_input_at: Cell::new(None),
            gesture_start: Cell::new(None),
            center_pending: Cell::new(false),
            title,
            artists_box,
            like,
            transport,
            visualizer,
            metadata_pending: Cell::new(false),
            lyrics_view,
            height_probe,
            queue_panel,
            ctx,
            on_dismiss: RefCell::new(None),
        });

        // The menu is rebuilt whenever the track changes, like the Python
        // view. A menu button with no model is insensitive, so building it
        // only on activate would leave the button dead.
        this.refresh_more_menu();

        this.connect_toggles();
        this.connect_carousel(&cover_frame);
        this.bind_state();
        this.refresh_metadata();
        this.sync_carousel();
        this
    }

    pub fn widget(&self) -> &gtk::Box {
        &self.root
    }

    pub fn set_on_dismiss(&self, f: impl Fn() + 'static) {
        self.on_dismiss.replace(Some(Rc::new(f)));
    }

    /// Inside the bottom sheet the toggle row shows and the probe is live.
    pub fn set_compact_mode(&self, compact: bool) {
        self.toggle_nav.set_visible(compact);
        self.height_probe.set_visible(compact);
        if compact {
            self.root.set_margin_top(32);
        } else {
            self.view_stack.set_visible_child_name("player");
            self.root.set_margin_top(12);
        }
    }

    fn connect_toggles(self: &Rc<Self>) {
        let stack = self.view_stack.clone();
        self.toggle_nav.connect_active_name_notify(move |group| {
            if let Some(name) = group.active_name() {
                if stack.visible_child_name().as_deref() != Some(name.as_str()) {
                    stack.set_visible_child_name(&name);
                }
            }
        });
        let toggles = self.toggle_nav.clone();
        self.view_stack
            .connect_visible_child_name_notify(move |stack| {
                if let Some(name) = stack.visible_child_name() {
                    if toggles.active_name().as_deref() != Some(name.as_str()) {
                        toggles.set_active_name(Some(&name));
                    }
                }
            });
    }

    fn connect_carousel(self: &Rc<Self>, cover_frame: &gtk::AspectFrame) {
        // Every part of a gesture counts, not only its start: a slow swipe can
        // take seconds, and the carousel settles after the finger lifts.
        let drag = gtk::GestureDrag::builder().propagation_phase(gtk::PropagationPhase::Capture).build();
        for signal in ["drag-begin", "drag-update", "drag-end"] {
            let weak = Rc::downgrade(self);
            let begins = signal == "drag-begin";
            drag.connect_local(signal, false, move |_| {
                if let Some(ep) = weak.upgrade() {
                    ep.note_input(begins);
                }
                None
            });
        }
        self.carousel.add_controller(drag);

        let click = gtk::GestureClick::builder().propagation_phase(gtk::PropagationPhase::Capture).build();
        let weak = Rc::downgrade(self);
        click.connect_pressed(move |_, _, _, _| {
            if let Some(ep) = weak.upgrade() {
                ep.note_input(true);
            }
        });
        let weak = Rc::downgrade(self);
        click.connect_released(move |_, _, _, _| {
            if let Some(ep) = weak.upgrade() {
                ep.note_input(false);
            }
        });
        self.carousel.add_controller(click);

        let scroll = gtk::EventControllerScroll::builder().flags(gtk::EventControllerScrollFlags::BOTH_AXES).propagation_phase(gtk::PropagationPhase::Capture).build();
        let weak = Rc::downgrade(self);
        scroll.connect_scroll(move |_, _, _| {
            if let Some(ep) = weak.upgrade() {
                ep.note_input(false);
            }
            glib::Propagation::Proceed
        });
        self.carousel.add_controller(scroll);

        let weak = Rc::downgrade(self);
        self.carousel.connect_position_notify(move |carousel| {
            let Some(ep) = weak.upgrade() else { return };
            if ep.ignore_page_change.get() {
                return;
            }
            let since_input = ep.user_input_at.get().map(|t| t.elapsed());
            let position = carousel.position();
            if !touched_recently(since_input) || !settled_on_a_page(position) {
                return;
            }
            // The carousel counts pages, not covers. Resolve the page that is
            // showing and ask the list where it sits, so a hidden cover can
            // never send the player somewhere else.
            let index = position.round() as u32;
            if index >= carousel.n_pages() {
                return;
            }
            let page = carousel.nth_page(index);
            let Some(cover_index) = ep.covers.borrow().iter().position(|c| c.widget().upcast_ref::<gtk::Widget>() == &page) else { return };
            let queue_index = ep.cover_offset.get() + cover_index;
            let current = ep.ctx.player.state().current_index();
            if queue_index as i32 == current {
                return;
            }
            // A flick can carry several covers, so any distance counts as long
            // as the carousel actually moved under the finger. A settle that
            // did not move is the carousel sitting out of step with the queue,
            // and following it is what used to jump to another track.
            if swiped(position, ep.gesture_start.get()) {
                tracing::debug!(queue_index, current, since_input = ?since_input, "carousel swipe");
                ep.ctx.player.play_queue_index(queue_index);
            } else {
                tracing::debug!(queue_index, current, "carousel out of step, putting it back");
                ep.center_carousel();
            }
        });

        // Tap on the cover toggles play, like the Python view.
        let tap = gtk::GestureClick::new();
        let player = self.ctx.player.clone();
        tap.connect_released(move |_, _, _, _| player.toggle_play());
        cover_frame.add_controller(tap);

        let weak = Rc::downgrade(self);
        self.root.connect_map(move |_| {
            if let Some(ep) = weak.upgrade() {
                ep.center_when_ready();
                ep.visualizer
                    .set_active(ep.ctx.player.state().status() == PlaybackStatus::Playing);
            }
        });
    }

    fn bind_state(self: &Rc<Self>) {
        let state = self.ctx.player.state();
        for prop in [
            "title",
            "artist",
            "thumbnail-url",
            "video-id",
            "like-status",
        ] {
            let weak = Rc::downgrade(self);
            // A track change moves all five properties in a row. One update on the
            // next idle covers them, where each used to rebuild the labels and the menu.
            state.connect_notify_local(Some(prop), move |_, _| {
                let Some(ep) = weak.upgrade() else { return };
                if ep.metadata_pending.replace(true) {
                    return;
                }
                let weak = weak.clone();
                glib::idle_add_local_once(move || {
                    if let Some(ep) = weak.upgrade() {
                        ep.metadata_pending.set(false);
                        ep.refresh_metadata();
                        ep.refresh_more_menu();
                    }
                });
            });
        }
        let weak = Rc::downgrade(self);
        state.connect_notify_local(Some("status"), move |state, _| {
            if let Some(ep) = weak.upgrade() {
                ep.visualizer
                    .set_active(state.status() == PlaybackStatus::Playing);
            }
        });
        let weak = Rc::downgrade(self);
        state.connect_local("queue-changed", false, move |_| {
            if let Some(ep) = weak.upgrade() {
                ep.sync_carousel();
            }
            None
        });
        let weak = Rc::downgrade(self);
        state.connect_notify_local(Some("current-index"), move |_, _| {
            if let Some(ep) = weak.upgrade() {
                ep.sync_carousel();
            }
        });
    }

    fn refresh_metadata(&self) {
        let state = self.ctx.player.state();
        let title = state.title();
        self.title.set_label(if title.is_empty() {
            "Not Playing"
        } else {
            &title
        });
        while let Some(child) = self.artists_box.first_child() {
            self.artists_box.remove(&child);
        }
        let track = self.ctx.player.current_track();
        let artists: Vec<(Option<String>, String)> =
            match track.as_ref().filter(|t| !t.artists.is_empty()) {
                Some(t) => t
                    .artists
                    .iter()
                    .map(|a| (a.id.clone(), a.name.clone()))
                    .collect(),
                None => vec![(None, state.artist())],
            };
        let count = artists.len();
        for (i, (id, name)) in artists.into_iter().enumerate() {
            if name.is_empty() {
                continue;
            }
            let label = gtk::Label::builder()
                .label(&name)
                .css_classes(["heading"])
                .opacity(0.7)
                .ellipsize(gtk::pango::EllipsizeMode::End)
                .build();
            let btn = gtk::Button::builder()
                .css_classes(["flat", "link-btn"])
                .has_frame(false)
                .child(&label)
                .build();
            btn.set_cursor(gdk::Cursor::from_name("pointer", None).as_ref());
            let ctx = self.ctx.clone();
            let dismiss = self.on_dismiss.borrow().clone();
            btn.connect_clicked(move |_| {
                ctx.nav.go(NavRequest::Artist {
                    id: id.clone(),
                    name: name.clone(),
                });
                if let Some(f) = &dismiss {
                    f();
                }
            });
            self.artists_box.append(&btn);
            if i + 1 < count {
                self.artists_box.append(
                    &gtk::Label::builder()
                        .label(", ")
                        .css_classes(["heading"])
                        .opacity(0.7)
                        .build(),
                );
            }
        }
        let video_id = state.video_id();
        if video_id.is_empty() {
            self.like.set_data(None, None);
        } else {
            self.like.set_data(
                Some(VideoId(video_id)),
                Some(LikeStatus::parse(&state.like_status())),
            );
        }
    }

    fn refresh_more_menu(self: &Rc<Self>) {
        let Some(track) = self.ctx.player.current_track() else {
            self.more_btn.set_menu_model(gtk::gio::MenuModel::NONE);
            return;
        };
        let this = self.clone();
        let extras = vec![crate::ui::context_menu::MenuAction::new("Stream Info (Debug)", crate::ui::context_menu::Section::Debug, move || this.show_stream_info())];
        let opts = crate::ui::context_menu::SongMenuOptions {
            prefix: "ep",
            // The view already shows the artist as a link and the cover for
            // the album, so those entries would only repeat it.
            hide: &["play_next", "add_to_queue", "goto_artist", "goto_album"],
            nav: Some(self.ctx.nav.clone()),
            ctx: Some(self.ctx.clone()),
            extras,
            ..Default::default()
        };
        let model = crate::ui::context_menu::build_song_menu(&self.more_btn, &track, &self.ctx.player, opts);
        self.more_btn.set_menu_model(model.as_ref());
    }

    /// Port of _show_stream_info: what is playing and how the pipeline sees it.
    pub fn visualizer(&self) -> &Rc<Visualizer> {
        &self.visualizer
    }

    pub fn show_stream_info(self: &Rc<Self>) {
        present_stream_info(&self.ctx, self.root.upcast_ref::<gtk::Widget>());
    }

    // -- carousel over the queue ------------------------------------------

    fn center_carousel(&self) {
        let idx = self.ctx.player.state().current_index();
        if idx < 0 {
            return;
        }
        let page = (idx as usize).saturating_sub(self.cover_offset.get());
        if let Some(cover) = self.covers.borrow().get(page) {
            // A sync in flight keeps its own guard: restore what was there.
            let guarded = self.ignore_page_change.replace(true);
            self.carousel.scroll_to(cover.widget(), false);
            self.ignore_page_change.set(guarded);
        }
    }

    /// Demo hook: a gesture that takes two seconds, then settles `covers`
    /// along. Stands in for a swipe, which no test harness can send.
    pub fn slow_swipe_for_demo(self: &Rc<Self>, covers: i32) {
        let this = self.clone();
        let ticks = std::cell::Cell::new(0u32);
        glib::timeout_add_local(Duration::from_millis(100), move || {
            let tick = ticks.replace(ticks.get() + 1);
            this.note_input(tick == 0);
            if tick == 20 {
                let page = (this.carousel.position().round() as i32 + covers).max(0) as usize;
                tracing::info!(page, "demo: the finger lifts after two seconds");
                if let Some(cover) = this.covers.borrow().get(page) {
                    this.carousel.scroll_to(cover.widget(), true);
                }
            }
            match tick > 22 {
                true => glib::ControlFlow::Break,
                false => glib::ControlFlow::Continue,
            }
        });
    }

    /// Remember that the listener is working the carousel.
    ///
    /// The clock runs from the last movement, so a swipe that takes its time
    /// still counts. `begins` marks the start of a gesture, where the baseline
    /// for judging a swipe is taken.
    fn note_input(&self, begins: bool) {
        let now = Instant::now();
        let fresh = begins || self.user_input_at.get().is_none_or(|last| now.duration_since(last) > GESTURE_GAP);
        if fresh {
            self.gesture_start.set(Some(self.carousel.position()));
        }
        self.user_input_at.set(Some(now));
    }

    /// Put the playing track's cover in view once the carousel has a size.
    ///
    /// A carousel that has never been laid out cannot scroll, and the sheet is
    /// laid out only when it opens. Centring at that moment alone leaves the
    /// view on the first cover, which is why opening the sheet used to show
    /// the wrong song and a swipe played the second one. The frame callback
    /// runs only while the widget is mapped, so this settles on the first
    /// frame the sheet is actually on screen.
    fn center_when_ready(self: &Rc<Self>) {
        if self.center_pending.replace(true) {
            return;
        }
        let weak = Rc::downgrade(self);
        self.carousel.add_tick_callback(move |carousel, _| {
            let Some(ep) = weak.upgrade() else { return glib::ControlFlow::Break };
            if !carousel.is_mapped() || carousel.width() == 0 {
                return glib::ControlFlow::Continue;
            }
            tracing::debug!(width = carousel.width(), position = carousel.position(), "centering the carousel");
            ep.center_carousel();
            ep.center_pending.set(false);
            glib::ControlFlow::Break
        });
    }

    fn sync_carousel(self: &Rc<Self>) {
        let tracks = self.ctx.player.queue_tracks();
        let queue_len = tracks.len();
        let mut covers = self.covers.borrow_mut();
        if queue_len == 0 {
            for cover in covers.drain(..) {
                self.carousel.remove(cover.widget());
            }
            self.cover_offset.set(0);
            return;
        }
        let idx = self.ctx.player.state().current_index().max(0) as usize;
        let idx = idx.min(queue_len - 1);
        self.ignore_page_change.set(true);

        let window_len = queue_len.min(MAX_CAROUSEL_COVERS);
        let max_offset = queue_len - window_len;
        let offset = idx.saturating_sub(window_len / 2).min(max_offset);
        self.cover_offset.set(offset);

        while covers.len() > window_len {
            if let Some(cover) = covers.pop() {
                self.carousel.remove(cover.widget());
            }
        }
        while covers.len() < window_len {
            let cover = CoverPicture::new(self.ctx.net.clone());
            cover.widget().add_css_class("rounded");
            cover.widget().set_hexpand(false);
            cover.widget().set_vexpand(true);
            self.carousel.append(cover.widget());
            covers.push(cover);
        }
        let page = idx - offset;
        let lo = page.saturating_sub(CAROUSEL_PRELOAD_RADIUS);
        let hi = (page + CAROUSEL_PRELOAD_RADIUS).min(covers.len() - 1);
        for (i, cover) in covers.iter().enumerate() {
            // The playing track falls back to the artwork the bar is showing:
            // a radio row often carries no thumbnail of its own.
            let thumb = match tracks.get(offset + i).and_then(|t| t.thumb.clone()) {
                Some(thumb) => thumb,
                None if i == page => self.ctx.player.state().thumbnail_url(),
                None => String::new(),
            };
            // Every cover stays a page, with a placeholder when there is no
            // art. Hiding one would shift every page index after it.
            cover.widget().set_visible(true);
            cover.load(if i >= lo && i <= hi { thumb.as_str() } else { "" });
        }
        if let Some(cover) = covers.get(page) {
            self.carousel.scroll_to(cover.widget(), false);
        }
        drop(covers);
        // The scroll above is lost while the sheet is closed, so ask again for
        // the first frame after it opens.
        self.center_when_ready();
        let weak = Rc::downgrade(self);
        glib::timeout_add_local_once(Duration::from_millis(200), move || {
            if let Some(ep) = weak.upgrade() {
                ep.ignore_page_change.set(false);
            }
        });
    }
}

/// The Stream Info panel, shared by the phone sheet and the desktop view.
///
/// The pipeline half is queried on the audio thread, so the text arrives after
/// a round trip. Copy takes the whole signed URI rather than the shortened one.
pub fn present_stream_info(ctx: &Rc<UiContext>, anchor: &gtk::Widget) {
    let ctx = ctx.clone();
    let anchor = anchor.clone();
    glib::spawn_future_local(async move {
        let text = ctx.player.stream_debug(false).await;
        let label = gtk::Label::builder().label(&text).selectable(true).wrap(true).xalign(0.0).margin_top(4).css_classes(["monospace"]).build();
        let dialog = adw::AlertDialog::builder().heading("Stream Info").extra_child(&label).build();
        dialog.add_response("close", "Close");
        dialog.add_response("copy", "Copy");
        dialog.set_default_response(Some("close"));
        dialog.set_close_response("close");
        let copy_ctx = ctx.clone();
        let copy_anchor = anchor.clone();
        dialog.connect_response(None, move |_, response| {
            if response != "copy" {
                return;
            }
            let ctx = copy_ctx.clone();
            let anchor = copy_anchor.clone();
            glib::spawn_future_local(async move {
                let full = ctx.player.stream_debug(true).await;
                if let Some(display) = gtk::gdk::Display::default() {
                    display.clipboard().set_text(&full);
                }
                crate::ui::toast(&anchor, "Stream info copied");
            });
        });
        dialog.present(Some(&anchor));
    });
}

/// Whether the listener has touched the carousel recently enough for a move
/// to be theirs rather than one the app made.
fn touched_recently(since_input: Option<Duration>) -> bool {
    since_input.is_some_and(|elapsed| elapsed < USER_INPUT_WINDOW)
}

/// Whether the carousel has come to rest on a page rather than between two.
fn settled_on_a_page(position: f64) -> bool {
    (position - position.round()).abs() <= 0.001
}

/// Whether the carousel moved under the gesture rather than settling where it
/// already was.
///
/// A flick can carry several covers at once, so distance is not capped. What
/// matters is that the position left the page the gesture started on: a tap
/// settles where it began, and following that would play whatever cover the
/// carousel happened to be showing.
fn swiped(position: f64, gesture_start: Option<f64>) -> bool {
    gesture_start.is_some_and(|start| (position - start).abs() >= 0.5)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_carousel_still_moving_is_left_alone() {
        assert!(!settled_on_a_page(3.4));
        assert!(settled_on_a_page(3.0));
        assert!(settled_on_a_page(2.9999));
    }

    #[test]
    fn a_slow_swipe_still_counts_because_the_clock_runs_from_the_last_movement() {
        assert!(touched_recently(Some(Duration::from_millis(400))));
        assert!(!touched_recently(Some(Duration::from_secs(5))));
        assert!(!touched_recently(None), "a move nobody asked for is the app's own");
    }

    #[test]
    fn a_flick_across_several_covers_counts() {
        assert!(swiped(4.0, Some(3.0)));
        assert!(swiped(18.0, Some(15.0)), "a hard flick travels further than one cover");
        assert!(swiped(12.0, Some(15.0)));
    }

    #[test]
    fn a_tap_settles_where_it_began_and_moves_nothing() {
        assert!(!swiped(3.0, Some(3.0)));
        assert!(!swiped(3.0, Some(2.7)), "a nudge that snapped back is not a swipe");
        assert!(!swiped(3.0, None), "no gesture, no swipe");
    }
}

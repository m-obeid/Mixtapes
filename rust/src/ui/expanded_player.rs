//! Port of ui/expanded_player.py: the mobile sheet with a Player / Queue /
//! Lyrics toggle, a cover carousel over the queue, metadata with like,
//! seek bar, transport row over the visualizer.

use std::cell::{Cell, RefCell};
use std::rc::Rc;
use std::time::{Duration, Instant};

use gtk::{gdk, glib, prelude::*};

use crate::model::{LikeStatus, PlaybackStatus, VideoId};
use crate::ui::context::{NavRequest, UiContext};
use crate::ui::like_button::LikeButton;
use crate::ui::marquee::MarqueeLabel;
use crate::ui::queue_panel::QueuePanel;
use crate::ui::widgets::cover_picture::CoverPicture;
use crate::ui::widgets::lyrics_view;
use crate::ui::widgets::transport::Transport;
use crate::ui::widgets::visualizer::Visualizer;

const MAX_CAROUSEL_COVERS: usize = 31;
const CAROUSEL_PRELOAD_RADIUS: usize = 5;
const USER_INPUT_WINDOW: Duration = Duration::from_millis(800);

pub struct ExpandedPlayer {
    root: gtk::Box,
    view_stack: adw::ViewStack,
    toggle_nav: adw::ToggleGroup,
    carousel: adw::Carousel,
    covers: RefCell<Vec<Rc<CoverPicture>>>,
    cover_offset: Cell<usize>,
    ignore_page_change: Cell<bool>,
    user_input_at: Cell<Option<Instant>>,
    title: Rc<MarqueeLabel>,
    artists_box: gtk::Box,
    like: Rc<LikeButton>,
    /// Held so the transport's state bindings stay alive with the view.
    #[allow(dead_code)]
    transport: Rc<Transport>,
    visualizer: Rc<Visualizer>,
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
        view_stack.add_titled_with_icon(
            queue_panel.widget(),
            Some("queue"),
            "Queue",
            "music-queue-symbolic",
        );
        view_stack.add_titled_with_icon(
            &lyrics_view::build(),
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
            user_input_at: Cell::new(None),
            title,
            artists_box,
            like,
            transport,
            visualizer,
            height_probe,
            queue_panel,
            ctx,
            on_dismiss: RefCell::new(None),
        });

        // Current-track menu on the more button.
        {
            let weak = Rc::downgrade(&this);
            more_btn.connect_activate(move |_| {});
            let weak2 = weak.clone();
            more_btn.connect_notify_local(Some("active"), move |btn, _| {
                if !btn.is_active() {
                    return;
                }
                if let Some(ep) = weak2.upgrade() {
                    ep.refresh_more_menu(btn);
                }
            });
        }

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
        let mark_input = {
            let weak = Rc::downgrade(self);
            Rc::new(move || {
                if let Some(ep) = weak.upgrade() {
                    ep.user_input_at.set(Some(Instant::now()));
                }
            })
        };
        let drag = gtk::GestureDrag::builder()
            .propagation_phase(gtk::PropagationPhase::Capture)
            .build();
        let m = mark_input.clone();
        drag.connect_drag_begin(move |_, _, _| m());
        self.carousel.add_controller(drag);
        let click = gtk::GestureClick::builder()
            .propagation_phase(gtk::PropagationPhase::Capture)
            .build();
        let m = mark_input.clone();
        click.connect_pressed(move |_, _, _, _| m());
        self.carousel.add_controller(click);
        let scroll = gtk::EventControllerScroll::builder()
            .flags(gtk::EventControllerScrollFlags::BOTH_AXES)
            .propagation_phase(gtk::PropagationPhase::Capture)
            .build();
        let m = mark_input.clone();
        scroll.connect_scroll(move |_, _, _| {
            m();
            glib::Propagation::Proceed
        });
        self.carousel.add_controller(scroll);

        let weak = Rc::downgrade(self);
        self.carousel.connect_position_notify(move |carousel| {
            let Some(ep) = weak.upgrade() else { return };
            if ep.ignore_page_change.get() {
                return;
            }
            let recent = ep
                .user_input_at
                .get()
                .is_some_and(|t| t.elapsed() < USER_INPUT_WINDOW);
            if !recent {
                return;
            }
            let position = carousel.position();
            if (position - position.round()).abs() > 0.001 {
                return;
            }
            let queue_index = ep.cover_offset.get() + position.round() as usize;
            if queue_index as i32 != ep.ctx.player.state().current_index() {
                ep.ctx.player.play_queue_index(queue_index);
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
                ep.center_carousel();
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
            state.connect_notify_local(Some(prop), move |_, _| {
                if let Some(ep) = weak.upgrade() {
                    ep.refresh_metadata();
                }
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
            self.like.set_data(None, LikeStatus::Indifferent);
        } else {
            self.like.set_data(
                Some(VideoId(video_id)),
                LikeStatus::parse(&state.like_status()),
            );
        }
    }

    fn refresh_more_menu(&self, btn: &gtk::MenuButton) {
        let Some(track) = self.ctx.player.current_track() else {
            btn.set_menu_model(gtk::gio::MenuModel::NONE);
            return;
        };
        let opts = crate::ui::context_menu::SongMenuOptions {
            prefix: "ep",
            hide: &["play_next", "add_to_queue"],
            nav: Some(self.ctx.nav.clone()),
            ctx: Some(self.ctx.clone()),
            ..Default::default()
        };
        let model = crate::ui::context_menu::build_song_menu(btn, &track, &self.ctx.player, opts);
        btn.set_menu_model(model.as_ref());
    }

    // -- carousel over the queue ------------------------------------------

    fn center_carousel(&self) {
        let idx = self.ctx.player.state().current_index();
        if idx < 0 {
            return;
        }
        let page = (idx as usize).saturating_sub(self.cover_offset.get());
        if let Some(cover) = self.covers.borrow().get(page) {
            self.ignore_page_change.set(true);
            self.carousel.scroll_to(cover.widget(), false);
            self.ignore_page_change.set(false);
        }
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
            let thumb = tracks
                .get(offset + i)
                .and_then(|t| t.thumb.clone())
                .unwrap_or_default();
            if i >= lo && i <= hi && !thumb.is_empty() {
                cover.widget().set_visible(true);
                cover.load(&thumb);
            } else {
                cover.load("");
                cover.widget().set_visible(!thumb.is_empty());
            }
        }
        if let Some(cover) = covers.get(page) {
            self.carousel.scroll_to(cover.widget(), false);
        }
        drop(covers);
        let weak = Rc::downgrade(self);
        glib::timeout_add_local_once(Duration::from_millis(200), move || {
            if let Some(ep) = weak.upgrade() {
                ep.ignore_page_change.set(false);
            }
        });
    }
}

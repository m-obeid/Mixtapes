//! Bottom player bar. Port of ui/player_bar.py on top of PlayerState bindings.
//!
//! No signal from the audio thread reaches this file. Every visual follows a
//! property on `PlayerState`; every button calls a method on `Player`.

use std::cell::{Cell, RefCell};
use std::rc::Rc;
use std::time::{Duration, Instant};

use gtk::{gdk, glib, prelude::*};

use crate::model::{LikeStatus, PlaybackStatus, VideoId};
use crate::net::NetHandle;
use crate::player::Player;
use crate::state::PlayerState;
use crate::ui::cover::CoverImage;
use crate::ui::format_time;
use crate::ui::like_button::LikeButton;
use crate::ui::marquee::MarqueeLabel;

/// Position updates are ignored this long after a user seek, until the pipeline catches up.
const SEEK_GUARD: Duration = Duration::from_millis(800);
const SCROLL_SEEK_STEP: f64 = 2.0;
const SWIPE_COOLDOWN: Duration = Duration::from_millis(500);

/// Callbacks the window provides. Artist and album carry (id, name).
pub struct PlayerBarCallbacks {
    pub on_artist_click: Rc<dyn Fn(Option<String>, String)>,
    pub on_album_click: Rc<dyn Fn(Option<String>, String)>,
    pub on_queue_click: Rc<dyn Fn()>,
    pub on_expand: Rc<dyn Fn()>,
}

/// Queue and expand handlers the window swaps in once it exists.
#[derive(Default)]
struct LateCallbacks {
    on_queue_click: RefCell<Option<Rc<dyn Fn()>>>,
    on_expand: RefCell<Option<Rc<dyn Fn()>>>,
}

pub struct PlayerBar {
    root: gtk::Box,
    content_box: gtk::Box,
    controls_box: gtk::Box,
    scale: gtk::Scale,
    timings: gtk::Label,
    prev_btn: gtk::Button,
    play_btn: gtk::Button,
    play_stack: gtk::Stack,
    play_icon: gtk::Image,
    next_btn: gtk::Button,
    volume_container: gtk::Box,
    volume_btn: gtk::Button,
    volume_scale: gtk::Scale,
    queue_btn: gtk::ToggleButton,
    like: Rc<LikeButton>,
    overflow_btn: gtk::MenuButton,
    overflow_box: gtk::Box,
    expand_btn: gtk::Button,
    cover: Rc<CoverImage>,
    title: Rc<MarqueeLabel>,
    artists_box: gtk::Box,
    player: Rc<Player>,
    callbacks: PlayerBarCallbacks,
    late: LateCallbacks,
    compact: Cell<bool>,
    sheet_bar: Cell<bool>,
    last_seek: Cell<Option<Instant>>,
    scroll_seek: RefCell<Option<glib::SourceId>>,
    updating_volume: Cell<bool>,
    skip_cooldown: Cell<bool>,
    last_responsive_width: Cell<i32>,
}

impl PlayerBar {
    pub fn new(player: Rc<Player>, net: NetHandle, callbacks: PlayerBarCallbacks) -> Rc<Self> {
        let root = gtk::Box::builder()
            .orientation(gtk::Orientation::Vertical)
            .css_classes(["player-bar"])
            .build();

        let scale = gtk::Scale::builder()
            .orientation(gtk::Orientation::Horizontal)
            .hexpand(true)
            .css_classes(["player-scale"])
            .build();
        scale.set_range(0.0, 100.0);
        root.append(&scale);

        let content_box = gtk::Box::builder()
            .orientation(gtk::Orientation::Horizontal)
            .spacing(12)
            .margin_top(8)
            .margin_bottom(12)
            .margin_start(12)
            .margin_end(12)
            .build();
        root.append(&content_box);

        // Cover, click opens the album.
        let cover = CoverImage::new(net, 48);
        let cover_wrapper = gtk::Box::builder()
            .overflow(gtk::Overflow::Hidden)
            .css_classes(["player-bar-cover"])
            .build();
        cover_wrapper.append(cover.widget());
        let cover_btn = gtk::Button::builder()
            .css_classes(["flat", "link-btn"])
            .has_frame(false)
            .child(&cover_wrapper)
            .build();
        content_box.append(&cover_btn);

        // Title marquee plus one link button per artist.
        let meta_box = gtk::Box::builder()
            .orientation(gtk::Orientation::Vertical)
            .spacing(2)
            .valign(gtk::Align::Center)
            .hexpand(true)
            .build();
        let title = MarqueeLabel::new();
        title.set_label("Not Playing");
        title.add_css_class("heading");
        let artists_box = gtk::Box::builder()
            .orientation(gtk::Orientation::Horizontal)
            .spacing(2)
            .halign(gtk::Align::Start)
            .build();
        meta_box.append(title.widget());
        meta_box.append(&artists_box);
        content_box.append(&meta_box);

        // Transport controls.
        let controls_box = gtk::Box::builder()
            .orientation(gtk::Orientation::Horizontal)
            .spacing(6)
            .valign(gtk::Align::Center)
            .build();

        let timings = gtk::Label::builder()
            .label("0:00 / 0:00")
            .css_classes(["caption", "numeric"])
            .valign(gtk::Align::Center)
            .build();
        controls_box.append(&timings);

        let prev_btn = flat_button("media-skip-backward-symbolic", "Previous");
        controls_box.append(&prev_btn);

        let play_stack = gtk::Stack::builder()
            .transition_type(gtk::StackTransitionType::Crossfade)
            .transition_duration(150)
            .build();
        let play_icon = gtk::Image::from_icon_name("media-playback-start-symbolic");
        play_stack.add_named(&play_icon, Some("icon"));
        let spinner = adw::Spinner::new();
        spinner.set_size_request(16, 16);
        play_stack.add_named(&spinner, Some("spinner"));
        let play_btn = gtk::Button::builder()
            .css_classes(["circular"])
            .valign(gtk::Align::Center)
            .child(&play_stack)
            .tooltip_text("Play")
            .build();
        controls_box.append(&play_btn);

        let next_btn = flat_button("media-skip-forward-symbolic", "Next");
        controls_box.append(&next_btn);

        // Volume: button plus a slider that slides out on hover.
        let volume_btn = flat_button("audio-volume-high-symbolic", "Mute");
        let volume_scale = gtk::Scale::builder()
            .orientation(gtk::Orientation::Horizontal)
            .width_request(80)
            .build();
        volume_scale.set_range(0.0, 1.0);
        volume_scale.set_value(1.0);
        let volume_revealer = gtk::Revealer::builder()
            .transition_type(gtk::RevealerTransitionType::SlideRight)
            .transition_duration(250)
            .child(&volume_scale)
            .build();
        let volume_container = gtk::Box::builder()
            .orientation(gtk::Orientation::Horizontal)
            .valign(gtk::Align::Center)
            .build();
        volume_container.append(&volume_btn);
        volume_container.append(&volume_revealer);
        let hover = gtk::EventControllerMotion::new();
        hover.connect_enter(glib::clone!(
            #[weak]
            volume_revealer,
            move |_, _, _| volume_revealer.set_reveal_child(true)
        ));
        hover.connect_leave(glib::clone!(
            #[weak]
            volume_revealer,
            move |_| volume_revealer.set_reveal_child(false)
        ));
        volume_container.add_controller(hover);
        controls_box.append(&volume_container);

        let queue_btn = gtk::ToggleButton::builder()
            .icon_name("music-queue-symbolic")
            .css_classes(["flat"])
            .valign(gtk::Align::Center)
            .tooltip_text("Toggle Queue")
            .build();
        controls_box.append(&queue_btn);

        let like = LikeButton::new(player.clone());
        like.widget().remove_css_class("circular");
        like.widget().set_visible(false);
        controls_box.append(like.widget());

        // Overflow popover receives controls the responsive tick folds away.
        let overflow_box = gtk::Box::builder()
            .orientation(gtk::Orientation::Vertical)
            .spacing(6)
            .margin_top(6)
            .margin_bottom(6)
            .margin_start(6)
            .margin_end(6)
            .build();
        let overflow_popover = gtk::Popover::builder().child(&overflow_box).build();
        let overflow_btn = gtk::MenuButton::builder()
            .icon_name("view-more-symbolic")
            .css_classes(["flat"])
            .valign(gtk::Align::Center)
            .tooltip_text("More")
            .visible(false)
            .build();
        overflow_btn.set_popover(Some(&overflow_popover));
        controls_box.append(&overflow_btn);

        let expand_btn = flat_button("go-up-symbolic", "Expand player");
        controls_box.append(&expand_btn);

        content_box.append(&controls_box);

        let bar = Rc::new(Self {
            root,
            content_box,
            controls_box,
            scale,
            timings,
            prev_btn,
            play_btn,
            play_stack,
            play_icon,
            next_btn,
            volume_container,
            volume_btn,
            volume_scale,
            queue_btn,
            like,
            overflow_btn,
            overflow_box,
            expand_btn,
            cover,
            title,
            artists_box,
            player,
            callbacks,
            late: LateCallbacks::default(),
            compact: Cell::new(false),
            sheet_bar: Cell::new(false),
            last_seek: Cell::new(None),
            scroll_seek: RefCell::new(None),
            updating_volume: Cell::new(false),
            skip_cooldown: Cell::new(false),
            last_responsive_width: Cell::new(-1),
        });

        bar.bind_state();
        bar.connect_controls(&cover_btn);
        bar.connect_gestures();
        bar.refresh_controls();
        bar.refresh_metadata();

        let weak = Rc::downgrade(&bar);
        bar.root
            .add_tick_callback(move |_, _| match weak.upgrade() {
                Some(bar) => {
                    bar.responsive_tick();
                    glib::ControlFlow::Continue
                }
                None => glib::ControlFlow::Break,
            });
        bar
    }

    pub fn widget(&self) -> &gtk::Box {
        &self.root
    }

    pub fn cover_url(&self) -> Option<String> {
        self.cover.url()
    }

    pub fn set_on_queue_click(&self, f: impl Fn() + 'static) {
        self.late.on_queue_click.replace(Some(Rc::new(f)));
    }

    pub fn set_on_expand(&self, f: impl Fn() + 'static) {
        self.late.on_expand.replace(Some(Rc::new(f)));
    }

    fn queue_click(&self) {
        match self.late.on_queue_click.borrow().as_ref() {
            Some(f) => f(),
            None => (self.callbacks.on_queue_click)(),
        }
    }

    fn expand(&self) {
        match self.late.on_expand.borrow().as_ref() {
            Some(f) => f(),
            None => (self.callbacks.on_expand)(),
        }
    }

    pub fn set_queue_active(&self, active: bool) {
        if self.queue_btn.is_active() != active {
            self.queue_btn.set_active(active);
        }
    }

    /// Flip the expand chevron: down while the cover view is open.
    pub fn set_expanded(&self, expanded: bool) {
        if expanded {
            self.expand_btn.set_icon_name("go-down-symbolic");
            self.expand_btn.set_tooltip_text(Some("Collapse player"));
        } else {
            self.expand_btn.set_icon_name("go-up-symbolic");
            self.expand_btn.set_tooltip_text(Some("Expand player"));
        }
    }

    /// The bar is AdwBottomSheet's bottom bar: tap and drag-up stand down.
    pub fn set_sheet_bar(&self, enabled: bool) {
        self.sheet_bar.set(enabled);
        if enabled {
            self.root.add_css_class("sheet-bar");
        } else {
            self.root.remove_css_class("sheet-bar");
        }
    }

    /// Mobile layout: only cover, meta, play and like stay inline.
    pub fn set_compact(&self, compact: bool) {
        self.compact.set(compact);
        self.last_responsive_width.set(-1);
        if compact {
            for control in self.responsive_order() {
                self.set_control_location(&control, true);
            }
            self.overflow_btn.set_visible(false);
            self.root.add_css_class("compact");
            self.scale.add_css_class("compact");
        } else {
            self.root.remove_css_class("compact");
            self.scale.remove_css_class("compact");
            self.like
                .widget()
                .set_visible(!self.state().video_id().is_empty());
        }
        for w in [
            self.timings.upcast_ref::<gtk::Widget>(),
            self.prev_btn.upcast_ref(),
            self.next_btn.upcast_ref(),
            self.volume_container.upcast_ref(),
            self.queue_btn.upcast_ref(),
            self.expand_btn.upcast_ref(),
        ] {
            w.set_visible(!compact);
        }
        let margin = 10;
        self.content_box.set_margin_start(margin);
        self.content_box.set_margin_end(margin);
        self.content_box.set_margin_top(margin);
        self.content_box.set_margin_bottom(margin);
        self.content_box.set_spacing(margin);
        self.controls_box.set_spacing(margin);
    }

    fn state(&self) -> &PlayerState {
        self.player.state()
    }

    // -- state to widgets -------------------------------------------------

    fn bind_state(self: &Rc<Self>) {
        let state = self.state();

        for prop in [
            "title",
            "artist",
            "thumbnail-url",
            "video-id",
            "like-status",
        ] {
            let weak = Rc::downgrade(self);
            state.connect_notify_local(Some(prop), move |_, _| {
                if let Some(bar) = weak.upgrade() {
                    bar.refresh_metadata();
                }
            });
        }
        for prop in ["status", "duration", "queue-length"] {
            let weak = Rc::downgrade(self);
            state.connect_notify_local(Some(prop), move |_, _| {
                if let Some(bar) = weak.upgrade() {
                    bar.refresh_controls();
                }
            });
        }
        let weak = Rc::downgrade(self);
        state.connect_notify_local(Some("position"), move |_, _| {
            if let Some(bar) = weak.upgrade() {
                bar.refresh_position();
            }
        });
        for prop in ["volume", "muted"] {
            let weak = Rc::downgrade(self);
            state.connect_notify_local(Some(prop), move |_, _| {
                if let Some(bar) = weak.upgrade() {
                    bar.refresh_volume();
                }
            });
        }
        self.refresh_volume();
    }

    /// Title, artist links, cover and like button from the current track.
    fn refresh_metadata(&self) {
        let state = self.state();
        let title = state.title();
        self.title.set_label(if title.is_empty() {
            "Not Playing"
        } else {
            &title
        });

        while let Some(child) = self.artists_box.first_child() {
            self.artists_box.remove(&child);
        }
        let track = self.player.current_track();
        let artists: Vec<(Option<String>, String)> =
            match track.as_ref().filter(|t| !t.artists.is_empty()) {
                Some(t) => t
                    .artists
                    .iter()
                    .map(|a| (a.id.clone(), a.name.clone()))
                    .collect(),
                None => {
                    let artist = state.artist();
                    if artist.is_empty() && title.is_empty() {
                        Vec::new()
                    } else {
                        vec![(
                            None,
                            if artist.is_empty() {
                                "Unknown Artist".to_owned()
                            } else {
                                artist
                            },
                        )]
                    }
                }
            };
        let count = artists.len();
        for (i, (id, name)) in artists.into_iter().enumerate() {
            let label = gtk::Label::builder()
                .label(&name)
                .css_classes(["caption"])
                .build();
            let btn = gtk::Button::builder()
                .css_classes(["flat", "link-btn"])
                .has_frame(false)
                .child(&label)
                .build();
            btn.set_cursor(gdk::Cursor::from_name("pointer", None).as_ref());
            let on_artist = self.callbacks.on_artist_click.clone();
            btn.connect_clicked(move |_| on_artist(id.clone(), name.clone()));
            self.artists_box.append(&btn);
            if i + 1 < count {
                self.artists_box.append(
                    &gtk::Label::builder()
                        .label(", ")
                        .css_classes(["caption"])
                        .build(),
                );
            }
        }

        self.cover.load(&state.thumbnail_url());

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

    /// Play button, spinner and seek bar sensitivity from status plus duration.
    fn refresh_controls(&self) {
        let state = self.state();
        let status = state.status();
        let duration = state.duration();
        let has_queue = state.queue_length() > 0;

        let (child, icon, sensitive, scale_sensitive) = match status {
            PlaybackStatus::Loading => ("spinner", "media-playback-start-symbolic", false, false),
            PlaybackStatus::Playing if duration <= 0.0 => {
                ("spinner", "media-playback-pause-symbolic", false, false)
            }
            PlaybackStatus::Playing => ("icon", "media-playback-pause-symbolic", true, true),
            PlaybackStatus::Paused => (
                "icon",
                "media-playback-start-symbolic",
                true,
                duration > 0.0,
            ),
            PlaybackStatus::Stopped => ("icon", "media-playback-start-symbolic", has_queue, false),
        };
        if self.play_icon.icon_name().as_deref() != Some(icon) {
            tracing::debug!(icon, ?status, "play button icon");
        }
        self.play_icon.set_icon_name(Some(icon));
        self.play_stack.set_visible_child_name(child);
        self.play_btn.set_sensitive(sensitive);
        self.play_btn
            .set_tooltip_text(Some(if status == PlaybackStatus::Playing {
                "Pause"
            } else {
                "Play"
            }));
        self.scale.set_sensitive(scale_sensitive);
        if status == PlaybackStatus::Loading {
            self.scale.set_value(0.0);
            self.timings.set_label("0:00 / 0:00");
        }
        self.refresh_position();
    }

    fn refresh_position(&self) {
        if self
            .last_seek
            .get()
            .is_some_and(|t| t.elapsed() < SEEK_GUARD)
            || self.scroll_seek.borrow().is_some()
        {
            return;
        }
        let state = self.state();
        let position = state.position();
        let duration = state.duration();
        self.scale.set_range(0.0, duration.max(1.0));
        self.scale.set_value(position.min(duration.max(1.0)));
        self.timings.set_label(&format!(
            "{} / {}",
            format_time(position),
            format_time(duration)
        ));
    }

    fn refresh_volume(&self) {
        let state = self.state();
        let volume = state.volume();
        let muted = state.muted();
        self.updating_volume.set(true);
        self.volume_scale
            .set_value(if muted { 0.0 } else { volume });
        self.updating_volume.set(false);
        let icon = if muted || volume <= 0.0 {
            "audio-volume-muted-symbolic"
        } else if volume < 0.33 {
            "audio-volume-low-symbolic"
        } else if volume < 0.66 {
            "audio-volume-medium-symbolic"
        } else {
            "audio-volume-high-symbolic"
        };
        self.volume_btn.set_icon_name(icon);
    }

    // -- widgets to player ------------------------------------------------

    fn connect_controls(self: &Rc<Self>, cover_btn: &gtk::Button) {
        let player = self.player.clone();
        self.prev_btn.connect_clicked(move |_| player.previous());
        let player = self.player.clone();
        self.next_btn.connect_clicked(move |_| player.next());
        let player = self.player.clone();
        self.play_btn.connect_clicked(move |_| {
            tracing::debug!(status = ?player.state().status(), "play button pressed");
            player.toggle_play();
        });

        let weak = Rc::downgrade(self);
        self.scale.connect_change_value(move |_, _, value| {
            if let Some(bar) = weak.upgrade() {
                bar.seek(value);
            }
            glib::Propagation::Proceed
        });
        let scroll = gtk::EventControllerScroll::new(gtk::EventControllerScrollFlags::VERTICAL);
        let weak = Rc::downgrade(self);
        scroll.connect_scroll(move |_, _, dy| match weak.upgrade() {
            Some(bar) => bar.scroll_seek(dy),
            None => glib::Propagation::Proceed,
        });
        self.scale.add_controller(scroll);

        let player = self.player.clone();
        self.volume_btn
            .connect_clicked(move |_| player.set_mute(!player.state().muted()));
        let weak = Rc::downgrade(self);
        self.volume_scale.connect_value_changed(move |scale| {
            if let Some(bar) = weak.upgrade() {
                if !bar.updating_volume.get() {
                    bar.player.set_volume(scale.value());
                }
            }
        });

        let weak = Rc::downgrade(self);
        self.queue_btn.connect_clicked(move |_| {
            if let Some(bar) = weak.upgrade() {
                bar.queue_click();
            }
        });
        let weak = Rc::downgrade(self);
        self.expand_btn.connect_clicked(move |_| {
            if let Some(bar) = weak.upgrade() {
                bar.expand();
            }
        });

        let weak = Rc::downgrade(self);
        cover_btn.connect_clicked(move |_| {
            let Some(bar) = weak.upgrade() else { return };
            let album = bar.player.current_track().and_then(|t| t.album);
            (bar.callbacks.on_album_click)(
                album.as_ref().and_then(|a| a.id.clone()),
                album.map(|a| a.name).unwrap_or_default(),
            );
        });
    }

    /// Compact-mode gestures: drag up or tap expands, horizontal swipe skips.
    fn connect_gestures(self: &Rc<Self>) {
        let drag = gtk::GestureDrag::builder()
            .propagation_phase(gtk::PropagationPhase::Bubble)
            .build();
        let weak = Rc::downgrade(self);
        drag.connect_drag_update(move |gesture, _, dy| {
            let Some(bar) = weak.upgrade() else { return };
            if bar.sheet_bar.get() || !bar.compact.get() {
                return;
            }
            if dy < -15.0 {
                bar.expand();
                gesture.set_state(gtk::EventSequenceState::Claimed);
            }
        });
        self.content_box.add_controller(drag);

        let tap = gtk::GestureClick::builder()
            .propagation_phase(gtk::PropagationPhase::Bubble)
            .build();
        let weak = Rc::downgrade(self);
        tap.connect_released(move |_, _, _, _| {
            let Some(bar) = weak.upgrade() else { return };
            if !bar.sheet_bar.get() && bar.compact.get() {
                bar.expand();
            }
        });
        self.content_box.add_controller(tap);

        let swipe = gtk::GestureSwipe::builder()
            .propagation_phase(gtk::PropagationPhase::Bubble)
            .build();
        let weak = Rc::downgrade(self);
        swipe.connect_swipe(move |gesture, vx, vy| {
            let Some(bar) = weak.upgrade() else { return };
            if !bar.compact.get() || bar.skip_cooldown.get() {
                return;
            }
            if vy.abs() > 100.0 || vy.abs() > vx.abs() * 0.5 || vx.abs() <= 350.0 {
                return;
            }
            bar.skip_cooldown.set(true);
            if vx < 0.0 {
                bar.player.next()
            } else {
                bar.player.previous()
            }
            gesture.set_state(gtk::EventSequenceState::Claimed);
            let weak = Rc::downgrade(&bar);
            glib::timeout_add_local_once(SWIPE_COOLDOWN, move || {
                if let Some(bar) = weak.upgrade() {
                    bar.skip_cooldown.set(false);
                }
            });
        });
        self.content_box.add_controller(swipe);
    }

    fn seek(&self, seconds: f64) {
        if self.state().duration() <= 0.0 {
            return;
        }
        self.last_seek.set(Some(Instant::now()));
        self.player.seek(seconds);
        self.timings.set_label(&format!(
            "{} / {}",
            format_time(seconds),
            format_time(self.state().duration())
        ));
    }

    /// Mouse wheel over the seek bar nudges by two seconds, applied once the wheel settles.
    fn scroll_seek(self: &Rc<Self>, dy: f64) -> glib::Propagation {
        let duration = self.state().duration();
        if duration <= 0.0 {
            return glib::Propagation::Proceed;
        }
        let target = (self.scale.value() - dy * SCROLL_SEEK_STEP).clamp(0.0, duration);
        self.scale.set_value(target);
        if let Some(id) = self.scroll_seek.borrow_mut().take() {
            id.remove();
        }
        let weak = Rc::downgrade(self);
        let id = glib::timeout_add_local_once(Duration::from_millis(100), move || {
            if let Some(bar) = weak.upgrade() {
                bar.scroll_seek.borrow_mut().take();
                bar.seek(target);
            }
        });
        self.scroll_seek.replace(Some(id));
        glib::Propagation::Stop
    }

    // -- responsive overflow ----------------------------------------------

    /// Canonical order of the controls that fold into the overflow popover.
    fn responsive_order(&self) -> Vec<gtk::Widget> {
        vec![
            self.volume_container.clone().upcast(),
            self.queue_btn.clone().upcast(),
            self.like.widget().clone().upcast(),
        ]
    }

    /// Fold like, then queue, then volume into the popover as the bar narrows.
    fn responsive_tick(&self) {
        if self.compact.get() {
            return;
        }
        let width = self.root.width();
        if width <= 1 || width == self.last_responsive_width.get() {
            return;
        }
        self.last_responsive_width.set(width);

        let candidates: [(gtk::Widget, bool); 3] = [
            (self.like.widget().clone().upcast(), width >= 720),
            (self.queue_btn.clone().upcast(), width >= 640),
            (self.volume_container.clone().upcast(), width >= 560),
        ];
        let mut overflow: Vec<gtk::Widget> = candidates
            .iter()
            .filter(|(_, inline)| !inline)
            .map(|(w, _)| w.clone())
            .collect();
        // Folding one control saves nothing: the 3-dot button takes its place.
        if overflow.len() < 2 {
            overflow.clear();
        }
        for (control, _) in &candidates {
            self.set_control_location(control, !overflow.contains(control));
        }
        self.overflow_btn.set_visible(!overflow.is_empty());
    }

    fn inline_anchor_for(&self, control: &gtk::Widget) -> gtk::Widget {
        let order = self.responsive_order();
        let idx = order.iter().position(|w| w == control).unwrap_or(0);
        for prev in order[..idx].iter().rev() {
            if prev.parent().as_ref() == Some(self.controls_box.upcast_ref()) {
                return prev.clone();
            }
        }
        self.next_btn.clone().upcast()
    }

    fn set_control_location(&self, control: &gtk::Widget, inline: bool) {
        let parent = control.parent();
        if inline && parent.as_ref() == Some(self.overflow_box.upcast_ref()) {
            self.overflow_box.remove(control);
            self.controls_box
                .insert_child_after(control, Some(&self.inline_anchor_for(control)));
        } else if !inline && parent.as_ref() == Some(self.controls_box.upcast_ref()) {
            self.controls_box.remove(control);
            self.overflow_box.append(control);
        }
    }
}

fn flat_button(icon: &str, tooltip: &str) -> gtk::Button {
    gtk::Button::builder()
        .icon_name(icon)
        .css_classes(["flat"])
        .valign(gtk::Align::Center)
        .tooltip_text(tooltip)
        .build()
}

//! Transport controls shared by the expanded player and the desktop cover
//! view: seek scale, time labels, play stack with spinner, skip buttons and
//! a vertical volume popover. All bound to PlayerState.

use std::cell::Cell;
use std::rc::Rc;
use std::time::{Duration, Instant};

use gtk::{glib, prelude::*};

use crate::model::PlaybackStatus;
use crate::player::Player;
use crate::ui::format_time;

const SEEK_GUARD: Duration = Duration::from_millis(800);

pub struct Transport {
    pub scale: gtk::Scale,
    pub pos_label: gtk::Label,
    pub dur_label: gtk::Label,
    pub prev_btn: gtk::Button,
    pub play_btn: gtk::Button,
    pub next_btn: gtk::Button,
    pub vol_btn: gtk::MenuButton,
    play_stack: gtk::Stack,
    play_icon: gtk::Image,
    volume_scale: gtk::Scale,
    player: Rc<Player>,
    last_seek: Cell<Option<Instant>>,
    updating_volume: Cell<bool>,
}

impl Transport {
    /// `play_size` is the play button's square size, `icon_px` its icon size.
    pub fn new(player: Rc<Player>, play_size: i32, icon_px: i32, skip_size: i32) -> Rc<Self> {
        let scale = gtk::Scale::builder().orientation(gtk::Orientation::Horizontal).hexpand(true).valign(gtk::Align::Center).css_classes(["progress-scale"]).build();
        scale.set_range(0.0, 100.0);
        let pos_label = gtk::Label::builder().label("0:00").css_classes(["caption", "numeric"]).halign(gtk::Align::Start).build();
        let dur_label = gtk::Label::builder().label("0:00").css_classes(["caption", "numeric"]).halign(gtk::Align::End).build();

        let prev_btn = gtk::Button::builder().icon_name("media-skip-backward-symbolic").css_classes(["circular"]).valign(gtk::Align::Center).build();
        let next_btn = gtk::Button::builder().icon_name("media-skip-forward-symbolic").css_classes(["circular"]).valign(gtk::Align::Center).build();
        if skip_size > 0 {
            prev_btn.set_size_request(skip_size, skip_size);
            next_btn.set_size_request(skip_size, skip_size);
        }

        let play_stack = gtk::Stack::builder().transition_type(gtk::StackTransitionType::Crossfade).transition_duration(150).build();
        let play_icon = gtk::Image::builder().icon_name("media-playback-start-symbolic").pixel_size(icon_px).build();
        play_stack.add_named(&play_icon, Some("icon"));
        let spinner = adw::Spinner::new();
        spinner.set_size_request(24, 24);
        play_stack.add_named(&spinner, Some("spinner"));
        let play_btn = gtk::Button::builder().css_classes(["circular", "suggested-action"]).valign(gtk::Align::Center).child(&play_stack).build();
        play_btn.set_size_request(play_size, play_size);

        let volume_scale = gtk::Scale::builder().orientation(gtk::Orientation::Vertical).inverted(true).height_request(150).build();
        volume_scale.set_range(0.0, 1.0);
        volume_scale.set_value(1.0);
        let vol_box = gtk::Box::builder().orientation(gtk::Orientation::Vertical).spacing(8).margin_top(12).margin_bottom(12).margin_start(8).margin_end(8).build();
        vol_box.append(&volume_scale);
        let vol_popover = gtk::Popover::builder().position(gtk::PositionType::Top).has_arrow(true).css_classes(["compact-popover"]).child(&vol_box).build();
        let vol_btn = gtk::MenuButton::builder().icon_name("audio-volume-high-symbolic").direction(gtk::ArrowType::Up).css_classes(["flat", "circular"]).valign(gtk::Align::Center).build();
        vol_btn.set_popover(Some(&vol_popover));

        let this = Rc::new(Self { scale, pos_label, dur_label, prev_btn, play_btn, next_btn, vol_btn, play_stack, play_icon, volume_scale, player, last_seek: Cell::new(None), updating_volume: Cell::new(false) });
        this.connect();
        this.refresh_controls();
        this.refresh_position();
        this.refresh_volume();
        this
    }

    fn connect(self: &Rc<Self>) {
        let state = self.player.state();
        for prop in ["status", "duration", "queue-length"] {
            let weak = Rc::downgrade(self);
            state.connect_notify_local(Some(prop), move |_, _| {
                if let Some(t) = weak.upgrade() {
                    t.refresh_controls();
                }
            });
        }
        let weak = Rc::downgrade(self);
        state.connect_notify_local(Some("position"), move |_, _| {
            if let Some(t) = weak.upgrade() {
                t.refresh_position();
            }
        });
        for prop in ["volume", "muted"] {
            let weak = Rc::downgrade(self);
            state.connect_notify_local(Some(prop), move |_, _| {
                if let Some(t) = weak.upgrade() {
                    t.refresh_volume();
                }
            });
        }

        let player = self.player.clone();
        self.prev_btn.connect_clicked(move |_| player.previous());
        let player = self.player.clone();
        self.next_btn.connect_clicked(move |_| player.next());
        let player = self.player.clone();
        self.play_btn.connect_clicked(move |_| player.toggle_play());
        let weak = Rc::downgrade(self);
        self.scale.connect_change_value(move |_, _, value| {
            if let Some(t) = weak.upgrade() {
                if t.player.state().duration() > 0.0 {
                    t.last_seek.set(Some(Instant::now()));
                    t.player.seek(value);
                    t.pos_label.set_label(&format_time(value));
                }
            }
            glib::Propagation::Proceed
        });
        let weak = Rc::downgrade(self);
        self.volume_scale.connect_value_changed(move |scale| {
            if let Some(t) = weak.upgrade() {
                if !t.updating_volume.get() {
                    t.player.set_volume(scale.value());
                }
            }
        });
    }

    fn refresh_controls(&self) {
        let state = self.player.state();
        let status = state.status();
        let duration = state.duration();
        let (child, icon, sensitive) = match status {
            PlaybackStatus::Loading => ("spinner", "media-playback-start-symbolic", false),
            PlaybackStatus::Playing if duration <= 0.0 => ("spinner", "media-playback-pause-symbolic", false),
            PlaybackStatus::Playing => ("icon", "media-playback-pause-symbolic", true),
            PlaybackStatus::Paused => ("icon", "media-playback-start-symbolic", true),
            PlaybackStatus::Stopped => ("icon", "media-playback-start-symbolic", state.queue_length() > 0),
        };
        self.play_icon.set_icon_name(Some(icon));
        self.play_stack.set_visible_child_name(child);
        self.play_btn.set_sensitive(sensitive);
        self.scale.set_sensitive(duration > 0.0 && status != PlaybackStatus::Loading);
        if status == PlaybackStatus::Loading {
            self.scale.set_value(0.0);
            self.pos_label.set_label("0:00");
            self.dur_label.set_label("0:00");
        }
    }

    fn refresh_position(&self) {
        if self.last_seek.get().is_some_and(|t| t.elapsed() < SEEK_GUARD) {
            return;
        }
        let state = self.player.state();
        let (position, duration) = (state.position(), state.duration());
        self.scale.set_range(0.0, duration.max(1.0));
        self.scale.set_value(position.min(duration.max(1.0)));
        self.pos_label.set_label(&format_time(position));
        self.dur_label.set_label(&format_time(duration));
    }

    fn refresh_volume(&self) {
        let state = self.player.state();
        let (volume, muted) = (state.volume(), state.muted());
        self.updating_volume.set(true);
        self.volume_scale.set_value(if muted { 0.0 } else { volume });
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
        self.vol_btn.set_icon_name(icon);
    }
}

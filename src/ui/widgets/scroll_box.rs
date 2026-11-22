//! Port of ui/widgets/scroll_box.py: a horizontal scroller with hover
//! arrow buttons that page the content with an eased scroll.
//!
//! A mouse drags the row sideways too, with a short glide on release, since
//! only touch scrolls a row by dragging on its own (#187).

use std::cell::Cell;
use std::rc::Rc;
use std::time::Duration;

use gtk::{gdk, glib, prelude::*};

use crate::ui::widgets::fade_edges::FadeEdges;

/// How far the pointer moves before a press turns into a drag instead of a click.
const DRAG_THRESHOLD: f64 = 8.0;
/// How fast a glide slows, per second, and the speed in pixels a second where it stops.
const GLIDE_DECAY: f64 = 5.0;
const GLIDE_MIN_SPEED: f64 = 40.0;
/// A release this long after the last move starts no glide.
const GLIDE_STALE_US: i64 = 80_000;
/// Width of the strip at each edge that sends a near miss of an arrow to the arrow.
const ARROW_STRIP: i32 = 52;

pub struct HorizontalScrollBox {
    overlay: gtk::Overlay,
    fade: FadeEdges,
    edge_fade: Cell<bool>,
    scrolled: gtk::ScrolledWindow,
    left_btn: gtk::Button,
    right_btn: gtk::Button,
    left_strip: gtk::Box,
    right_strip: gtk::Box,
    hovered: Cell<bool>,
    animating: Cell<bool>,
    target: Cell<f64>,
    /// Set once a mouse press has moved far enough to scroll the row.
    dragging: Cell<bool>,
    drag_origin: Cell<f64>,
    /// The last drag sample, as (time in microseconds, value), and the speed it gives.
    drag_sample: Cell<(i64, f64)>,
    drag_speed: Cell<f64>,
    /// Bumped to stop a running glide.
    glide: Rc<Cell<u32>>,
}

impl HorizontalScrollBox {
    pub fn new() -> Rc<Self> {
        let scrolled = gtk::ScrolledWindow::builder().hscrollbar_policy(gtk::PolicyType::External).vscrollbar_policy(gtk::PolicyType::Never).hexpand(true).build();
        // External: it scrolls by swipe, wheel and the arrows, with no bar drawn over the row.
        // Cards sliding under a resting pointer would each run a hover transition.
        crate::ui::suppress_hover_while_scrolling(&scrolled);
        let fade = FadeEdges::new();
        scrolled.set_hexpand(true);
        fade.append(&scrolled);
        let overlay = gtk::Overlay::builder().child(&fade).build();
        let left_btn = gtk::Button::builder().icon_name("go-previous-symbolic").css_classes(["circular", "osd"]).valign(gtk::Align::Center).halign(gtk::Align::Start).margin_start(8).visible(false).build();
        let right_btn = gtk::Button::builder().icon_name("go-next-symbolic").css_classes(["circular", "osd"]).valign(gtk::Align::Center).halign(gtk::Align::End).margin_end(8).visible(false).build();
        // Full height strips under the arrows, so a click beside an arrow pages the row instead of opening the card under it.
        let left_strip = gtk::Box::builder().width_request(ARROW_STRIP).halign(gtk::Align::Start).valign(gtk::Align::Fill).visible(false).build();
        let right_strip = gtk::Box::builder().width_request(ARROW_STRIP).halign(gtk::Align::End).valign(gtk::Align::Fill).visible(false).build();
        overlay.add_overlay(&left_strip);
        overlay.add_overlay(&right_strip);
        overlay.add_overlay(&left_btn);
        overlay.add_overlay(&right_btn);
        for (strip, btn) in [(&left_strip, &left_btn), (&right_strip, &right_btn)] {
            let click = gtk::GestureClick::builder().button(gdk::BUTTON_PRIMARY).build();
            let btn = btn.downgrade();
            click.connect_released(move |_, _, _, _| {
                if let Some(btn) = btn.upgrade() {
                    btn.emit_clicked();
                }
            });
            strip.add_controller(click);
        }

        let this = Rc::new(Self {
            overlay,
            fade,
            edge_fade: Cell::new(false),
            scrolled,
            left_btn,
            right_btn,
            left_strip,
            right_strip,
            hovered: Cell::new(false),
            animating: Cell::new(false),
            target: Cell::new(0.0),
            dragging: Cell::new(false),
            drag_origin: Cell::new(0.0),
            drag_sample: Cell::new((0, 0.0)),
            drag_speed: Cell::new(0.0),
            glide: Rc::new(Cell::new(0)),
        });
        this.connect_drag();

        let adj = this.scrolled.hadjustment();
        for signal in ["value-changed", "changed"] {
            let weak = Rc::downgrade(&this);
            adj.connect_local(signal, false, move |_| {
                if let Some(s) = weak.upgrade() {
                    s.update_buttons();
                }
                None
            });
        }
        let motion = gtk::EventControllerMotion::new();
        let weak = Rc::downgrade(&this);
        motion.connect_enter(move |_, _, _| {
            if let Some(s) = weak.upgrade() {
                s.hovered.set(true);
                s.update_buttons();
            }
        });
        let weak = Rc::downgrade(&this);
        motion.connect_leave(move |_| {
            if let Some(s) = weak.upgrade() {
                s.hovered.set(false);
                s.update_buttons();
            }
        });
        this.overlay.add_controller(motion);

        let weak = Rc::downgrade(&this);
        this.left_btn.connect_clicked(move |_| {
            if let Some(s) = weak.upgrade() {
                let adj = s.scrolled.hadjustment();
                s.animate_to((adj.value() - adj.page_size() * 0.8).max(adj.lower()));
            }
        });
        let weak = Rc::downgrade(&this);
        this.right_btn.connect_clicked(move |_| {
            if let Some(s) = weak.upgrade() {
                let adj = s.scrolled.hadjustment();
                s.animate_to((adj.value() + adj.page_size() * 0.8).min(adj.upper() - adj.page_size()));
            }
        });
        this
    }

    /// Grab scrolling for a mouse. Capture phase, so the drag sees the press
    /// before the card does, and claiming it once the pointer moves cancels the
    /// card's click. Touch is left to the scrolled window's own kinetic drag.
    fn connect_drag(self: &Rc<Self>) {
        let drag = gtk::GestureDrag::builder().button(gdk::BUTTON_PRIMARY).propagation_phase(gtk::PropagationPhase::Capture).build();
        let weak = Rc::downgrade(self);
        drag.connect_drag_begin(move |gesture, _, _| {
            let Some(s) = weak.upgrade() else { return };
            let touch = gesture.current_event().is_some_and(|e| e.event_type() == gdk::EventType::TouchBegin);
            if touch {
                gesture.set_state(gtk::EventSequenceState::Denied);
                return;
            }
            s.stop_motion();
            let value = s.scrolled.hadjustment().value();
            s.dragging.set(false);
            s.drag_origin.set(value);
            s.drag_sample.set((glib::monotonic_time(), value));
            s.drag_speed.set(0.0);
        });
        let weak = Rc::downgrade(self);
        drag.connect_drag_update(move |gesture, dx, _| {
            let Some(s) = weak.upgrade() else { return };
            if !s.dragging.get() {
                if dx.abs() < DRAG_THRESHOLD {
                    return;
                }
                s.dragging.set(true);
                gesture.set_state(gtk::EventSequenceState::Claimed);
                s.scrolled.set_cursor_from_name(Some("grabbing"));
            }
            let adj = s.scrolled.hadjustment();
            adj.set_value(s.drag_origin.get() - dx);
            let (then, before) = s.drag_sample.get();
            let now = glib::monotonic_time();
            let value = adj.value();
            if now > then {
                let speed = (value - before) * 1_000_000.0 / (now - then) as f64;
                // Smoothed, so one jittery sample does not set the glide.
                s.drag_speed.set(s.drag_speed.get() * 0.4 + speed * 0.6);
                s.drag_sample.set((now, value));
            }
        });
        let weak = Rc::downgrade(self);
        drag.connect_drag_end(move |_, _, _| {
            let Some(s) = weak.upgrade() else { return };
            if !s.dragging.replace(false) {
                return;
            }
            s.scrolled.set_cursor(None);
            let (then, _) = s.drag_sample.get();
            if glib::monotonic_time() - then < GLIDE_STALE_US {
                s.start_glide(s.drag_speed.get());
            }
        });
        let weak = Rc::downgrade(self);
        drag.connect_cancel(move |_, _| {
            if let Some(s) = weak.upgrade() {
                if s.dragging.replace(false) {
                    s.scrolled.set_cursor(None);
                }
            }
        });
        self.scrolled.add_controller(drag);
    }

    /// Keep the row moving after a drag, slowing until it stops or hits an end.
    fn start_glide(&self, speed: f64) {
        if speed.abs() < GLIDE_MIN_SPEED {
            return;
        }
        let generation = self.glide.get().wrapping_add(1);
        self.glide.set(generation);
        let glide = Cell::new((speed, None::<i64>));
        let owner = self.glide.clone();
        let scrolled = self.scrolled.downgrade();
        self.scrolled.add_tick_callback(move |_, clock| {
            let Some(scrolled) = scrolled.upgrade() else { return glib::ControlFlow::Break };
            if owner.get() != generation {
                return glib::ControlFlow::Break;
            }
            let (speed, last) = glide.get();
            let now = clock.frame_time();
            let Some(before) = last else {
                glide.set((speed, Some(now)));
                return glib::ControlFlow::Continue;
            };
            let dt = (now - before) as f64 / 1_000_000.0;
            let adj = scrolled.hadjustment();
            let wanted = adj.value() + speed * dt;
            adj.set_value(wanted);
            let speed = speed * (-GLIDE_DECAY * dt).exp();
            glide.set((speed, Some(now)));
            // Clamped at an end, or slow enough to call it stopped.
            if (adj.value() - wanted).abs() > 0.5 || speed.abs() < GLIDE_MIN_SPEED {
                return glib::ControlFlow::Break;
            }
            glib::ControlFlow::Continue
        });
    }

    /// Stop a glide or an arrow's eased scroll, as a new press takes over.
    fn stop_motion(&self) {
        self.glide.set(self.glide.get().wrapping_add(1));
        self.animating.set(false);
    }

    /// Fade the row out where more of it lies past the edge. It repaints the row through a mask, so not for rows of covers.
    pub fn set_edge_fade(&self, on: bool) {
        self.edge_fade.set(on);
        self.update_buttons();
    }

    pub fn widget(&self) -> &gtk::Overlay {
        &self.overlay
    }

    pub fn hadjustment(&self) -> gtk::Adjustment {
        self.scrolled.hadjustment()
    }

    pub fn set_content(self: &Rc<Self>, child: &impl IsA<gtk::Widget>) {
        self.scrolled.set_child(Some(child));
        let weak = Rc::downgrade(self);
        glib::idle_add_local_once(move || {
            if let Some(s) = weak.upgrade() {
                s.update_buttons();
            }
        });
    }

    fn animate_to(self: &Rc<Self>, target: f64) {
        self.glide.set(self.glide.get().wrapping_add(1));
        self.target.set(target);
        if self.animating.replace(true) {
            return;
        }
        let weak = Rc::downgrade(self);
        glib::timeout_add_local(Duration::from_millis(16), move || {
            let Some(s) = weak.upgrade() else { return glib::ControlFlow::Break };
            // A press on the row took over.
            if !s.animating.get() {
                return glib::ControlFlow::Break;
            }
            let adj = s.scrolled.hadjustment();
            let diff = s.target.get() - adj.value();
            if diff.abs() < 2.0 {
                adj.set_value(s.target.get());
                s.animating.set(false);
                return glib::ControlFlow::Break;
            }
            adj.set_value(adj.value() + diff * 0.15);
            glib::ControlFlow::Continue
        });
    }

    fn update_buttons(&self) {
        if self.edge_fade.get() {
            let adj = self.scrolled.hadjustment();
            self.fade.set_fades(adj.value() > adj.lower() + 1.0, adj.value() + adj.page_size() < adj.upper() - 1.0);
        }
        let adj = self.scrolled.hadjustment();
        let hovered = self.hovered.get();
        let left = hovered && adj.value() > adj.lower() + 1.0;
        let right = hovered && adj.value() + adj.page_size() < adj.upper() - 1.0;
        self.left_btn.set_visible(left);
        self.left_strip.set_visible(left);
        self.right_btn.set_visible(right);
        self.right_strip.set_visible(right);
    }
}

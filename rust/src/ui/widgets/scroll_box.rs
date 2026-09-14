//! Port of ui/widgets/scroll_box.py: a horizontal scroller with hover
//! arrow buttons that page the content with an eased scroll.

use std::cell::Cell;
use std::rc::Rc;
use std::time::Duration;

use gtk::{glib, prelude::*};

pub struct HorizontalScrollBox {
    overlay: gtk::Overlay,
    scrolled: gtk::ScrolledWindow,
    left_btn: gtk::Button,
    right_btn: gtk::Button,
    hovered: Cell<bool>,
    animating: Cell<bool>,
    target: Cell<f64>,
}

impl HorizontalScrollBox {
    pub fn new() -> Rc<Self> {
        let scrolled = gtk::ScrolledWindow::builder().hscrollbar_policy(gtk::PolicyType::Automatic).vscrollbar_policy(gtk::PolicyType::Never).hexpand(true).build();
        let overlay = gtk::Overlay::builder().child(&scrolled).build();
        let left_btn = gtk::Button::builder().icon_name("go-previous-symbolic").css_classes(["circular", "osd"]).valign(gtk::Align::Center).halign(gtk::Align::Start).margin_start(8).visible(false).build();
        let right_btn = gtk::Button::builder().icon_name("go-next-symbolic").css_classes(["circular", "osd"]).valign(gtk::Align::Center).halign(gtk::Align::End).margin_end(8).visible(false).build();
        overlay.add_overlay(&left_btn);
        overlay.add_overlay(&right_btn);

        let this = Rc::new(Self { overlay, scrolled, left_btn, right_btn, hovered: Cell::new(false), animating: Cell::new(false), target: Cell::new(0.0) });

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
        self.target.set(target);
        if self.animating.replace(true) {
            return;
        }
        let weak = Rc::downgrade(self);
        glib::timeout_add_local(Duration::from_millis(16), move || {
            let Some(s) = weak.upgrade() else { return glib::ControlFlow::Break };
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
        if !self.hovered.get() {
            self.left_btn.set_visible(false);
            self.right_btn.set_visible(false);
            return;
        }
        let adj = self.scrolled.hadjustment();
        self.left_btn.set_visible(adj.value() > adj.lower() + 1.0);
        self.right_btn.set_visible(adj.value() + adj.page_size() < adj.upper() - 1.0);
    }
}

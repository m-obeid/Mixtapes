//! Port of ui.utils.MarqueeLabel: a label that scrolls when the text
//! is wider than the space it gets, looping with a second copy.

use std::cell::{Cell, RefCell};
use std::rc::Rc;

use gtk::{gdk, glib, prelude::*};

const LOOP_SPACING: i32 = 60;
const SPEED_PX_PER_SEC: f64 = 40.0;

pub struct MarqueeLabel {
    root: gtk::ScrolledWindow,
    label1: gtk::Label,
    label2: gtk::Label,
    tick: RefCell<Option<gtk::TickCallbackId>>,
    last_frame: Cell<Option<i64>>,
}

impl MarqueeLabel {
    pub fn new() -> Rc<Self> {
        let root = gtk::ScrolledWindow::builder()
            .hscrollbar_policy(gtk::PolicyType::External)
            .vscrollbar_policy(gtk::PolicyType::Never)
            .hexpand(true)
            .build();
        let row = gtk::Box::builder().orientation(gtk::Orientation::Horizontal).spacing(LOOP_SPACING).build();
        let label1 = gtk::Label::builder().halign(gtk::Align::Start).build();
        let label2 = gtk::Label::builder().halign(gtk::Align::Start).visible(false).build();
        row.append(&label1);
        row.append(&label2);
        root.set_child(Some(&row));

        let this = Rc::new(Self { root, label1, label2, tick: RefCell::new(None), last_frame: Cell::new(None) });
        let weak = Rc::downgrade(&this);
        this.root.connect_map(move |_| {
            if let Some(m) = weak.upgrade() {
                m.start();
            }
        });
        let weak = Rc::downgrade(&this);
        this.root.connect_unmap(move |_| {
            if let Some(m) = weak.upgrade() {
                m.stop();
            }
        });
        this
    }

    pub fn widget(&self) -> &gtk::ScrolledWindow {
        &self.root
    }

    pub fn add_css_class(&self, class: &str) {
        self.label1.add_css_class(class);
        self.label2.add_css_class(class);
    }

    pub fn set_label(&self, text: &str) {
        self.label1.set_label(text);
        self.label2.set_label(text);
        self.root.hadjustment().set_value(0.0);
        self.last_frame.set(None);
    }

    fn start(self: &Rc<Self>) {
        if self.tick.borrow().is_some() {
            return;
        }
        let weak = Rc::downgrade(self);
        let id = self.root.add_tick_callback(move |_, clock| {
            match weak.upgrade() {
                Some(m) => {
                    m.on_tick(clock);
                    glib::ControlFlow::Continue
                }
                None => glib::ControlFlow::Break,
            }
        });
        self.tick.replace(Some(id));
    }

    fn stop(&self) {
        if let Some(id) = self.tick.borrow_mut().take() {
            id.remove();
        }
    }

    fn on_tick(&self, clock: &gdk::FrameClock) {
        let width = self.root.width();
        let label_w = self.label1.width();
        if label_w <= width {
            self.label2.set_visible(false);
            self.root.hadjustment().set_value(0.0);
            return;
        }
        self.label2.set_visible(true);
        let now = clock.frame_time();
        let Some(last) = self.last_frame.replace(Some(now)) else { return };
        let delta = (now - last) as f64 / 1_000_000.0;
        let adj = self.root.hadjustment();
        let loop_point = (label_w + LOOP_SPACING) as f64;
        let mut value = adj.value() + SPEED_PX_PER_SEC * delta;
        if value >= loop_point {
            value -= loop_point;
        }
        adj.set_value(value);
    }
}

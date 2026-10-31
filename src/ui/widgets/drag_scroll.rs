//! Scrolling a list while a dragged row sits near its top or bottom edge.
//!
//! GTK's list views do this on their own, but never stop when the drag ends
//! outside the list, so a list using this takes that part over.

use std::cell::Cell;
use std::rc::Rc;

use gtk::{gio, glib, prelude::*};

/// How far from the edge a drag starts to scroll, and the top speed in pixels a second.
const ZONE: f64 = 56.0;
const MAX_SPEED: f64 = 600.0;

pub struct DragScroll {
    list: gtk::GridView,
    scrolled: gtk::ScrolledWindow,
    speed: Cell<f64>,
    ticking: Cell<bool>,
}

impl DragScroll {
    pub fn attach(list: &gtk::GridView, scrolled: &gtk::ScrolledWindow) -> Rc<Self> {
        let controllers: gio::ListModel = list.observe_controllers();
        let built_in: Vec<gtk::DropControllerMotion> = (0..controllers.n_items()).filter_map(|i| controllers.item(i).and_downcast()).collect();
        for controller in built_in {
            list.remove_controller(&controller);
        }
        let this = Rc::new(Self { list: list.clone(), scrolled: scrolled.clone(), speed: Cell::new(0.0), ticking: Cell::new(false) });
        let motion = gtk::DropControllerMotion::new();
        let weak = Rc::downgrade(&this);
        motion.connect_motion(move |_, _, y| {
            if let Some(this) = weak.upgrade() {
                this.set_speed(speed_at(y, f64::from(this.scrolled.height())));
            }
        });
        let weak = Rc::downgrade(&this);
        motion.connect_leave(move |_| {
            if let Some(this) = weak.upgrade() {
                this.stop();
            }
        });
        scrolled.add_controller(motion);
        this
    }

    /// Call when a drag ends, however it ended.
    pub fn stop(&self) {
        self.speed.set(0.0);
    }

    fn set_speed(self: &Rc<Self>, speed: f64) {
        self.speed.set(speed);
        if speed == 0.0 || self.ticking.replace(true) {
            return;
        }
        let weak = Rc::downgrade(self);
        let last = Cell::new(None::<i64>);
        self.list.add_tick_callback(move |_, clock| {
            let Some(this) = weak.upgrade() else { return glib::ControlFlow::Break };
            let speed = this.speed.get();
            if speed == 0.0 {
                this.ticking.set(false);
                return glib::ControlFlow::Break;
            }
            let now = clock.frame_time();
            if let Some(before) = last.replace(Some(now)) {
                let adj = this.scrolled.vadjustment();
                adj.set_value(adj.value() + speed * (now - before) as f64 / 1_000_000.0);
            }
            glib::ControlFlow::Continue
        });
    }
}

/// How fast a drag at height `y` scrolls a list `height` tall, up is negative.
/// Nothing away from the edges, faster the closer the pointer gets.
fn speed_at(y: f64, height: f64) -> f64 {
    let zone = ZONE.min(height / 3.0);
    if zone <= 0.0 {
        0.0
    } else if y < zone {
        -MAX_SPEED * (1.0 - y.max(0.0) / zone)
    } else if y > height - zone {
        MAX_SPEED * (1.0 - (height - y).max(0.0) / zone)
    } else {
        0.0
    }
}

#[cfg(test)]
mod tests {
    use super::speed_at as speed;

    #[test]
    fn a_drag_scrolls_only_near_the_edges() {
        assert_eq!(speed(300.0, 600.0), 0.0);
        assert!(speed(0.0, 600.0) < speed(20.0, 600.0) && speed(20.0, 600.0) < 0.0);
        assert!(speed(600.0, 600.0) > speed(580.0, 600.0) && speed(580.0, 600.0) > 0.0);
        assert_eq!(speed(10.0, 0.0), 0.0);
    }
}

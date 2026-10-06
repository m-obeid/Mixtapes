//! Port of ui/widgets/fade_bottom_bin.py: a box that fades its content to
//! alpha zero towards the bottom through a mask node in its snapshot. Used
//! by the artist banner in blur mode, where the scrim cannot go opaque.
//!
//! The child hangs from the top edge at the height its width asks for, so a
//! picture taller than the bin loses its bottom, under the fade, and keeps its top.

use std::cell::Cell;

use gtk::{gdk, glib, graphene, gsk, prelude::*, subclass::prelude::*};

mod imp {
    use super::*;

    pub struct FadeBottomBin {
        pub fade_active: Cell<bool>,
        /// Fraction of the height where the fade starts; above it alpha is one.
        pub fade_start: Cell<f64>,
    }

    impl Default for FadeBottomBin {
        fn default() -> Self {
            Self { fade_active: Cell::new(false), fade_start: Cell::new(0.55) }
        }
    }

    #[glib::object_subclass]
    impl ObjectSubclass for FadeBottomBin {
        const NAME: &'static str = "MixtapesFadeBottomBin";
        type Type = super::FadeBottomBin;
        type ParentType = gtk::Box;
    }

    impl ObjectImpl for FadeBottomBin {}

    impl WidgetImpl for FadeBottomBin {
        fn size_allocate(&self, width: i32, height: i32, _baseline: i32) {
            let Some(child) = self.obj().first_child() else { return };
            let (_, natural, _, _) = child.measure(gtk::Orientation::Vertical, width);
            child.allocate(width, natural.max(height), -1, None);
        }

        fn snapshot(&self, snapshot: &gtk::Snapshot) {
            let widget = self.obj();
            let (w, h) = (widget.width() as f32, widget.height() as f32);
            if !self.fade_active.get() || w <= 0.0 || h <= 0.0 {
                self.parent_snapshot(snapshot);
                return;
            }
            // The mask source comes first, then the content, popped once each.
            snapshot.push_mask(gsk::MaskMode::Alpha);
            let opaque = gdk::RGBA::new(0.0, 0.0, 0.0, 1.0);
            let clear = gdk::RGBA::new(0.0, 0.0, 0.0, 0.0);
            let stops = [gsk::ColorStop::new(0.0, opaque), gsk::ColorStop::new(self.fade_start.get() as f32, opaque), gsk::ColorStop::new(1.0, clear)];
            snapshot.append_linear_gradient(&graphene::Rect::new(0.0, 0.0, w, h), &graphene::Point::new(0.0, 0.0), &graphene::Point::new(0.0, h), &stops);
            snapshot.pop();
            self.parent_snapshot(snapshot);
            snapshot.pop();
        }
    }

    impl BoxImpl for FadeBottomBin {}
}

glib::wrapper! {
    pub struct FadeBottomBin(ObjectSubclass<imp::FadeBottomBin>) @extends gtk::Box, gtk::Widget, @implements gtk::Accessible, gtk::Buildable, gtk::ConstraintTarget, gtk::Orientable;
}

impl Default for FadeBottomBin {
    fn default() -> Self {
        Self::new(0.55)
    }
}

impl FadeBottomBin {
    pub fn new(fade_start: f64) -> Self {
        let bin: Self = glib::Object::new();
        bin.imp().fade_start.set(fade_start.clamp(0.0, 1.0));
        bin
    }

    pub fn set_fade_active(&self, active: bool) {
        if self.imp().fade_active.replace(active) != active {
            self.queue_draw();
        }
    }
}

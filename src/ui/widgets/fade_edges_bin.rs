//! Port of ui/widgets/fade_edges_bin.py: a box whose content fades out at the
//! top and bottom edges through a mask node in its snapshot. The lyrics column
//! uses it so scrolling lines dissolve instead of cropping at the chrome.

use std::cell::Cell;

use gtk::{gdk, glib, graphene, gsk, prelude::*, subclass::prelude::*};

mod imp {
    use super::*;

    #[derive(Default)]
    pub struct FadeEdgesBin {
        pub fade_top: Cell<f32>,
        pub fade_bottom: Cell<f32>,
    }

    #[glib::object_subclass]
    impl ObjectSubclass for FadeEdgesBin {
        const NAME: &'static str = "MixtapesFadeEdgesBin";
        type Type = super::FadeEdgesBin;
        type ParentType = gtk::Box;
    }

    impl ObjectImpl for FadeEdgesBin {}

    impl WidgetImpl for FadeEdgesBin {
        fn snapshot(&self, snapshot: &gtk::Snapshot) {
            let widget = self.obj();
            let (w, h) = (widget.width() as f32, widget.height() as f32);
            let (top, bottom) = (self.fade_top.get(), self.fade_bottom.get());
            // Content shorter than both bands would be dimmed everywhere, so it goes unfaded.
            if w <= 0.0 || h <= 0.0 || (top <= 0.0 && bottom <= 0.0) || h <= top + bottom + 1.0 {
                self.parent_snapshot(snapshot);
                return;
            }
            snapshot.push_mask(gsk::MaskMode::Alpha);
            let opaque = gdk::RGBA::new(0.0, 0.0, 0.0, 1.0);
            let clear = gdk::RGBA::new(0.0, 0.0, 0.0, 0.0);
            let stops = [
                gsk::ColorStop::new(0.0, if top > 0.0 { clear } else { opaque }),
                gsk::ColorStop::new(top / h, opaque),
                gsk::ColorStop::new(1.0 - bottom / h, opaque),
                gsk::ColorStop::new(1.0, if bottom > 0.0 { clear } else { opaque }),
            ];
            snapshot.append_linear_gradient(&graphene::Rect::new(0.0, 0.0, w, h), &graphene::Point::new(0.0, 0.0), &graphene::Point::new(0.0, h), &stops);
            snapshot.pop();
            self.parent_snapshot(snapshot);
            snapshot.pop();
        }
    }

    impl BoxImpl for FadeEdgesBin {}
}

glib::wrapper! {
    pub struct FadeEdgesBin(ObjectSubclass<imp::FadeEdgesBin>) @extends gtk::Box, gtk::Widget, @implements gtk::Accessible, gtk::Buildable, gtk::ConstraintTarget, gtk::Orientable;
}

impl FadeEdgesBin {
    /// Fade band sizes in CSS pixels, per edge.
    pub fn new(fade_top_px: f32, fade_bottom_px: f32) -> Self {
        let bin: Self = glib::Object::new();
        bin.imp().fade_top.set(fade_top_px.max(0.0));
        bin.imp().fade_bottom.set(fade_bottom_px.max(0.0));
        bin
    }
}

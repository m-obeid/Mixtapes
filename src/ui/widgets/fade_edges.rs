//! A box that fades its child out at the left and right edges through a mask
//! node, so a row running past the window ends softly. A side without more
//! content past it stays sharp.

use std::cell::Cell;

use gtk::{gdk, glib, graphene, gsk, prelude::*, subclass::prelude::*};

/// How wide the fade is, in pixels.
const FADE: f32 = 40.0;

mod imp {
    use super::*;

    #[derive(Default)]
    pub struct FadeEdges {
        pub left: Cell<bool>,
        pub right: Cell<bool>,
    }

    #[glib::object_subclass]
    impl ObjectSubclass for FadeEdges {
        const NAME: &'static str = "MixtapesFadeEdges";
        type Type = super::FadeEdges;
        type ParentType = gtk::Box;
    }

    impl ObjectImpl for FadeEdges {}

    impl WidgetImpl for FadeEdges {
        fn snapshot(&self, snapshot: &gtk::Snapshot) {
            let widget = self.obj();
            let (w, h) = (widget.width() as f32, widget.height() as f32);
            let (left, right) = (self.left.get(), self.right.get());
            if (!left && !right) || w <= FADE * 2.0 || h <= 0.0 {
                self.parent_snapshot(snapshot);
                return;
            }
            let color = |alpha: f32| gdk::RGBA::new(0.0, 0.0, 0.0, alpha);
            let edge = FADE / w;
            let stops = [
                gsk::ColorStop::new(0.0, color(if left { 0.0 } else { 1.0 })),
                gsk::ColorStop::new(edge, color(1.0)),
                gsk::ColorStop::new(1.0 - edge, color(1.0)),
                gsk::ColorStop::new(1.0, color(if right { 0.0 } else { 1.0 })),
            ];
            // The mask source comes first, then the content, popped once each.
            snapshot.push_mask(gsk::MaskMode::Alpha);
            snapshot.append_linear_gradient(&graphene::Rect::new(0.0, 0.0, w, h), &graphene::Point::new(0.0, 0.0), &graphene::Point::new(w, 0.0), &stops);
            snapshot.pop();
            self.parent_snapshot(snapshot);
            snapshot.pop();
        }
    }

    impl BoxImpl for FadeEdges {}
}

glib::wrapper! {
    pub struct FadeEdges(ObjectSubclass<imp::FadeEdges>)
        @extends gtk::Box, gtk::Widget,
        @implements gtk::Accessible, gtk::Buildable, gtk::ConstraintTarget, gtk::Orientable;
}

impl FadeEdges {
    pub fn new() -> Self {
        glib::Object::new()
    }

    pub fn set_fades(&self, left: bool, right: bool) {
        let imp = imp::FadeEdges::from_obj(self);
        if imp.left.replace(left) != left || imp.right.replace(right) != right {
            self.queue_draw();
        }
    }
}

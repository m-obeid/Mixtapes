//! A one-child container that reports when it and everything inside it has been laid out.
//!
//! GTK has no signal for "allocation finished". A handler that needs the new
//! bounds of a descendant, and wants to act on them before the frame is
//! painted, has nowhere else to run: a tick callback only sees them one frame
//! later, after the stale frame went to the screen.
//!
//! A plain widget and not a box: GTK calls `size_allocate` only on a widget
//! without a layout manager, and a box has one.

use std::cell::RefCell;

use gtk::{glib, prelude::*, subclass::prelude::*};

mod imp {
    use super::*;

    #[derive(Default)]
    pub struct AllocateBin {
        pub child: RefCell<Option<gtk::Widget>>,
        pub on_allocated: RefCell<Option<Box<dyn Fn()>>>,
    }

    #[glib::object_subclass]
    impl ObjectSubclass for AllocateBin {
        const NAME: &'static str = "MixtapesAllocateBin";
        type Type = super::AllocateBin;
        type ParentType = gtk::Widget;
    }

    impl ObjectImpl for AllocateBin {
        fn dispose(&self) {
            if let Some(child) = self.child.take() {
                child.unparent();
            }
        }
    }

    impl WidgetImpl for AllocateBin {
        fn request_mode(&self) -> gtk::SizeRequestMode {
            self.child.borrow().as_ref().map_or(gtk::SizeRequestMode::ConstantSize, |c| c.request_mode())
        }

        fn measure(&self, orientation: gtk::Orientation, for_size: i32) -> (i32, i32, i32, i32) {
            self.child.borrow().as_ref().map_or((0, 0, -1, -1), |c| c.measure(orientation, for_size))
        }

        fn size_allocate(&self, width: i32, height: i32, baseline: i32) {
            if let Some(child) = self.child.borrow().as_ref() {
                child.allocate(width, height, baseline, None);
            }
            if let Some(f) = self.on_allocated.borrow().as_ref() {
                f();
            }
        }
    }
}

glib::wrapper! {
    pub struct AllocateBin(ObjectSubclass<imp::AllocateBin>) @extends gtk::Widget, @implements gtk::Accessible, gtk::Buildable, gtk::ConstraintTarget;
}

impl AllocateBin {
    pub fn new(child: &impl IsA<gtk::Widget>) -> Self {
        let bin: Self = glib::Object::new();
        child.set_parent(&bin);
        bin.imp().child.replace(Some(child.clone().upcast()));
        bin
    }

    /// Run `f` each time the children have been allocated, still inside the layout pass.
    pub fn set_on_allocated(&self, f: impl Fn() + 'static) {
        self.imp().on_allocated.replace(Some(Box::new(f)));
    }
}

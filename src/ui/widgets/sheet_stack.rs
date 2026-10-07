//! A page with a sheet that rises over it from the bottom edge, for the
//! desktop player view. The sheet follows a drag and settles with a spring,
//! the way the phone layout's bottom sheet does.
//!
//! Both children fill the widget. The sheet is shifted down by the part of it
//! that is still closed, and each child is clipped at the sheet's top edge.
//! Nothing overlaps, so neither needs a background of its own and the
//! window's blurred cover shows through both.

use std::cell::{Cell, RefCell};

use adw::prelude::*;
use gtk::{glib, graphene, gsk, subclass::prelude::*};

/// A drag on something that opens or closes the sheet, in pixels along the window's height.
#[derive(Debug, Clone, Copy)]
pub enum SheetDrag {
    /// The pointer is `dy` from where it went down. Up is negative.
    Update { dy: f64 },
    /// Let go. The sheet carries on at the speed it was moving.
    End,
}

/// Report a vertical drag on `widget` to `report`, once `accepts` agrees with the way it set off.
///
/// The drag is taken after 4px. Desktop bars are window handles, which start moving the
/// window at 8px. A sideways drag is left alone and still does that.
pub fn watch_drag(widget: &impl IsA<gtk::Widget>, accepts: impl Fn(f64) -> bool + 'static, report: impl Fn(SheetDrag) + 'static) {
    let drag = gtk::GestureDrag::builder().propagation_phase(gtk::PropagationPhase::Bubble).build();
    let active = std::rc::Rc::new(Cell::new(false));
    let report = std::rc::Rc::new(report);
    let begun = active.clone();
    drag.connect_drag_begin(move |_, _, _| begun.set(false));
    let (state, tell) = (active.clone(), report.clone());
    drag.connect_drag_update(move |gesture, dx, dy| {
        if !state.get() {
            if dy.abs() <= 4.0 || dy.abs() <= dx.abs() || !accepts(dy) {
                return;
            }
            gesture.set_state(gtk::EventSequenceState::Claimed);
            state.set(true);
        }
        tell(SheetDrag::Update { dy });
    });
    let (state, tell) = (active.clone(), report.clone());
    drag.connect_drag_end(move |_, _, _| {
        if state.replace(false) {
            tell(SheetDrag::End);
        }
    });
    drag.connect_cancel(move |_, _| {
        if active.replace(false) {
            report(SheetDrag::End);
        }
    });
    widget.add_controller(drag);
}

/// How far ahead a release looks: a flick that would carry the sheet past half way opens it.
const PROJECTION_SECS: f64 = 0.15;

mod imp {
    use super::*;

    #[derive(Default)]
    pub struct SheetStack {
        pub base: RefCell<Option<gtk::Widget>>,
        pub sheet: RefCell<Option<gtk::Widget>>,
        /// 0 with the sheet away, 1 with it covering the page.
        pub progress: Cell<f64>,
        /// Where the sheet rests or is heading.
        pub open: Cell<bool>,
        /// Progress and sheet offset when the drag in hand began.
        pub drag_start: Cell<Option<(f64, f64)>>,
        /// The sheet offset the last layout placed it with, which is what pointer events are measured against.
        pub placed_top: Cell<f64>,
        /// The two latest drag positions as (microseconds, progress), for the speed at release.
        pub drag_samples: Cell<[(i64, f64); 2]>,
        pub animation: RefCell<Option<adw::SpringAnimation>>,
        pub on_open: RefCell<Option<Box<dyn Fn(bool)>>>,
    }

    #[glib::object_subclass]
    impl ObjectSubclass for SheetStack {
        const NAME: &'static str = "MixtapesSheetStack";
        type Type = super::SheetStack;
        type ParentType = gtk::Widget;
    }

    impl ObjectImpl for SheetStack {
        fn dispose(&self) {
            self.animation.take();
            for child in [self.base.take(), self.sheet.take()].into_iter().flatten() {
                child.unparent();
            }
        }
    }

    impl SheetStack {
        /// How far the sheet's top edge sits below the widget's.
        pub fn sheet_top(&self) -> f64 {
            ((1.0 - self.progress.get()) * f64::from(self.obj().height())).round()
        }
    }

    impl WidgetImpl for SheetStack {
        fn request_mode(&self) -> gtk::SizeRequestMode {
            self.base.borrow().as_ref().map_or(gtk::SizeRequestMode::ConstantSize, |c| c.request_mode())
        }

        fn measure(&self, orientation: gtk::Orientation, for_size: i32) -> (i32, i32, i32, i32) {
            let (mut minimum, mut natural) = (0, 0);
            for child in [self.base.borrow().as_ref(), self.sheet.borrow().as_ref()].into_iter().flatten() {
                let (min, nat, _, _) = child.measure(orientation, for_size);
                (minimum, natural) = (minimum.max(min), natural.max(nat));
            }
            (minimum, natural, -1, -1)
        }

        fn size_allocate(&self, width: i32, height: i32, baseline: i32) {
            if let Some(base) = self.base.borrow().as_ref().filter(|c| c.is_child_visible()) {
                base.allocate(width, height, baseline, None);
            }
            if let Some(sheet) = self.sheet.borrow().as_ref().filter(|c| c.is_child_visible()) {
                let top = ((1.0 - self.progress.get()) * f64::from(height)).round() as f32;
                self.placed_top.set(f64::from(top));
                sheet.allocate(width, height, baseline, Some(gsk::Transform::new().translate(&graphene::Point::new(0.0, top))));
            }
        }

        fn snapshot(&self, snapshot: &gtk::Snapshot) {
            let widget = self.obj();
            let (width, height) = (widget.width() as f32, widget.height() as f32);
            let top = self.sheet_top() as f32;
            if let Some(base) = self.base.borrow().as_ref().filter(|c| c.is_child_visible()) {
                snapshot.push_clip(&graphene::Rect::new(0.0, 0.0, width, top));
                widget.snapshot_child(base, snapshot);
                snapshot.pop();
            }
            if let Some(sheet) = self.sheet.borrow().as_ref().filter(|c| c.is_child_visible()) {
                snapshot.push_clip(&graphene::Rect::new(0.0, top, width, height - top));
                widget.snapshot_child(sheet, snapshot);
                snapshot.pop();
            }
        }
    }
}

glib::wrapper! {
    pub struct SheetStack(ObjectSubclass<imp::SheetStack>) @extends gtk::Widget, @implements gtk::Accessible, gtk::Buildable, gtk::ConstraintTarget;
}

impl SheetStack {
    pub fn new(base: &impl IsA<gtk::Widget>, sheet: &impl IsA<gtk::Widget>) -> Self {
        let stack: Self = glib::Object::new();
        base.set_parent(&stack);
        sheet.set_parent(&stack);
        sheet.set_child_visible(false);
        stack.imp().base.replace(Some(base.clone().upcast()));
        stack.imp().sheet.replace(Some(sheet.clone().upcast()));
        stack
    }

    /// Whether the sheet is open or on its way there.
    pub fn is_open(&self) -> bool {
        self.imp().open.get()
    }

    /// Called with the new state whenever the sheet starts opening or closing.
    pub fn connect_open_changed(&self, f: impl Fn(bool) + 'static) {
        self.imp().on_open.replace(Some(Box::new(f)));
    }

    /// Open or close. Animated, the sheet springs there from where it is.
    pub fn set_open(&self, open: bool, animate: bool) {
        self.settle(open, if animate { Some(0.0) } else { None });
    }

    /// Follow a drag, or let go of one.
    ///
    /// `on_sheet` says the drag is on the sheet itself, which moves under the
    /// pointer: its own coordinates then leave out how far it has travelled.
    pub fn drag(&self, drag: SheetDrag, on_sheet: bool) {
        let imp = self.imp();
        let height = f64::from(self.height()).max(1.0);
        match drag {
            SheetDrag::Update { dy } => {
                let (start, start_top) = match imp.drag_start.get() {
                    Some(start) => start,
                    None => {
                        if let Some(animation) = imp.animation.take() {
                            animation.pause();
                        }
                        let start = (imp.progress.get(), imp.placed_top.get());
                        imp.drag_start.set(Some(start));
                        imp.drag_samples.set([(glib::monotonic_time(), start.0); 2]);
                        start
                    }
                };
                let travelled = if on_sheet { imp.placed_top.get() - start_top } else { 0.0 };
                self.set_progress((start - (dy + travelled) / height).clamp(0.0, 1.0));
                // The older sample is kept until it is 30 ms old, so one late event does not set the speed.
                let [older, newer] = imp.drag_samples.get();
                let now = (glib::monotonic_time(), imp.progress.get());
                imp.drag_samples.set(if now.0 - older.0 >= 30_000 { [newer, now] } else { [older, now] });
            }
            SheetDrag::End => {
                if imp.drag_start.take().is_none() {
                    return;
                }
                let [older, newer] = imp.drag_samples.get();
                let elapsed = (newer.0 - older.0) as f64 / 1e6;
                // A pointer that stopped before it let go carries no speed.
                let moving = elapsed > 0.0 && glib::monotonic_time() - newer.0 < 100_000;
                let per_second = if moving { (newer.1 - older.1) / elapsed } else { 0.0 };
                let open = imp.progress.get() + per_second * PROJECTION_SECS > 0.5;
                self.settle(open, Some(per_second));
            }
        }
    }

    /// Head for open or closed: with a spring starting at `velocity` progress a second, or at once.
    fn settle(&self, open: bool, velocity: Option<f64>) {
        let imp = self.imp();
        if let Some(animation) = imp.animation.take() {
            animation.pause();
        }
        imp.drag_start.set(None);
        let changed = imp.open.replace(open) != open;
        let target = f64::from(u8::from(open));
        match velocity.filter(|_| self.is_mapped() && (imp.progress.get() - target).abs() > 0.001) {
            Some(velocity) => {
                let weak = self.downgrade();
                let callback = adw::CallbackAnimationTarget::new(move |value| {
                    if let Some(stack) = weak.upgrade() {
                        stack.set_progress(value.clamp(0.0, 1.0));
                    }
                });
                let animation = adw::SpringAnimation::builder()
                    .widget(self)
                    .value_from(imp.progress.get())
                    .value_to(target)
                    .spring_params(&adw::SpringParams::new(1.0, 1.0, 500.0))
                    .initial_velocity(velocity)
                    .epsilon(0.0005)
                    .target(&callback)
                    .build();
                animation.play();
                imp.animation.replace(Some(animation));
            }
            None => self.set_progress(target),
        }
        if changed {
            if let Some(f) = imp.on_open.borrow().as_ref() {
                f(open);
            }
        }
    }

    fn set_progress(&self, progress: f64) {
        let imp = self.imp();
        imp.progress.set(progress);
        // A child fully out of sight is not laid out, drawn or styled.
        if let Some(base) = imp.base.borrow().as_ref() {
            base.set_child_visible(progress < 1.0);
        }
        if let Some(sheet) = imp.sheet.borrow().as_ref() {
            sheet.set_child_visible(progress > 0.0);
        }
        self.queue_allocate();
        self.queue_draw();
    }
}

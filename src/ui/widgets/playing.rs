//! Keeps the `playing` CSS class on whichever registered widget shows the
//! current track. One state subscription per page instead of one per card.

use std::cell::RefCell;
use std::rc::Rc;

use gtk::{glib, prelude::*};

use crate::state::PlayerState;

pub struct PlayingTracker {
    entries: RefCell<Vec<(glib::WeakRef<gtk::Widget>, String)>>,
    state: PlayerState,
}

impl PlayingTracker {
    pub fn new(state: &PlayerState) -> Rc<Self> {
        let tracker = Rc::new(Self { entries: RefCell::new(Vec::new()), state: state.clone() });
        let weak = Rc::downgrade(&tracker);
        state.connect_notify_local(Some("video-id"), move |_, _| {
            if let Some(t) = weak.upgrade() {
                t.refresh();
            }
        });
        tracker
    }

    /// Forget every widget, before a page rebuilds its content.
    pub fn clear(&self) {
        self.entries.borrow_mut().clear();
    }

    pub fn track(&self, widget: &impl IsA<gtk::Widget>, video_id: &str) {
        if video_id.is_empty() {
            return;
        }
        let widget = widget.upcast_ref::<gtk::Widget>();
        apply(widget, self.state.is_playing_id(video_id));
        self.entries.borrow_mut().push((widget.downgrade(), video_id.to_owned()));
    }

    fn refresh(&self) {
        self.entries.borrow_mut().retain(|(w, _)| w.upgrade().is_some());
        for (weak, id) in self.entries.borrow().iter() {
            if let Some(widget) = weak.upgrade() {
                apply(&widget, self.state.is_playing_id(id));
            }
        }
    }
}

fn apply(widget: &gtk::Widget, playing: bool) {
    if playing {
        widget.add_css_class("playing");
        widget.remove_css_class("flat");
    } else {
        widget.remove_css_class("playing");
        widget.add_css_class("flat");
    }
}

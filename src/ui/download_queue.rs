//! The download queue behind the header pie.
//!
//! Port of the popover half of window.py: one row per queued track with its
//! own progress bar and a cancel button, the pie following the queue, and the
//! whole list clearing itself a few seconds after the last download lands.

use std::cell::RefCell;
use std::collections::HashMap;
use std::rc::Rc;
use std::time::Duration;

use gtk::prelude::*;

use crate::downloads::Event;
use crate::model::Track;
use crate::ui::context::UiContext;

/// How long finished rows stay up before the popover empties itself.
const LINGER: Duration = Duration::from_secs(5);

struct Row {
    widget: gtk::Box,
    status: gtk::Label,
    bar: gtk::ProgressBar,
    cancel: gtk::Button,
}

pub struct DownloadQueue {
    ctx: Rc<UiContext>,
    items: gtk::Box,
    rows: RefCell<HashMap<String, Row>>,
    /// Set while a clear is pending, so a new download cancels it.
    clearing: RefCell<Option<glib::SourceId>>,
}

impl DownloadQueue {
    /// Wire the popover box to the manager's events.
    pub fn new(ctx: Rc<UiContext>, items: gtk::Box) -> Rc<Self> {
        let queue = Rc::new(Self { ctx: ctx.clone(), items, rows: RefCell::new(HashMap::new()), clearing: RefCell::new(None) });
        let weak = Rc::downgrade(&queue);
        ctx.on_download(move |event| match weak.upgrade() {
            Some(queue) => {
                queue.apply(event);
                true
            }
            None => false,
        });
        queue
    }

    /// Queue tracks and show them in the popover. This is what every Download
    /// action in the app calls.
    pub fn start(self: &Rc<Self>, tracks: Vec<Track>, album_title: &str, album_id: &str) -> usize {
        let pending: Vec<Track> = tracks.into_iter().filter(|t| !t.video_id.0.is_empty() && !self.ctx.downloads.is_downloaded(&t.video_id.0)).collect();
        if pending.is_empty() {
            return 0;
        }
        if let Some(id) = self.clearing.borrow_mut().take() {
            id.remove();
        }
        for track in &pending {
            self.add_row(&track.video_id.0, &track.title);
        }
        if !album_title.is_empty() {
            self.ctx.downloads.register_playlist(album_id, album_title, pending.clone());
        }
        let count = pending.len();
        self.ctx.downloads.queue_tracks(pending, album_title, album_id);
        count
    }

    fn add_row(self: &Rc<Self>, video_id: &str, title: &str) {
        if self.rows.borrow().contains_key(video_id) {
            return;
        }
        let widget = gtk::Box::builder().orientation(gtk::Orientation::Horizontal).spacing(8).build();
        let info = gtk::Box::builder().orientation(gtk::Orientation::Vertical).spacing(2).hexpand(true).margin_top(4).margin_bottom(4).build();
        let name = gtk::Label::builder().label(title).halign(gtk::Align::Start).ellipsize(gtk::pango::EllipsizeMode::End).css_classes(["caption"]).build();
        let status = gtk::Label::builder().label("Queued").halign(gtk::Align::Start).css_classes(["caption", "dim-label"]).build();
        let bar = gtk::ProgressBar::builder().visible(false).build();
        info.append(&name);
        info.append(&status);
        info.append(&bar);
        widget.append(&info);

        let cancel = gtk::Button::builder().icon_name("window-close-symbolic").valign(gtk::Align::Center).css_classes(["flat", "circular"]).tooltip_text("Cancel").build();
        let this = self.clone();
        let id = video_id.to_owned();
        cancel.connect_clicked(move |_| {
            this.ctx.downloads.cancel(&id);
        });
        widget.append(&cancel);

        self.items.append(&widget);
        self.rows.borrow_mut().insert(video_id.to_owned(), Row { widget, status, bar, cancel });
        tracing::debug!(video_id, rows = self.rows.borrow().len(), "download row");
    }

    fn apply(self: &Rc<Self>, event: &Event) {
        match event {
            Event::Queued { video_id } => {
                if !self.rows.borrow().contains_key(video_id) {
                    self.add_row(video_id, "Downloading");
                }
            }
            Event::Progress { video_id, fraction } => {
                if let Some(row) = self.rows.borrow().get(video_id) {
                    row.bar.set_visible(true);
                    row.bar.set_fraction(*fraction);
                    row.status.set_label(&format!("{}%", (fraction * 100.0) as u32));
                    row.cancel.set_visible(false);
                }
            }
            Event::Item { video_id, ok, message } => {
                if let Some(row) = self.rows.borrow().get(video_id) {
                    row.status.set_label(match (ok, message.as_str()) {
                        (true, _) => "Done",
                        (false, "Cancelled") => "Cancelled",
                        (false, _) => "Failed",
                    });
                    if *ok {
                        row.bar.set_fraction(1.0);
                    }
                    row.bar.set_visible(false);
                    row.cancel.set_visible(false);
                }
            }
            Event::Advanced { .. } | Event::Idle { .. } | Event::Removed { .. } => {}
        }
    }

    /// Empty the popover and hide the pie, a few seconds after the queue drains.
    pub fn clear_later(self: &Rc<Self>) {
        if self.clearing.borrow().is_some() {
            return;
        }
        let this = self.clone();
        let id = glib::timeout_add_local_once(LINGER, move || {
            this.clearing.borrow_mut().take();
            for (_, row) in this.rows.borrow_mut().drain() {
                this.items.remove(&row.widget);
            }
        });
        self.clearing.replace(Some(id));
    }
}

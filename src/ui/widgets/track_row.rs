//! Port of the ListView row PlaylistPage builds in _setup_list_item: a flat
//! button holding cover, title with badges, artist line and duration, with
//! the like button overlaid on the right. Lazily created children (check
//! box, track number, explicit badge, download icon) appear on first use.

use std::cell::RefCell;
use std::rc::{Rc, Weak};

use adw::prelude::*;
use gtk::{gdk, glib};

use crate::model::{Track, VideoId};
use crate::ui::context::UiContext;
use crate::ui::cover::CoverImage;
use crate::ui::like_button::LikeButton;

const THUMB_SIZE: i32 = 56;

/// What a row asks its page for. Weak so rows never keep a page alive.
pub trait TrackRowHost {
    fn multi_select(&self) -> bool;
    fn is_selected(&self, video_id: &str) -> bool;
    fn toggle_selection(&self, video_id: &str, row: Option<&Rc<TrackRow>>);
    fn row_clicked(&self, row: &Rc<TrackRow>);
    fn row_menu(&self, row: &Rc<TrackRow>, x: f64, y: f64);
    fn is_album_view(&self) -> bool;
    fn page_cover(&self) -> Option<String>;
}

pub struct TrackRow {
    bin: adw::Bin,
    overlay: gtk::Overlay,
    button: gtk::Button,
    inner: gtk::Box,
    img: Rc<CoverImage>,
    title: gtk::Label,
    title_box: gtk::Box,
    subtitle: gtk::Label,
    duration: gtk::Label,
    like: Rc<LikeButton>,
    check: RefCell<Option<gtk::CheckButton>>,
    check_handler: RefCell<Option<glib::SignalHandlerId>>,
    track_num: RefCell<Option<gtk::Label>>,
    explicit: RefCell<Option<gtk::Label>>,
    dl_icon: RefCell<Option<gtk::Image>>,
    track: RefCell<Option<Track>>,
    state_handler: RefCell<Option<glib::SignalHandlerId>>,
    host: Weak<dyn TrackRowHost>,
    ctx: Rc<UiContext>,
}

impl TrackRow {
    pub fn new(ctx: &Rc<UiContext>, host: Weak<dyn TrackRowHost>) -> Rc<Self> {
        let bin = adw::Bin::builder().css_classes(["list-item-bin"]).build();
        let overlay = gtk::Overlay::builder().hexpand(true).build();
        // The row is its own button, so .activatable would double the hover highlight.
        let button = gtk::Button::builder()
            .css_classes(["song-row", "song-row-button", "flat"])
            .hexpand(true)
            .focus_on_click(false)
            .build();
        overlay.set_child(Some(&button));
        let inner = gtk::Box::builder()
            .orientation(gtk::Orientation::Horizontal)
            .spacing(12)
            .hexpand(true)
            .can_target(true)
            .build();
        button.set_child(Some(&inner));

        let img = CoverImage::in_context(ctx, THUMB_SIZE);
        img.widget().add_css_class("song-img");
        img.widget().set_can_target(false);
        inner.append(img.widget());

        let vbox = gtk::Box::builder()
            .orientation(gtk::Orientation::Vertical)
            .spacing(2)
            .valign(gtk::Align::Center)
            .hexpand(true)
            .can_target(false)
            .build();
        let title = gtk::Label::builder()
            .halign(gtk::Align::Start)
            .ellipsize(gtk::pango::EllipsizeMode::End)
            .lines(1)
            .hexpand(false)
            .xalign(0.0)
            .width_chars(1)
            .can_target(false)
            .build();
        let subtitle = gtk::Label::builder()
            .halign(gtk::Align::Start)
            .ellipsize(gtk::pango::EllipsizeMode::End)
            .lines(1)
            .hexpand(true)
            .xalign(0.0)
            .width_chars(1)
            .css_classes(["dim-label", "caption"])
            .can_target(false)
            .build();
        let title_box = gtk::Box::builder()
            .orientation(gtk::Orientation::Horizontal)
            .spacing(6)
            .hexpand(true)
            .can_target(false)
            .build();
        title_box.append(&title);
        title_box.append(&gtk::Box::builder().hexpand(true).can_target(false).build());
        vbox.append(&title_box);
        vbox.append(&subtitle);
        inner.append(&vbox);

        let duration = gtk::Label::builder()
            .css_classes(["caption"])
            .valign(gtk::Align::Center)
            .margin_end(44)
            .can_target(false)
            .build();
        inner.append(&duration);

        let like = LikeButton::new(ctx.player.clone());
        like.widget().set_valign(gtk::Align::Center);
        like.widget().set_halign(gtk::Align::End);
        like.widget().set_margin_end(10);
        like.widget().set_can_target(true);
        overlay.add_overlay(like.widget());
        bin.set_child(Some(&overlay));

        let row = Rc::new(Self {
            bin,
            overlay,
            button,
            inner,
            img,
            title,
            title_box,
            subtitle,
            duration,
            like,
            check: RefCell::new(None),
            check_handler: RefCell::new(None),
            track_num: RefCell::new(None),
            explicit: RefCell::new(None),
            dl_icon: RefCell::new(None),
            track: RefCell::new(None),
            state_handler: RefCell::new(None),
            host,
            ctx: ctx.clone(),
        });
        // Tree walks (compact mode, selection refresh) find the row through its bin.
        unsafe { row.bin.set_data("track-row", Rc::downgrade(&row)) };

        let weak = Rc::downgrade(&row);
        row.button.connect_clicked(move |_| {
            if let Some(row) = weak.upgrade() {
                if let Some(host) = row.host.upgrade() {
                    host.row_clicked(&row);
                }
            }
        });
        let open_menu = {
            let weak = Rc::downgrade(&row);
            Rc::new(move |x: f64, y: f64| {
                if let Some(row) = weak.upgrade() {
                    if let Some(host) = row.host.upgrade() {
                        host.row_menu(&row, x, y);
                    }
                }
            })
        };
        let right = gtk::GestureClick::builder()
            .button(gdk::BUTTON_SECONDARY)
            .build();
        let open = open_menu.clone();
        right.connect_released(move |_, _, x, y| open(x, y));
        row.button.add_controller(right);
        let long = gtk::GestureLongPress::new();
        long.connect_pressed(move |_, x, y| open_menu(x, y));
        row.button.add_controller(long);
        row
    }

    /// The list item's child. Holds the track UI, or the header container for the header item.
    pub fn bin(&self) -> &adw::Bin {
        &self.bin
    }

    /// The button the context menu anchors to.
    pub fn widget(&self) -> &gtk::Button {
        &self.button
    }

    pub fn track(&self) -> Option<Track> {
        self.track.borrow().clone()
    }

    pub fn video_id(&self) -> Option<String> {
        self.track
            .borrow()
            .as_ref()
            .map(|t| t.video_id.0.clone())
            .filter(|v| !v.is_empty())
    }

    /// Recover the row from a bin found by walking the list view.
    pub fn from_bin(bin: &adw::Bin) -> Option<Rc<Self>> {
        unsafe { bin.data::<Weak<TrackRow>>("track-row") }
            .and_then(|p| unsafe { p.as_ref() }.upgrade())
    }

    pub fn bind_header(&self, header: &gtk::Widget) {
        self.bin.set_child(Some(header));
    }

    // -- lazy children ----------------------------------------------------

    fn ensure_track_num(&self) -> gtk::Label {
        if let Some(l) = self.track_num.borrow().as_ref() {
            return l.clone();
        }
        let label = gtk::Label::builder()
            .css_classes(["dim-label", "caption", "song-index"])
            .valign(gtk::Align::Center)
            .halign(gtk::Align::Center)
            .build();
        self.inner
            .insert_child_after(&label, Some(self.img.widget()));
        self.track_num.replace(Some(label.clone()));
        label
    }

    fn ensure_check(&self) -> gtk::CheckButton {
        if let Some(c) = self.check.borrow().as_ref() {
            return c.clone();
        }
        let check = gtk::CheckButton::builder()
            .valign(gtk::Align::Center)
            .build();
        // Claim the press so the row button under it does not also fire.
        let gesture = gtk::GestureClick::new();
        gesture.set_propagation_phase(gtk::PropagationPhase::Capture);
        gesture.connect_pressed(|g, _, _, _| {
            g.set_state(gtk::EventSequenceState::Claimed);
        });
        check.add_controller(gesture);
        self.inner.insert_child_after(&check, gtk::Widget::NONE);
        self.check.replace(Some(check.clone()));
        check
    }

    fn ensure_explicit(&self) -> gtk::Label {
        if let Some(b) = self.explicit.borrow().as_ref() {
            return b.clone();
        }
        let badge = gtk::Label::builder()
            .label("E")
            .css_classes(["explicit-badge"])
            .valign(gtk::Align::Center)
            .build();
        self.title_box.insert_child_after(&badge, Some(&self.title));
        self.explicit.replace(Some(badge.clone()));
        badge
    }

    #[allow(dead_code)]
    /// The cover kept beside a downloaded file, extracting it the first time.
    fn local_cover(self: &Rc<Self>, video_id: &str) -> Option<String> {
        if video_id.is_empty() || !self.ctx.downloads.is_downloaded(video_id) {
            return None;
        }
        if let Some(path) = self.ctx.downloads.cached_cover(video_id) {
            return Some(path.to_string_lossy().into_owned());
        }
        let downloads = self.ctx.downloads.clone();
        let id = video_id.to_owned();
        let weak = Rc::downgrade(self);
        let handle = self.ctx.net.spawn(async move { tokio::task::spawn_blocking(move || downloads.extract_cover(&id)).await.ok().flatten() });
        let wanted = video_id.to_owned();
        glib::spawn_future_local(async move {
            let Ok(Some(path)) = handle.await else { return };
            let Some(row) = weak.upgrade() else { return };
            if row.track().map(|t| t.video_id.0) == Some(wanted) {
                row.img.load(&path.to_string_lossy());
            }
        });
        None
    }

    /// The badge at the end of the row: downloaded, waiting in the queue, or
    /// nothing at all.
    pub fn show_download_state(&self, video_id: &str) {
        let downloads = &self.ctx.downloads;
        let (icon_name, queued) = match video_id {
            "" => (None, false),
            id if downloads.is_downloaded(id) => (Some("folder-download-symbolic"), false),
            id if downloads.is_queued(id) => (Some("content-loading-symbolic"), true),
            _ => (None, false),
        };
        let Some(name) = icon_name else {
            if let Some(icon) = self.dl_icon.borrow().as_ref() {
                icon.remove_css_class("queued-icon");
                icon.set_visible(false);
            }
            return;
        };
        let icon = self.ensure_dl_icon();
        icon.set_icon_name(Some(name));
        if queued {
            icon.add_css_class("queued-icon");
        } else {
            icon.remove_css_class("queued-icon");
        }
        icon.set_visible(true);
    }

    fn ensure_dl_icon(&self) -> gtk::Image {
        if let Some(i) = self.dl_icon.borrow().as_ref() {
            return i.clone();
        }
        let icon = gtk::Image::builder()
            .icon_name("folder-download-symbolic")
            .pixel_size(14)
            .css_classes(["dim-label"])
            .valign(gtk::Align::Center)
            .build();
        let anchor: gtk::Widget = self
            .explicit
            .borrow()
            .as_ref()
            .map(|b| b.clone().upcast())
            .unwrap_or_else(|| self.title.clone().upcast());
        self.title_box.insert_child_after(&icon, Some(&anchor));
        self.dl_icon.replace(Some(icon.clone()));
        icon
    }

    // -- bind / unbind ----------------------------------------------------

    /// Port of _bind_list_item. `position` is the row's place in the flattened
    /// model, where 0 is the header, so it doubles as the album track number.
    pub fn bind(self: &Rc<Self>, track: &Track, position: u32) {
        self.bin.set_child(Some(&self.overlay));
        let Some(host) = self.host.upgrade() else {
            return;
        };
        self.track.replace(Some(track.clone()));
        let video_id = track.video_id.0.clone();
        let has_id = !video_id.is_empty();
        let multi = host.multi_select();
        let selected = has_id && host.is_selected(&video_id);

        if multi {
            let check = self.ensure_check();
            check.set_visible(true);
            self.disconnect_check();
            check.set_active(selected);
            let weak = Rc::downgrade(self);
            let vid = video_id.clone();
            let id = check.connect_toggled(move |_| {
                if let Some(row) = weak.upgrade() {
                    if let Some(host) = row.host.upgrade() {
                        host.toggle_selection(&vid, Some(&row));
                    }
                }
            });
            self.check_handler.replace(Some(id));
        } else if let Some(check) = self.check.borrow().as_ref() {
            check.set_visible(false);
        }
        if !multi {
            self.disconnect_check();
        }
        self.apply_selection(selected && multi);

        self.title.set_label(&track.title);
        self.subtitle.set_label(&track.artist);

        let thumb_url = track.thumb.clone().or_else(|| host.page_cover());
        if host.is_album_view() {
            let num = self.ensure_track_num();
            num.set_label(&position.max(1).to_string());
            num.set_visible(true);
            self.img.widget().set_visible(false);
        } else {
            if let Some(num) = self.track_num.borrow().as_ref() {
                num.set_visible(false);
            }
            self.img.widget().set_visible(true);
            // A downloaded track carries its own cover, which is what offline
            // rows render. Extraction only happens for files this app did not
            // download itself.
            match self.local_cover(&video_id).or_else(|| thumb_url.clone()) {
                Some(url) => self.img.load(&url),
                None => self.img.set_placeholder("media-optical-symbolic"),
            }
        }

        let dur_text = track
            .duration_seconds
            .map(|d| format!("{}:{:02}", d / 60, d % 60))
            .unwrap_or_default();
        self.duration.set_label(&dur_text);
        self.duration.set_visible(!dur_text.is_empty() && !multi);

        if track.is_explicit {
            self.ensure_explicit().set_visible(true);
        } else if let Some(badge) = self.explicit.borrow().as_ref() {
            badge.set_visible(false);
        }

        if has_id {
            self.like
                .set_data(Some(VideoId(video_id.clone())), Some(track.like_status));
            self.like.widget().set_visible(!multi);
        } else {
            self.like.set_data(None, Some(track.like_status));
        }
        self.show_download_state(&video_id);

        let online = self.ctx.online.is_online() || self.ctx.downloads.is_downloaded(&video_id);
        if has_id && !online {
            self.button.set_sensitive(false);
            self.button.set_opacity(0.4);
        } else {
            self.button.set_sensitive(has_id);
            self.button.set_opacity(1.0);
        }

        // Follow the current track. One handler per bound row, dropped on unbind.
        self.disconnect_state();
        self.sync_playing(has_id && self.ctx.player.state().is_playing_id(&video_id));
        if has_id {
            let weak = Rc::downgrade(self);
            let id =
                self.ctx
                    .player
                    .state()
                    .connect_notify_local(Some("video-id"), move |state, _| {
                        if let Some(row) = weak.upgrade() {
                            row.sync_playing(state.is_playing_id(&video_id));
                        }
                    });
            self.state_handler.replace(Some(id));
        }
    }

    /// Port of _unbind_list_item.
    pub fn unbind(&self) {
        self.disconnect_state();
        self.button.remove_css_class("playing");
        self.button.add_css_class("flat");
        self.title.set_label("");
        self.subtitle.set_label("");
        self.img.clear();
        self.duration.set_label("");
        self.duration.set_visible(false);
        if let Some(badge) = self.explicit.borrow().as_ref() {
            badge.set_visible(false);
        }
        self.like.widget().set_visible(false);
        if let Some(check) = self.check.borrow().as_ref() {
            check.set_visible(false);
        }
        self.button.remove_css_class("selected");
        self.disconnect_check();
        self.track.replace(None);
    }

    fn sync_playing(&self, playing: bool) {
        if playing {
            self.button.add_css_class("playing");
            self.button.remove_css_class("flat");
        } else {
            self.button.remove_css_class("playing");
            self.button.add_css_class("flat");
        }
    }

    /// Port of _apply_row_selection: tint the row and mirror the checkbox.
    pub fn apply_selection(self: &Rc<Self>, selected: bool) {
        if selected {
            self.button.add_css_class("selected");
        } else {
            self.button.remove_css_class("selected");
        }
        if selected || self.check.borrow().is_some() {
            let check = self.ensure_check();
            self.disconnect_check();
            check.set_active(selected);
            if let Some(vid) = self.video_id() {
                let weak = Rc::downgrade(self);
                let id = check.connect_toggled(move |_| {
                    if let Some(row) = weak.upgrade() {
                        if let Some(host) = row.host.upgrade() {
                            host.toggle_selection(&vid, Some(&row));
                        }
                    }
                });
                self.check_handler.replace(Some(id));
            }
        }
    }

    /// Port of the per-row part of _refresh_all_row_visuals.
    pub fn refresh_visuals(self: &Rc<Self>, multi: bool, selected: bool) {
        let has_id = self.video_id().is_some();
        if multi {
            self.ensure_check().set_visible(true);
            self.apply_selection(selected);
        } else {
            if let Some(check) = self.check.borrow().as_ref() {
                check.set_visible(false);
            }
            self.button.remove_css_class("selected");
        }
        // Right-side widgets hide in multi-select to give the checkbox and titles room.
        self.like.widget().set_visible(has_id && !multi);
        let has_dur = !self.duration.label().is_empty();
        self.duration.set_visible(has_dur && !multi);
    }

    pub fn set_check_active(&self, active: bool) {
        if let Some(check) = self.check.borrow().as_ref() {
            check.set_active(active);
        }
    }

    fn disconnect_check(&self) {
        if let (Some(id), Some(check)) = (
            self.check_handler.borrow_mut().take(),
            self.check.borrow().as_ref(),
        ) {
            check.disconnect(id);
        }
    }

    fn disconnect_state(&self) {
        if let Some(id) = self.state_handler.borrow_mut().take() {
            self.ctx.player.state().disconnect(id);
        }
    }
}

impl Drop for TrackRow {
    fn drop(&mut self) {
        if let Some(id) = self.state_handler.borrow_mut().take() {
            self.ctx.player.state().disconnect(id);
        }
    }
}

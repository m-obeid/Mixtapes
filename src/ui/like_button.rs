//! Port of ui.utils.LikeButton: click toggles like, hold or right-click
//! opens a small popover with the dislike action.

use std::cell::{Cell, RefCell};
use std::rc::Rc;

use gtk::{gdk, glib, prelude::*};

use crate::model::{LikeStatus, VideoId};
use crate::player::Player;

pub struct LikeButton {
    button: gtk::Button,
    popover: gtk::Popover,
    dislike_label: gtk::Label,
    player: Rc<Player>,
    video_id: RefCell<Option<VideoId>>,
    status: Cell<LikeStatus>,
    suppress_next_click: Cell<bool>,
}

impl LikeButton {
    pub fn new(player: Rc<Player>) -> Rc<Self> {
        let button = gtk::Button::builder().css_classes(["flat", "circular"]).valign(gtk::Align::Center).build();

        let popover = gtk::Popover::builder().has_arrow(true).build();
        popover.set_parent(&button);
        let menu_box = gtk::Box::builder().orientation(gtk::Orientation::Vertical).spacing(4).margin_top(4).margin_bottom(4).margin_start(4).margin_end(4).build();
        let dislike_content = gtk::Box::builder().orientation(gtk::Orientation::Horizontal).spacing(8).build();
        dislike_content.append(&gtk::Image::from_icon_name("heart-broken-symbolic"));
        let dislike_label = gtk::Label::new(Some("Dislike"));
        dislike_content.append(&dislike_label);
        let dislike_btn = gtk::Button::builder().css_classes(["flat"]).child(&dislike_content).build();
        menu_box.append(&dislike_btn);
        popover.set_child(Some(&menu_box));

        let this = Rc::new(Self {
            button,
            popover,
            dislike_label,
            player,
            video_id: RefCell::new(None),
            status: Cell::new(LikeStatus::Indifferent),
            suppress_next_click: Cell::new(false),
        });

        let weak = Rc::downgrade(&this);
        this.button.connect_clicked(move |_| {
            if let Some(b) = weak.upgrade() {
                b.on_clicked();
            }
        });
        let weak = Rc::downgrade(&this);
        dislike_btn.connect_clicked(move |_| {
            if let Some(b) = weak.upgrade() {
                b.popover.popdown();
                b.suppress_next_click.set(false);
                let target = if b.status.get() == LikeStatus::Dislike { LikeStatus::Indifferent } else { LikeStatus::Dislike };
                b.apply_rating(target);
            }
        });
        let weak = Rc::downgrade(&this);
        this.popover.connect_closed(move |_| {
            let weak = weak.clone();
            glib::idle_add_local_once(move || {
                if let Some(b) = weak.upgrade() {
                    b.suppress_next_click.set(false);
                }
            });
        });

        let right_click = gtk::GestureClick::builder().button(gdk::BUTTON_SECONDARY).build();
        let weak = Rc::downgrade(&this);
        right_click.connect_pressed(move |_, _, _, _| {
            if let Some(b) = weak.upgrade() {
                b.show_menu();
            }
        });
        this.button.add_controller(right_click);

        let long_press = gtk::GestureLongPress::builder().touch_only(false).build();
        let weak = Rc::downgrade(&this);
        long_press.connect_pressed(move |_, _, _| {
            if let Some(b) = weak.upgrade() {
                b.suppress_next_click.set(true);
                b.show_menu();
            }
        });
        this.button.add_controller(long_press);

        this.update_icon();
        this
    }

    pub fn widget(&self) -> &gtk::Button {
        &self.button
    }

    /// Point the button at a track. Hidden when there is no track.
    /// `status` is what the row's own source says, or None when it does not
    /// know: a card off a carousel carries no rating, and seeding the session
    /// cache with its guess would tell every other row the song is unrated.
    pub fn set_data(&self, video_id: Option<VideoId>, status: Option<LikeStatus>) {
        let Some(video_id) = video_id.filter(|v| !v.0.is_empty()) else {
            self.video_id.replace(None);
            self.status.set(LikeStatus::Indifferent);
            self.update_icon();
            self.button.set_visible(false);
            return;
        };
        let client = self.player.net().client();
        // Without a session the local library is the only source of likes.
        if !client.auth_state().has_session() {
            let liked = self.player.local().is_liked(video_id.as_str());
            self.video_id.replace(Some(video_id));
            self.status.set(if liked { LikeStatus::Like } else { LikeStatus::Indifferent });
            self.update_icon();
            self.button.set_visible(true);
            return;
        }
        // What this session has seen wins: it is the only thing that knows
        // about a like the listener has just made.
        let resolved = match (client.known_like_status(video_id.as_str()), status) {
            (Some(known), _) => known,
            (None, Some(status)) => {
                client.set_known_like_status(video_id.as_str(), status);
                status
            }
            (None, None) => LikeStatus::Indifferent,
        };
        self.video_id.replace(Some(video_id));
        self.status.set(resolved);
        self.update_icon();
        self.button.set_visible(true);
    }

    fn on_clicked(&self) {
        if self.video_id.borrow().is_none() {
            return;
        }
        if self.suppress_next_click.get() || self.popover.is_visible() {
            self.suppress_next_click.set(false);
            return;
        }
        let target = if self.status.get() == LikeStatus::Like { LikeStatus::Indifferent } else { LikeStatus::Like };
        self.apply_rating(target);
    }

    fn show_menu(&self) {
        if self.video_id.borrow().is_none() {
            return;
        }
        self.dislike_label.set_label(if self.status.get() == LikeStatus::Dislike { "Remove Dislike" } else { "Dislike" });
        self.popover.set_pointing_to(Some(&gdk::Rectangle::new(0, 0, self.button.width(), self.button.height())));
        self.popover.popup();
    }

    fn apply_rating(&self, status: LikeStatus) {
        let Some(video_id) = self.video_id.borrow().clone() else { return };
        self.status.set(status);
        self.update_icon();
        self.player.set_like_status(video_id, status);
    }

    fn update_icon(&self) {
        let (icon, add, remove, tip) = match self.status.get() {
            LikeStatus::Like => ("heart-filled-symbolic", "liked-button", "disliked-button", "Unlike (Hold or right-click for Dislike)"),
            LikeStatus::Dislike => ("heart-broken-symbolic", "disliked-button", "liked-button", "Disliked (Hold or right-click to remove)"),
            LikeStatus::Indifferent => ("heart-outline-thick-symbolic", "", "", "Like (Hold or right-click for Dislike)"),
        };
        self.button.set_icon_name(icon);
        self.button.remove_css_class("liked-button");
        self.button.remove_css_class("disliked-button");
        if !add.is_empty() {
            self.button.add_css_class(add);
        }
        let _ = remove;
        self.button.set_tooltip_text(Some(tip));
    }
}

impl Drop for LikeButton {
    fn drop(&mut self) {
        // The popover is a child GtkButton's dispose does not unparent.
        self.popover.unparent();
    }
}

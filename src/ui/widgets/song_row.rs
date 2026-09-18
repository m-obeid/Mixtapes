//! Port of ui/widgets/song_row.py: thumbnail with a playing indicator, title
//! with explicit and downloaded badges, subtitle, duration, like button,
//! right-click menu, and a click that ignores drags.

use std::cell::{Cell, RefCell};
use std::rc::Rc;
use std::time::Duration;

use gtk::{gdk, glib, prelude::*};

use crate::model::{ItemKind, MediaItem, VideoId};
use crate::ui::context::UiContext;
use crate::ui::context_menu::{MenuAction, SongMenuOptions, show_song_menu};
use crate::ui::cover::CoverImage;
use crate::ui::like_button::LikeButton;
use crate::ui::widgets::song_list::{kind_subtitle, kind_subtitle_text, search_subtitle};

const THUMB_SIZE: i32 = 56;
const CLICK_SLOP: f64 = 10.0;
const ANIMATION_STEP: Duration = Duration::from_millis(350);

type ActivateHandler = Rc<dyn Fn(&MediaItem)>;
/// What a page adds to this row's menu, built fresh each time it opens.
type MenuExtras = Rc<dyn Fn() -> Vec<MenuAction>>;

pub struct SongRow {
    row: gtk::ListBoxRow,
    inner: gtk::Box,
    cover: Rc<CoverImage>,
    img_overlay: gtk::Overlay,
    track_num: gtk::Label,
    indicator: gtk::Box,
    bars: [gtk::Box; 3],
    title: gtk::Label,
    explicit: gtk::Label,
    dl_icon: gtk::Image,
    subtitle_box: gtk::Box,
    duration: gtk::Label,
    like: Rc<LikeButton>,
    ctx: Rc<UiContext>,
    item: RefCell<Option<MediaItem>>,
    anim: RefCell<Option<glib::SourceId>>,
    anim_state: Cell<bool>,
    state_handler: RefCell<Option<glib::SignalHandlerId>>,
    on_activate: RefCell<Option<ActivateHandler>>,
    press_at: Cell<(f64, f64)>,
    /// Search rows tint the ListBoxRow and show no indicator, like attach_playing_highlight.
    row_highlight: Cell<bool>,
    /// A subtitle the page named itself, drawn plain with no kind icon.
    plain_subtitle: RefCell<Option<String>>,
    /// Extra menu entries the page contributes, built when the menu opens.
    menu_extras: RefCell<Option<MenuExtras>>,
}

impl SongRow {
    pub fn new(ctx: Rc<UiContext>) -> Rc<Self> {
        let row = gtk::ListBoxRow::builder().activatable(true).build();
        let inner = gtk::Box::builder().orientation(gtk::Orientation::Horizontal).spacing(12).hexpand(true).css_classes(["song-row"]).build();
        row.set_child(Some(&inner));

        let cover = CoverImage::in_context(&ctx, THUMB_SIZE);
        cover.widget().add_css_class("song-img");
        let img_overlay = gtk::Overlay::builder().valign(gtk::Align::Center).child(cover.widget()).build();

        let track_num = gtk::Label::builder().css_classes(["dim-label", "caption"]).valign(gtk::Align::Center).halign(gtk::Align::Center).width_request(40).height_request(40).visible(false).build();

        // Same square and radius as the thumbnail, bars centered near the bottom.
        let indicator = gtk::Box::builder().orientation(gtk::Orientation::Horizontal).halign(gtk::Align::Center).valign(gtk::Align::Center).css_classes(["playing-indicator", "song-img"]).visible(false).build();
        let bars_box = gtk::Box::builder().orientation(gtk::Orientation::Horizontal).spacing(3).hexpand(true).halign(gtk::Align::Center).valign(gtk::Align::End).margin_bottom(12).build();
        let bars = [1, 2, 3].map(|n| {
            let bar = gtk::Box::builder().valign(gtk::Align::End).css_classes(["playing-bar", &format!("playing-bar-{n}")]).build();
            bars_box.append(&bar);
            bar
        });
        indicator.append(&bars_box);
        img_overlay.add_overlay(&indicator);
        inner.append(&track_num);
        inner.append(&img_overlay);

        let text = gtk::Box::builder().orientation(gtk::Orientation::Vertical).spacing(2).valign(gtk::Align::Center).hexpand(true).build();
        let title_box = gtk::Box::builder().orientation(gtk::Orientation::Horizontal).spacing(6).build();
        let title = gtk::Label::builder().halign(gtk::Align::Start).xalign(0.0).hexpand(false).ellipsize(gtk::pango::EllipsizeMode::End).lines(1).width_chars(1).build();
        let explicit = gtk::Label::builder().label("E").css_classes(["explicit-badge"]).valign(gtk::Align::Center).halign(gtk::Align::Center).visible(false).build();
        let dl_icon = gtk::Image::builder().icon_name("folder-download-symbolic").pixel_size(14).css_classes(["dim-label"]).valign(gtk::Align::Center).visible(false).build();
        title_box.append(&title);
        title_box.append(&explicit);
        title_box.append(&dl_icon);
        title_box.append(&gtk::Box::builder().hexpand(true).build());
        let subtitle_box = gtk::Box::builder().orientation(gtk::Orientation::Horizontal).build();
        text.append(&title_box);
        text.append(&subtitle_box);
        inner.append(&text);

        let duration = gtk::Label::builder().css_classes(["caption"]).valign(gtk::Align::Center).margin_end(6).build();
        inner.append(&duration);
        let like = LikeButton::new(ctx.player.clone());
        like.widget().set_valign(gtk::Align::Center);
        inner.append(like.widget());

        let this = Rc::new(Self {
            row,
            inner,
            cover,
            img_overlay,
            track_num,
            indicator,
            bars,
            title,
            explicit,
            dl_icon,
            subtitle_box,
            duration,
            like,
            ctx,
            item: RefCell::new(None),
            anim: RefCell::new(None),
            anim_state: Cell::new(false),
            state_handler: RefCell::new(None),
            on_activate: RefCell::new(None),
            press_at: Cell::new((0.0, 0.0)),
            row_highlight: Cell::new(false),
            plain_subtitle: RefCell::new(None),
            menu_extras: RefCell::new(None),
        });
        this.connect_gestures();
        this
    }

    pub fn widget(&self) -> &gtk::ListBoxRow {
        &self.row
    }

    /// Search-page row: tint the outer ListBoxRow, no indicator, no duration column,
    /// and the "Kind • Artist • Album" subtitle search.py built.
    pub fn set_search_style(&self, enabled: bool) {
        self.row_highlight.set(enabled);
        self.duration.set_visible(!enabled);
    }

    /// Entries this row's menu carries on top of the standard ones, the way
    /// the history page adds Play and Remove from History.
    pub fn set_menu_extras(&self, f: impl Fn() -> Vec<MenuAction> + 'static) {
        self.menu_extras.replace(Some(Rc::new(f)));
    }

    /// Draw this text under the title instead of the kind line, the way the
    /// category page and the history page write their own.
    pub fn set_plain_subtitle(&self, text: Option<String>) {
        self.plain_subtitle.replace(text);
    }

    /// Without this the row relies on its ListBox's row-activated signal.
    #[allow(dead_code)]
    pub fn set_on_activate(&self, f: impl Fn(&MediaItem) + 'static) {
        self.on_activate.replace(Some(Rc::new(f)));
    }

    /// Fill the row. A track number replaces the thumbnail on album pages.
    pub fn bind(self: &Rc<Self>, item: &MediaItem, track_number: Option<u32>) {
        self.item.replace(Some(item.clone()));
        self.title.set_label(&item.title);
        self.title.set_tooltip_text(Some(&item.title));
        while let Some(child) = self.subtitle_box.first_child() {
            self.subtitle_box.remove(&child);
        }
        if let Some(text) = self.plain_subtitle.borrow().clone() {
            if !text.is_empty() {
                self.subtitle_box.append(&gtk::Label::builder().label(&text).halign(gtk::Align::Start).xalign(0.0).ellipsize(gtk::pango::EllipsizeMode::End).lines(1).width_chars(1).tooltip_text(&text).css_classes(["dim-label", "caption"]).build());
            }
        } else if self.row_highlight.get() {
            self.subtitle_box.append(&kind_subtitle_text(item, &search_subtitle(item), false));
        } else {
            self.subtitle_box.append(&kind_subtitle(item, true, false));
        }
        self.duration.set_label(&item.duration_text().unwrap_or_default());
        self.explicit.set_visible(item.explicit);
        self.dl_icon.set_visible(item.kind == ItemKind::Song && self.ctx.downloads.is_downloaded(&item.id));

        match track_number {
            Some(n) => {
                self.track_num.set_label(&n.to_string());
                self.track_num.set_visible(true);
                self.img_overlay.set_visible(false);
            }
            None => {
                self.track_num.set_visible(false);
                self.img_overlay.set_visible(true);
                match &item.thumb {
                    Some(url) => self.cover.load(url),
                    None => self.cover.set_placeholder("media-optical-symbolic"),
                }
            }
        }

        let playable = item.kind.is_playable() && !item.id.is_empty();
        self.inner.set_sensitive(playable);
        if playable {
            self.like.set_data(Some(VideoId(item.id.clone())), item.like_status);
        } else {
            self.like.set_data(None, None);
        }

        // Follow the current track. One handler per row, dropped with the row.
        if let Some(id) = self.state_handler.borrow_mut().take() {
            self.ctx.player.state().disconnect(id);
        }
        self.apply_playing(playable && self.ctx.player.state().is_playing_id(&item.id));
        if playable {
            let weak = Rc::downgrade(self);
            let video_id = item.id.clone();
            let id = self.ctx.player.state().connect_notify_local(Some("video-id"), move |state, _| {
                if let Some(row) = weak.upgrade() {
                    row.apply_playing(state.is_playing_id(&video_id));
                }
            });
            self.state_handler.replace(Some(id));
        }
    }

    fn apply_playing(self: &Rc<Self>, playing: bool) {
        let target: &gtk::Widget = if self.row_highlight.get() { self.row.upcast_ref() } else { self.inner.upcast_ref() };
        if playing {
            target.add_css_class("playing");
            target.remove_css_class("flat");
        } else {
            target.remove_css_class("playing");
            target.add_css_class("flat");
        }
        let show_indicator = playing && !self.row_highlight.get();
        self.indicator.set_visible(show_indicator);
        if show_indicator {
            self.start_animation();
        } else {
            self.stop_animation();
        }
    }

    fn start_animation(self: &Rc<Self>) {
        if self.anim.borrow().is_some() {
            return;
        }
        self.anim_state.set(false);
        let weak = Rc::downgrade(self);
        let id = glib::timeout_add_local(ANIMATION_STEP, move || {
            let Some(row) = weak.upgrade() else { return glib::ControlFlow::Break };
            let up = !row.anim_state.replace(!row.anim_state.get());
            let [a, b, c] = &row.bars;
            for (bar, raised) in [(a, up), (b, !up), (c, up)] {
                if raised {
                    bar.add_css_class("bar-up");
                } else {
                    bar.remove_css_class("bar-up");
                }
            }
            glib::ControlFlow::Continue
        });
        self.anim.replace(Some(id));
    }

    fn stop_animation(&self) {
        if let Some(id) = self.anim.borrow_mut().take() {
            id.remove();
        }
        for bar in &self.bars {
            bar.remove_css_class("bar-up");
        }
    }

    fn connect_gestures(self: &Rc<Self>) {
        let open_menu = {
            let weak = Rc::downgrade(self);
            Rc::new(move |x: f64, y: f64| {
                let Some(row) = weak.upgrade() else { return };
                let Some(track) = row.item.borrow().as_ref().and_then(MediaItem::to_track) else { return };
                let extras = row.menu_extras.borrow().clone().map(|build| build()).unwrap_or_default();
                let opts = SongMenuOptions { prefix: "row", nav: Some(row.ctx.nav.clone()), ctx: Some(row.ctx.clone()), extras, ..SongMenuOptions::default() };
                show_song_menu(&row.inner, x, y, &track, &row.ctx.player, opts);
            })
        };
        let right = gtk::GestureClick::builder().button(gdk::BUTTON_SECONDARY).build();
        let open = open_menu.clone();
        right.connect_released(move |_, _, x, y| open(x, y));
        self.inner.add_controller(right);
        let long = gtk::GestureLongPress::new();
        let open = open_menu.clone();
        long.connect_pressed(move |_, x, y| open(x, y));
        self.inner.add_controller(long);

        let left = gtk::GestureClick::builder().button(gdk::BUTTON_PRIMARY).build();
        let weak = Rc::downgrade(self);
        left.connect_pressed(move |_, _, x, y| {
            if let Some(row) = weak.upgrade() {
                row.press_at.set((x, y));
            }
        });
        let weak = Rc::downgrade(self);
        left.connect_released(move |_, _, x, y| {
            let Some(row) = weak.upgrade() else { return };
            let (sx, sy) = row.press_at.get();
            if (x - sx).abs() > CLICK_SLOP || (y - sy).abs() > CLICK_SLOP {
                return;
            }
            let handler = row.on_activate.borrow().clone();
            if let (Some(handler), Some(item)) = (handler, row.item.borrow().as_ref()) {
                handler(item);
            }
        });
        self.inner.add_controller(left);
    }
}

impl Drop for SongRow {
    fn drop(&mut self) {
        if let Some(id) = self.state_handler.borrow_mut().take() {
            self.ctx.player.state().disconnect(id);
        }
        if let Some(id) = self.anim.borrow_mut().take() {
            id.remove();
        }
    }
}

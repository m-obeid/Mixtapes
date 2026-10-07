//! The tab pages and everything they push.

pub mod all_moods;
pub mod artist;
pub mod category;
pub mod discography;
pub mod explore;
pub mod history;
pub mod home;
pub mod library;
pub mod playlist;
pub mod track_list;

use std::rc::Rc;

use gtk::prelude::*;

use crate::model::{ItemKind, MediaItem, Track};
use crate::queue::QueueSource;
use crate::ui::context::{NavRequest, UiContext};

/// Port of home.py _activate_item: play songs and videos, navigate for the rest.
/// `from` names the queue a song starts: the shelf or the search it was clicked in.
pub fn activate_item(ctx: &Rc<UiContext>, item: &MediaItem, pool: &[MediaItem], from: &str) {
    match item.kind {
        ItemKind::Song | ItemKind::Video => {
            let tracks: Vec<_> = pool.iter().filter_map(MediaItem::to_track).collect();
            let index = tracks.iter().position(|t| t.video_id.as_str() == item.id).unwrap_or(0);
            if tracks.is_empty() {
                if let Some(track) = item.to_track() {
                    ctx.player.play_tracks(vec![track], 0, false, QueueSource::shelf(from), false);
                }
            } else {
                ctx.player.play_tracks(tracks, index, false, QueueSource::shelf(from), false);
            }
        }
        ItemKind::Album => {
            // An album card without a browse id still has the OLAK playlist behind it.
            let id = if item.id.is_empty() { item.playlist_id.clone().unwrap_or_default() } else { item.id.clone() };
            if !id.is_empty() {
                ctx.nav.go(NavRequest::Album { id, title: item.title.clone(), thumb: item.thumb.clone() });
            }
        }
        ItemKind::Playlist => ctx.nav.go(NavRequest::Playlist { id: item.id.clone(), title: item.title.clone(), thumb: item.thumb.clone() }),
        ItemKind::Artist => ctx.nav.go(NavRequest::Artist { id: Some(item.id.clone()), name: item.title.clone() }),
    }
}

/// Home's variant: a song or video plays the whole shelf it was in and then
/// keeps going with a radio seeded from the shelf's last track, like
/// _play_with_radio. Everything else navigates as usual.
pub fn activate_item_with_radio(ctx: &Rc<UiContext>, item: &MediaItem, pool: &[MediaItem], from: &str) {
    if !item.kind.is_playable() {
        activate_item(ctx, item, pool, from);
        return;
    }
    let mut tracks: Vec<Track> = pool.iter().filter_map(MediaItem::to_track).collect();
    let mut index = tracks.iter().position(|t| t.video_id.as_str() == item.id).unwrap_or(0);
    if tracks.is_empty() {
        let Some(track) = item.to_track() else { return };
        tracks = vec![track];
        index = 0;
    }
    let seed = tracks.last().map(|t| t.video_id.0.clone()).unwrap_or_default();
    ctx.player.play_then_radio(tracks, index, &seed, from);
}

/// Right-click and long-press on a widget open the item menu.
pub fn attach_item_menu(ctx: &Rc<UiContext>, widget: &impl IsA<gtk::Widget>, item: MediaItem) {
    let open = {
        let ctx = ctx.clone();
        let widget = widget.clone().upcast::<gtk::Widget>();
        Rc::new(move |x: f64, y: f64| crate::ui::context_menu::show_item_menu(&widget, x, y, &item, &ctx))
    };
    let right = gtk::GestureClick::builder().button(gtk::gdk::BUTTON_SECONDARY).build();
    let o = open.clone();
    right.connect_released(move |_, _, x, y| o(x, y));
    widget.add_controller(right);
    let long = gtk::GestureLongPress::new();
    long.connect_pressed(move |_, x, y| open(x, y));
    widget.add_controller(long);
}

/// Loading placeholder used while a page fetches.
/// A section's heading, with View All at the far end of the line when the
/// section holds more than it shows. At the end of a carousel it took a
/// scroll through every card to reach.
pub fn section_header(title: &str, on_view_all: Option<Box<dyn Fn()>>) -> gtk::Box {
    let header = gtk::Box::builder().orientation(gtk::Orientation::Horizontal).spacing(12).build();
    header.append(&gtk::Label::builder().label(title).css_classes(["heading"]).halign(gtk::Align::Start).hexpand(true).ellipsize(gtk::pango::EllipsizeMode::End).build());
    if let Some(on_view_all) = on_view_all {
        let content = gtk::Box::builder().orientation(gtk::Orientation::Horizontal).spacing(4).build();
        content.append(&gtk::Label::new(Some("View All")));
        content.append(&gtk::Image::from_icon_name("go-next-symbolic"));
        let button = gtk::Button::builder().child(&content).css_classes(["flat", "dim-label", "caption", "view-all"]).valign(gtk::Align::Center).tooltip_text(format!("View All {title}")).build();
        button.set_cursor_from_name(Some("pointer"));
        button.connect_clicked(move |_| on_view_all());
        header.append(&button);
    }
    header
}

pub fn loading_box(text: &str) -> gtk::Box {
    let b = gtk::Box::builder().orientation(gtk::Orientation::Vertical).spacing(12).valign(gtk::Align::Center).halign(gtk::Align::Center).build();
    let spinner = adw::Spinner::new();
    spinner.set_size_request(32, 32);
    b.append(&spinner);
    b.append(&gtk::Label::builder().label(text).css_classes(["dim-label"]).build());
    b
}

pub fn clear_children(container: &impl IsA<gtk::Widget>) {
    let container = container.upcast_ref::<gtk::Widget>();
    while let Some(child) = container.first_child() {
        if let Some(b) = container.downcast_ref::<gtk::Box>() {
            b.remove(&child);
        } else if let Some(l) = container.downcast_ref::<gtk::ListBox>() {
            l.remove(&child);
        } else if let Some(w) = container.downcast_ref::<adw::WrapBox>() {
            w.remove(&child);
        } else if let Some(s) = container.downcast_ref::<gtk::Stack>() {
            s.remove(&child);
        } else {
            child.unparent();
        }
    }
}

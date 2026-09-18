//! Song rows and kind subtitles as home.py builds them: thumbnail, title
//! with explicit badge, and an icon plus detail line.

use std::rc::Rc;

use gtk::prelude::*;

use crate::model::{ItemKind, MediaItem};
use crate::ui::context::UiContext;
use crate::ui::cover::CoverImage;

pub const SONG_THUMB_SIZE: i32 = 56;
const LABEL_NATURAL_MAX_CHARS: i32 = 12;

/// Icon and detail text under a title. `include_kind_word` prepends "Song", "Album" and so on.
pub fn kind_subtitle(item: &MediaItem, include_kind_word: bool, constrain_width: bool) -> gtk::Box {
    let mut parts = Vec::new();
    if include_kind_word {
        parts.push(item.kind_word());
    }
    let detail = item.detail();
    if !detail.is_empty() {
        parts.push(detail);
    }
    kind_subtitle_text(item, &parts.join(" · "), constrain_width)
}

/// Port of search.py's subtitle: kind word, then artists and album, joined by bullets.
pub fn search_subtitle(item: &MediaItem) -> String {
    let detail = match item.kind {
        ItemKind::Artist => item.subscribers.clone().unwrap_or_default(),
        ItemKind::Song | ItemKind::Video => {
            let mut text = item.artists_text();
            if let Some(album) = &item.album {
                if !album.name.is_empty() {
                    text = if text.is_empty() { album.name.clone() } else { format!("{text} • {}", album.name) };
                }
            }
            text
        }
        ItemKind::Album => item.artists_text(),
        ItemKind::Playlist => match (&item.count, &item.views) {
            (Some(count), _) if !count.is_empty() => format!("{count} songs"),
            (_, Some(views)) => views.clone(),
            _ => item.artists_text(),
        },
    };
    let kind = item.kind_word();
    if detail.is_empty() { kind } else { format!("{kind} • {detail}") }
}

/// Kind icon plus a given subtitle text.
pub fn kind_subtitle_text(item: &MediaItem, text: &str, constrain_width: bool) -> gtk::Box {
    let row = gtk::Box::builder().orientation(gtk::Orientation::Horizontal).spacing(4).halign(if constrain_width { gtk::Align::Fill } else { gtk::Align::Start }).build();
    let icon = gtk::Image::builder().icon_name(item.kind.icon()).pixel_size(12).valign(gtk::Align::Center).css_classes(["home-kind-icon", "dim-label"]).build();
    row.append(&icon);
    if !text.is_empty() {
        let label = gtk::Label::builder().label(text).halign(gtk::Align::Start).ellipsize(gtk::pango::EllipsizeMode::End).lines(1).width_chars(1).css_classes(["caption", "dim-label"]).build();
        if constrain_width {
            label.set_halign(gtk::Align::Fill);
            label.set_xalign(0.0);
            label.set_hexpand(true);
            label.set_max_width_chars(LABEL_NATURAL_MAX_CHARS);
        }
        row.append(&label);
    }
    row
}

/// One row for a boxed song list. Returns the row plus its inner box for gestures.
pub fn song_row(ctx: &Rc<UiContext>, item: &MediaItem) -> (gtk::ListBoxRow, gtk::Box) {
    song_row_with_subtitle(ctx, item, None)
}

/// Same row with a caller-provided subtitle text, as the search page needs.
pub fn song_row_with_subtitle(ctx: &Rc<UiContext>, item: &MediaItem, subtitle: Option<&str>) -> (gtk::ListBoxRow, gtk::Box) {
    let row = gtk::ListBoxRow::builder().activatable(true).build();
    let inner = gtk::Box::builder().orientation(gtk::Orientation::Horizontal).spacing(12).css_classes(["song-row"]).build();
    row.set_child(Some(&inner));

    let cover = CoverImage::in_context(ctx, SONG_THUMB_SIZE);
    cover.widget().add_css_class("song-img");
    match &item.thumb {
        Some(url) => cover.load(url),
        None => cover.set_placeholder("media-optical-symbolic"),
    }
    inner.append(cover.widget());
    // Keep the loader alive as long as the row.
    unsafe { row.set_data("cover", cover) };

    let text = gtk::Box::builder().orientation(gtk::Orientation::Vertical).spacing(2).valign(gtk::Align::Center).hexpand(true).build();
    let title_box = gtk::Box::builder().orientation(gtk::Orientation::Horizontal).spacing(6).build();
    title_box.append(&gtk::Label::builder().label(&item.title).halign(gtk::Align::Start).xalign(0.0).ellipsize(gtk::pango::EllipsizeMode::End).lines(1).width_chars(1).build());
    if item.explicit {
        title_box.append(&gtk::Label::builder().label("E").css_classes(["explicit-badge"]).valign(gtk::Align::Center).build());
    }
    text.append(&title_box);
    match subtitle {
        Some(subtitle) => text.append(&kind_subtitle_text(item, subtitle, true)),
        None => text.append(&kind_subtitle(item, true, true)),
    }
    inner.append(&text);
    (row, inner)
}

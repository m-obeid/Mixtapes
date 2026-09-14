//! Port of ui/widgets/media_card.py: a square cover, a wrapping title and a
//! one-line subtitle inside a flat button. Sized by the strip or grid it sits in.

use std::cell::Cell;
use std::rc::Rc;

use gtk::prelude::*;

use crate::model::{ItemKind, MediaItem};
use crate::ui::context::UiContext;
use crate::ui::widgets::card_grid::CardLayout;
use crate::ui::cover::CoverImage;

pub const CARD_SIZE_DEFAULT: i32 = 150;
pub const CARD_SIZE_COMPACT: i32 = 130;
pub const GRID_SPACING: i32 = 12;
pub const GRID_LINE_SPACING: i32 = 12;
pub const STRIP_SPACING: i32 = 16;
pub const STRIP_SPACING_COMPACT: i32 = 8;

pub struct CardOptions {
    pub title_lines: i32,
    pub subtitle: Option<String>,
    pub fallback_icon: &'static str,
    pub custom_icon: Option<&'static str>,
}

impl Default for CardOptions {
    fn default() -> Self {
        Self { title_lines: 1, subtitle: None, fallback_icon: "folder-music-symbolic", custom_icon: None }
    }
}

pub struct MediaCard {
    button: gtk::Button,
    main_box: gtk::Box,
    cover: Option<Rc<CoverImage>>,
    icon_box: Option<(gtk::Box, gtk::Image)>,
    item: MediaItem,
    base_size: Cell<i32>,
    grid_size: Cell<Option<i32>>,
    size: Cell<i32>,
}

impl MediaCard {
    pub fn new(ctx: &Rc<UiContext>, item: MediaItem, opts: CardOptions) -> Rc<Self> {
        let button = gtk::Button::builder().css_classes(["activatable", "artist-horizontal-item", "flat"]).hexpand(false).halign(gtk::Align::Start).build();
        // CardBinLayout: the card is only ever as wide as the size set on it.
        button.set_layout_manager(Some(CardLayout::new()));
        let main_box = gtk::Box::builder().orientation(gtk::Orientation::Vertical).spacing(4).build();
        main_box.set_size_request(CARD_SIZE_DEFAULT, -1);
        button.set_child(Some(&main_box));

        let wrapper = gtk::Box::builder().overflow(gtk::Overflow::Hidden).css_classes(["card-cover"]).halign(gtk::Align::Start).valign(gtk::Align::Start).build();
        let (cover, icon_box) = match opts.custom_icon {
            Some(icon_name) => {
                let icon_box = gtk::Box::builder().orientation(gtk::Orientation::Vertical).css_classes(["card-download-icon"]).build();
                let icon = gtk::Image::builder().icon_name(icon_name).hexpand(true).vexpand(true).build();
                icon_box.append(&icon);
                wrapper.append(&icon_box);
                (None, Some((icon_box, icon)))
            }
            None => {
                let cover = CoverImage::new(ctx.net.clone(), CARD_SIZE_DEFAULT);
                cover.widget().add_css_class("card-cover-img");
                match &item.thumb {
                    Some(url) => cover.load(url),
                    None => cover.set_placeholder(opts.fallback_icon),
                }
                wrapper.append(cover.widget());
                (Some(cover), None)
            }
        };
        main_box.append(&wrapper);

        let title = gtk::Label::builder()
            .label(&item.title)
            .halign(gtk::Align::Start)
            .xalign(0.0)
            .hexpand(true)
            .width_chars(1)
            .max_width_chars(1)
            .ellipsize(gtk::pango::EllipsizeMode::End)
            .wrap(true)
            .wrap_mode(gtk::pango::WrapMode::WordChar)
            .lines(opts.title_lines)
            .justify(gtk::Justification::Left)
            .tooltip_text(&item.title)
            .build();
        main_box.append(&title);

        let subtitle = opts.subtitle.clone().unwrap_or_else(|| resolve_subtitle(&item));
        if !subtitle.is_empty() || item.explicit {
            let subtitle_box = gtk::Box::builder().orientation(gtk::Orientation::Horizontal).spacing(4).halign(gtk::Align::Fill).hexpand(true).build();
            if item.explicit {
                subtitle_box.append(&gtk::Label::builder().label("E").css_classes(["explicit-badge"]).valign(gtk::Align::Center).build());
            }
            if !subtitle.is_empty() {
                subtitle_box.append(
                    &gtk::Label::builder()
                        .label(&subtitle)
                        .css_classes(["caption", "dim-label"])
                        .ellipsize(gtk::pango::EllipsizeMode::End)
                        .lines(1)
                        .width_chars(1)
                        .max_width_chars(1)
                        .hexpand(true)
                        .halign(gtk::Align::Fill)
                        .xalign(0.0)
                        .build(),
                );
            }
            main_box.append(&subtitle_box);
        }

        let card = Rc::new(Self { button, main_box, cover, icon_box, item, base_size: Cell::new(CARD_SIZE_DEFAULT), grid_size: Cell::new(None), size: Cell::new(0) });
        card.set_compact(ctx.compact.get());
        card
    }

    pub fn widget(&self) -> &gtk::Button {
        &self.button
    }

    #[allow(dead_code)]
    pub fn item(&self) -> &MediaItem {
        &self.item
    }

    pub fn connect_clicked(&self, f: impl Fn(&MediaItem) + 'static) {
        let item = self.item.clone();
        self.button.connect_clicked(move |_| f(&item));
    }

    pub fn set_compact(&self, compact: bool) {
        self.base_size.set(if compact { CARD_SIZE_COMPACT } else { CARD_SIZE_DEFAULT });
        self.grid_size.set(None);
        if compact {
            self.button.add_css_class("compact");
        } else {
            self.button.remove_css_class("compact");
        }
        self.apply_size();
    }

    /// Grow to the width a grid column offers.
    pub fn set_card_size(&self, size: i32) {
        self.grid_size.set(Some(size));
        self.apply_size();
    }

    fn apply_size(&self) {
        let size = self.grid_size.get().unwrap_or(self.base_size.get());
        if size == self.size.replace(size) {
            return;
        }
        self.main_box.set_size_request(size, -1);
        if let Some(cover) = &self.cover {
            cover.set_size(size);
        }
        if let Some((icon_box, icon)) = &self.icon_box {
            icon_box.set_size_request(size, size);
            icon.set_pixel_size((size as f64 * 0.48).round() as i32);
        }
    }
}

fn resolve_subtitle(item: &MediaItem) -> String {
    let mut parts: Vec<String> = Vec::new();
    if let Some(year) = &item.year {
        parts.push(year.clone());
    }
    if let Some(kind) = &item.item_type {
        if !parts.iter().any(|p| p.eq_ignore_ascii_case(kind)) {
            parts.push(kind.clone());
        }
    }
    if !parts.is_empty() {
        return parts.join(" • ");
    }
    // A playlist card shows its whole description, "Author • N tracks":
    // ytmusicapi files that author under `author`, which _resolve_subtitle
    // never reads, so the description is what it falls through to.
    if item.kind == ItemKind::Playlist {
        if let Some(description) = item.description.clone().filter(|d| !d.is_empty()) {
            return description;
        }
    }
    let artists = item.artists_text();
    if !artists.is_empty() {
        return artists;
    }
    item.description.clone().or_else(|| item.subscribers.clone()).unwrap_or_default()
}

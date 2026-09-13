//! Placeholder for ui/widgets/lyrics_view.py until the lyrics providers are ported.

use gtk::prelude::*;

pub fn build() -> gtk::Widget {
    adw::StatusPage::builder()
        .icon_name("format-justify-fill-symbolic")
        .title("Lyrics")
        .description("Lyrics providers are not ported yet")
        .hexpand(true)
        .vexpand(true)
        .build()
        .upcast()
}

//! Widgets. Everything here runs on the GTK thread and drives playback only
//! through `Rc<Player>` and `PlayerState` bindings.

use std::cell::RefCell;
use std::rc::Rc;

pub mod context;
pub mod context_menu;
pub mod cover;
pub mod cover_view;
pub mod crop_dialog;
pub mod download_queue;
pub mod expanded_player;
pub mod like_button;
pub mod login;
pub mod marquee;
pub mod pages;
pub mod player_bar;
pub mod queue_panel;
pub mod upload_queue;
pub mod widgets;
pub mod window;

use gtk::{gdk, prelude::*};

/// The Python app's stylesheet, loaded verbatim while the port proceeds.
const BASE_CSS: &str = include_str!("../../../src/ui/style.css");

/// Rules player_bar.py injected at runtime.
const PLAYER_BAR_CSS: &str = r#"
.player-bar {
  padding: 0px;
  background-color: @headerbar_bg_color;
  border-top: 1px solid @borders;
}
.link-btn {
  padding: 0px;
  margin: 0px;
  min-height: 0px;
  background: transparent;
  box-shadow: none;
}
.link-btn:hover {
  color: @accent_color;
}
.player-scale {
  margin-top: -1px;
  margin-bottom: 2px;
  min-height: 4px;
  padding: 0px;
}
.player-scale.compact {
  margin-top: -4px;
}
.player-bar.sheet-bar .player-scale.compact {
  margin-top: 0px;
}
.player-scale trough {
  min-height: 4px;
  margin-top: 0px;
  margin-bottom: 0px;
  padding: 0px;
}
.player-scale slider {
  min-height: 0px;
  min-width: 0px;
  margin: 0px;
  background-color: transparent;
}
.player-scale:hover slider {
  min-height: 12px;
  min-width: 12px;
  margin: -5px;
  background-color: white;
  box-shadow: 0 0 4px rgba(0,0,0,0.3);
}
.player-bar-cover {
  border-radius: 6px;
}
"#;

/// Rules desktop_cover_view.py injected at runtime.
const COVER_VIEW_CSS: &str = r#"
.progress-scale {
  padding-left: 0;
  padding-right: 0;
}
.progress-scale trough {
  min-height: 6px;
  border-radius: 4px;
  background-color: alpha(@window_fg_color, 0.2);
}
.progress-scale highlight {
  min-height: 4px;
  border-radius: 2px;
  background-color: @accent_color;
}
.progress-scale slider {
  border-radius: 50%;
  background-color: @window_fg_color;
  box-shadow: 0 1px 3px rgba(0, 0, 0, 0.4);
  opacity: 0;
  transition: opacity 150ms ease;
}
.progress-scale:hover slider {
  opacity: 1;
}
.cover-visualizer {
  transform: translateY(14px);
}
"#;

/// Playing indicator on song rows. song_row.py adds these classes but the
/// Python stylesheet never styled them, so the bars were invisible there.
const SONG_ROW_CSS: &str = r#"
.playing-indicator {
  background-color: alpha(black, 0.35);
}
.playing-bar {
  min-width: 4px;
  min-height: 8px;
  border-radius: 2px;
  background-color: @accent_color;
  transition: min-height 300ms ease;
}
.playing-bar.bar-up {
  min-height: 22px;
}
"#;

pub fn load_css() {
    let Some(display) = gdk::Display::default() else { return };
    for css in [BASE_CSS, PLAYER_BAR_CSS, COVER_VIEW_CSS, SONG_ROW_CSS] {
        let provider = gtk::CssProvider::new();
        provider.load_from_string(css);
        gtk::style_context_add_provider_for_display(&display, &provider, gtk::STYLE_PROVIDER_PRIORITY_APPLICATION);
    }
}

/// "m:ss" for the timings label.
pub fn format_time(seconds: f64) -> String {
    if seconds.is_nan() || seconds <= 0.0 {
        return "0:00".to_owned();
    }
    let total = seconds as u64;
    format!("{}:{:02}", total / 60, total % 60)
}

/// Port of ui.utils.get_high_res_url: ask YouTube's image hosts for a sharper size.
pub fn high_res_url(url: &str, target_size: Option<u32>) -> String {
    if url.is_empty() {
        return String::new();
    }
    let clean = if url.contains("vi_locker") || url.contains("/pl_c/") {
        url.to_owned()
    } else {
        let re = regex::Regex::new(r"([?&])(sqp|rs)=[^&]*&?").unwrap();
        re.replace_all(url, "$1").replace("?&", "?").trim_end_matches(['?', '&']).to_owned()
    };
    if clean.contains("i.ytimg.com") {
        for quality in ["maxresdefault", "sddefault", "hqdefault", "mqdefault", "default"] {
            if clean.contains(quality) {
                return clean.replace(quality, "maxresdefault");
            }
        }
        return clean;
    }
    let dim = target_size.map(|s| s * 2).unwrap_or(544);
    if clean.contains("googleusercontent.com") || clean.contains("ggpht.com") {
        let wh = regex::Regex::new(r"([=-])w\d+-h\d+").unwrap();
        if wh.is_match(&clean) {
            return wh.replace_all(&clean, format!("${{1}}w{dim}-h{dim}")).into_owned();
        }
        let s = regex::Regex::new(r"([=-])s\d+(-|$)").unwrap();
        return s.replace_all(&clean, format!("${{1}}s{dim}$2")).into_owned();
    }
    clean
}

/// Port of suppress_hover_while_scrolling: while the content scrolls, stop
/// pointer hit-testing on it so GTK does no per-frame hover restyle of the
/// row sliding under a stationary pointer, and restore it once motion settles.
pub fn suppress_hover_while_scrolling(scrolled: &gtk::ScrolledWindow) {
    const SETTLE: std::time::Duration = std::time::Duration::from_millis(110);
    let pending: Rc<RefCell<Option<glib::SourceId>>> = Rc::new(RefCell::new(None));
    let weak = scrolled.downgrade();
    scrolled.vadjustment().connect_value_changed(move |_| {
        let Some(scrolled) = weak.upgrade() else { return };
        let previous = pending.borrow_mut().take();
        match previous {
            Some(id) => id.remove(),
            None => {
                scrolled.add_css_class("is-scrolling");
                if let Some(child) = scrolled.child() {
                    child.set_can_target(false);
                }
            }
        }
        let weak = weak.clone();
        let slot = pending.clone();
        let id = glib::timeout_add_local_once(SETTLE, move || {
            slot.borrow_mut().take();
            if let Some(scrolled) = weak.upgrade() {
                scrolled.remove_css_class("is-scrolling");
                if let Some(child) = scrolled.child() {
                    child.set_can_target(true);
                }
            }
        });
        pending.replace(Some(id));
    });
}

pub fn copy_to_clipboard(text: &str) {
    if let Some(display) = gdk::Display::default() {
        display.clipboard().set_text(text);
    }
}

/// Toast through the nearest ToastOverlay above `widget`. Popovers count as descendants of their parent.
pub fn toast(widget: &impl IsA<gtk::Widget>, message: &str) {
    if let Some(overlay) = widget.ancestor(adw::ToastOverlay::static_type()).and_downcast::<adw::ToastOverlay>() {
        overlay.add_toast(adw::Toast::new(message));
    } else {
        tracing::info!(message, "toast without overlay");
    }
}

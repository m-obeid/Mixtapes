//! Cover-derived appearance: the blurred cover background, the dynamic accent
//! with its optional tint, and the contrast-checked colors derived from
//! whichever accent is in force. Port of the appearance half of ui/window.py.
//!
//! One display-wide CSS provider, one step above the user's gtk.css, holding
//! three parts in cascade order: the background, the accent and the derived
//! colors. Loading a display-wide provider restyles every widget in the
//! window, which is tens of milliseconds with a full home feed, and a track
//! change used to do it up to four times. The parts are now joined and loaded
//! once per change, and only after the blur and the accent that are still on
//! their way have both landed.

use std::cell::{Cell, RefCell};
use std::rc::Rc;

use adw::prelude::*;
use gtk::{gdk, glib};
use serde_json::Value;

use crate::ui::color_utils::{self as color, Rgb};
use crate::ui::context::UiContext;
use crate::ui::cover_effects;
use crate::ui::window::AppearancePref;

/// Contrast the playing-row label clears against its row. AAA reads as the highlight it is.
const PLAYING_FG_CONTRAST: f64 = color::WCAG_AAA;
/// Contrast white has to hold against the solid accent to stay its label color.
const ACCENT_FG_MIN_CONTRAST: f64 = 3.5;
/// How far the sidebar pane steps from what sits behind it. Adwaita's own stands at about this.
const SIDEBAR_SEPARATION: f64 = 1.17;
/// Share of the accent's chroma the sidebar overlay keeps.
const SIDEBAR_TINT: f64 = 0.15;
const SIDEBAR_OPACITY: f64 = 0.16;
/// The visualizer's brightest bar alpha, what Visualizer.ACTIVE_ALPHA_MAX is.
const VISUALIZER_ALPHA_MAX: f64 = 0.6;

/// How long the sheet waits for a blur or an accent still being computed.
const MAX_HOLD: std::time::Duration = std::time::Duration::from_millis(1500);
const HOLD_RETRY: std::time::Duration = std::time::Duration::from_millis(100);

/// Typical luminance the blur normalizer lands a cover on, until the measured value arrives.
fn default_backdrop(dark: bool) -> f64 {
    if dark { 0.025 } else { 0.55 }
}

const BLUR_OVERRIDE_CSS: &str = r#"
window.cover-bg-active toolbarview,
window.cover-bg-active overlaysplitview,
window.cover-bg-active navigation-view,
window.cover-bg-active stack,
window.cover-bg-active listview > row,
window.cover-bg-active listbox > row {
  background: none;
  background-color: transparent;
}

window.cover-bg-active .sidebar-pane {
  background: none;
  background-color: @blur_sidebar_bg;
}

window.cover-bg-active headerbar {
  background: none;
  background-color: transparent;
  box-shadow: none;
  border: none;
}

window.cover-bg-active .sidebar,
window.cover-bg-active .lyrics-split > .sidebar-pane {
  background: none;
  background-color: transparent;
}

window.cover-bg-active bottom-sheet .player-drawer,
window.cover-bg-active bottom-sheet .queue-panel,
window.cover-bg-active bottom-sheet .player-bar,
window.cover-bg-active .sheet-bottom-bar actionbar > revealer > box {
  background: none;
  background-color: transparent;
}

window.cover-bg-active .banner-scrim {
  background: linear-gradient(
    to bottom,
    transparent 0%,
    alpha(@window_bg_color, 0.25) 55%,
    alpha(@window_bg_color, 0.45) 75%,
    transparent 100%
  );
}

window.cover-bg-active .songs-list,
window.cover-bg-active .card.home-speed-tile {
  background-color: alpha(currentColor, 0.1);
}

window.cover-bg-active .home-speed-tile:hover {
  background-color: alpha(currentColor, 0.18);
}

window.cover-bg-active .home-speed-tile:active {
  background-color: alpha(currentColor, 0.25);
}

window.cover-bg-active listview > row:hover .queue-row {
  background-color: alpha(currentColor, 0.1);
}

window.cover-bg-active toggle:checked {
  background-color: @toggle_checked_bg;
}
"#;

const TINT_DARK: &str = "
@define-color window_bg_color mix(#111113, @accent_bg_color, 0.10);
@define-color view_bg_color mix(#0e0e10, @accent_bg_color, 0.10);
@define-color headerbar_bg_color mix(#171719, @accent_bg_color, 0.10);
@define-color headerbar_backdrop_color mix(#111113, @accent_bg_color, 0.10);
@define-color popover_bg_color mix(#1b1b1d, @accent_bg_color, 0.10);
@define-color dialog_bg_color mix(#1b1b1d, @accent_bg_color, 0.10);
@define-color card_bg_color mix(rgba(255, 255, 255, 0.08), @accent_bg_color, 0.10);
@define-color sidebar_bg_color mix(#212123, @accent_bg_color, 0.10);
@define-color sidebar_backdrop_color mix(#1b1b1d, @accent_bg_color, 0.10);
@define-color sidebar_border_color mix(rgba(0, 0, 0, 0.36), @accent_bg_color, 0.10);
@define-color secondary_sidebar_bg_color mix(#1a1a1c, @accent_bg_color, 0.10);
@define-color secondary_sidebar_backdrop_color mix(#161618, @accent_bg_color, 0.10);
@define-color secondary_sidebar_border_color mix(rgba(0, 0, 0, 0.25), @accent_bg_color, 0.10);
";

const TINT_LIGHT: &str = "
@define-color window_bg_color mix(#fafafb, @accent_bg_color, 0.12);
@define-color view_bg_color mix(#ffffff, @accent_bg_color, 0.12);
@define-color headerbar_bg_color mix(#ffffff, @accent_bg_color, 0.12);
@define-color headerbar_backdrop_color mix(#fafafb, @accent_bg_color, 0.12);
@define-color popover_bg_color mix(#ffffff, @accent_bg_color, 0.12);
@define-color dialog_bg_color mix(#fafafb, @accent_bg_color, 0.12);
@define-color card_bg_color mix(#ffffff, @accent_bg_color, 0.06);
@define-color sidebar_bg_color mix(#ebebed, @accent_bg_color, 0.12);
@define-color sidebar_backdrop_color mix(#f2f2f4, @accent_bg_color, 0.12);
@define-color sidebar_border_color mix(rgba(0, 0, 3, 0.07), @accent_bg_color, 0.12);
@define-color secondary_sidebar_bg_color mix(#f3f3f5, @accent_bg_color, 0.12);
@define-color secondary_sidebar_backdrop_color mix(#f6f6fa, @accent_bg_color, 0.12);
@define-color secondary_sidebar_border_color mix(rgba(0, 0, 0, 0.07), @accent_bg_color, 0.12);
";

/// Shared by both tints: panels and the legacy theme names follow the tinted surfaces.
const TINT_COMMON: &str = "
@define-color panel_bg_color @window_bg_color;
@define-color panel_button_bg_color transparent;
@define-color panel_hover_bg_color @card_bg_color;
@define-color theme_bg_color @window_bg_color;
@define-color theme_base_color @view_bg_color;
@define-color theme_selected_bg_color @accent_bg_color;
@define-color theme_selected_fg_color @accent_fg_color;
";

struct Prefs {
    blurred_background: bool,
    dynamic_accent: bool,
    tinted_background: bool,
}

fn hex(value: &str) -> Rgb {
    color::from_hex(value).unwrap_or((0.0, 0.0, 0.0))
}

pub struct Appearance {
    window: adw::ApplicationWindow,
    ctx: Rc<UiContext>,
    css: gtk::CssProvider,
    /// The three parts of the sheet, joined in this order when it is loaded.
    bg_part: RefCell<String>,
    accent_part: RefCell<String>,
    /// What the provider holds now, so an unchanged sheet is not loaded again.
    loaded: RefCell<String>,
    /// Blur and accent requests still running. The sheet waits for them, briefly.
    inflight: Cell<u32>,
    held_since: Cell<Option<std::time::Instant>>,
    last_cover_url: RefCell<Option<String>>,
    last_dominant: Cell<Option<Rgb>>,
    /// (solid, standalone, view background) while the cover-derived accent is in force.
    accent_override: Cell<Option<(Rgb, Rgb, Rgb)>>,
    /// Typical luminance of the backdrop painted now, measured from the blurred cover.
    blur_backdrop: Cell<Option<f64>>,
    derive_pending: Cell<bool>,
    /// Bumped per request so a slow blur or accent never lands over a newer cover.
    blur_request: Cell<u64>,
    accent_request: Cell<u64>,
}

impl Appearance {
    pub fn new(window: &adw::ApplicationWindow, ctx: &Rc<UiContext>) -> Rc<Self> {
        let this = Rc::new(Self {
            window: window.clone(),
            ctx: ctx.clone(),
            css: gtk::CssProvider::new(),
            bg_part: RefCell::new(String::new()),
            accent_part: RefCell::new(String::new()),
            loaded: RefCell::new(String::new()),
            inflight: Cell::new(0),
            held_since: Cell::new(None),
            last_cover_url: RefCell::new(None),
            last_dominant: Cell::new(None),
            accent_override: Cell::new(None),
            blur_backdrop: Cell::new(None),
            derive_pending: Cell::new(false),
            blur_request: Cell::new(0),
            accent_request: Cell::new(0),
        });
        if let Some(display) = gdk::Display::default() {
            gtk::style_context_add_provider_for_display(&display, &this.css, gtk::STYLE_PROVIDER_PRIORITY_USER + 1);
        }
        this.refresh_derived_colors();

        let state = ctx.player.state().clone();
        for property in ["thumbnail-url", "queue-length"] {
            let weak = Rc::downgrade(&this);
            state.connect_notify_local(Some(property), move |_, _| {
                if let Some(this) = weak.upgrade() {
                    this.on_metadata();
                }
            });
        }
        let manager = adw::StyleManager::default();
        let weak = Rc::downgrade(&this);
        manager.connect_dark_notify(move |_| {
            if let Some(this) = weak.upgrade() {
                this.on_color_scheme_changed();
            }
        });
        for property in ["accent-color", "high-contrast"] {
            let weak = Rc::downgrade(&this);
            manager.connect_notify_local(Some(property), move |_, _| {
                if let Some(this) = weak.upgrade() {
                    this.refresh_derived_colors();
                }
            });
        }
        this.on_metadata();
        this
    }

    fn prefs(&self) -> Prefs {
        let prefs = self.ctx.paths.read_prefs();
        let flag = |key: &str| prefs.get(key).and_then(Value::as_bool).unwrap_or(false);
        Prefs { blurred_background: flag("blurred_background"), dynamic_accent: flag("dynamic_accent"), tinted_background: flag("tinted_background") }
    }

    fn is_dark(&self) -> bool {
        adw::StyleManager::default().is_dark()
    }

    /// AA normally, AAA under high contrast.
    fn contrast_target(&self) -> f64 {
        if adw::StyleManager::default().is_high_contrast() { color::WCAG_AAA } else { color::WCAG_AA }
    }

    fn current_cover(&self) -> Option<String> {
        let state = self.ctx.player.state();
        let url = state.thumbnail_url();
        (!url.is_empty() && state.queue_length() > 0).then_some(url)
    }

    /// Port of _on_metadata_for_appearance.
    fn on_metadata(self: &Rc<Self>) {
        // Stopped or cleared: fall back to the normal theme background and accent.
        let Some(url) = self.current_cover() else {
            self.last_cover_url.replace(None);
            self.deactivate_cover_bg();
            self.clear_dynamic_accent();
            return;
        };
        let same_cover = self.last_cover_url.borrow().as_deref() == Some(url.as_str());
        self.last_cover_url.replace(Some(url.clone()));
        let prefs = self.prefs();
        if prefs.blurred_background && !same_cover {
            self.activate_cover_bg(&url);
        } else if prefs.blurred_background {
            self.window.add_css_class("cover-bg-active");
        }
        if prefs.dynamic_accent && !same_cover {
            self.update_dynamic_accent(&url);
        }
    }

    /// A switch moved in Preferences.
    pub fn pref_changed(self: &Rc<Self>, pref: AppearancePref) {
        let prefs = self.prefs();
        let cover = self.last_cover_url.borrow().clone().or_else(|| self.current_cover());
        match pref {
            AppearancePref::BlurredBackground => match cover.filter(|_| prefs.blurred_background) {
                Some(url) => self.activate_cover_bg(&url),
                None => self.deactivate_cover_bg(),
            },
            AppearancePref::DynamicAccent => match cover.filter(|_| prefs.dynamic_accent) {
                Some(url) => self.update_dynamic_accent(&url),
                None => self.clear_dynamic_accent(),
            },
            AppearancePref::TintedBackground => {
                if let Some(url) = cover.filter(|_| prefs.dynamic_accent) {
                    self.update_dynamic_accent(&url);
                }
            }
        }
    }

    fn on_color_scheme_changed(self: &Rc<Self>) {
        // The accent and the blur normalization are computed against the active scheme.
        let prefs = self.prefs();
        let cover = self.last_cover_url.borrow().clone();
        if prefs.dynamic_accent {
            if let Some(rgb) = self.last_dominant.get() {
                self.set_dynamic_accent(rgb);
            } else if let Some(url) = &cover {
                self.accent_override.set(None);
                self.update_dynamic_accent(url);
            }
        }
        if let Some(url) = cover.filter(|_| prefs.blurred_background) {
            self.update_blurred_background(&url);
        }
        self.refresh_derived_colors();
    }

    // -- blurred background ---------------------------------------------------

    /// Go translucent at once, before the blur exists. The picture joins when it is ready.
    fn activate_cover_bg(self: &Rc<Self>, url: &str) {
        self.window.add_css_class("cover-bg-active");
        if self.bg_part.borrow().is_empty() {
            self.bg_part.replace(BLUR_OVERRIDE_CSS.to_owned());
        }
        self.update_blurred_background(url);
        self.refresh_derived_colors();
    }

    fn deactivate_cover_bg(self: &Rc<Self>) {
        self.window.remove_css_class("cover-bg-active");
        self.blur_backdrop.set(None);
        self.blur_request.set(self.blur_request.get() + 1);
        self.bg_part.replace(String::new());
        self.refresh_derived_colors();
    }

    fn update_blurred_background(self: &Rc<Self>, url: &str) {
        let request = self.blur_request.get() + 1;
        self.blur_request.set(request);
        let http = self.ctx.net.client().http().clone();
        let cache_dir = self.ctx.paths.cache_dir.clone();
        let handle = self.ctx.net.spawn(cover_effects::get_blurred_cover(http, cache_dir, url.to_owned(), self.is_dark()));
        self.inflight.set(self.inflight.get() + 1);
        let weak = Rc::downgrade(self);
        glib::spawn_future_local(async move {
            let blurred = handle.await.ok().flatten();
            let Some(this) = weak.upgrade() else { return };
            this.inflight.set(this.inflight.get().saturating_sub(1));
            if this.blur_request.get() != request {
                this.refresh_derived_colors();
                return;
            }
            match blurred.filter(|b| b.path.exists()) {
                Some(blurred) => {
                    // The sidebar is sized off the typical luminance, not the worst one for text.
                    this.blur_backdrop.set(Some(blurred.backdrop.0));
                    this.set_blurred_background_css(&blurred.path);
                }
                None => this.deactivate_cover_bg(),
            }
            this.refresh_derived_colors();
        });
    }

    /// The picture covers the whole window, so the color under it never shows
    /// while the file is there. It is what shows when the file is not, such as
    /// after the cache was cleared: the rule used to say transparent, and the
    /// window went see-through until the next blur arrived.
    fn set_blurred_background_css(&self, path: &std::path::Path) {
        let Ok(uri) = glib::filename_to_uri(path, None) else { return };
        let rule = format!(
            "\nwindow.cover-bg-active, window.cover-bg-active.background, window.cover-bg-active bottom-sheet sheet {{\n  background: none;\n  background-color: @window_bg_color;\n  background-image: url(\"{uri}\");\n  background-size: cover;\n  background-position: center;\n  background-repeat: no-repeat;\n}}\n"
        );
        self.bg_part.replace(format!("{BLUR_OVERRIDE_CSS}{rule}"));
    }

    // -- dynamic accent -----------------------------------------------------------

    fn update_dynamic_accent(self: &Rc<Self>, url: &str) {
        let request = self.accent_request.get() + 1;
        self.accent_request.set(request);
        let http = self.ctx.net.client().http().clone();
        let cache_dir = self.ctx.paths.cache_dir.clone();
        let handle = self.ctx.net.spawn(cover_effects::get_dominant_color(http, cache_dir, url.to_owned()));
        self.inflight.set(self.inflight.get() + 1);
        let weak = Rc::downgrade(self);
        glib::spawn_future_local(async move {
            let rgb = handle.await.ok().flatten();
            let Some(this) = weak.upgrade() else { return };
            this.inflight.set(this.inflight.get().saturating_sub(1));
            if this.accent_request.get() != request {
                this.refresh_derived_colors();
                return;
            }
            match rgb {
                Some(rgb) => this.set_dynamic_accent(rgb),
                None => this.clear_dynamic_accent(),
            }
        });
    }

    fn set_dynamic_accent(self: &Rc<Self>, rgb: Rgb) {
        self.last_dominant.set(Some(rgb));
        let prefs = self.prefs();
        let dark = self.is_dark();

        let solid = color::clamp_lightness(rgb, 0.45, 0.85);
        let solid = color::ensure_contrast(solid, hex(if dark { "#242424" } else { "#fafafa" }), 3.0);
        let view_bg = color::mix(hex(if dark { "#1e1e1e" } else { "#ffffff" }), solid, 0.08);
        let standalone = color::ensure_contrast(solid, view_bg, self.contrast_target());
        // White stops being readable on a bright cover-derived accent, so the label flips to black.
        let accent_fg = color::best_foreground(solid, ACCENT_FG_MIN_CONTRAST);

        let tint = match (prefs.dynamic_accent && prefs.tinted_background, dark) {
            (false, _) => String::new(),
            (true, true) => format!("{TINT_DARK}{TINT_COMMON}"),
            (true, false) => format!("{TINT_LIGHT}{TINT_COMMON}"),
        };
        let css = format!(
            "@define-color accent_bg_color {};\n@define-color accent_color {};\n@define-color accent_fg_color {};\n{tint}\ntoast {{\n  background-color: mix(#28282a, @accent_bg_color, 0.12);\n  color: #ffffff;\n}}\ntoggle:checked {{\n  background-color: @card_bg_color;\n}}\n.inline {{\n  background-color: rgba(0, 0, 0, 0);\n}}\nbanner {{ --banner-color: mix(#3e3e42, @accent_bg_color, 0.12); }}\n",
            color::to_css(solid),
            color::to_css(standalone),
            color::to_css(accent_fg),
        );
        self.window.add_css_class("tinted");
        self.accent_part.replace(css);
        self.accent_override.set(Some((solid, standalone, view_bg)));
        self.refresh_derived_colors();
    }

    fn clear_dynamic_accent(self: &Rc<Self>) {
        self.last_dominant.set(None);
        self.accent_request.set(self.accent_request.get() + 1);
        self.accent_part.replace(String::new());
        self.window.remove_css_class("tinted");
        self.accent_override.set(None);
        self.refresh_derived_colors();
    }

    // -- colors derived from whichever accent is in force -------------------------------

    /// A named color as the live cascade resolves it, which covers a user's gtk.css.
    #[allow(deprecated)]
    fn theme_color(&self, name: &str) -> Option<gdk::RGBA> {
        self.window.style_context().lookup_color(name)
    }

    fn theme_rgb(&self, name: &str) -> Option<Rgb> {
        self.theme_color(name).map(|c| (f64::from(c.red()), f64::from(c.green()), f64::from(c.blue())))
    }

    /// Like theme_rgb, composited on `base` when the token carries alpha.
    /// Adwaita's light window foreground is 80% black, not black.
    fn theme_rgb_over(&self, name: &str, base: Rgb) -> Option<Rgb> {
        let rgba = self.theme_color(name)?;
        let rgb = (f64::from(rgba.red()), f64::from(rgba.green()), f64::from(rgba.blue()));
        Some(if rgba.alpha() >= 1.0 { rgb } else { color::mix(base, rgb, f64::from(rgba.alpha())) })
    }

    /// (solid, standalone, view background) for the accent in force.
    fn accent_in_force(&self) -> (Rgb, Rgb, Rgb) {
        if let Some(accent) = self.accent_override.get() {
            return accent;
        }
        let dark = self.is_dark();
        let view_bg = self.theme_rgb("view_bg_color").unwrap_or_else(|| hex(if dark { "#1e1e1e" } else { "#ffffff" }));
        if let (Some(solid), Some(standalone)) = (self.theme_rgb("accent_bg_color"), self.theme_rgb("accent_color")) {
            return (solid, standalone, view_bg);
        }
        let accent = adw::StyleManager::default().accent_color();
        let to_rgb = |c: gdk::RGBA| (f64::from(c.red()), f64::from(c.green()), f64::from(c.blue()));
        (to_rgb(accent.to_rgba()), to_rgb(accent.to_standalone_rgba(dark)), view_bg)
    }

    /// Queue one load of the whole sheet for the next idle. GTK notifies a
    /// scheme change before it swaps the stylesheet, so deriving inside the
    /// notify reads the theme being left. Coalesced, since several callers
    /// fire together, and held while a blur or an accent is still on its way
    /// so a track change restyles the window once instead of once per part.
    pub fn refresh_derived_colors(self: &Rc<Self>) {
        if self.derive_pending.replace(true) {
            return;
        }
        let weak = Rc::downgrade(self);
        glib::idle_add_local_once(move || {
            let Some(this) = weak.upgrade() else { return };
            this.derive_pending.set(false);
            if this.inflight.get() > 0 {
                let since = *this.held_since.get().get_or_insert_with(std::time::Instant::now);
                this.held_since.set(Some(since));
                if since.elapsed() < MAX_HOLD {
                    let weak = Rc::downgrade(&this);
                    glib::timeout_add_local_once(HOLD_RETRY, move || {
                        if let Some(this) = weak.upgrade() {
                            this.refresh_derived_colors();
                        }
                    });
                    return;
                }
            }
            this.held_since.set(None);
            let sheet = format!("{}\n{}\n{}", this.bg_part.borrow(), this.accent_part.borrow(), this.derive_colors());
            if *this.loaded.borrow() != sheet {
                this.css.load_from_string(&sheet);
                this.loaded.replace(sheet);
            }
        });
    }

    /// The derived color definitions, last in the sheet so they win over the accent part.
    fn derive_colors(&self) -> String {
        let (solid, standalone, view_bg) = self.accent_in_force();
        let dark = self.is_dark();
        let row_bg = color::mix(view_bg, solid, 0.18);
        // The row's accent tint eats most of the AA margin, so the label clears more than that.
        let playing_fg = color::ensure_contrast(standalone, row_bg, self.contrast_target().max(PLAYING_FG_CONTRAST));

        let (panel, panel_weak, toggle_checked) = if dark {
            ("rgba(18, 18, 20, 0.55)", "rgba(18, 18, 20, 0.35)", "rgba(255, 255, 255, 0.14)")
        } else {
            ("rgba(255, 255, 255, 0.65)", "rgba(255, 255, 255, 0.45)", "rgba(255, 255, 255, 0.65)")
        };

        // The sidebar pane is sized against the backdrop painted, not a fixed
        // opacity. Like Adwaita's it steps lighter in dark and darker in light.
        let typical = self.blur_backdrop.get().unwrap_or_else(|| default_backdrop(dark));
        let (lightness, chroma, hue) = color::rgb_to_oklch(solid);
        let overlay = color::overlay_for_contrast(color::gray(typical), color::oklch_to_rgb(lightness, chroma * SIDEBAR_TINT, hue), SIDEBAR_OPACITY, SIDEBAR_SEPARATION, dark);
        let byte = |c: f64| (c.clamp(0.0, 1.0) * 255.0).round() as u8;
        let sidebar_bg = format!("rgba({}, {}, {}, {SIDEBAR_OPACITY})", byte(overlay.0), byte(overlay.1), byte(overlay.2));

        // Transport buttons and time labels are drawn over the bars, so the tallest bar stays clear of them.
        let bar_base = match self.blur_backdrop.get() {
            Some(_) => color::gray(typical),
            None => self.theme_rgb("window_bg_color").unwrap_or_else(|| hex(if dark { "#1e1e1e" } else { "#fafafb" })),
        };
        let label_fg = self.theme_rgb_over("window_fg_color", bar_base).unwrap_or(if dark { (1.0, 1.0, 1.0) } else { (0.0, 0.0, 0.0) });
        let visualizer_bar = color::overlay_clear_of(bar_base, standalone, VISUALIZER_ALPHA_MAX, label_fg, self.contrast_target());

        format!(
            "@define-color playing_fg {};\n@define-color blur_panel_bg {panel};\n@define-color blur_panel_bg_weak {panel_weak};\n@define-color toggle_checked_bg {toggle_checked};\n@define-color blur_sidebar_bg {sidebar_bg};\n@define-color visualizer_bar {};\n",
            color::to_css(playing_fg),
            color::to_css(visualizer_bar),
        )
    }
}

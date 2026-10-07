//! The Lyrics page of the preferences dialog. Port of _build_lyrics_page.

use std::cell::RefCell;
use std::rc::Rc;

use adw::prelude::*;

use crate::App;
use crate::lyrics::prefs;
use crate::lyrics::Lyrics;
use crate::ui::preferences::{combo_row, scale_row, switch_row, translated};
use crate::ui::window::MainWindow;

/// What each provider is good at, so ordering the queue is an informed choice.
fn provider_blurb(name: &str) -> String {
    match name {
        "Apple Music" => tr!("Word-level timing. Best coverage for Western pop"),
        "BetterLyrics" => tr!("Word-level timing. Mirrors Apple's database"),
        "BiniLyrics" => tr!("Word-level timing. Strong on Japanese tracks"),
        "NetEase" => tr!("Line-synced. Romanization and translation for CJK"),
        "LRCLIB" => tr!("Line-synced. Large community LRC database"),
        "YouTube Music" => tr!("Plain text, no timing. Always available when signed in"),
        _ => String::new(),
    }
}

// Translated where the rows show them.
const MATCH_LABELS: [&str; 2] = [tr_noop!("Quality-aware"), tr_noop!("Strict")];
// Translators: the choices for the second lyric line. "Background" is the background vocals of a song.
const SECOND_LINE_LABELS: [&str; 5] = [tr_noop!("Off"), tr_noop!("Auto"), tr_noop!("Romanization"), tr_noop!("Translation"), tr_noop!("Background")];
const EFFECT_LABELS: [&str; 3] = [tr_noop!("Off"), tr_noop!("Subtle"), tr_noop!("Full")];

/// The provider queue: numbered rows with move buttons and a switch each.
struct ProviderQueue {
    group: adw::PreferencesGroup,
    rows: RefCell<Vec<adw::ActionRow>>,
    lyrics: Lyrics,
    win: std::rc::Weak<MainWindow>,
}

impl ProviderQueue {
    fn rebuild(self: &Rc<Self>) {
        for row in self.rows.borrow_mut().drain(..) {
            self.group.remove(&row);
        }
        let order = self.lyrics.prefs().full_provider_order();
        let disabled = self.lyrics.prefs().disabled_providers();
        for (i, name) in order.iter().enumerate() {
            let row = adw::ActionRow::builder().title(format!("{}. {name}", i + 1)).subtitle(provider_blurb(name)).build();
            for (icon, tooltip, delta, sensitive) in [("go-up-symbolic", tr!("Move up"), -1i32, i > 0), ("go-down-symbolic", tr!("Move down"), 1, i + 1 < order.len())] {
                let button = gtk::Button::builder().icon_name(icon).tooltip_text(tooltip).valign(gtk::Align::Center).sensitive(sensitive).build();
                // The builder would replace the image-button class the icon brings.
                button.add_css_class("flat");
                let this = Rc::downgrade(self);
                let name = name.clone();
                button.connect_clicked(move |_| {
                    if let Some(this) = this.upgrade() {
                        this.move_by(&name, delta);
                    }
                });
                row.add_suffix(&button);
            }
            let switch = gtk::Switch::builder().valign(gtk::Align::Center).active(!disabled.contains(name)).build();
            {
                let this = Rc::downgrade(self);
                let name = name.clone();
                switch.connect_active_notify(move |switch| {
                    if let Some(this) = this.upgrade() {
                        this.toggle(&name, switch.is_active());
                    }
                });
            }
            row.add_suffix(&switch);
            row.set_activatable_widget(Some(&switch));
            self.group.add(&row);
            self.rows.borrow_mut().push(row);
        }
    }

    fn move_by(self: &Rc<Self>, name: &str, delta: i32) {
        let mut order = self.lyrics.prefs().full_provider_order();
        let Some(i) = order.iter().position(|n| n == name) else { return };
        let j = i as i32 + delta;
        if j < 0 || j as usize >= order.len() {
            return;
        }
        order.swap(i, j as usize);
        self.lyrics.prefs().set_provider_order(&order);
        self.rebuild_later();
    }

    fn toggle(self: &Rc<Self>, name: &str, enabled: bool) {
        // An empty queue silently means no lyrics, ever, with nothing on screen to say why.
        if !enabled && self.lyrics.prefs().provider_order().len() <= 1 {
            if let Some(win) = self.win.upgrade() {
                win.add_toast(&tr!("Keep at least one lyrics provider enabled"));
            }
        } else {
            self.lyrics.prefs().set_provider_enabled(name, enabled);
        }
        self.rebuild_later();
    }

    /// Rebuilding removes the widget whose signal is still being delivered, so wait for idle.
    fn rebuild_later(self: &Rc<Self>) {
        let this = Rc::downgrade(self);
        glib::idle_add_local_once(move || {
            if let Some(this) = this.upgrade() {
                this.rebuild();
            }
        });
    }
}

pub fn build_page(win: &Rc<MainWindow>, ctx: &Rc<App>) -> adw::PreferencesPage {
    let page = adw::PreferencesPage::builder().name("lyrics").title(tr!("Lyrics")).icon_name("format-justify-fill-symbolic").build();
    let lyrics = ctx.lyrics.clone();
    let apply = {
        let win = Rc::downgrade(win);
        move || {
            if let Some(win) = win.upgrade() {
                for view in win.lyrics_views() {
                    view.apply_display_prefs();
                }
            }
        }
    };

    // -- search queue ---------------------------------------------------
    let queue_group = adw::PreferencesGroup::builder().title(tr!("Search Queue")).description(tr!("Tried from the top down. Switch one off to skip it.")).build();
    page.add(&queue_group);
    let queue = Rc::new(ProviderQueue { group: queue_group.clone(), rows: RefCell::new(Vec::new()), lyrics: ctx.lyrics.clone(), win: Rc::downgrade(win) });
    queue.rebuild();
    // The closures above hold weak references, so the group keeps the queue alive.
    unsafe { queue_group.set_data("queue", queue) };

    // -- matching ---------------------------------------------------------
    let match_group = adw::PreferencesGroup::builder()
        .title(tr!("Matching"))
        .description(tr!("Quality-aware keeps looking for synced lyrics before settling for plain text. Strict takes the first hit of any kind."))
        .build();
    page.add(&match_group);
    let match_keys = [prefs::MATCH_QUALITY, prefs::MATCH_STRICT];
    let selected = match_keys.iter().position(|k| *k == lyrics.prefs().match_mode()).unwrap_or(0);
    let match_row = combo_row(&tr!("When to Stop Searching"), "", &translated(&MATCH_LABELS), selected);
    {
        let lyrics = lyrics.clone();
        match_row.connect_selected_notify(move |row| {
            if let Some(key) = match_keys.get(row.selected() as usize) {
                lyrics.prefs().set_match_mode(key);
            }
        });
    }
    match_group.add(&match_row);


    // -- second line ----------------------------------------------------------
    let display_group = adw::PreferencesGroup::builder()
        .title(tr!("Second Line"))
        .description(tr!("An extra line under each lyric. Auto picks a romanization for non-Latin scripts and background vocals otherwise. What's available depends on the provider."))
        .build();
    page.add(&display_group);
    let selected = prefs::SECOND_LINE_MODES.iter().position(|k| *k == lyrics.prefs().second_line_mode()).unwrap_or(1);
    // Translators: row title, followed by what the second lyric line shows: Off, Auto, Romanization, Translation or Background.
    let second_row = combo_row(&tr!("Show"), "", &translated(&SECOND_LINE_LABELS), selected);
    {
        let lyrics = lyrics.clone();
        let apply = apply.clone();
        second_row.connect_selected_notify(move |row| {
            if let Some(key) = prefs::SECOND_LINE_MODES.get(row.selected() as usize) {
                lyrics.prefs().set_second_line_mode(key);
                apply();
            }
        });
    }
    display_group.add(&second_row);

    // -- effects -----------------------------------------------------------------
    let effects_group = adw::PreferencesGroup::builder()
        .title(tr!("Effects"))
        .description(tr!("Subtle fades each word in over the time it's actually held and grows the active line. Full adds a glow on the active line and blurs the lines furthest from it."))
        .build();
    page.add(&effects_group);
    let selected = prefs::EFFECTS_LEVELS.iter().position(|k| *k == lyrics.prefs().effects_level()).unwrap_or(2);
    // Translators: how strong the lyric effects are: Off, Subtle or Full.
    let effect_row = combo_row(&tr!("Level"), "", &translated(&EFFECT_LABELS), selected);
    effects_group.add(&effect_row);

    let sweep_row = switch_row(&tr!("Emulate Word Timing"), &tr!("On sources with no word timing, move the highlight across the line instead of lighting the whole line at once"), lyrics.prefs().line_sweep());
    {
        let lyrics = lyrics.clone();
        let apply = apply.clone();
        sweep_row.connect_active_notify(move |row| {
            lyrics.prefs().set_line_sweep(row.is_active());
            apply();
        });
    }
    effects_group.add(&sweep_row);

    // -- text size -------------------------------------------------------------------
    let size_group = adw::PreferencesGroup::builder()
        .title(tr!("Text Size"))
        .description(tr!("Resting size of the lyric column, and how much bigger the line being sung is drawn. The active line is scaled when it is painted, so growing it never changes the row's height or disturbs the scrolling."))
        .build();
    page.add(&size_group);

    let (base_row, base_scale) = scale_row(&tr!("Lyrics Size"), "", (prefs::FONT_SCALE_MIN, prefs::FONT_SCALE_MAX, 0.05), lyrics.prefs().font_scale(), 2);
    base_scale.add_mark(prefs::FONT_SCALE_DEFAULT, gtk::PositionType::Bottom, None);
    {
        let lyrics = lyrics.clone();
        let apply = apply.clone();
        base_scale.connect_value_changed(move |scale| {
            lyrics.prefs().set_font_scale(scale.value());
            apply();
        });
    }
    size_group.add(&base_row);

    let (grown_row, grown_scale) = scale_row(&tr!("Active Line Size"), "", (prefs::ACTIVE_SCALE_MIN, prefs::ACTIVE_SCALE_MAX, 0.01), lyrics.prefs().active_scale(), 2);
    grown_scale.add_mark(prefs::ACTIVE_SCALE_DEFAULT, gtk::PositionType::Bottom, None);
    {
        let lyrics = lyrics.clone();
        let apply = apply.clone();
        grown_scale.connect_value_changed(move |scale| {
            lyrics.prefs().set_active_scale(scale.value());
            apply();
        });
    }
    grown_row.set_sensitive(lyrics.prefs().effects_level() != "off");
    size_group.add(&grown_row);

    {
        let lyrics = lyrics.clone();
        effect_row.connect_selected_notify(move |row| {
            if let Some(key) = prefs::EFFECTS_LEVELS.get(row.selected() as usize) {
                lyrics.prefs().set_effects_level(key);
                grown_row.set_sensitive(*key != "off");
                apply();
            }
        });
    }
    // -- cache -------------------------------------------------------------
    // Styled like Reset Mixtapes: its own group, a destructive button row.
    let cache_group = adw::PreferencesGroup::builder().description(tr!("Queue changes only apply to tracks that aren't cached yet")).build();
    let clear_row = adw::ButtonRow::builder().title(tr!("Clear Cached Lyrics")).end_icon_name("user-trash-symbolic").build();
    // The builder would replace the button class the row styles itself with.
    clear_row.add_css_class("destructive-action");
    {
        let ctx = ctx.clone();
        let win = Rc::downgrade(win);
        clear_row.connect_activated(move |_| {
            let removed = ctx.lyrics.cache().clear_all();
            let Some(win) = win.upgrade() else { return };
            win.add_toast(&if removed > 0 { trn!("Cleared {n} cached track(s)", "Cleared {n} cached track(s)", removed) } else { tr!("No cached lyrics to clear") });
            for view in win.lyrics_views() {
                view.refresh();
            }
        });
    }
    cache_group.add(&clear_row);
    page.add(&cache_group);

    page
}

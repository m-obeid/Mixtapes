//! The preferences dialog. Port of show_preferences and its group builders in
//! ui/window.py. Every switch writes the prefs.json both apps share and
//! applies the change live where Python did.

use std::cell::Cell;
use std::rc::Rc;
use std::time::{Duration, Instant};

use adw::prelude::*;
use gtk::{gdk, glib};
use serde_json::Value;

use crate::App;
use crate::discord::{STATUS_DISPLAY_DEFAULT, STATUS_DISPLAY_KEYS};
use crate::downloads::naming;
use crate::scrobbler::{SERVICES, Service};
use crate::ui::cover::load_texture;
use crate::ui::window::{AppearancePref, MainWindow};

const RENDERERS: [(&str, &str); 5] = [("default", "Default (recommended)"), ("ngl", "NGL"), ("gl", "Legacy GL"), ("vulkan", "Vulkan"), ("cairo", "Cairo (Software)")];
const HISTORY_MODES: [(&str, &str); 3] = [("immediate", "Immediately"), ("after_30s", "After 30 seconds"), ("never", "Never")];
const FORMAT_LABELS: [&str; 5] = ["Opus (smallest)", "MP3 (universal)", "M4A (Apple)", "FLAC (lossless)", "OGG (Vorbis)"];
const STRUCTURE_LABELS: [&str; 3] = ["Artist / Album / Song", "Artist / Song", "No folders"];
const DISPLAY_LABELS: [&str; 3] = ["App Name (Mixtapes)", "Artist", "Song Title"];
/// How long the Last.fm approval dialog waits for the browser.
const LASTFM_APPROVAL_WINDOW: Duration = Duration::from_secs(300);
const LASTFM_POLL: Duration = Duration::from_secs(2);

pub fn present(win: &Rc<MainWindow>, ctx: &Rc<App>) -> adw::PreferencesDialog {
    let dialog = adw::PreferencesDialog::new();
    let page = adw::PreferencesPage::builder().title("General").icon_name("preferences-system-symbolic").build();
    dialog.add(&page);

    page.add(&account_group(win, ctx, &dialog));
    page.add(&application_group(win, ctx));
    page.add(&appearance_group(win, ctx));
    page.add(&visualizer_group(win, ctx));
    page.add(&discord_group(ctx));
    page.add(&scrobbler_group(win, ctx, &dialog));
    page.add(&downloads_group(win, ctx));
    dialog.add(&crate::ui::preferences_lyrics::build_page(win, ctx));

    dialog.present(Some(win.window()));
    dialog
}

// -- small builders ---------------------------------------------------------

fn save(ctx: &App, key: &str, value: impl Into<Value>) {
    let value = value.into();
    ctx.paths.update_prefs(|p| {
        p.insert(key.to_owned(), value);
    });
}

fn pref_bool(ctx: &App, key: &str, default: bool) -> bool {
    ctx.paths.read_prefs().get(key).and_then(Value::as_bool).unwrap_or(default)
}

fn pref_str(ctx: &App, key: &str, default: &str) -> String {
    ctx.paths.read_prefs().get(key).and_then(Value::as_str).unwrap_or(default).to_owned()
}

pub(super) fn switch_row(title: &str, subtitle: &str, active: bool) -> adw::SwitchRow {
    adw::SwitchRow::builder().title(title).subtitle(subtitle).active(active).build()
}

/// A combo row over fixed labels with `selected` already in place, so
/// connecting afterwards never fires for the initial value.
pub(super) fn combo_row(title: &str, subtitle: &str, labels: &[&str], selected: usize) -> adw::ComboRow {
    let row = adw::ComboRow::builder().title(title).subtitle(subtitle).model(&gtk::StringList::new(labels)).build();
    row.set_selected(selected as u32);
    row
}

/// The 220 px value slider every numeric setting uses.
pub(super) fn scale_row(title: &str, subtitle: &str, (min, max, step): (f64, f64, f64), value: f64, digits: i32) -> (adw::ActionRow, gtk::Scale) {
    let row = adw::ActionRow::builder().title(title).subtitle(subtitle).build();
    let scale = gtk::Scale::with_range(gtk::Orientation::Horizontal, min, max, step);
    scale.set_value(value);
    scale.set_draw_value(true);
    scale.set_value_pos(gtk::PositionType::Right);
    scale.set_digits(digits);
    scale.set_size_request(220, -1);
    scale.set_valign(gtk::Align::Center);
    scale.set_hexpand(false);
    row.add_suffix(&scale);
    (row, scale)
}

// -- account ------------------------------------------------------------------

fn account_group(win: &Rc<MainWindow>, ctx: &Rc<App>, dialog: &adw::PreferencesDialog) -> adw::PreferencesGroup {
    let group = adw::PreferencesGroup::builder().title("Account").build();
    let row = adw::ActionRow::new();
    let avatar = adw::Avatar::new(40, None, false);
    row.add_prefix(&avatar);

    let state = ctx.player.state();
    let authed = state.authenticated();
    let button = gtk::Button::builder().label(if authed { "Sign Out" } else { "Sign In" }).valign(gtk::Align::Center).build();
    button.add_css_class(if authed { "destructive-action" } else { "suggested-action" });
    {
        let dialog = dialog.downgrade();
        let win = Rc::downgrade(win);
        button.connect_clicked(move |_| {
            if let Some(dialog) = dialog.upgrade() {
                dialog.close();
            }
            let Some(win) = win.upgrade() else { return };
            if authed {
                // The auth watcher clears the library and offers the login dialog again.
                let _ = gtk::prelude::WidgetExt::activate_action(win.window(), "win.logout", None);
            } else {
                win.show_login();
            }
        });
    }
    row.add_suffix(&button);

    if !authed {
        row.set_title("Not signed in");
        row.set_subtitle("Sign in to YouTube Music to access your library");
        group.add(&row);
        return group;
    }

    let name = state.account_name();
    let name = if name.is_empty() { "Signed in".to_owned() } else { name };
    let handle = state.account_handle();
    row.set_title(&glib::markup_escape_text(&name));
    row.set_subtitle(&glib::markup_escape_text(if handle.is_empty() { "YouTube Music account" } else { &handle }));
    avatar.set_text(Some(&name));
    avatar.set_show_initials(true);
    let photo = state.account_photo_url();
    if !photo.is_empty() {
        let net = ctx.net.clone();
        let avatar = avatar.downgrade();
        glib::spawn_future_local(async move {
            let texture = load_texture(&net, &photo, None).await;
            if let (Some(avatar), Some(texture)) = (avatar.upgrade(), texture) {
                avatar.set_custom_image(Some(texture.upcast_ref::<gdk::Paintable>()));
            }
        });
    }
    group.add(&row);
    group
}

// -- application ----------------------------------------------------------------

fn application_group(win: &Rc<MainWindow>, ctx: &Rc<App>) -> adw::PreferencesGroup {
    let group = adw::PreferencesGroup::builder().title("Application").build();

    let debug_row = switch_row("Enable Debug Logs", "Print diagnostic information to the terminal", crate::bootstrap::debug_logs(&ctx.paths));
    {
        let ctx = ctx.clone();
        debug_row.connect_active_notify(move |row| crate::bootstrap::set_debug_logs(&ctx.paths, row.is_active()));
    }
    group.add(&debug_row);

    let stream_row = adw::ActionRow::builder().title("Stream Info (Debug)").subtitle("Show format, protocol and seek range of the current stream").activatable(true).build();
    stream_row.add_suffix(&gtk::Image::from_icon_name("go-next-symbolic"));
    {
        let win = Rc::downgrade(win);
        stream_row.connect_activated(move |_| {
            if let Some(win) = win.upgrade() {
                win.show_stream_info();
            }
        });
    }
    group.add(&stream_row);

    let offline_row = switch_row("Force Offline Mode", "Disable all network requests and use only downloaded content", pref_bool(ctx, "force_offline", false));
    {
        let ctx = ctx.clone();
        let win = Rc::downgrade(win);
        offline_row.connect_active_notify(move |row| {
            save(&ctx, "force_offline", row.is_active());
            if let Some(win) = win.upgrade() {
                win.force_offline_changed();
            }
        });
    }
    group.add(&offline_row);

    let background_row = switch_row("Background Playback", "Allow music to keep playing when the window is closed", pref_bool(ctx, "background_play", true));
    {
        let ctx = ctx.clone();
        background_row.connect_active_notify(move |row| save(&ctx, "background_play", row.is_active()));
    }
    group.add(&background_row);

    let sidebar_row = switch_row("Sidebar on the Right", "Place the queue sidebar on the right edge", pref_str(ctx, "sidebar_position", "left") == "right");
    {
        let ctx = ctx.clone();
        let win = Rc::downgrade(win);
        sidebar_row.connect_active_notify(move |row| {
            save(&ctx, "sidebar_position", if row.is_active() { "right" } else { "left" });
            if let Some(win) = win.upgrade() {
                win.set_sidebar_on_right(row.is_active());
            }
        });
    }
    group.add(&sidebar_row);

    // Some GPU and driver pairs crash inside the default renderer. Read at the next launch.
    let current = pref_str(ctx, "gsk_renderer", "default");
    let labels: Vec<&str> = RENDERERS.iter().map(|(_, label)| *label).collect();
    let selected = RENDERERS.iter().position(|(key, _)| *key == current).unwrap_or(0);
    let renderer_row = combo_row("Renderer", "Switch if you hit GPU-related crashes. Applies on next launch.", &labels, selected);
    {
        let ctx = ctx.clone();
        renderer_row.connect_selected_notify(move |row| {
            if let Some((key, _)) = RENDERERS.get(row.selected() as usize) {
                save(&ctx, "gsk_renderer", *key);
            }
        });
    }
    group.add(&renderer_row);

    let current = pref_str(ctx, "history_mode", "immediate");
    let labels: Vec<&str> = HISTORY_MODES.iter().map(|(_, label)| *label).collect();
    let selected = HISTORY_MODES.iter().position(|(key, _)| *key == current).unwrap_or(0);
    let history_row = combo_row("Record Plays to History", "When Mixtapes should tell YouTube Music a song was played", &labels, selected);
    {
        let ctx = ctx.clone();
        history_row.connect_selected_notify(move |row| {
            if let Some((key, _)) = HISTORY_MODES.get(row.selected() as usize) {
                save(&ctx, "history_mode", *key);
                // The next track respects the new mode without a restart.
                ctx.player.set_history_mode(key);
            }
        });
    }
    group.add(&history_row);
    group
}

// -- appearance -------------------------------------------------------------------

fn appearance_group(win: &Rc<MainWindow>, ctx: &Rc<App>) -> adw::PreferencesGroup {
    let group = adw::PreferencesGroup::builder().title("Appearance").build();

    let blur_row = switch_row("Blurred Cover Background", "Use the current track's cover as a blurred window background", pref_bool(ctx, "blurred_background", false));
    {
        let ctx = ctx.clone();
        let win = Rc::downgrade(win);
        blur_row.connect_active_notify(move |row| {
            save(&ctx, "blurred_background", row.is_active());
            if let Some(win) = win.upgrade() {
                win.appearance_pref_changed(AppearancePref::BlurredBackground);
            }
        });
    }
    group.add(&blur_row);

    let dynamic = pref_bool(ctx, "dynamic_accent", false);
    let accent_row = adw::ExpanderRow::builder()
        .title("Dynamic Cover Color")
        .subtitle("Match the app accent color to the current track's cover")
        .show_enable_switch(true)
        .enable_expansion(dynamic)
        .expanded(dynamic)
        .build();
    let tinted_row = switch_row("Tinted Background", "Tint the app background with accent color", pref_bool(ctx, "tinted_background", false));
    {
        let ctx = ctx.clone();
        let win = Rc::downgrade(win);
        accent_row.connect_enable_expansion_notify(move |row| {
            let on = row.enables_expansion();
            row.set_expanded(on);
            save(&ctx, "dynamic_accent", on);
            if let Some(win) = win.upgrade() {
                win.appearance_pref_changed(AppearancePref::DynamicAccent);
            }
        });
    }
    {
        let ctx = ctx.clone();
        let win = Rc::downgrade(win);
        tinted_row.connect_active_notify(move |row| {
            save(&ctx, "tinted_background", row.is_active());
            if let Some(win) = win.upgrade() {
                win.appearance_pref_changed(AppearancePref::TintedBackground);
            }
        });
    }
    group.add(&accent_row);
    accent_row.add_row(&tinted_row);
    group
}

// -- visualizer ---------------------------------------------------------------------

fn visualizer_group(win: &Rc<MainWindow>, ctx: &Rc<App>) -> adw::PreferencesGroup {
    let group = adw::PreferencesGroup::builder().title("Visualizer").description("Bar visualizer beneath the cover art in the expanded player").build();
    let prefs = ctx.paths.read_prefs();
    let enabled = prefs.get("visualizer_enabled").and_then(Value::as_bool).unwrap_or(true);
    let bars = prefs.get("visualizer_bars").and_then(Value::as_f64).unwrap_or(56.0).clamp(8.0, 100.0);
    let smoothing = prefs.get("visualizer_smoothing").and_then(Value::as_f64).unwrap_or(1.5).clamp(1.05, 3.0);

    let enabled_row = switch_row("Enable Visualizer", "Show audio bars beneath the cover art", enabled);
    let (bars_row, bars_scale) = scale_row("Bar Count", "Number of bars in the visualizer (more = finer)", (16.0, 100.0, 4.0), bars, 0);
    let (smooth_row, smooth_scale) = scale_row("Smoothing", "Higher = tighter spikes, lower = peaks bleed into neighbors", (1.1, 3.0, 0.05), smoothing, 2);
    bars_row.set_sensitive(enabled);
    smooth_row.set_sensitive(enabled);

    {
        let ctx = ctx.clone();
        let win = Rc::downgrade(win);
        let (bars_row, smooth_row) = (bars_row.clone(), smooth_row.clone());
        enabled_row.connect_active_notify(move |row| {
            let on = row.is_active();
            save(&ctx, "visualizer_enabled", on);
            if let Some(win) = win.upgrade() {
                for viz in win.visualizers() {
                    viz.widget().set_visible(on);
                }
            }
            bars_row.set_sensitive(on);
            smooth_row.set_sensitive(on);
        });
    }
    {
        let ctx = ctx.clone();
        let win = Rc::downgrade(win);
        bars_scale.connect_value_changed(move |scale| {
            let n = scale.value() as u64;
            save(&ctx, "visualizer_bars", n);
            if let Some(win) = win.upgrade() {
                for viz in win.visualizers() {
                    viz.set_bar_count(n as usize);
                }
            }
        });
    }
    {
        let ctx = ctx.clone();
        let win = Rc::downgrade(win);
        smooth_scale.connect_value_changed(move |scale| {
            let v = scale.value();
            save(&ctx, "visualizer_smoothing", v);
            if let Some(win) = win.upgrade() {
                for viz in win.visualizers() {
                    viz.set_smoothing(v);
                }
            }
        });
    }
    group.add(&enabled_row);
    group.add(&bars_row);
    group.add(&smooth_row);
    group
}

// -- Discord ------------------------------------------------------------------------

fn discord_group(ctx: &Rc<App>) -> adw::PreferencesGroup {
    let group = adw::PreferencesGroup::builder().title("Discord Rich Presence").build();

    let status_row = adw::ActionRow::builder().title("Connection Status").build();
    let status_label = gtk::Label::builder().label(ctx.discord.status()).valign(gtk::Align::Center).css_classes(["dim-label"]).build();
    status_row.add_suffix(&status_label);
    group.add(&status_row);

    let enabled = pref_bool(ctx, "discord_rpc_enabled", true);
    let enabled_row = switch_row("Enable Discord RPC", "Show what you're listening to on Discord", enabled);
    let current = pref_str(ctx, "discord_rpc_status_display", STATUS_DISPLAY_DEFAULT);
    let selected = STATUS_DISPLAY_KEYS.iter().position(|key| *key == current).unwrap_or(1);
    let display_row = combo_row("Status Display", "What appears in the status line under your name", &DISPLAY_LABELS, selected);
    let hide_pause_row = switch_row("Hide on Pause", "Hide Discord RPC when music is paused", pref_bool(ctx, "discord_rpc_hide_pause_enabled", false));
    let small_icon_row = switch_row("Show Play/Pause Icon", "Display a small play or pause indicator on the album art", pref_bool(ctx, "discord_rpc_small_icon_enabled", true));
    for row in [display_row.upcast_ref::<gtk::Widget>(), hide_pause_row.upcast_ref(), small_icon_row.upcast_ref()] {
        row.set_sensitive(enabled);
    }

    {
        let ctx = ctx.clone();
        let status_label = status_label.clone();
        let (display_row, small_icon_row) = (display_row.clone(), small_icon_row.clone());
        enabled_row.connect_active_notify(move |row| {
            let on = row.is_active();
            save(&ctx, "discord_rpc_enabled", on);
            display_row.set_sensitive(on);
            small_icon_row.set_sensitive(on);
            ctx.discord.set_enabled(on);
            if on {
                crate::presence::update_discord(&ctx);
            }
            status_label.set_label(&ctx.discord.status());
        });
    }
    {
        let ctx = ctx.clone();
        display_row.connect_selected_notify(move |row| {
            if let Some(key) = STATUS_DISPLAY_KEYS.get(row.selected() as usize) {
                save(&ctx, "discord_rpc_status_display", *key);
                crate::presence::update_discord(&ctx);
            }
        });
    }
    for (row, key) in [(&hide_pause_row, "discord_rpc_hide_pause_enabled"), (&small_icon_row, "discord_rpc_small_icon_enabled")] {
        let ctx = ctx.clone();
        row.connect_active_notify(move |row| {
            save(&ctx, key, row.is_active());
            crate::presence::update_discord(&ctx);
        });
    }
    // The worker connects in the background, so keep the label honest while the dialog is open.
    {
        let ctx = Rc::downgrade(ctx);
        let label = status_label.downgrade();
        glib::timeout_add_seconds_local(1, move || match (ctx.upgrade(), label.upgrade()) {
            (Some(ctx), Some(label)) if label.root().is_some() => {
                label.set_label(&ctx.discord.status());
                glib::ControlFlow::Continue
            }
            _ => glib::ControlFlow::Break,
        });
    }

    group.add(&enabled_row);
    group.add(&display_row);
    group.add(&hide_pause_row);
    group.add(&small_icon_row);
    group
}

// -- scrobbling -----------------------------------------------------------------------

struct ScrobblerRows {
    ctx: Rc<App>,
    win: std::rc::Weak<MainWindow>,
    dialog: glib::WeakRef<adw::PreferencesDialog>,
    rows: Vec<(Service, adw::ActionRow, gtk::Button)>,
    pending_row: adw::ActionRow,
}

impl ScrobblerRows {
    fn toast(&self, message: &str) {
        if let Some(dialog) = self.dialog.upgrade() {
            dialog.add_toast(adw::Toast::new(message));
        }
    }

    fn refresh(&self) {
        let scrobbler = &self.ctx.scrobbler;
        for (service, row, button) in &self.rows {
            if *service == Service::LastFm && !scrobbler.lastfm_configured() {
                row.set_subtitle("This build ships without Last.fm API credentials");
                button.set_label("Connect");
                button.set_sensitive(false);
                continue;
            }
            button.set_sensitive(true);
            if scrobbler.is_connected(*service) {
                let name = scrobbler.username(*service);
                let subtitle = if name.is_empty() { "Connected".to_owned() } else { format!("Connected as {name}") };
                row.set_subtitle(&glib::markup_escape_text(&subtitle));
                button.set_label("Disconnect");
                button.remove_css_class("suggested-action");
                button.add_css_class("destructive-action");
            } else {
                let error = scrobbler.last_error();
                let subtitle = if error.starts_with(service.label()) { error } else { "Not connected".to_owned() };
                row.set_subtitle(&glib::markup_escape_text(&subtitle));
                button.set_label("Connect");
                button.remove_css_class("destructive-action");
                button.add_css_class("suggested-action");
            }
        }
        let waiting = scrobbler.pending_count();
        self.pending_row.set_visible(waiting > 0);
        let noun = if waiting == 1 { "play" } else { "plays" };
        self.pending_row.set_subtitle(&format!("{waiting} {noun} saved while offline, retried automatically"));
    }

    fn open_uri(self: &Rc<Self>, uri: &str) {
        let Some(win) = self.win.upgrade() else { return };
        let this = Rc::downgrade(self);
        let uri = uri.to_owned();
        gtk::UriLauncher::new(&uri).launch(Some(win.window()), gtk::gio::Cancellable::NONE, move |result| {
            if let Err(err) = result {
                tracing::warn!(%uri, %err, "could not open the browser");
                if let Some(this) = this.upgrade() {
                    this.toast("Could not open your browser");
                }
            }
        });
    }

    fn clicked(self: &Rc<Self>, service: Service) {
        if self.ctx.scrobbler.is_connected(service) {
            self.ctx.scrobbler.disconnect(service);
            self.refresh();
            self.toast(&format!("Disconnected from {}", service.label()));
            return;
        }
        match service {
            Service::LastFm => self.lastfm_connect(),
            Service::ListenBrainz => self.listenbrainz_connect(),
        }
    }

    fn lastfm_connect(self: &Rc<Self>) {
        if let Some((_, _, button)) = self.rows.iter().find(|(s, _, _)| *s == Service::LastFm) {
            button.set_sensitive(false);
        }
        let this = self.clone();
        glib::spawn_future_local(async move {
            let scrobbler = this.ctx.scrobbler.clone();
            let asked = this.ctx.net.spawn(async move { scrobbler.lastfm_request_token().await }).await;
            this.refresh();
            match asked {
                Ok(Ok((token, url))) => this.await_lastfm_approval(token, url).await,
                Ok(Err(err)) => this.toast(&format!("Last.fm: {err}")),
                Err(err) => this.toast(&format!("Last.fm: {err}")),
            }
        });
    }

    /// Show the waiting dialog, open the browser, and poll until Last.fm confirms.
    async fn await_lastfm_approval(self: &Rc<Self>, token: String, url: String) {
        let Some(win) = self.win.upgrade() else { return };
        let dialog = adw::AlertDialog::builder()
            .heading("Authorize Mixtapes")
            .body("Approve access in the browser tab that just opened. This closes on its own once Last.fm confirms.")
            .close_response("cancel")
            .build();
        dialog.add_response("cancel", "Cancel");
        let spinner = adw::Spinner::builder().width_request(32).height_request(32).margin_top(6).build();
        dialog.set_extra_child(Some(&spinner));
        let done = Rc::new(Cell::new(false));
        {
            let done = done.clone();
            dialog.connect_response(None, move |_, _| done.set(true));
        }
        dialog.present(Some(win.window()));
        self.open_uri(&url);

        let deadline = Instant::now() + LASTFM_APPROVAL_WINDOW;
        while !done.get() && Instant::now() < deadline {
            glib::timeout_future(LASTFM_POLL).await;
            if done.get() {
                return;
            }
            let scrobbler = self.ctx.scrobbler.clone();
            let token = token.clone();
            // An error here means the user has not pressed Allow yet.
            let Ok(Ok(name)) = self.ctx.net.spawn(async move { scrobbler.lastfm_finish_auth(&token).await }).await else { continue };
            done.set(true);
            dialog.close();
            self.refresh();
            self.toast(&if name.is_empty() { "Connected to Last.fm".to_owned() } else { format!("Scrobbling to Last.fm as {name}") });
            return;
        }
        if !done.replace(true) {
            dialog.close();
            self.toast("Last.fm authorization timed out");
        }
    }

    fn listenbrainz_connect(self: &Rc<Self>) {
        let Some(win) = self.win.upgrade() else { return };
        let dialog = adw::AlertDialog::builder()
            .heading("Connect ListenBrainz")
            .body("Paste the user token from your ListenBrainz settings.")
            .default_response("connect")
            .close_response("cancel")
            .build();
        let entry = adw::PasswordEntryRow::builder().title("User Token").build();
        let listbox = gtk::ListBox::builder().selection_mode(gtk::SelectionMode::None).css_classes(["boxed-list", "songs-list"]).build();
        listbox.append(&entry);
        let link = gtk::Button::builder().label("Get Your Token").halign(gtk::Align::Center).build();
        link.add_css_class("flat");
        {
            let this = Rc::downgrade(self);
            link.connect_clicked(move |_| {
                if let Some(this) = this.upgrade() {
                    this.open_uri("https://listenbrainz.org/settings/");
                }
            });
        }
        let content = gtk::Box::builder().orientation(gtk::Orientation::Vertical).spacing(8).margin_top(6).build();
        content.append(&listbox);
        content.append(&link);
        dialog.set_extra_child(Some(&content));
        dialog.add_response("cancel", "Cancel");
        dialog.add_response("connect", "Connect");
        dialog.set_response_appearance("connect", adw::ResponseAppearance::Suggested);

        let this = self.clone();
        dialog.connect_response(None, move |_, response| {
            if response != "connect" {
                return;
            }
            let token = entry.text().trim().to_owned();
            let this = this.clone();
            glib::spawn_future_local(async move {
                let scrobbler = this.ctx.scrobbler.clone();
                match this.ctx.net.spawn(async move { scrobbler.listenbrainz_connect(&token).await }).await {
                    Ok(Ok(name)) => {
                        this.refresh();
                        this.toast(&if name.is_empty() { "Connected to ListenBrainz".to_owned() } else { format!("Scrobbling to ListenBrainz as {name}") });
                    }
                    Ok(Err(err)) => this.toast(&format!("ListenBrainz: {err}")),
                    Err(err) => this.toast(&format!("ListenBrainz: {err}")),
                }
            });
        });
        dialog.present(Some(win.window()));
    }
}

fn scrobbler_group(win: &Rc<MainWindow>, ctx: &Rc<App>, dialog: &adw::PreferencesDialog) -> adw::PreferencesGroup {
    let group = adw::PreferencesGroup::builder().title("Scrobbling").description("Submit the tracks you play to Last.fm and ListenBrainz").build();

    let enabled_row = switch_row("Enable Scrobbling", "Submit a play once you've heard half a track, or four minutes", ctx.scrobbler.enabled());
    {
        let ctx = ctx.clone();
        enabled_row.connect_active_notify(move |row| {
            save(&ctx, "scrobble_enabled", row.is_active());
            ctx.scrobbler.set_enabled(row.is_active());
        });
    }
    group.add(&enabled_row);

    let now_playing_row = switch_row("Send \"Now Playing\"", "Show the current track on your profile while it plays", pref_bool(ctx, "scrobble_now_playing", true));
    {
        let ctx = ctx.clone();
        now_playing_row.connect_active_notify(move |row| {
            save(&ctx, "scrobble_now_playing", row.is_active());
            ctx.scrobbler.set_now_playing_enabled(row.is_active());
        });
    }
    group.add(&now_playing_row);

    let mut rows = Vec::new();
    for service in SERVICES {
        let row = adw::ActionRow::builder().title(service.label()).build();
        let button = gtk::Button::builder().valign(gtk::Align::Center).build();
        row.add_suffix(&button);
        group.add(&row);
        rows.push((service, row, button));
    }
    let pending_row = adw::ActionRow::builder().title("Queued Listens").build();
    group.add(&pending_row);

    let state = Rc::new(ScrobblerRows { ctx: ctx.clone(), win: Rc::downgrade(win), dialog: dialog.downgrade(), rows, pending_row });
    for (service, _, button) in &state.rows {
        let state = state.clone();
        let service = *service;
        button.connect_clicked(move |_| state.clicked(service));
    }
    state.refresh();
    group
}

// -- downloads ------------------------------------------------------------------------

/// Move existing downloads into the layout just chosen, and say how it went.
fn reorganize(win: &Rc<MainWindow>, ctx: &Rc<App>, saved: &str) {
    if ctx.downloads.progress().is_some() {
        win.add_toast(&format!("{saved} saved. Existing files will be reorganized after downloads finish."));
        return;
    }
    win.add_toast("Reorganizing downloads...");
    let downloads = ctx.downloads.clone();
    let handle = ctx.net.spawn(async move { tokio::task::spawn_blocking(move || downloads.migrate_layout()).await });
    let win = Rc::downgrade(win);
    glib::spawn_future_local(async move {
        let Ok(Ok((moved, errors))) = handle.await else { return };
        let message = match (moved, errors) {
            (0, 0) => "Downloads already organized".to_owned(),
            (_, 0) => format!("Reorganized {moved} file(s)"),
            _ => format!("Reorganized {moved} file(s); {errors} skipped"),
        };
        if let Some(win) = win.upgrade() {
            win.add_toast(&message);
        }
    });
}

fn downloads_group(win: &Rc<MainWindow>, ctx: &Rc<App>) -> adw::PreferencesGroup {
    let group = adw::PreferencesGroup::builder().title("Downloads").build();

    let current = naming::preferred_format(&ctx.paths);
    let selected = naming::FORMATS.iter().position(|(name, _)| *name == current).unwrap_or(0);
    let subtitle = format!("Songs are saved to {}", ctx.paths.music_dir().display());
    let format_row = combo_row("Audio Format", &glib::markup_escape_text(&subtitle), &FORMAT_LABELS, selected);
    {
        let ctx = ctx.clone();
        format_row.connect_selected_notify(move |row| {
            if let Some((name, _)) = naming::FORMATS.get(row.selected() as usize) {
                save(&ctx, "download_format", *name);
            }
        });
    }
    group.add(&format_row);

    let current = naming::folder_structure(&ctx.paths);
    let selected = naming::FOLDER_STRUCTURES.iter().position(|name| *name == current).unwrap_or(0);
    let structure_row = combo_row("Folder Structure", "How new downloads are organized on disk", &STRUCTURE_LABELS, selected);
    {
        let ctx = ctx.clone();
        let win = Rc::downgrade(win);
        structure_row.connect_selected_notify(move |row| {
            let Some(name) = naming::FOLDER_STRUCTURES.get(row.selected() as usize) else { return };
            if naming::folder_structure(&ctx.paths) == *name {
                return;
            }
            save(&ctx, "download_folder_structure", *name);
            if let Some(win) = win.upgrade() {
                reorganize(&win, &ctx, "Structure");
            }
        });
    }
    group.add(&structure_row);

    let subdir_row = switch_row("Use Songs Subfolder", "Place downloads inside a Songs/ subfolder within the music directory", naming::use_songs_subdir(&ctx.paths));
    {
        let ctx = ctx.clone();
        let win = Rc::downgrade(win);
        subdir_row.connect_active_notify(move |row| {
            save(&ctx, "use_songs_subdir", row.is_active());
            if let Some(win) = win.upgrade() {
                reorganize(&win, &ctx, "Subfolder setting");
            }
        });
    }
    group.add(&subdir_row);
    group
}

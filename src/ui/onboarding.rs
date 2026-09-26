//! The first-launch wizard: welcome, sign in or skip, a few popular settings,
//! done. Replaces the bare login window Python opened on a fresh install. The
//! login dialog itself is unchanged and opens from the account step.

use std::rc::Rc;

use adw::prelude::*;
use gtk::glib;

use crate::App;
use crate::net::ytmusic::AuthState;
use crate::ui::login::LoginDialog;
use crate::ui::preferences::{pref_bool, save, switch_row};
use crate::ui::release_notes;
use crate::ui::window::{AppearancePref, MainWindow};

pub const DONE_PREF: &str = "onboarding_done";

/// A fresh install has no saved session and never declined the sign-in. A
/// signed-in user updating from an older build counts as set up already, so
/// the wizard never opens over an existing account. Startup writes a default
/// or two into prefs.json before this runs, so an empty file is no test.
pub fn pending(ctx: &App) -> bool {
    let prefs = ctx.paths.read_prefs();
    if prefs.get(DONE_PREF).and_then(|v| v.as_bool()).unwrap_or(false) {
        return false;
    }
    let declined = prefs.get(crate::ui::login::LOGIN_SKIPPED_PREF).and_then(|v| v.as_bool()).unwrap_or(false);
    let fresh = !declined && matches!(ctx.net.client().auth_state(), AuthState::Anonymous);
    if !fresh {
        finish(ctx);
    }
    fresh
}

fn finish(ctx: &App) {
    save(ctx, DONE_PREF, true);
    release_notes::mark_seen(ctx);
    // Leaving the wizard signed out is a choice, so startup does not ask again.
    if matches!(ctx.net.client().auth_state(), AuthState::Anonymous) {
        crate::ui::login::set_login_skipped(&ctx.paths, true);
    }
}

struct Wizard {
    dialog: adw::Dialog,
    nav: adw::NavigationView,
    win: Rc<MainWindow>,
    ctx: Rc<App>,
}

/// `start` opens on a later step, for screenshots.
pub fn present(win: &Rc<MainWindow>, ctx: &Rc<App>, start: Option<&str>) -> adw::Dialog {
    let nav = adw::NavigationView::new();
    // Sized to its content: a taller dialog left the status pages floating in empty space.
    let dialog = adw::Dialog::builder().title("Welcome").content_width(460).content_height(540).child(&nav).build();
    let wizard = Rc::new(Wizard { dialog: dialog.clone(), nav, win: win.clone(), ctx: ctx.clone() });
    wizard.nav.add(&wizard.welcome_page());
    wizard.nav.add(&wizard.account_page());
    wizard.nav.add(&wizard.extras_page());
    wizard.nav.add(&wizard.done_page());
    {
        let ctx = ctx.clone();
        // Closing the dialog at any step counts as finished. The prefs keep their defaults.
        dialog.connect_closed(move |_| finish(&ctx));
    }
    if let Some(tag) = start {
        wizard.nav.push_by_tag(tag);
    }
    dialog.present(Some(win.window()));
    // The wizard lives as long as its dialog.
    let holder = wizard.clone();
    dialog.connect_closed(move |_| {
        let _ = &holder;
    });
    dialog
}

impl Wizard {
    fn page(&self, tag: &str, title: &str, content: &impl IsA<gtk::Widget>) -> adw::NavigationPage {
        let toolbar = adw::ToolbarView::new();
        toolbar.add_top_bar(&adw::HeaderBar::builder().show_title(false).build());
        toolbar.set_content(Some(content));
        adw::NavigationPage::builder().tag(tag).title(title).child(&toolbar).build()
    }

    fn status(&self, icon: &str, title: &str, description: &str) -> adw::StatusPage {
        // The compact style drops the page's own side padding, so the margins give it back.
        let page = adw::StatusPage::builder().icon_name(icon).title(title).css_classes(["compact"]).margin_start(24).margin_end(24).build();
        if !description.is_empty() {
            page.set_description(Some(description));
        }
        page
    }

    fn pill(&self, label: &str, suggested: bool) -> gtk::Button {
        let button = gtk::Button::builder().label(label).halign(gtk::Align::Center).build();
        button.add_css_class("pill");
        if suggested {
            button.add_css_class("suggested-action");
        }
        button
    }

    fn push(&self, tag: &str) {
        self.nav.push_by_tag(tag);
    }

    fn welcome_page(self: &Rc<Self>) -> adw::NavigationPage {
        let status = self.status(crate::APP_ID, "Welcome to Mixtapes", "");
        let column = gtk::Box::builder().orientation(gtk::Orientation::Vertical).spacing(12).build();
        let start = self.pill("Get Started", true);
        let weak = Rc::downgrade(self);
        start.connect_clicked(move |_| {
            if let Some(w) = weak.upgrade() {
                w.push("account");
            }
        });
        column.append(&start);
        let skip = gtk::Button::builder().label("Skip Setup").halign(gtk::Align::Center).css_classes(["flat"]).build();
        let dialog = self.dialog.clone();
        skip.connect_clicked(move |_| {
            dialog.close();
        });
        column.append(&skip);
        status.set_child(Some(&column));
        self.page("welcome", "Welcome", &status)
    }

    fn account_page(self: &Rc<Self>) -> adw::NavigationPage {
        let status = self.status("avatar-default-symbolic", "Your YouTube Music Account", "Sign in for your library, likes, playlists and uploads. Without an account you still get search, radio, downloads, and playlists and likes kept on this device.");
        let column = gtk::Box::builder().orientation(gtk::Orientation::Vertical).spacing(12).build();

        let google = self.pill("Sign in with Google", true);
        let weak = Rc::downgrade(self);
        google.connect_clicked(move |_| {
            if let Some(w) = weak.upgrade() {
                w.sign_in();
            }
        });
        column.append(&google);

        let skip = gtk::Button::builder().label("Continue Without an Account").halign(gtk::Align::Center).css_classes(["flat"]).margin_top(12).build();
        let weak = Rc::downgrade(self);
        skip.connect_clicked(move |_| {
            if let Some(w) = weak.upgrade() {
                w.push("extras");
            }
        });
        column.append(&skip);
        status.set_child(Some(&column));
        self.page("account", "Account", &status)
    }

    /// The login window opens over the wizard. A success moves on to the settings step.
    fn sign_in(self: &Rc<Self>) {
        let login = LoginDialog::new(self.win.ui().clone(), self.win.window());
        let library = self.win.library_page();
        let weak = Rc::downgrade(self);
        login.set_on_success(move || {
            library.load_library(false);
            if let Some(w) = weak.upgrade() {
                w.win.add_toast("Signed in");
                w.after_sign_in();
            }
        });
        login.present();
        let holder = login.clone();
        login.connect_close(move || {
            let _ = &holder;
        });
    }

    /// An account with brand channels gets to pick one before the settings step.
    fn after_sign_in(self: &Rc<Self>) {
        let client = self.ctx.net.client().clone();
        let handle = self.ctx.net.spawn(async move { client.accounts().await });
        let weak = Rc::downgrade(self);
        glib::spawn_future_local(async move {
            let accounts = match handle.await {
                Ok(Ok(accounts)) if accounts.len() > 1 => accounts,
                _ => Vec::new(),
            };
            let Some(w) = weak.upgrade() else { return };
            if accounts.is_empty() {
                w.push("extras");
                return;
            }
            let page = w.channel_page(accounts);
            w.nav.push(&page);
        });
    }

    fn channel_page(self: &Rc<Self>, accounts: Vec<crate::net::ytmusic::Account>) -> adw::NavigationPage {
        let status = self.status("avatar-default-symbolic", "Which Channel?", "Your Google account has more than one channel. Pick the one whose library Mixtapes should show. You can change this under Preferences.");
        let group = adw::PreferencesGroup::new();
        let current = self.ctx.net.client().channel();
        for account in accounts {
            let subtitle = account.handle.clone().or(account.byline.clone()).unwrap_or_default();
            let row = adw::ActionRow::builder().title(glib::markup_escape_text(&account.name)).subtitle(glib::markup_escape_text(&subtitle)).activatable(true).build();
            let avatar = adw::Avatar::new(32, Some(&account.name), true);
            row.add_prefix(&avatar);
            if let Some(url) = account.photo_url.clone() {
                let (net, avatar) = (self.ctx.net.clone(), avatar.downgrade());
                glib::spawn_future_local(async move {
                    if let (Some(avatar), Some(texture)) = (avatar.upgrade(), crate::ui::cover::load_texture(&net, &url, None).await) {
                        avatar.set_custom_image(Some(texture.upcast_ref::<gtk::gdk::Paintable>()));
                    }
                });
            }
            if account.page_id == current || (current.is_none() && account.selected) {
                row.add_suffix(&gtk::Image::from_icon_name("object-select-symbolic"));
            }
            let weak = Rc::downgrade(self);
            let page_id = account.page_id.clone();
            row.connect_activated(move |_| {
                if let Some(w) = weak.upgrade() {
                    w.choose_channel(page_id.clone());
                }
            });
            group.add(&row);
        }
        let column = gtk::Box::builder().orientation(gtk::Orientation::Vertical).spacing(18).build();
        column.append(&group);
        status.set_child(Some(&column));
        self.page("channel", "Channel", &status)
    }

    fn choose_channel(self: &Rc<Self>, page_id: Option<String>) {
        if self.ctx.net.client().channel() == page_id {
            self.push("extras");
            return;
        }
        save(&self.ctx, crate::net::ytmusic::CHANNEL_PREF, page_id.clone().unwrap_or_default());
        let client = self.ctx.net.client().clone();
        let handle = self.ctx.net.spawn(async move { client.set_channel(page_id).await });
        let weak = Rc::downgrade(self);
        glib::spawn_future_local(async move {
            let outcome = handle.await;
            let Some(w) = weak.upgrade() else { return };
            match outcome {
                Ok(Ok(state)) if state.is_authenticated() => {
                    w.ctx.net.caches().clear_library_ids();
                    w.win.library_page().clear();
                    w.win.library_page().load_library(false);
                }
                other => {
                    tracing::warn!(?other, "channel switch failed in the wizard");
                    w.win.add_toast("Could not switch the channel");
                }
            }
            w.push("extras");
        });
    }

    fn extras_page(self: &Rc<Self>) -> adw::NavigationPage {
        let ctx = &self.ctx;
        let column = gtk::Box::builder().orientation(gtk::Orientation::Vertical).spacing(24).margin_top(12).margin_bottom(24).margin_start(24).margin_end(24).build();
        let heading = gtk::Box::builder().orientation(gtk::Orientation::Vertical).spacing(6).margin_bottom(6).build();
        heading.append(&gtk::Label::builder().label("A Few Settings").css_classes(["title-1"]).build());
        heading.append(&gtk::Label::builder().label("Each of these lives under Preferences too.").css_classes(["dim-label"]).wrap(true).justify(gtk::Justification::Center).build());
        column.append(&heading);

        let group = adw::PreferencesGroup::new();
        let discord = switch_row("Discord Rich Presence", "Show what you're listening to on your Discord profile", pref_bool(ctx, "discord_rpc_enabled", true));
        {
            let ctx = ctx.clone();
            discord.connect_active_notify(move |row| {
                let on = row.is_active();
                save(&ctx, "discord_rpc_enabled", on);
                ctx.discord.set_enabled(on);
                if on {
                    crate::presence::update_discord(&ctx);
                }
            });
        }
        group.add(&discord);

        let scrobble = switch_row("Scrobbling", "Submit your plays to Last.fm or ListenBrainz. Connect an account under Preferences.", ctx.scrobbler.enabled());
        {
            let ctx = ctx.clone();
            scrobble.connect_active_notify(move |row| {
                save(&ctx, "scrobble_enabled", row.is_active());
                ctx.scrobbler.set_enabled(row.is_active());
            });
        }
        group.add(&scrobble);

        let accent = switch_row("Dynamic Cover Color", "Match the accent color to the cover of the playing song", pref_bool(ctx, "dynamic_accent", false));
        {
            let ctx = ctx.clone();
            let win = Rc::downgrade(&self.win);
            accent.connect_active_notify(move |row| {
                save(&ctx, "dynamic_accent", row.is_active());
                if let Some(win) = win.upgrade() {
                    win.appearance_pref_changed(AppearancePref::DynamicAccent);
                }
            });
        }
        group.add(&accent);

        let blur = switch_row("Blurred Cover Background", "Use the cover as a blurred window background", pref_bool(ctx, "blurred_background", false));
        {
            let ctx = ctx.clone();
            let win = Rc::downgrade(&self.win);
            blur.connect_active_notify(move |row| {
                save(&ctx, "blurred_background", row.is_active());
                if let Some(win) = win.upgrade() {
                    win.appearance_pref_changed(AppearancePref::BlurredBackground);
                }
            });
        }
        group.add(&blur);

        let notes = switch_row("Release Notes After Updates", "Open what's new once for each new version", pref_bool(ctx, release_notes::SHOW_PREF, true));
        {
            let ctx = ctx.clone();
            notes.connect_active_notify(move |row| save(&ctx, release_notes::SHOW_PREF, row.is_active()));
        }
        group.add(&notes);
        column.append(&group);

        let next = self.pill("Continue", true);
        let weak = Rc::downgrade(self);
        next.connect_clicked(move |_| {
            if let Some(w) = weak.upgrade() {
                w.push("done");
            }
        });
        column.append(&next);

        let clamp = adw::Clamp::builder().maximum_size(500).child(&column).build();
        let scroller = gtk::ScrolledWindow::builder().hscrollbar_policy(gtk::PolicyType::Never).child(&clamp).build();
        self.page("extras", "Settings", &scroller)
    }

    fn done_page(self: &Rc<Self>) -> adw::NavigationPage {
        let status = self.status("object-select-symbolic", "You're All Set", "Preferences, keyboard shortcuts and the release notes are in the main menu.");
        let column = gtk::Box::builder().orientation(gtk::Orientation::Vertical).spacing(12).build();
        let start = self.pill("Start Listening", true);
        let dialog = self.dialog.clone();
        start.connect_clicked(move |_| {
            dialog.close();
        });
        column.append(&start);
        // Mixtapes and my other projects are free. A quiet line and two links, no panel.
        column.append(&gtk::Label::builder().label("Mixtapes is free. If you like it, you can support my work.").css_classes(["dim-label"]).wrap(true).justify(gtk::Justification::Center).margin_top(18).build());
        column.append(&release_notes::donate_buttons());
        status.set_child(Some(&column));
        self.page("done", "Done", &status)
    }
}

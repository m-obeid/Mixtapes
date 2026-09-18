//! Mixtapes entry point.
//!
//! Boot order matters and mirrors src/main.py:
//! 1. process tunables (malloc arenas, fd limit) before any thread exists,
//! 2. logging and the GSK renderer preference before GTK loads,
//! 3. GStreamer init, the tokio runtime and the audio thread,
//! 4. the libadwaita application, which owns the GTK main loop.

mod audio;
mod bootstrap;
mod demo;
mod discord;
mod downloads;
mod lyrics;
mod model;
mod mpris;
mod net;
mod paths;
mod player;
mod presence;
mod queue;
mod scrobbler;
mod state;
mod ui;

use std::cell::RefCell;
use std::rc::Rc;
use std::sync::Arc;
use std::time::Duration;

use adw::prelude::*;
use gtk::{gdk, gio, glib};

use crate::mpris::Mpris;
use crate::paths::Paths;
use crate::player::Player;
use crate::ui::window::MainWindow;

const APP_ID: &str = "com.pocoguy.Muse";
const APP_NAME: &str = "Mixtapes";

/// Everything the UI layer receives. Cloned into signal handlers as `Rc`.
pub struct App {
    pub player: Rc<Player>,
    pub net: net::NetHandle,
    /// Offline downloads: the library on disk and the queue filling it.
    pub downloads: Arc<downloads::Downloads>,
    /// Handed to the window, which pumps it on the GTK thread.
    pub download_events: RefCell<Option<async_channel::Receiver<downloads::Event>>>,
    pub paths: Paths,
    pub demo: Option<demo::Demo>,
    pub window: RefCell<Option<Rc<MainWindow>>>,
    /// System media controls. None when the bus name could not be taken.
    pub mpris: RefCell<Option<Rc<Mpris>>>,
    /// Last.fm and ListenBrainz. Idle until a service is connected in Preferences.
    pub scrobbler: Arc<scrobbler::Scrobbler>,
    /// Lyrics providers, their cache and the display prefs.
    pub lyrics: lyrics::Lyrics,
    /// Discord Rich Presence. A no-op while Discord is not running.
    pub discord: discord::Discord,
}

fn main() -> glib::ExitCode {
    bootstrap::cap_malloc_arenas();
    bootstrap::raise_fd_limit();

    let paths = Paths::discover();
    bootstrap::init_logging(&paths);
    bootstrap::apply_gsk_renderer_pref(&paths);

    // The stylesheet and icons, compiled in by build.rs.
    if let Err(err) = gio::resources_register_include!("mixtapes.gresource") {
        eprintln!("resource bundle failed to load: {err}");
        return glib::ExitCode::FAILURE;
    }
    glib::set_application_name(APP_NAME);
    if let Err(err) = gst::init() {
        eprintln!("GStreamer init failed: {err}");
        return glib::ExitCode::FAILURE;
    }

    // Network runtime. Worker threads only run reqwest, yt-dlp subprocesses and file IO.
    let runtime = match tokio::runtime::Builder::new_multi_thread()
        .worker_threads(4)
        .thread_name("mixtapes-net")
        .enable_all()
        .build()
    {
        Ok(rt) => rt,
        Err(err) => {
            eprintln!("tokio runtime failed: {err}");
            return glib::ExitCode::FAILURE;
        }
    };

    let mut net = match net::NetHandle::new(runtime.handle().clone(), &paths) {
        Ok(net) => net,
        Err(err) => {
            eprintln!("network client failed: {err:#}");
            return glib::ExitCode::FAILURE;
        }
    };
    let demo = demo::from_env();
    if let Some(demo) = &demo {
        let inner = net.resolver().clone();
        net = net.with_resolver(Arc::new(net::stream::DemoResolver::new(inner, demo.uris.clone())));
    }

    let (audio, audio_events) = match audio::spawn() {
        Ok(pair) => pair,
        Err(err) => {
            eprintln!("audio engine failed: {err:#}");
            return glib::ExitCode::FAILURE;
        }
    };

    let (downloads, download_events) = downloads::Downloads::new(paths.clone(), net.clone());
    // A demo run plays scratch tracks. MIXTAPES_DEMO_PRESENCE=1 lets them through on purpose.
    let publish = demo.is_none() || std::env::var_os("MIXTAPES_DEMO_PRESENCE").is_some();
    let scrobbler = scrobbler::Scrobbler::start(&paths, runtime.handle());
    let lyrics = lyrics::Lyrics::new(&paths, net.client().http().clone(), net.client().clone());
    scrobbler.set_muted(!publish);
    let app_ctx = Rc::new(App {
        player: Player::new(net.clone(), downloads.clone(), audio, audio_events, &paths),
        net,
        downloads,
        download_events: RefCell::new(Some(download_events)),
        scrobbler,
        lyrics,
        discord: discord::Discord::new(publish && discord::enabled_pref(&paths.read_prefs())),
        paths,
        demo,
        window: RefCell::new(None),
        mpris: RefCell::new(None),
    });

    let app = adw::Application::builder()
        .application_id(APP_ID)
        // A demo run is its own instance. As a unique application it handed off to
        // whatever Mixtapes was already open and exited, so testing meant closing
        // the instance the listener was using.
        .flags(if app_ctx.demo.is_some() { gio::ApplicationFlags::NON_UNIQUE } else { gio::ApplicationFlags::FLAGS_NONE })
        .build();

    app.connect_startup(glib::clone!(
        #[strong]
        app_ctx,
        move |_| on_startup(&app_ctx)
    ));
    app.connect_activate(glib::clone!(
        #[strong]
        app_ctx,
        move |app| on_activate(app, &app_ctx)
    ));
    app.connect_shutdown(glib::clone!(
        #[strong]
        app_ctx,
        move |_| {
            tracing::info!("shutting down");
            if let Some(mpris) = app_ctx.mpris.borrow_mut().take() {
                mpris.shutdown();
            }
            app_ctx.scrobbler.stop();
            app_ctx.discord.stop();
            app_ctx.player.shutdown();
        }
    ));

    let code = app.run();
    runtime.shutdown_timeout(Duration::from_secs(2));
    code
}

fn on_startup(ctx: &Rc<App>) {
    if let Some(display) = gdk::Display::default() {
        let theme = gtk::IconTheme::for_display(&display);
        // GTK looks under <path>/scalable/actions and so on, so the path names the theme folder.
        theme.add_resource_path("/com/pocoguy/muse/icons/hicolor");
    }
    gtk::Window::set_default_icon_name(APP_ID);
    ui::load_css();
    ui::cover::init_disk_cache(&ctx.paths.cache_dir);

    // Event pumps must attach to the running GTK main context.
    ctx.player.start();
    ctx.mpris.replace(Some(Mpris::start(ctx)));
    presence::wire(ctx);
    tracing::info!(auth = ?ctx.net.client().auth_state(), "core started");
}

fn on_activate(app: &adw::Application, ctx: &Rc<App>) {
    if let Some(window) = app.active_window() {
        window.present();
        return;
    }
    let window = MainWindow::new(app, ctx);
    ctx.window.replace(Some(window.clone()));
    if let Some(demo) = &ctx.demo {
        demo::install(demo, ctx, &window);
    }
    window.present();
    tracing::info!("main window presented");
}

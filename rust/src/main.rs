//! Mixtapes entry point.
//!
//! Boot order matters and mirrors src/main.py:
//! 1. process tunables (malloc arenas, fd limit) before any thread exists,
//! 2. logging and the GSK renderer preference before GTK loads,
//! 3. GStreamer init, the tokio runtime and the audio thread,
//! 4. the libadwaita application, which owns the GTK main loop.
//!
//! Widgets are not ported yet. `activate` opens a placeholder window whose
//! only job is to prove the store binds and the pumps run.

mod audio;
mod bootstrap;
mod demo;
mod downloads;
mod model;
mod mpris;
mod net;
mod paths;
mod player;
mod queue;
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
}

fn main() -> glib::ExitCode {
    bootstrap::cap_malloc_arenas();
    bootstrap::raise_fd_limit();

    let paths = Paths::discover();
    bootstrap::init_logging(&paths);
    bootstrap::apply_gsk_renderer_pref(&paths);

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
    let app_ctx = Rc::new(App {
        player: Player::new(net.clone(), downloads.clone(), audio, audio_events, &paths),
        net,
        downloads,
        download_events: RefCell::new(Some(download_events)),
        paths,
        demo,
        window: RefCell::new(None),
        mpris: RefCell::new(None),
    });

    let app = adw::Application::builder()
        .application_id(APP_ID)
        .flags(gio::ApplicationFlags::FLAGS_NONE)
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
        theme.add_resource_path("/com/pocoguy/muse/icons");
        // Source checkout: project icons take priority over installed ones.
        if let Some(dir) = &ctx.paths.dev_icons_dir {
            theme.add_search_path(dir);
        }
    }
    gtk::Window::set_default_icon_name(APP_ID);
    ui::load_css();

    // Event pumps must attach to the running GTK main context.
    ctx.player.start();
    ctx.mpris.replace(Some(Mpris::start(ctx)));
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

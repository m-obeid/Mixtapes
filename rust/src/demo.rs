//! Demo queue for verifying the playback path before any page is ported.
//!
//! MIXTAPES_DEMO=1            stage a queue at startup (play stays a click away)
//! MIXTAPES_DEMO_URI=a,b      URIs or paths to use; default: audio files under ~/Music
//! MIXTAPES_DEMO_VIDEO=a,b    append real YouTube ids so yt-dlp resolution runs too
//! MIXTAPES_DEMO_AUTOPLAY=1   press play three seconds after the window shows
//! MIXTAPES_DEMO_SNAPSHOT=p   write p-1.png at 2.5 s and p-2.png at 11 s from inside GTK
//! MIXTAPES_DEMO_QUEUE=1      open the queue sidebar after staging
//! MIXTAPES_DEMO_EXPAND=1     open the expanded player five seconds in
//! MIXTAPES_DEMO_TAB=name     select home, library or search at startup
//! MIXTAPES_DEMO_SEARCH=text  run a search at startup
//! MIXTAPES_DEMO_ACTIVATE=1   five seconds in, activate the first playable search result
//! MIXTAPES_DEMO_WIDTH=px     initial window width, under 500 for the phone layout
//! MIXTAPES_DEMO_LOGIN=1      open the sign-in dialog and snapshot it as <prefix>-login.png
//! MIXTAPES_DEMO_SNAPSHOT_AT=ms  delay of the second snapshot, default 11000
//! MIXTAPES_DEMO_PLAYLIST=id  open a playlist or album page 1.5 s in
//! MIXTAPES_DEMO_PLAYLIST_PLAY=1  press Play on that page six seconds in
//! MIXTAPES_DEMO_DISCOGRAPHY=id  open a discography grid for a browse id 1.5 s in
//! MIXTAPES_DEMO_SEEK=secs    seek to that position seven seconds in
//! MIXTAPES_DEMO_NEXT_AT=ms   skip to the next queue entry after that many ms
//! MIXTAPES_DEMO_ARTIST=id    open an artist page 1.5 s in
//! MIXTAPES_DEMO_ARTIST_RADIO=1  press the artist page's radio button six seconds in

use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::rc::Rc;
use std::time::Duration;

use gtk::{glib, graphene, prelude::*};

use crate::App;
use crate::model::{Track, VideoId};
use crate::ui::window::MainWindow;

/// Late enough for a yt-dlp resolution started by autoplay or activation to reach Playing.
const SECOND_SNAPSHOT_MS: u64 = 11_000;
const FALLBACK_URI: &str = "https://download.samplelib.com/mp3/sample-15s.mp3";
const AUDIO_EXTENSIONS: &[&str] = &["opus", "mp3", "m4a", "flac", "ogg", "wav"];

pub struct Demo {
    pub tracks: Vec<Track>,
    pub uris: HashMap<String, String>,
    pub autoplay: bool,
    pub snapshot: Option<PathBuf>,
}

pub fn from_env() -> Option<Demo> {
    if std::env::var("MIXTAPES_DEMO").ok().as_deref() != Some("1") {
        return None;
    }
    let sources: Vec<String> = match std::env::var("MIXTAPES_DEMO_URI") {
        Ok(list) => list.split(',').map(str::trim).filter(|s| !s.is_empty()).map(str::to_owned).collect(),
        Err(_) => {
            let found = local_audio_files(3);
            if found.is_empty() { vec![FALLBACK_URI.to_owned()] } else { found }
        }
    };

    let mut tracks = Vec::new();
    let mut uris = HashMap::new();
    for (i, source) in sources.iter().enumerate() {
        let id = format!("demo:{}", i + 1);
        let (uri, title, artist) = describe(source);
        uris.insert(id.clone(), uri);
        tracks.push(Track { video_id: VideoId(id), title, artist, ..Track::default() });
    }
    if let Ok(list) = std::env::var("MIXTAPES_DEMO_VIDEO") {
        for video in list.split(',').map(str::trim).filter(|v| !v.is_empty()) {
            tracks.push(Track {
                video_id: VideoId(video.to_owned()),
                title: format!("YouTube {video}"),
                artist: "resolved by yt-dlp".to_owned(),
                thumb: Some(format!("https://i.ytimg.com/vi/{video}/hqdefault.jpg")),
                ..Track::default()
            });
        }
    }
    Some(Demo {
        tracks,
        uris,
        autoplay: std::env::var("MIXTAPES_DEMO_AUTOPLAY").ok().as_deref() == Some("1"),
        snapshot: std::env::var_os("MIXTAPES_DEMO_SNAPSHOT").map(PathBuf::from),
    })
}

/// Stage the queue and schedule autoplay and snapshots. Call after the window is presented.
pub fn install(demo: &Demo, ctx: &Rc<App>, main_window: &MainWindow) {
    tracing::info!(tracks = demo.tracks.len(), autoplay = demo.autoplay, "demo queue staged");
    ctx.player.stage_tracks(demo.tracks.clone(), 0);
    let window = main_window.window();
    if let Some(width) = std::env::var("MIXTAPES_DEMO_WIDTH").ok().and_then(|w| w.parse::<i32>().ok()) {
        window.set_default_size(width, 700);
    }
    if let Ok(tab) = std::env::var("MIXTAPES_DEMO_TAB") {
        main_window.select_tab(&tab);
    }
    if let Ok(query) = std::env::var("MIXTAPES_DEMO_SEARCH") {
        let win = window.downgrade();
        let ctx_w = ctx.clone();
        glib::timeout_add_local_once(Duration::from_millis(300), move || {
            if win.upgrade().is_some() {
                if let Some(mw) = ctx_w.window.borrow().as_ref() {
                    mw.search(&query);
                }
            }
        });
    }
    if let Ok(id) = std::env::var("MIXTAPES_DEMO_PLAYLIST") {
        let ctx_w = ctx.clone();
        glib::timeout_add_local_once(Duration::from_millis(1500), move || {
            if let Some(mw) = ctx_w.window.borrow().as_ref() {
                mw.open_playlist_for_demo(&id);
            }
        });
    }
    if std::env::var("MIXTAPES_DEMO_PLAYLIST_PLAY").ok().as_deref() == Some("1") {
        let ctx_w = ctx.clone();
        glib::timeout_add_local_once(Duration::from_millis(6000), move || {
            if let Some(mw) = ctx_w.window.borrow().as_ref() {
                tracing::info!(pressed = mw.press_play_on_visible_playlist(), "demo: play on playlist page");
            }
        });
    }
    if let Ok(id) = std::env::var("MIXTAPES_DEMO_DISCOGRAPHY") {
        let ctx_w = ctx.clone();
        glib::timeout_add_local_once(Duration::from_millis(1500), move || {
            if let Some(mw) = ctx_w.window.borrow().as_ref() {
                mw.open_discography_for_demo(&id);
            }
        });
    }
    if let Ok(id) = std::env::var("MIXTAPES_DEMO_ARTIST") {
        let ctx_w = ctx.clone();
        glib::timeout_add_local_once(Duration::from_millis(1500), move || {
            if let Some(mw) = ctx_w.window.borrow().as_ref() {
                mw.open_artist_for_demo(&id);
            }
        });
    }
    if std::env::var("MIXTAPES_DEMO_ARTIST_RADIO").ok().as_deref() == Some("1") {
        let ctx_w = ctx.clone();
        glib::timeout_add_local_once(Duration::from_millis(6000), move || {
            if let Some(mw) = ctx_w.window.borrow().as_ref() {
                tracing::info!(pressed = mw.press_radio_on_visible_artist(), "demo: radio on artist page");
            }
        });
    }
    if let Some(ms) = std::env::var("MIXTAPES_DEMO_NEXT_AT").ok().and_then(|v| v.parse::<u64>().ok()) {
        let player = ctx.player.clone();
        glib::timeout_add_local_once(Duration::from_millis(ms), move || {
            tracing::info!(status = ?player.state().status(), "demo: next");
            player.next();
        });
    }
    if let Some(secs) = std::env::var("MIXTAPES_DEMO_SEEK").ok().and_then(|v| v.parse::<f64>().ok()) {
        let player = ctx.player.clone();
        glib::timeout_add_local_once(Duration::from_millis(7000), move || {
            tracing::info!(secs, before = player.state().position(), "demo: seek");
            player.seek(secs);
        });
    }
    if std::env::var("MIXTAPES_DEMO_LOGIN").ok().as_deref() == Some("1") {
        let ctx_w = ctx.clone();
        let prefix = demo.snapshot.clone();
        glib::timeout_add_local_once(Duration::from_millis(1500), move || {
            if let Some(mw) = ctx_w.window.borrow().as_ref() {
                mw.show_login();
            }
            if let Some(prefix) = prefix {
                glib::timeout_add_local_once(Duration::from_millis(6000), move || {
                    let dialog = gtk::Window::list_toplevels().into_iter().filter_map(|w| w.downcast::<gtk::Window>().ok()).find(|w| w.title().is_some_and(|t| t.starts_with("Login")));
                    match dialog {
                        Some(dialog) => snapshot_any(&dialog, &with_suffix(&prefix, "login")),
                        None => tracing::warn!("demo: login dialog not found"),
                    }
                });
            }
        });
    }
    if std::env::var("MIXTAPES_DEMO_ACTIVATE").ok().as_deref() == Some("1") {
        let ctx_w = ctx.clone();
        glib::timeout_add_local_once(Duration::from_millis(5000), move || {
            if let Some(mw) = ctx_w.window.borrow().as_ref() {
                if !mw.activate_first_result() {
                    tracing::warn!("demo: no playable search result to activate");
                }
            }
        });
    }
    if std::env::var("MIXTAPES_DEMO_EXPAND").ok().as_deref() == Some("1") {
        let ctx_w = ctx.clone();
        glib::timeout_add_local_once(Duration::from_millis(5000), move || {
            if let Some(mw) = ctx_w.window.borrow().as_ref() {
                tracing::info!("demo: expanding player");
                mw.expand_player();
            }
        });
    }
    if std::env::var("MIXTAPES_DEMO_QUEUE").ok().as_deref() == Some("1") {
        // Open after the first layout pass, like a user click would.
        let win = window.downgrade();
        glib::timeout_add_local_once(Duration::from_millis(600), move || {
            if let Some(w) = win.upgrade() {
                if let Some(sv) = find_split_view(w.upcast_ref()) {
                    sv.set_show_sidebar(true);
                }
            }
        });
    }

    if let Some(prefix) = demo.snapshot.clone() {
        let win = window.downgrade();
        glib::timeout_add_local_once(Duration::from_millis(2500), move || {
            if let Some(w) = win.upgrade() {
                snapshot(&w, &with_suffix(&prefix, "1"));
            }
        });
    }
    if demo.autoplay {
        let player = ctx.player.clone();
        glib::timeout_add_local_once(Duration::from_millis(3000), move || {
            tracing::info!("demo: pressing play");
            player.toggle_play();
        });
    }
    if let Some(prefix) = demo.snapshot.clone() {
        let win = window.downgrade();
        let player = ctx.player.clone();
        let second_ms = std::env::var("MIXTAPES_DEMO_SNAPSHOT_AT").ok().and_then(|v| v.parse().ok()).unwrap_or(SECOND_SNAPSHOT_MS);
        glib::timeout_add_local_once(Duration::from_millis(second_ms), move || {
            let state = player.state();
            tracing::info!(status = ?state.status(), position = state.position(), duration = state.duration(), queue_length = state.queue_length(), "demo: state at snapshot");
            if let Some(w) = win.upgrade() {
                snapshot(&w, &with_suffix(&prefix, "2"));
            }
        });
    }
}

fn find_split_view(widget: &gtk::Widget) -> Option<adw::OverlaySplitView> {
    if let Some(sv) = widget.downcast_ref::<adw::OverlaySplitView>() {
        return Some(sv.clone());
    }
    let mut child = widget.first_child();
    while let Some(c) = child {
        if let Some(found) = find_split_view(&c) {
            return Some(found);
        }
        child = c.next_sibling();
    }
    None
}

fn describe(source: &str) -> (String, String, String) {
    if source.contains("://") {
        let name = source.rsplit('/').next().unwrap_or(source).to_owned();
        return (source.to_owned(), name, "demo stream".to_owned());
    }
    let path = Path::new(source);
    let uri = glib::filename_to_uri(path, None).map(|u| u.to_string()).unwrap_or_else(|_| source.to_owned());
    let title = path.file_stem().map(|s| s.to_string_lossy().into_owned()).unwrap_or_else(|| source.to_owned());
    let artist = path.parent().and_then(Path::file_name).map(|s| s.to_string_lossy().into_owned()).unwrap_or_else(|| "local file".to_owned());
    (uri, title, artist)
}

fn local_audio_files(limit: usize) -> Vec<String> {
    let Some(music) = glib::user_special_dir(glib::UserDirectory::Music) else { return Vec::new() };
    let mut out = Vec::new();
    walk(&music, 0, limit, &mut out);
    out.sort();
    out
}

fn walk(dir: &Path, depth: usize, limit: usize, out: &mut Vec<String>) {
    if depth > 3 || out.len() >= limit {
        return;
    }
    let Ok(read) = std::fs::read_dir(dir) else { return };
    let mut entries: Vec<PathBuf> = read.filter_map(|e| e.ok().map(|e| e.path())).collect();
    entries.sort();
    for path in entries {
        if out.len() >= limit {
            return;
        }
        if path.is_dir() {
            walk(&path, depth + 1, limit, out);
        } else if path.extension().and_then(|e| e.to_str()).is_some_and(|e| AUDIO_EXTENSIONS.contains(&e.to_ascii_lowercase().as_str())) {
            out.push(path.to_string_lossy().into_owned());
        }
    }
}

fn with_suffix(prefix: &Path, suffix: &str) -> PathBuf {
    let mut name = prefix.file_name().map(|s| s.to_string_lossy().into_owned()).unwrap_or_else(|| "snapshot".to_owned());
    name = name.trim_end_matches(".png").to_owned();
    prefix.with_file_name(format!("{name}-{suffix}.png"))
}

/// Render the window to a PNG through its own GSK renderer.
/// Runs right after GTK paints a frame, when the widget's render node is fresh.
fn snapshot(window: &adw::ApplicationWindow, path: &Path) {
    snapshot_any(window.upcast_ref::<gtk::Window>(), path);
}

fn snapshot_any(window: &gtk::Window, path: &Path) {
    let Some(clock) = window.frame_clock() else {
        tracing::warn!("demo: no frame clock yet");
        return;
    };
    let target = window.clone();
    let path = path.to_path_buf();
    let handler: Rc<std::cell::RefCell<Option<glib::SignalHandlerId>>> = Rc::new(std::cell::RefCell::new(None));
    let slot = handler.clone();
    let id = clock.connect_after_paint(move |clock| {
        if let Some(id) = slot.borrow_mut().take() {
            clock.disconnect(id);
        }
        match render_window(&target, &path) {
            Ok(true) => tracing::info!(path = %path.display(), "demo: snapshot written"),
            Ok(false) => tracing::warn!("demo: snapshot skipped, render node empty after paint"),
            Err(err) => tracing::warn!(%err, "demo: snapshot failed"),
        }
    });
    handler.replace(Some(id));
    window.queue_draw();
}

fn render_window(window: &gtk::Window, path: &Path) -> anyhow::Result<bool> {
    let paintable = gtk::WidgetPaintable::new(Some(window));
    let width = paintable.intrinsic_width().max(1) as f32;
    let height = paintable.intrinsic_height().max(1) as f32;
    let snapshot = gtk::Snapshot::new();
    paintable.snapshot(&snapshot, width as f64, height as f64);
    let Some(node) = snapshot.to_node() else { return Ok(false) };
    let renderer = window.native().and_then(|n| n.renderer()).ok_or_else(|| anyhow::anyhow!("no renderer"))?;
    let texture = renderer.render_texture(node, Some(&graphene::Rect::new(0.0, 0.0, width, height)));
    texture.save_to_png(path)?;
    Ok(true)
}

//! The sign-in window: Google's own sign-in page in a WebKit view, nothing
//! else. The view watches the first authenticated request to
//! music.youtube.com and captures its headers, falling back to the cookie jar
//! when WebKit redacts the Cookie header. That ends in `YtMusic::login`,
//! which writes headers_auth.json and publishes the auth state the rest of
//! the app follows. The Python app also offered browser.json and pasted
//! headers; those tabs are gone.

use std::cell::{Cell, RefCell};
#[cfg(not(windows))]
use std::collections::BTreeMap;
use std::rc::Rc;

use adw::prelude::*;
use gtk::glib;
#[cfg(not(windows))]
use webkit6::prelude::*;

use crate::ui::context::UiContext;

const LOGIN_URL: &str = "https://accounts.google.com/ServiceLogin?ltmpl=music&service=youtube&uilel=3&passive=true&continue=https%3A%2F%2Fmusic.youtube.com%2Flibrary";
#[cfg_attr(windows, allow(dead_code))]
const BROWSER_UA: &str = "Mozilla/5.0 (X11; Linux x86_64; rv:147.0) Gecko/20100101 Firefox/147.0";

#[cfg(not(windows))]
pub struct LoginDialog {
    window: adw::Window,
    webview: webkit6::WebView,
    web_status: gtk::Label,
    captured: RefCell<BTreeMap<String, String>>,
    finished: Cell<bool>,
    ctx: Rc<UiContext>,
    on_success: RefCell<Option<Rc<dyn Fn()>>>,
}

#[cfg(not(windows))]
impl LoginDialog {
    pub fn new(ctx: Rc<UiContext>, parent: &impl IsA<gtk::Window>) -> Rc<Self> {
        let window = adw::Window::builder()
            .modal(true)
            .transient_for(parent)
            .default_width(520)
            .default_height(640)
            .title(tr!("Sign in to YouTube Music"))
            .build();
        let toolbar = adw::ToolbarView::new();
        window.set_content(Some(&toolbar));
        let header = adw::HeaderBar::builder().show_title(true).build();
        let skip = gtk::Button::builder().label(tr!("Skip")).build();
        header.pack_end(&skip);
        toolbar.add_top_bar(&header);

        let webview = webkit6::WebView::new();
        if let Some(settings) = WebViewExt::settings(&webview) {
            // Google blocks unknown browsers; look like a desktop Firefox.
            settings.set_user_agent(Some(BROWSER_UA));
            settings.set_enable_javascript(true);
            settings.set_enable_webgl(true);
            settings.set_enable_html5_local_storage(true);
            settings.set_enable_html5_database(true);
        }
        webview.set_vexpand(true);
        let web_status = gtk::Label::builder().visible(false).margin_top(6).margin_bottom(6).build();
        let page = gtk::Box::builder().orientation(gtk::Orientation::Vertical).build();
        page.append(&webview);
        page.append(&web_status);
        toolbar.set_content(Some(&page));

        let dialog = Rc::new(Self {
            window,
            webview,
            web_status,
            captured: RefCell::new(BTreeMap::new()),
            finished: Cell::new(false),
            ctx,
            on_success: RefCell::new(None),
        });
        {
            let win = dialog.window.clone();
            skip.connect_clicked(move |_| win.close());
        }
        {
            // Closing without signing in is a choice. Startup stops asking until the next sign-in.
            let weak = Rc::downgrade(&dialog);
            dialog.window.connect_close_request(move |_| {
                if let Some(d) = weak.upgrade() {
                    if !d.ctx.net.client().is_authenticated() {
                        set_login_skipped(&d.ctx.paths, true);
                    }
                }
                glib::Propagation::Proceed
            });
        }
        {
            let weak = Rc::downgrade(&dialog);
            dialog.webview.connect_resource_load_started(move |_, _, request| {
                if let Some(d) = weak.upgrade() {
                    d.on_resource_load_started(request);
                }
            });
        }
        dialog.webview.load_uri(LOGIN_URL);
        dialog
    }

    pub fn set_on_success(&self, f: impl Fn() + 'static) {
        self.on_success.replace(Some(Rc::new(f)));
    }

    pub fn present(&self) {
        self.window.present();
    }

    pub fn connect_close(&self, f: impl Fn() + 'static) {
        self.window.connect_close_request(move |_| {
            f();
            glib::Propagation::Proceed
        });
    }

    fn set_status(&self, color: &str, text: &str) {
        self.web_status.set_markup(&format!("<span color='{color}'>{}</span>", glib::markup_escape_text(text)));
        self.web_status.set_visible(true);
    }

    /// The first authenticated browse request carries everything we need.
    fn on_resource_load_started(self: &Rc<Self>, request: &webkit6::URIRequest) {
        if self.finished.get() {
            return;
        }
        let uri = request.uri().map(|u| u.to_string()).unwrap_or_default();
        if !uri.contains("music.youtube.com/youtubei/v1/browse") {
            return;
        }
        let Some(headers) = request.http_headers() else {
            return;
        };
        let cookie = headers.one("Cookie").map(|c| c.to_string()).unwrap_or_default();
        let auth = headers.one("Authorization").map(|a| a.to_string()).unwrap_or_default();
        let has_sapisid = cookie.contains("SAPISID");
        if !has_sapisid && !auth.contains("SAPISIDHASH") {
            return;
        }
        let mut captured = BTreeMap::new();
        headers.foreach(|name, value| {
            captured.insert(name.to_string(), value.to_string());
        });
        self.captured.replace(captured);
        if has_sapisid {
            self.finished.set(true);
            let weak = Rc::downgrade(self);
            glib::idle_add_local_once(move || {
                if let Some(d) = weak.upgrade() {
                    d.finish_login();
                }
            });
        } else {
            // WebKit redacts the Cookie header here; read the jar instead.
            self.fetch_cookies_from_jar();
        }
    }

    fn fetch_cookies_from_jar(self: &Rc<Self>) {
        let Some(manager) = self.webview.network_session().and_then(|s| s.cookie_manager()) else {
            return;
        };
        let weak = Rc::downgrade(self);
        glib::spawn_future_local(async move {
            let cookies = manager.cookies_future("https://music.youtube.com").await;
            let Some(d) = weak.upgrade() else { return };
            let mut cookies = match cookies {
                Ok(cookies) => cookies,
                Err(err) => {
                    d.set_status("red", &tr!("Could not read cookies: {err}", err));
                    return;
                }
            };
            let cookie = cookies.iter_mut().filter_map(|c| Some(format!("{}={}", c.name()?, c.value()?))).collect::<Vec<_>>().join("; ");
            if !cookie.contains("SAPISID") {
                d.set_status("orange", &tr!("Not signed in yet, the session has no SAPISID cookie."));
                return;
            }
            let mut captured = d.captured.borrow().clone();
            captured.insert("Cookie".into(), cookie);
            captured.entry("User-Agent".into()).or_insert_with(|| BROWSER_UA.to_owned());
            d.captured.replace(captured);
            d.finished.set(true);
            d.finish_login();
        });
    }

    fn finish_login(self: &Rc<Self>) {
        self.set_status("blue", &tr!("Signing in..."));
        let json = serde_json::to_string(&*self.captured.borrow()).unwrap_or_default();
        self.login_with(json, move |d, ok| {
            if ok {
                d.set_status("green", &tr!("Signed in."));
                d.clear_webkit_cookies();
                d.window.close();
            } else {
                d.finished.set(false);
                d.set_status("red", &tr!("The server did not accept the session. Try again."));
            }
        });
    }

    /// Drop the embedded browser's cookies once the app has its own copy.
    fn clear_webkit_cookies(&self) {
        if let Some(manager) = self.webview.network_session().and_then(|s| s.website_data_manager()) {
            manager.clear(webkit6::WebsiteDataTypes::COOKIES, glib::TimeSpan::from_seconds(0), None::<&gtk::gio::Cancellable>, |_| {});
        }
    }

    /// Run `YtMusic::login` on the runtime and report back on the GTK thread.
    fn login_with(self: &Rc<Self>, input: String, done: impl Fn(&Rc<Self>, bool) + 'static) {
        let client = self.ctx.net.client().clone();
        let handle = self.ctx.net.spawn(async move { client.login(&input).await });
        let weak = Rc::downgrade(self);
        glib::spawn_future_local(async move {
            let outcome = handle.await;
            let Some(d) = weak.upgrade() else { return };
            match outcome {
                Ok(Ok(info)) => {
                    tracing::info!(name = %info.name, "login successful");
                    set_login_skipped(&d.ctx.paths, false);
                    if let Some(f) = d.on_success.borrow().as_ref() {
                        f();
                    }
                    done(&d, true);
                }
                Ok(Err(err)) => {
                    tracing::warn!(%err, "login failed");
                    done(&d, false);
                }
                Err(_) => done(&d, false),
            }
        });
    }
}

/// Windows has no WebKitGTK, so the same page runs in WebView2, placed as a
/// native child window over the empty area under the header bar. It watches
/// page loads instead of requests: once music.youtube.com has loaded with a
/// SAPISID cookie, the cookie jar is the session.
#[cfg(windows)]
pub struct LoginDialog {
    window: adw::Window,
    /// Where the webview sits. It is a native window above GTK's drawing, so
    /// GTK only keeps this space free and reports its size.
    area: gtk::DrawingArea,
    web_status: gtk::Label,
    webview: RefCell<Option<wry::WebView>>,
    /// Owns the WebView2 profile folder; must outlive the webview.
    web_context: RefCell<Option<wry::WebContext>>,
    finished: Cell<bool>,
    ctx: Rc<UiContext>,
    on_success: RefCell<Option<Rc<dyn Fn()>>>,
}

/// What the webview sends as its browser. WebView2 is Edge, so the page and
/// the saved session carry an Edge user agent.
#[cfg(windows)]
const WINDOWS_BROWSER_UA: &str = "Mozilla/5.0 (Windows NT 10.0; Win64; x64) AppleWebKit/537.36 (KHTML, like Gecko) Chrome/140.0.0.0 Safari/537.36 Edg/140.0.0.0";

#[cfg(windows)]
impl LoginDialog {
    pub fn new(ctx: Rc<UiContext>, parent: &impl IsA<gtk::Window>) -> Rc<Self> {
        let window = adw::Window::builder()
            .modal(true)
            .transient_for(parent)
            .default_width(520)
            .default_height(640)
            .title(tr!("Sign in to YouTube Music"))
            .build();
        let toolbar = adw::ToolbarView::new();
        window.set_content(Some(&toolbar));
        let header = adw::HeaderBar::builder().show_title(true).build();
        let skip = gtk::Button::builder().label(tr!("Skip")).build();
        header.pack_end(&skip);
        toolbar.add_top_bar(&header);

        let area = gtk::DrawingArea::builder().vexpand(true).hexpand(true).build();
        let web_status = gtk::Label::builder().visible(false).margin_top(6).margin_bottom(6).build();
        let page = gtk::Box::builder().orientation(gtk::Orientation::Vertical).build();
        page.append(&area);
        page.append(&web_status);
        toolbar.set_content(Some(&page));

        let dialog = Rc::new(Self {
            window,
            area,
            web_status,
            webview: RefCell::new(None),
            web_context: RefCell::new(None),
            finished: Cell::new(false),
            ctx,
            on_success: RefCell::new(None),
        });
        {
            let win = dialog.window.clone();
            skip.connect_clicked(move |_| win.close());
        }
        {
            let weak = Rc::downgrade(&dialog);
            dialog.window.connect_close_request(move |_| {
                if let Some(d) = weak.upgrade() {
                    if !d.ctx.net.client().is_authenticated() {
                        set_login_skipped(&d.ctx.paths, true);
                    }
                    // Dropping the webview destroys its native window with ours.
                    d.webview.replace(None);
                }
                glib::Propagation::Proceed
            });
        }
        {
            // The native window exists once GTK has mapped ours.
            let weak = Rc::downgrade(&dialog);
            dialog.area.connect_map(move |_| {
                let weak = weak.clone();
                glib::idle_add_local_once(move || {
                    if let Some(d) = weak.upgrade() {
                        d.create_webview();
                    }
                });
            });
        }
        {
            let weak = Rc::downgrade(&dialog);
            dialog.area.connect_resize(move |_, _, _| {
                if let Some(d) = weak.upgrade() {
                    d.place_webview();
                }
            });
        }
        dialog
    }

    pub fn set_on_success(&self, f: impl Fn() + 'static) {
        self.on_success.replace(Some(Rc::new(f)));
    }

    pub fn present(&self) {
        self.window.present();
    }

    pub fn connect_close(&self, f: impl Fn() + 'static) {
        self.window.connect_close_request(move |_| {
            f();
            glib::Propagation::Proceed
        });
    }

    fn set_status(&self, color: &str, text: &str) {
        self.web_status.set_markup(&format!("<span color='{color}'>{}</span>", glib::markup_escape_text(text)));
        self.web_status.set_visible(true);
    }

    fn create_webview(self: &Rc<Self>) {
        if self.webview.borrow().is_some() {
            return;
        }
        let Some(parent) = native_parent(&self.window) else {
            self.set_status("red", &tr!("The sign-in page could not be shown."));
            return;
        };
        // WebView2 keeps its profile beside the exe unless told otherwise, and
        // Program Files is not writable.
        let mut context = wry::WebContext::new(Some(self.ctx.paths.cache_dir.join("webview2")));
        let weak = Rc::downgrade(self);
        let built = wry::WebViewBuilder::new_with_web_context(&mut context)
            .with_url(LOGIN_URL)
            .with_user_agent(WINDOWS_BROWSER_UA)
            .with_on_page_load_handler(move |event, url| {
                if !matches!(event, wry::PageLoadEvent::Finished) || !url.starts_with("https://music.youtube.com") {
                    return;
                }
                // Reading cookies waits on WebView2, so not from inside its own callback.
                let weak = weak.clone();
                glib::idle_add_local_once(move || {
                    if let Some(d) = weak.upgrade() {
                        d.check_session();
                    }
                });
            })
            .build_as_child(&parent);
        match built {
            Ok(webview) => {
                self.webview.replace(Some(webview));
                self.web_context.replace(Some(context));
                self.place_webview();
            }
            Err(err) => {
                tracing::warn!(%err, "webview2 did not start");
                self.set_status("red", &tr!("The sign-in page could not be shown. Is the WebView2 runtime installed?"));
            }
        }
    }

    /// Lay the native webview over the drawing area, in the surface's physical pixels.
    fn place_webview(&self) {
        let webview = self.webview.borrow();
        let Some(webview) = webview.as_ref() else { return };
        let Some(bounds) = self.area.compute_bounds(&self.window) else { return };
        let (shadow_x, shadow_y) = self.window.surface_transform();
        let scale = self.window.surface().map(|s| s.scale()).unwrap_or(1.0);
        let rect = wry::Rect {
            position: wry::dpi::PhysicalPosition::new((f64::from(bounds.x()) + shadow_x) * scale, (f64::from(bounds.y()) + shadow_y) * scale).into(),
            size: wry::dpi::PhysicalSize::new(f64::from(bounds.width()) * scale, f64::from(bounds.height()) * scale).into(),
        };
        if let Err(err) = webview.set_bounds(rect) {
            tracing::debug!(%err, "webview bounds not set");
        }
    }

    /// music.youtube.com loaded. Signed in once the jar holds SAPISID.
    fn check_session(self: &Rc<Self>) {
        if self.finished.get() {
            return;
        }
        let cookies = match self.webview.borrow().as_ref().map(|w| w.cookies_for_url("https://music.youtube.com")) {
            Some(Ok(cookies)) => cookies,
            Some(Err(err)) => {
                self.set_status("red", &tr!("Could not read cookies: {err}", err));
                return;
            }
            None => return,
        };
        let cookie = cookies.iter().map(|c| format!("{}={}", c.name(), c.value())).collect::<Vec<_>>().join("; ");
        if !cookie.contains("SAPISID") {
            return;
        }
        self.finished.set(true);
        self.set_status("blue", &tr!("Signing in..."));
        let captured = std::collections::BTreeMap::from([("Cookie".to_owned(), cookie), ("User-Agent".to_owned(), WINDOWS_BROWSER_UA.to_owned())]);
        let json = serde_json::to_string(&captured).unwrap_or_default();
        self.login_with(json, move |d, ok| {
            if ok {
                d.set_status("green", &tr!("Signed in."));
                // The app keeps its own copy of the session.
                if let Some(webview) = d.webview.borrow().as_ref() {
                    let _ = webview.clear_all_browsing_data();
                }
                d.window.close();
            } else {
                d.finished.set(false);
                d.set_status("red", &tr!("The server did not accept the session. Try again."));
            }
        });
    }

    /// Run `YtMusic::login` on the runtime and report back on the GTK thread.
    fn login_with(self: &Rc<Self>, input: String, done: impl Fn(&Rc<Self>, bool) + 'static) {
        let client = self.ctx.net.client().clone();
        let handle = self.ctx.net.spawn(async move { client.login(&input).await });
        let weak = Rc::downgrade(self);
        glib::spawn_future_local(async move {
            let outcome = handle.await;
            let Some(d) = weak.upgrade() else { return };
            match outcome {
                Ok(Ok(info)) => {
                    tracing::info!(name = %info.name, "login successful");
                    set_login_skipped(&d.ctx.paths, false);
                    if let Some(f) = d.on_success.borrow().as_ref() {
                        f();
                    }
                    done(&d, true);
                }
                Ok(Err(err)) => {
                    tracing::warn!(%err, "login failed");
                    done(&d, false);
                }
                Err(_) => done(&d, false),
            }
        });
    }
}

/// The GTK window's native handle, in the form wry takes a parent in.
#[cfg(windows)]
struct NativeParent(std::num::NonZeroIsize);

#[cfg(windows)]
impl wry::raw_window_handle::HasWindowHandle for NativeParent {
    fn window_handle(&self) -> Result<wry::raw_window_handle::WindowHandle<'_>, wry::raw_window_handle::HandleError> {
        let raw = wry::raw_window_handle::RawWindowHandle::Win32(wry::raw_window_handle::Win32WindowHandle::new(self.0));
        // SAFETY: the handle belongs to a window that outlives the webview, which is dropped on close.
        Ok(unsafe { wry::raw_window_handle::WindowHandle::borrow_raw(raw) })
    }
}

#[cfg(windows)]
fn native_parent(window: &adw::Window) -> Option<NativeParent> {
    use gtk::glib::translate::ToGlibPtr;
    unsafe extern "C" {
        fn gdk_win32_surface_get_handle(surface: *mut gtk::gdk::ffi::GdkSurface) -> *mut std::ffi::c_void;
    }
    let surface = window.surface()?;
    // SAFETY: a realized GdkWin32Surface; the call only reads its HWND.
    let hwnd = unsafe { gdk_win32_surface_get_handle(surface.to_glib_none().0) };
    std::num::NonZeroIsize::new(hwnd as isize).map(NativeParent)
}

/// Pref set when the listener closed the sign-in without signing in.
pub const LOGIN_SKIPPED_PREF: &str = "login_skipped";

pub fn login_skipped(paths: &crate::paths::Paths) -> bool {
    paths.read_prefs().get(LOGIN_SKIPPED_PREF).and_then(|v| v.as_bool()).unwrap_or(false)
}

pub fn set_login_skipped(paths: &crate::paths::Paths, skipped: bool) {
    paths.update_prefs(|p| {
        p.insert(LOGIN_SKIPPED_PREF.to_owned(), serde_json::Value::Bool(skipped));
    });
}

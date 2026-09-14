//! Port of ui/login.py and ui/login_webview.py: a modal sign-in window with
//! three pages. Direct Login embeds WebKit, watches the first authenticated
//! request to music.youtube.com, and captures its headers (falling back to
//! the cookie jar). Browser Login imports browser.json from the working
//! directory. Manual Headers takes pasted JSON or a raw header block.
//! Every path ends in `YtMusic::login`, which writes headers_auth.json and
//! publishes the auth state the rest of the app follows.

use std::cell::{Cell, RefCell};
use std::collections::BTreeMap;
use std::rc::Rc;

use adw::prelude::*;
use gtk::glib;
use webkit6::prelude::*;

use crate::ui::context::UiContext;

const LOGIN_URL: &str = "https://accounts.google.com/ServiceLogin?ltmpl=music&service=youtube&uilel=3&passive=true&continue=https%3A%2F%2Fmusic.youtube.com%2Flibrary";
const BROWSER_UA: &str = "Mozilla/5.0 (X11; Linux x86_64; rv:147.0) Gecko/20100101 Firefox/147.0";

pub struct LoginDialog {
    window: adw::Window,
    stack: adw::ViewStack,
    status: gtk::Label,
    text_view: gtk::TextView,
    webview: Option<webkit6::WebView>,
    web_status: gtk::Label,
    captured: RefCell<BTreeMap<String, String>>,
    finished: Cell<bool>,
    ctx: Rc<UiContext>,
    on_success: RefCell<Option<Rc<dyn Fn()>>>,
}

impl LoginDialog {
    pub fn new(ctx: Rc<UiContext>, parent: &impl IsA<gtk::Window>) -> Rc<Self> {
        let window = adw::Window::builder()
            .modal(true)
            .transient_for(parent)
            .default_width(600)
            .default_height(500)
            .title("Login to YouTube Music")
            .build();
        let toolbar = adw::ToolbarView::new();
        window.set_content(Some(&toolbar));
        let header = adw::HeaderBar::builder().show_title(true).build();
        let skip = gtk::Button::builder().label("Skip").build();
        header.pack_end(&skip);
        toolbar.add_top_bar(&header);

        let stack = adw::ViewStack::new();
        let status = gtk::Label::new(None);
        let text_view = gtk::TextView::builder()
            .wrap_mode(gtk::WrapMode::WordChar)
            .monospace(true)
            .left_margin(12)
            .right_margin(12)
            .top_margin(12)
            .bottom_margin(12)
            .build();
        let web_status = gtk::Label::new(None);

        // 0. Direct login through WebKit.
        let (webview, direct_page) = build_direct_page(&web_status);
        stack.add_titled(&direct_page, Some("direct"), "Direct Login");
        // 1. browser.json in the working directory.
        stack.add_titled(
            &build_browser_page(&status),
            Some("browser"),
            "Browser Login",
        );
        // 2. Pasted headers.
        stack.add_titled(
            &build_manual_page(&text_view),
            Some("manual"),
            "Manual Headers",
        );
        toolbar.set_content(Some(&stack));
        let switcher = adw::ViewSwitcherBar::builder()
            .stack(&stack)
            .reveal(true)
            .build();
        toolbar.add_bottom_bar(&switcher);

        let dialog = Rc::new(Self {
            window,
            stack,
            status,
            text_view,
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
        dialog.connect_pages();
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

    fn connect_pages(self: &Rc<Self>) {
        // Buttons live inside the pages; find them by name.
        for name in ["import-browser", "manual-login", "webkit-done"] {
            let Some(button) = find_named(&self.stack, name).and_downcast::<gtk::Button>() else {
                continue;
            };
            let weak = Rc::downgrade(self);
            match name {
                "import-browser" => button.connect_clicked(move |_| {
                    if let Some(d) = weak.upgrade() {
                        d.import_browser_json();
                    }
                }),
                "manual-login" => button.connect_clicked(move |_| {
                    if let Some(d) = weak.upgrade() {
                        d.manual_login();
                    }
                }),
                _ => button.connect_clicked(move |_| {
                    if let Some(d) = weak.upgrade() {
                        d.fetch_cookies_from_jar();
                    }
                }),
            };
        }
        if let Some(webview) = &self.webview {
            let weak = Rc::downgrade(self);
            webview.connect_resource_load_started(move |_, _, request| {
                if let Some(d) = weak.upgrade() {
                    d.on_resource_load_started(request);
                }
            });
            webview.load_uri(LOGIN_URL);
        }
    }

    // -- direct login -----------------------------------------------------

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
        let cookie = headers
            .one("Cookie")
            .map(|c| c.to_string())
            .unwrap_or_default();
        let auth = headers
            .one("Authorization")
            .map(|a| a.to_string())
            .unwrap_or_default();
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
                    d.finish_direct_login();
                }
            });
        } else {
            // WebKit redacts the Cookie header here; read the jar instead.
            self.fetch_cookies_from_jar();
        }
    }

    fn fetch_cookies_from_jar(self: &Rc<Self>) {
        let Some(webview) = &self.webview else { return };
        let Some(manager) = webview.network_session().and_then(|s| s.cookie_manager()) else {
            return;
        };
        let weak = Rc::downgrade(self);
        glib::spawn_future_local(async move {
            let cookies = manager.cookies_future("https://music.youtube.com").await;
            let Some(d) = weak.upgrade() else { return };
            let mut cookies = match cookies {
                Ok(cookies) => cookies,
                Err(err) => {
                    d.web_status.set_markup(&format!(
                        "<span color='red'>Could not read cookies: {err}</span>"
                    ));
                    return;
                }
            };
            let cookie = cookies
                .iter_mut()
                .filter_map(|c| Some(format!("{}={}", c.name()?, c.value()?)))
                .collect::<Vec<_>>()
                .join("; ");
            if !cookie.contains("SAPISID") {
                d.web_status.set_markup("<span color='orange'>Not signed in yet, the session has no SAPISID cookie.</span>");
                return;
            }
            let mut captured = d.captured.borrow().clone();
            captured.insert("Cookie".into(), cookie);
            captured
                .entry("User-Agent".into())
                .or_insert_with(|| BROWSER_UA.to_owned());
            d.captured.replace(captured);
            d.finished.set(true);
            d.finish_direct_login();
        });
    }

    fn finish_direct_login(self: &Rc<Self>) {
        self.web_status
            .set_markup("<span color='blue'>Capture successful, logging in...</span>");
        let json = serde_json::to_string(&*self.captured.borrow()).unwrap_or_default();
        let weak = Rc::downgrade(self);
        self.login_with(json, move |d, ok| {
            if ok {
                d.web_status
                    .set_markup("<span color='green'>Login Successful!</span>");
                d.clear_webkit_cookies();
                d.window.close();
            } else {
                d.finished.set(false);
                d.web_status
                    .set_markup("<span color='red'>Login Failed after capture.</span>");
            }
            let _ = weak.upgrade();
        });
    }

    /// Drop the embedded browser's cookies once the app has its own copy.
    fn clear_webkit_cookies(&self) {
        if let Some(manager) = self
            .webview
            .as_ref()
            .and_then(|w| w.network_session())
            .and_then(|s| s.website_data_manager())
        {
            manager.clear(
                webkit6::WebsiteDataTypes::COOKIES,
                glib::TimeSpan::from_seconds(0),
                None::<&gtk::gio::Cancellable>,
                |_| {},
            );
        }
    }

    // -- browser.json and manual ------------------------------------------

    fn import_browser_json(self: &Rc<Self>) {
        let path = std::env::current_dir()
            .map(|d| d.join("browser.json"))
            .unwrap_or_default();
        if !path.is_file() {
            self.status.set_markup(&format!(
                "<span color='orange'>File not found at {}</span>",
                glib::markup_escape_text(&path.display().to_string())
            ));
            return;
        }
        self.status
            .set_text(&format!("Found {}...", path.display()));
        self.login_with(path.display().to_string(), |d, ok| {
            if ok {
                d.status
                    .set_markup("<span color='green'>Login Successful!</span>");
                d.window.close();
            } else {
                d.status
                    .set_markup("<span color='red'>Login Failed. Check keys/headers.</span>");
            }
        });
    }

    fn manual_login(self: &Rc<Self>) {
        let buffer = self.text_view.buffer();
        let text = buffer
            .text(&buffer.start_iter(), &buffer.end_iter(), true)
            .to_string();
        if text.trim().is_empty() {
            return;
        }
        self.login_with(text, |d, ok| {
            if ok {
                d.window.close();
            } else {
                crate::ui::toast(&d.stack, "Login failed");
            }
        });
    }

    /// Run `YtMusic::login` on the runtime and report back on the GTK thread.
    fn login_with(self: &Rc<Self>, input: String, done: impl Fn(&Rc<Self>, bool) + 'static) {
        let client = self.ctx.net.client().clone();
        let handle = self
            .ctx
            .net
            .spawn(async move { client.login(&input).await });
        let weak = Rc::downgrade(self);
        glib::spawn_future_local(async move {
            let outcome = handle.await;
            let Some(d) = weak.upgrade() else { return };
            match outcome {
                Ok(Ok(info)) => {
                    tracing::info!(name = %info.name, "login successful");
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

// -- pages ---------------------------------------------------------------

fn build_direct_page(web_status: &gtk::Label) -> (Option<webkit6::WebView>, gtk::Widget) {
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
    let page = gtk::Box::builder()
        .orientation(gtk::Orientation::Vertical)
        .build();
    page.append(&webview);
    let done = gtk::Button::builder()
        .label("I'm Logged In (Manual)")
        .halign(gtk::Align::Center)
        .css_classes(["pill"])
        .margin_top(8)
        .margin_bottom(8)
        .build();
    done.set_widget_name("webkit-done");
    page.append(&done);
    page.append(web_status);
    (Some(webview), page.upcast())
}

fn build_browser_page(status: &gtk::Label) -> gtk::Widget {
    let page = adw::StatusPage::builder()
        .title("Browser Login")
        .description("The most reliable way to login is via `browser.json`.")
        .icon_name("web-browser-symbolic")
        .build();
    let clamp = adw::Clamp::builder().maximum_size(500).build();
    let column = gtk::Box::builder()
        .orientation(gtk::Orientation::Vertical)
        .spacing(16)
        .build();
    let steps = gtk::Label::builder()
        .wrap(true)
        .justify(gtk::Justification::Left)
        .xalign(0.0)
        .use_markup(true)
        .build();
    steps.set_markup("<span size='large'><b>Step 1:</b> Run this in your terminal:</span>\n<tt>ytmusicapi browser</tt>\n\n<span size='large'><b>Step 2:</b> Follow instructions to paste headers.</span>\n\n<span size='large'><b>Step 3:</b> Click 'Import browser.json' below.</span>");
    column.append(&steps);
    let import = gtk::Button::builder()
        .label("Import browser.json")
        .halign(gtk::Align::Center)
        .css_classes(["pill", "suggested-action"])
        .build();
    import.set_widget_name("import-browser");
    column.append(&import);
    column.append(status);
    clamp.set_child(Some(&column));
    page.set_child(Some(&clamp));
    page.upcast()
}

fn build_manual_page(text_view: &gtk::TextView) -> gtk::Widget {
    let page = adw::StatusPage::builder()
        .title("Manual / Advanced")
        .description("Paste headers JSON content directly.")
        .build();
    let clamp = adw::Clamp::builder().maximum_size(600).build();
    let column = gtk::Box::builder()
        .orientation(gtk::Orientation::Vertical)
        .spacing(12)
        .build();
    let scrolled = gtk::ScrolledWindow::builder()
        .vexpand(true)
        .min_content_height(300)
        .child(text_view)
        .build();
    let frame = gtk::Frame::builder().child(&scrolled).build();
    column.append(&frame);
    let login = gtk::Button::builder()
        .label("Login with JSON")
        .halign(gtk::Align::Center)
        .css_classes(["pill", "suggested-action"])
        .build();
    login.set_widget_name("manual-login");
    column.append(&login);
    clamp.set_child(Some(&column));
    page.set_child(Some(&clamp));
    page.upcast()
}

/// Depth-first search for a widget by name.
fn find_named(root: &impl IsA<gtk::Widget>, name: &str) -> Option<gtk::Widget> {
    let root = root.upcast_ref::<gtk::Widget>();
    if root.widget_name() == name {
        return Some(root.clone());
    }
    let mut child = root.first_child();
    while let Some(c) = child {
        if let Some(found) = find_named(&c, name) {
            return Some(found);
        }
        child = c.next_sibling();
    }
    None
}

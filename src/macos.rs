//! macOS glue GTK leaves out. A click on the Dock icon brings back a window
//! closed for background play, mixtapes:// links reach the app, Quit in the
//! app menu (or a logout) runs the app's own quit instead of ending the
//! process on the spot, and the menu bar carries the app's actions, as Mac
//! apps do.

use std::cell::RefCell;
use std::rc::Rc;

use adw::prelude::*;
use gtk::{gdk, gio, glib};
use objc2::encode::Encode;
use objc2::runtime::{AnyClass, AnyObject, Bool, Sel};
use objc2::{MainThreadMarker, sel};
use objc2_app_kit::NSApplication;
use objc2::rc::Retained;
use objc2::{AnyThread, define_class, msg_send};
use objc2_foundation::{NSAppleEventDescriptor, NSAppleEventManager, NSObject};

use crate::model::RepeatMode;
use crate::player::Player;

thread_local! {
    /// The application the Dock and link handlers reach. AppKit calls them on the main thread.
    static APP: RefCell<Option<glib::WeakRef<adw::Application>>> = const { RefCell::new(None) };
    /// mixtapes:// links that arrived before the application started.
    static PENDING_LINKS: RefCell<Vec<String>> = const { RefCell::new(Vec::new()) };
}

/// 'GURL', the class and id of the Apple event a URL arrives in, and '----', its direct object.
const URL_EVENT: u32 = u32::from_be_bytes(*b"GURL");
const DIRECT_OBJECT: u32 = u32::from_be_bytes(*b"----");

define_class!(
    /// Receives the Apple event macOS sends a mixtapes:// link in.
    #[unsafe(super(NSObject))]
    #[name = "MixtapesLinkHandler"]
    struct LinkHandler;

    impl LinkHandler {
        #[unsafe(method(handleGetURLEvent:withReplyEvent:))]
        fn handle_get_url(&self, event: &NSAppleEventDescriptor, _reply: &NSAppleEventDescriptor) {
            // SAFETY: paramDescriptorForKeyword: takes an AEKeyword (a u32) and returns a descriptor or nil.
            let direct: Option<Retained<NSAppleEventDescriptor>> = unsafe { msg_send![event, paramDescriptorForKeyword: DIRECT_OBJECT] };
            let Some(link) = direct.and_then(|d| d.stringValue()) else { return };
            let link = link.to_string();
            if APP.with(|cell| cell.borrow().is_some()) {
                open_link(&link);
            } else {
                PENDING_LINKS.with(|pending| pending.borrow_mut().push(link));
            }
        }
    }
);

/// Listen for mixtapes:// links. Call from main, before the application runs:
/// a link that launched the app arrives while AppKit finishes launching, and
/// GTK's delegate would hand it nowhere.
pub fn listen_for_links() {
    // SAFETY: a plain NSObject subclass, allocated and initialised.
    let handler: Retained<LinkHandler> = unsafe { msg_send![LinkHandler::alloc(), init] };
    // SAFETY: the selector matches the method defined above; the manager does
    // not retain the handler, so it is leaked to live as long as the process.
    unsafe {
        let manager = NSAppleEventManager::sharedAppleEventManager();
        let _: () = msg_send![
            &manager,
            setEventHandler: &*handler,
            andSelector: sel!(handleGetURLEvent:withReplyEvent:),
            forEventClass: URL_EVENT,
            andEventID: URL_EVENT
        ];
    }
    std::mem::forget(handler);
}

/// Hand a link to GApplication's open, as a link on the command line does on Linux.
fn open_link(link: &str) {
    let app = APP.with(|cell| cell.borrow().as_ref().and_then(|weak| weak.upgrade()));
    if let Some(app) = app {
        tracing::debug!(link, "link from macOS");
        app.open(&[gio::File::for_uri(link)], "");
    }
}

/// Call from the application's startup, once GTK has installed its NSApplication delegate.
pub fn install(app: &adw::Application) {
    // GTK's delegate answers applicationShouldTerminate: by activating app.quit
    // when there is one, and by terminating at once when there is not. Its app
    // menu also names app.about and app.preferences. All three live on the
    // main window, which may be hidden for background play.
    for (name, target) in [("quit", "win.quit"), ("about", "win.about"), ("preferences", "win.preferences")] {
        let action = gio::SimpleAction::new(name, None);
        let weak = app.downgrade();
        action.connect_activate(move |_, _| {
            let Some(app) = weak.upgrade() else { return };
            match main_window(&app) {
                Some(window) => {
                    if target != "win.quit" {
                        window.present();
                    }
                    let _ = WidgetExt::activate_action(&window, target, None);
                }
                None if target == "win.quit" => app.quit(),
                None => {}
            }
        });
        app.add_action(&action);
    }

    APP.with(|cell| cell.replace(Some(app.downgrade())));
    add_reopen_handler();
    // Links that arrived while the app was starting, once startup has finished.
    glib::idle_add_local_once(|| {
        for link in PENDING_LINKS.with(|pending| pending.take()) {
            open_link(&link);
        }
    });
}

fn main_window(app: &adw::Application) -> Option<gtk::Window> {
    app.windows().into_iter().find(|w| w.is::<adw::ApplicationWindow>())
}

/// The menu bar. Window items act on the main window and grey out while it is
/// hidden; the Controls menu belongs to the app, so it works either way.
pub fn install_menu(app: &adw::Application, player: &Rc<Player>) {
    install_playback_actions(app, player);

    let menubar = gio::Menu::new();

    let file = gio::Menu::new();
    let account = gio::Menu::new();
    account.append(Some("Sign In…"), Some("win.sign-in"));
    account.append(Some("Sign Out"), Some("win.logout"));
    file.append_section(None, &account);
    let upload = gio::Menu::new();
    upload.append(Some("Upload Songs…"), Some("win.open-upload"));
    file.append_section(None, &upload);
    let close = gio::Menu::new();
    close.append(Some("Close Window"), Some("window.close"));
    file.append_section(None, &close);
    menubar.append_submenu(Some("File"), &file);

    // GTK's own Edit menu, which the text fields need, plus Find.
    let edit = gio::Menu::new();
    let history = gio::Menu::new();
    history.append(Some("Undo"), Some("text.undo"));
    history.append(Some("Redo"), Some("text.redo"));
    edit.append_section(None, &history);
    let clipboard = gio::Menu::new();
    clipboard.append(Some("Cut"), Some("clipboard.cut"));
    clipboard.append(Some("Copy"), Some("clipboard.copy"));
    clipboard.append(Some("Paste"), Some("clipboard.paste"));
    clipboard.append(Some("Delete"), Some("selection.delete"));
    clipboard.append(Some("Select All"), Some("selection.select-all"));
    edit.append_section(None, &clipboard);
    let find = gio::Menu::new();
    find.append(Some("Find"), Some("win.search"));
    edit.append_section(None, &find);
    menubar.append_submenu(Some("Edit"), &edit);

    let view = gio::Menu::new();
    let player_section = gio::Menu::new();
    player_section.append(Some("Show or Hide Player"), Some("win.toggle-player"));
    player_section.append(Some("Refresh"), Some("win.refresh"));
    view.append_section(None, &player_section);
    let places = gio::Menu::new();
    places.append(Some("History"), Some("win.open-history"));
    places.append(Some("Downloaded Songs"), Some("win.open-downloads"));
    places.append(Some("Uploaded Songs"), Some("win.open-uploads"));
    places.append(Some("Your Channel"), Some("win.open-channel"));
    view.append_section(None, &places);
    menubar.append_submenu(Some("View"), &view);

    let controls = gio::Menu::new();
    let transport = gio::Menu::new();
    transport.append(Some("Play/Pause"), Some("app.play-pause"));
    transport.append(Some("Next"), Some("app.next"));
    transport.append(Some("Previous"), Some("app.previous"));
    controls.append_section(None, &transport);
    let modes = gio::Menu::new();
    modes.append(Some("Shuffle"), Some("app.shuffle"));
    let repeat = gio::Menu::new();
    repeat.append(Some("Off"), Some("app.repeat::off"));
    repeat.append(Some("All"), Some("app.repeat::all"));
    repeat.append(Some("One"), Some("app.repeat::one"));
    modes.append_submenu(Some("Repeat"), &repeat);
    controls.append_section(None, &modes);
    menubar.append_submenu(Some("Controls"), &controls);

    // GTK fills this one with Minimize, Zoom and the window list.
    let window = gio::MenuItem::new_submenu(Some("Window"), &gio::Menu::new());
    window.set_attribute_value("gtk-macos-special", Some(&"window-submenu".to_variant()));
    menubar.append_item(&window);

    let help = gio::Menu::new();
    help.append(Some("Keyboard Shortcuts"), Some("win.shortcuts"));
    help.append(Some("What's New"), Some("win.whats-new"));
    menubar.append_submenu(Some("Help"), &help);

    app.set_menubar(Some(&menubar));
}

fn install_playback_actions(app: &adw::Application, player: &Rc<Player>) {
    let simple = |name: &str, run: fn(&Player)| {
        let action = gio::SimpleAction::new(name, None);
        let player = player.clone();
        action.connect_activate(move |_, _| run(&player));
        app.add_action(&action);
    };
    simple("play-pause", Player::toggle_play);
    simple("next", Player::next);
    simple("previous", Player::previous);

    let state = player.state();
    let shuffle = gio::SimpleAction::new_stateful("shuffle", None, &state.shuffle().to_variant());
    {
        let player = player.clone();
        shuffle.connect_change_state(move |_, value| {
            if let Some(on) = value.and_then(|v| v.get::<bool>()) {
                player.set_shuffle(on);
            }
        });
    }
    {
        let shuffle = shuffle.clone();
        state.connect_shuffle_notify(move |state| shuffle.set_state(&state.shuffle().to_variant()));
    }
    app.add_action(&shuffle);

    let repeat = gio::SimpleAction::new_stateful("repeat", Some(glib::VariantTy::STRING), &repeat_word(state.repeat()).to_variant());
    {
        let player = player.clone();
        repeat.connect_change_state(move |_, value| {
            let mode = match value.and_then(|v| v.str()) {
                Some("all") => RepeatMode::All,
                Some("one") => RepeatMode::Track,
                _ => RepeatMode::Off,
            };
            player.set_repeat(mode);
        });
    }
    {
        let repeat = repeat.clone();
        state.connect_repeat_notify(move |state| repeat.set_state(&repeat_word(state.repeat()).to_variant()));
    }
    app.add_action(&repeat);
}

fn repeat_word(mode: RepeatMode) -> &'static str {
    match mode {
        RepeatMode::Off => "off",
        RepeatMode::All => "all",
        RepeatMode::Track => "one",
    }
}

/// applicationShouldHandleReopen:hasVisibleWindows:, what AppKit asks when the
/// Dock icon of a running app is clicked.
extern "C-unwind" fn should_handle_reopen(_this: &AnyObject, _cmd: Sel, _sender: &AnyObject, _visible: Bool) -> Bool {
    let app = APP.with(|cell| cell.borrow().as_ref().and_then(|weak| weak.upgrade()));
    if let Some(app) = app {
        // on_activate presents the main window, hidden or not.
        app.activate();
    }
    Bool::NO
}

/// GTK's delegate has no reopen handler, so one is added to its class.
fn add_reopen_handler() {
    let Some(mtm) = MainThreadMarker::new() else { return };
    let ns_app = NSApplication::sharedApplication(mtm);
    let Some(delegate) = ns_app.delegate() else {
        tracing::warn!("no NSApplication delegate, the Dock icon cannot reopen the window");
        return;
    };
    let object: &AnyObject = delegate.as_ref();
    let class = (object.class() as *const AnyClass).cast_mut();

    let reopen: unsafe extern "C-unwind" fn(&AnyObject, Sel, &AnyObject, Bool) -> Bool = should_handle_reopen;
    let types = format!("{}@:@{}\0", Bool::ENCODING, Bool::ENCODING);
    // SAFETY: the function matches the selector's signature and type string,
    // and GTK's delegate class does not implement the selector itself.
    let added = unsafe {
        objc2::ffi::class_addMethod(
            class,
            sel!(applicationShouldHandleReopen:hasVisibleWindows:),
            std::mem::transmute::<unsafe extern "C-unwind" fn(&AnyObject, Sel, &AnyObject, Bool) -> Bool, unsafe extern "C-unwind" fn()>(reopen),
            types.as_ptr().cast(),
        )
    };
    if !added.as_bool() {
        tracing::warn!("could not add the Dock reopen handler");
    }
}

/// Label the pointer as a touchpad, so libadwaita's swipe tracker follows
/// two-finger swipes with the page under the fingers, as on GNOME. GTK's macOS
/// backend sends every scroll from one pointer it calls a mouse, and the
/// tracker ignores scrolls from mice. A mouse wheel without precise deltas
/// sends discrete steps, which the tracker ignores either way.
///
/// The source is construct-only, so this writes GdkDevice's private field. The
/// layout is checked against the public getters first, and false means
/// nothing was changed.
pub fn report_trackpad_as_touchpad() -> bool {
    use gtk::glib::translate::ToGlibPtr;
    let Some(pointer) = gdk::Display::default().and_then(|d| d.default_seat()).and_then(|s| s.pointer()) else {
        return false;
    };
    if pointer.source() == gdk::InputSource::Touchpad {
        return true;
    }
    let raw: *mut gdk::ffi::GdkDevice = pointer.to_glib_none().0;
    // struct _GdkDevice { GObject parent_instance; char *name; GdkInputSource source; ... }
    // SAFETY: the reads stay inside the device struct, and the write happens
    // only once the name field sits where the layout above puts it.
    unsafe {
        let name_slot = raw.cast::<u8>().add(std::mem::size_of::<glib::gobject_ffi::GObject>()).cast::<*const std::ffi::c_char>();
        if *name_slot != gdk::ffi::gdk_device_get_name(raw) {
            tracing::warn!("unexpected GdkDevice layout, trackpad swipes stay emulated");
            return false;
        }
        let source_slot = name_slot.add(1).cast::<gdk::ffi::GdkInputSource>();
        if *source_slot != gdk::ffi::GDK_SOURCE_MOUSE {
            return false;
        }
        *source_slot = gdk::ffi::GDK_SOURCE_TOUCHPAD;
    }
    pointer.source() == gdk::InputSource::Touchpad
}

/// Trust the root certificates in the macOS keychain for GIO's TLS, which
/// GStreamer streams through. gnutls looks for its trust store under the
/// prefix it was built in (a p11-kit module under /opt/local, a PEM file under
/// /opt/homebrew), which a Mac without that package manager does not have.
/// The anchors are written out as PEM, since GIO's file database is the one
/// that takes them. Call before anything opens a TLS connection.
pub fn use_keychain_roots(cache_dir: &std::path::Path) {
    use std::ffi::c_void;
    #[link(name = "Security", kind = "framework")]
    unsafe extern "C" {
        fn SecTrustCopyAnchorCertificates(anchors: *mut *const c_void) -> i32;
        fn SecCertificateCopyData(certificate: *const c_void) -> *const c_void;
    }
    #[link(name = "CoreFoundation", kind = "framework")]
    unsafe extern "C" {
        fn CFArrayGetCount(array: *const c_void) -> isize;
        fn CFArrayGetValueAtIndex(array: *const c_void, index: isize) -> *const c_void;
        fn CFDataGetLength(data: *const c_void) -> isize;
        fn CFDataGetBytePtr(data: *const c_void) -> *const u8;
        fn CFRelease(object: *const c_void);
    }

    let mut pem = String::new();
    // SAFETY: Core Foundation calls on objects this function owns and releases;
    // the byte slices are read before their CFData is released.
    unsafe {
        let mut anchors: *const c_void = std::ptr::null();
        if SecTrustCopyAnchorCertificates(&mut anchors) != 0 || anchors.is_null() {
            tracing::warn!("could not read the keychain's root certificates");
            return;
        }
        for index in 0..CFArrayGetCount(anchors) {
            let data = SecCertificateCopyData(CFArrayGetValueAtIndex(anchors, index));
            if data.is_null() {
                continue;
            }
            let der = std::slice::from_raw_parts(CFDataGetBytePtr(data), CFDataGetLength(data) as usize);
            let encoded = glib::base64_encode(der);
            pem.push_str("-----BEGIN CERTIFICATE-----\n");
            for line in encoded.as_bytes().chunks(64) {
                pem.push_str(std::str::from_utf8(line).unwrap_or_default());
                pem.push('\n');
            }
            pem.push_str("-----END CERTIFICATE-----\n");
            CFRelease(data);
        }
        CFRelease(anchors);
    }

    let path = cache_dir.join("keychain-roots.pem");
    if std::fs::create_dir_all(cache_dir).and_then(|()| std::fs::write(&path, &pem)).is_err() {
        tracing::warn!(?path, "could not write the root certificates");
        return;
    }
    match gio::TlsFileDatabase::new(&path) {
        Ok(database) => gio::TlsBackend::default().set_default_database(Some(&database)),
        Err(err) => tracing::warn!(%err, "root certificates not loaded"),
    }
}

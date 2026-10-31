//! The notification-area icon on Windows, where a window closed for background
//! playback has no other way back than launching the app again. A click shows
//! the window; the menu has the transport controls and Quit. Other platforms
//! have their shell's media controls and get nothing here.

#[cfg(not(windows))]
pub fn start(_ctx: &std::rc::Rc<crate::App>) {}

#[cfg(not(windows))]
pub fn stop() {}

#[cfg(windows)]
pub use win::{start, stop};

#[cfg(windows)]
mod win {
    use std::cell::RefCell;
    use std::rc::{Rc, Weak};

    use gtk::prelude::*;
    use gtk::{gio, glib};
    use windows::Win32::Foundation::{HWND, LPARAM, LRESULT, POINT, WPARAM};
    use windows::Win32::System::LibraryLoader::GetModuleHandleW;
    use windows::Win32::UI::Shell::{NIF_ICON, NIF_MESSAGE, NIF_TIP, NIM_ADD, NIM_DELETE, NIM_MODIFY, NOTIFYICONDATAW, Shell_NotifyIconW};
    use windows::Win32::UI::WindowsAndMessaging::{
        AppendMenuW, CreatePopupMenu, CreateWindowExW, DefWindowProcW, DestroyMenu, GetCursorPos, LoadIconW, MF_SEPARATOR, MF_STRING, PostMessageW,
        RegisterClassW, RegisterWindowMessageW, SetForegroundWindow, TPM_RETURNCMD, TPM_RIGHTBUTTON, TrackPopupMenu, WINDOW_EX_STYLE, WINDOW_STYLE,
        WM_APP, WM_CONTEXTMENU, WM_LBUTTONUP, WM_NULL, WM_RBUTTONUP, WNDCLASSW,
    };
    use windows::core::{PCWSTR, w};

    use crate::App;
    use crate::model::PlaybackStatus;

    /// The message the icon's clicks arrive as.
    const WM_TRAY: u32 = WM_APP + 1;
    const ICON_ID: u32 = 1;

    const MENU_PLAY_PAUSE: usize = 1;
    const MENU_PREVIOUS: usize = 2;
    const MENU_NEXT: usize = 3;
    const MENU_SHOW: usize = 4;
    const MENU_QUIT: usize = 5;

    struct Tray {
        hwnd: HWND,
        ctx: Weak<App>,
        /// Explorer broadcasts this after it restarts, and every icon must be added again.
        taskbar_created: u32,
    }

    thread_local! {
        /// The window procedure is a plain function, so it reaches the icon through here.
        /// GDK dispatches every message on the GTK thread, which is this one.
        static TRAY: RefCell<Option<Tray>> = const { RefCell::new(None) };
    }

    pub fn start(ctx: &Rc<App>) {
        match create_window() {
            Ok(hwnd) => {
                // SAFETY: a static NUL-terminated name.
                let taskbar_created = unsafe { RegisterWindowMessageW(w!("TaskbarCreated")) };
                TRAY.with(|t| t.replace(Some(Tray { hwnd, ctx: Rc::downgrade(ctx), taskbar_created })));
                add_icon(hwnd);
                watch_track(ctx);
            }
            Err(err) => tracing::warn!(%err, "tray icon unavailable"),
        }
    }

    /// Take the icon down at quit, or it lingers until the pointer passes over it.
    pub fn stop() {
        if let Some(tray) = TRAY.with(|t| t.borrow_mut().take()) {
            let data = icon_data(tray.hwnd);
            // SAFETY: the data names our own window and icon id.
            let _ = unsafe { Shell_NotifyIconW(NIM_DELETE, &data) };
        }
    }

    fn create_window() -> windows::core::Result<HWND> {
        let class_name = w!("MixtapesTray");
        // SAFETY: plain Win32 calls with a static class name; the window is never shown.
        unsafe {
            let instance = GetModuleHandleW(None)?;
            let class = WNDCLASSW { lpfnWndProc: Some(window_proc), hInstance: instance.into(), lpszClassName: class_name, ..Default::default() };
            RegisterClassW(&class);
            CreateWindowExW(WINDOW_EX_STYLE(0), class_name, w!("Mixtapes"), WINDOW_STYLE(0), 0, 0, 0, 0, None, None, Some(instance.into()), None)
        }
    }

    fn icon_data(hwnd: HWND) -> NOTIFYICONDATAW {
        NOTIFYICONDATAW { cbSize: std::mem::size_of::<NOTIFYICONDATAW>() as u32, hWnd: hwnd, uID: ICON_ID, ..Default::default() }
    }

    fn add_icon(hwnd: HWND) {
        let mut data = icon_data(hwnd);
        data.uFlags = NIF_ICON | NIF_MESSAGE | NIF_TIP;
        data.uCallbackMessage = WM_TRAY;
        // SAFETY: resource 1 is the app icon build.rs embeds from windows/mixtapes.rc.
        if let Ok(icon) = unsafe { GetModuleHandleW(None).and_then(|module| LoadIconW(Some(module.into()), PCWSTR(1 as *const u16))) } {
            data.hIcon = icon;
        }
        set_tip(&mut data, "Mixtapes");
        // SAFETY: the data names our own window and outlives the call.
        if !unsafe { Shell_NotifyIconW(NIM_ADD, &data) }.as_bool() {
            tracing::warn!("tray icon was not added");
        }
    }

    /// The tooltip names the playing track.
    fn watch_track(ctx: &Rc<App>) {
        let update = {
            let player = ctx.player.clone();
            move || {
                let state = player.state();
                let (title, artist) = (state.title(), state.artist());
                let tip = match (title.is_empty(), artist.is_empty()) {
                    (true, _) => "Mixtapes".to_owned(),
                    (false, true) => title,
                    (false, false) => format!("{title} - {artist}"),
                };
                let Some(hwnd) = TRAY.with(|t| t.borrow().as_ref().map(|t| t.hwnd)) else { return };
                let mut data = icon_data(hwnd);
                data.uFlags = NIF_TIP;
                set_tip(&mut data, &tip);
                // SAFETY: the data names our own window and outlives the call.
                let _ = unsafe { Shell_NotifyIconW(NIM_MODIFY, &data) };
            }
        };
        for name in ["title", "artist"] {
            let update = update.clone();
            ctx.player.state().connect_notify_local(Some(name), move |_, _| update());
        }
    }

    /// Copy `text` into the fixed tooltip buffer, cut to fit with its NUL.
    fn set_tip(data: &mut NOTIFYICONDATAW, text: &str) {
        let wide: Vec<u16> = text.encode_utf16().take(data.szTip.len() - 1).collect();
        data.szTip = [0; 128];
        data.szTip[..wide.len()].copy_from_slice(&wide);
    }

    unsafe extern "system" fn window_proc(hwnd: HWND, msg: u32, wparam: WPARAM, lparam: LPARAM) -> LRESULT {
        if msg == WM_TRAY {
            match lparam.0 as u32 {
                WM_LBUTTONUP => {
                    glib::idle_add_local_once(show_window);
                }
                WM_RBUTTONUP | WM_CONTEXTMENU => {
                    // Not from inside the icon's own message: the menu runs a modal loop.
                    glib::idle_add_local_once(move || show_menu(hwnd));
                }
                _ => {}
            }
            return LRESULT(0);
        }
        if TRAY.with(|t| t.borrow().as_ref().is_some_and(|t| t.taskbar_created == msg)) {
            add_icon(hwnd);
            return LRESULT(0);
        }
        // SAFETY: forwarding the arguments Windows passed in.
        unsafe { DefWindowProcW(hwnd, msg, wparam, lparam) }
    }

    fn ctx() -> Option<Rc<App>> {
        TRAY.with(|t| t.borrow().as_ref().and_then(|t| t.ctx.upgrade()))
    }

    fn show_window() {
        let Some(ctx) = ctx() else { return };
        if let Some(window) = ctx.window.borrow().clone() {
            window.window().set_visible(true);
            window.present();
        }
    }

    fn show_menu(hwnd: HWND) {
        let Some(ctx) = ctx() else { return };
        let playing = ctx.player.state().status() == PlaybackStatus::Playing;
        let has_track = ctx.player.current_track().is_some();
        let bounds = ctx.player.bounds();
        // SAFETY: a menu we create, show and destroy here; the window is ours.
        let chosen = unsafe {
            let Ok(menu) = CreatePopupMenu() else { return };
            let item = |id: usize, text: String, enabled: bool| {
                let flags = if enabled { MF_STRING } else { MF_STRING | windows::Win32::UI::WindowsAndMessaging::MF_GRAYED };
                // The menu copies the text, so the buffer only has to live through the call.
                let wide: Vec<u16> = text.encode_utf16().chain(std::iter::once(0)).collect();
                let _ = AppendMenuW(menu, flags, id, PCWSTR(wide.as_ptr()));
            };
            item(MENU_PLAY_PAUSE, if playing { tr!("Pause") } else { tr!("Play") }, has_track);
            item(MENU_PREVIOUS, tr!("Previous"), has_track);
            item(MENU_NEXT, tr!("Next"), bounds.can_next);
            let _ = AppendMenuW(menu, MF_SEPARATOR, 0, None);
            item(MENU_SHOW, tr!("Show Mixtapes"), true);
            item(MENU_QUIT, tr!("Quit"), true);
            let mut point = POINT::default();
            let _ = GetCursorPos(&mut point);
            // Without this the menu stays open when the pointer clicks elsewhere.
            let _ = SetForegroundWindow(hwnd);
            let chosen = TrackPopupMenu(menu, TPM_RETURNCMD | TPM_RIGHTBUTTON, point.x, point.y, None, hwnd, None).0 as usize;
            let _ = PostMessageW(Some(hwnd), WM_NULL, WPARAM(0), LPARAM(0));
            let _ = DestroyMenu(menu);
            chosen
        };
        match chosen {
            MENU_PLAY_PAUSE => ctx.player.toggle_play(),
            MENU_PREVIOUS => ctx.player.previous(),
            MENU_NEXT => ctx.player.next(),
            MENU_SHOW => show_window(),
            MENU_QUIT => {
                ctx.player.stop();
                if let Some(app) = gio::Application::default() {
                    app.quit();
                }
            }
            _ => {}
        }
    }
}

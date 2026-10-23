//! Process-level setup that must run before GTK, GStreamer or any thread exists.
//! Mirrors the top of src/main.py.

use tracing_subscriber::EnvFilter;
use tracing_subscriber::layer::SubscriberExt;
use tracing_subscriber::util::SubscriberInitExt;

use crate::paths::Paths;

/// Cap glibc malloc arenas at 2.
/// GStreamer decodes on short-lived streaming threads and glibc gives each one
/// its own arena. Those arenas never shrink, so RSS climbed per track change.
/// Python had to re-exec with MALLOC_ARENA_MAX set; Rust can call mallopt
/// directly because main runs before any thread is spawned.
pub fn cap_malloc_arenas() {
    #[cfg(all(target_os = "linux", target_env = "gnu"))]
    {
        // SAFETY: mallopt only writes allocator tunables; called before threads exist.
        let rc = unsafe { libc::mallopt(libc::M_ARENA_MAX, 2) };
        if rc != 1 {
            tracing::warn!("mallopt(M_ARENA_MAX) rejected");
        }
    }
}

/// Raise the open-file soft limit toward 65536.
/// Long sessions leaked into the default 1024 limit and network calls died.
pub fn raise_fd_limit() {
    #[cfg(unix)]
    {
        const TARGET: libc::rlim_t = 65_536;
        let mut lim = libc::rlimit { rlim_cur: 0, rlim_max: 0 };
        // SAFETY: rlimit is a plain C struct and the pointer is valid for the call.
        if unsafe { libc::getrlimit(libc::RLIMIT_NOFILE, &mut lim) } != 0 {
            return;
        }
        let target = if lim.rlim_max == libc::RLIM_INFINITY { TARGET } else { lim.rlim_max.min(TARGET) };
        if lim.rlim_cur >= target {
            return;
        }
        let old = lim.rlim_cur;
        lim.rlim_cur = target;
        // SAFETY: same struct, now populated.
        if unsafe { libc::setrlimit(libc::RLIMIT_NOFILE, &lim) } == 0 {
            tracing::info!(old, new = target, "raised open-file soft limit");
        }
    }
}

/// Give the process the AppUserModelID the installer registers, before GTK
/// opens a window. Windows uses it to group the taskbar entry with the Start
/// menu shortcut and to name the app in the media flyout.
pub fn set_app_user_model_id() {
    #[cfg(windows)]
    // SAFETY: a static NUL-terminated string, set before any window exists.
    if let Err(err) = unsafe { windows::Win32::UI::Shell::SetCurrentProcessExplicitAppUserModelID(windows::core::w!("com.pocoguy.Muse")) } {
        tracing::warn!(%err, "AppUserModelID not set");
    }
}

/// Make mixtapes:// links open this exe on Windows, for the current user. The
/// installer registers the same keys; doing it here too covers the portable
/// build and a moved install. The link reaches the running window through
/// GApplication's open, as on Linux.
pub fn register_link_scheme() {
    #[cfg(windows)]
    {
        let Ok(exe) = std::env::current_exe() else { return };
        let exe = exe.display().to_string();
        let command = format!("\"{exe}\" \"%1\"");
        let values: [(&str, Option<&str>, &str); 4] = [
            (r"Software\Classes\mixtapes", None, "URL:Mixtapes link"),
            (r"Software\Classes\mixtapes", Some("URL Protocol"), ""),
            (r"Software\Classes\mixtapes\DefaultIcon", None, &exe),
            (r"Software\Classes\mixtapes\shell\open\command", None, &command),
        ];
        for (key, name, value) in values {
            if let Err(err) = set_registry_string(key, name, value) {
                tracing::warn!(%err, key, "mixtapes:// link registration failed");
                return;
            }
        }
    }
}

/// Write one string value under HKEY_CURRENT_USER, creating the key.
#[cfg(windows)]
fn set_registry_string(key: &str, name: Option<&str>, value: &str) -> windows::core::Result<()> {
    use windows::Win32::System::Registry::{HKEY, HKEY_CURRENT_USER, KEY_SET_VALUE, REG_OPTION_NON_VOLATILE, REG_SZ, RegCloseKey, RegCreateKeyExW, RegSetValueExW};
    use windows::core::{HSTRING, PCWSTR};
    let mut handle = HKEY::default();
    // SAFETY: NUL-terminated strings that outlive each call; the key is closed below.
    unsafe {
        RegCreateKeyExW(HKEY_CURRENT_USER, &HSTRING::from(key), None, None, REG_OPTION_NON_VOLATILE, KEY_SET_VALUE, None, &mut handle, None).ok()?;
        let data: Vec<u8> = value.encode_utf16().chain(std::iter::once(0)).flat_map(u16::to_le_bytes).collect();
        let name = name.map(HSTRING::from);
        let result = RegSetValueExW(handle, name.as_ref().map_or(PCWSTR::null(), |n| PCWSTR(n.as_ptr())), None, REG_SZ, Some(&data)).ok();
        let _ = RegCloseKey(handle);
        result
    }
}

/// Put the install's own folder first on PATH on Windows and macOS.
/// yt-dlp looks up node and ffmpeg on PATH, and the installer (or Mixtapes.app's
/// Contents/MacOS) puts them beside the app. A Finder launch on macOS also has
/// none of the shell's PATH to find them elsewhere.
pub fn prefer_bundled_programs() {
    #[cfg(any(windows, target_os = "macos"))]
    {
        let Some(dir) = std::env::current_exe().ok().and_then(|exe| exe.parent().map(std::path::Path::to_path_buf)) else {
            return;
        };
        let rest = std::env::var_os("PATH").unwrap_or_default();
        let Ok(joined) = std::env::join_paths(std::iter::once(dir).chain(std::env::split_paths(&rest))) else {
            return;
        };
        // SAFETY: called from main before any other thread exists.
        unsafe { std::env::set_var("PATH", joined) };
    }
}

/// Point GLib, GTK and GStreamer at what Mixtapes.app carries in
/// Contents/Resources. Homebrew's builds look in Homebrew's prefix, which a
/// Mac without Homebrew lacks. Variables already set win, and a run outside
/// a bundle changes nothing.
pub fn use_bundled_runtime() {
    #[cfg(target_os = "macos")]
    {
        let Some(contents) = std::env::current_exe().ok().and_then(|exe| exe.parent()?.parent().map(std::path::Path::to_path_buf)) else {
            return;
        };
        let resources = contents.join("Resources");
        if !contents.ends_with("Contents") || !resources.join("lib").is_dir() {
            return;
        }
        let registry = glib::user_cache_dir().join("muse").join("gstreamer-registry.bin");
        let vars = [
            ("XDG_DATA_DIRS", resources.join("share")),
            ("GSETTINGS_SCHEMA_DIR", resources.join("share/glib-2.0/schemas")),
            ("GIO_MODULE_DIR", resources.join("lib/gio/modules")),
            // Empty: the built-in PNG and JPEG loaders are all the app needs, and
            // the default path would load a package manager's loaders on top.
            ("GDK_PIXBUF_MODULE_FILE", resources.join("lib/gdk-pixbuf-2.0/loaders.cache")),
            ("GST_PLUGIN_SYSTEM_PATH_1_0", resources.join("lib/gstreamer-1.0")),
            ("GST_PLUGIN_SCANNER_1_0", contents.join("MacOS/gst-plugin-scanner")),
            // Apart from a Homebrew GStreamer's registry, which lists other plugin paths.
            ("GST_REGISTRY_1_0", registry),
        ];
        for (key, value) in vars {
            if std::env::var_os(key).is_none() {
                // SAFETY: called from main before any other thread exists.
                unsafe { std::env::set_var(key, value) };
            }
        }
        // GStreamer's soup plugins (souphttpsrc, and the HLS downloader) open
        // libsoup by its bare file name, which macOS never looks for inside an
        // app. bundle.sh gives the bundled copy that bare name as its install
        // name, and a library already loaded answers to its install name.
        let soup = contents.join("Frameworks/libsoup-3.0.0.dylib");
        if let Ok(path) = std::ffi::CString::new(soup.to_string_lossy().as_bytes()) {
            // SAFETY: a NUL-terminated path; the handle is kept for the life of the process.
            if unsafe { libc::dlopen(path.as_ptr(), libc::RTLD_NOW | libc::RTLD_GLOBAL) }.is_null() {
                tracing::warn!(?soup, "bundled libsoup did not load, streaming will fail");
            }
        }
    }
}

/// Lay text out through fontconfig on Windows, like the Linux build, instead of Pango's win32 backend.
/// The bundled etc/fonts/fonts.conf adds the install's Adwaita fonts and the Windows font folders.
/// Both variables are read once when Pango creates its font map, so this runs before GTK loads.
pub fn use_bundled_fonts() {
    // macOS lays text out through CoreText, which only knows installed fonts.
    // Registering the app's copies for this process makes Adwaita Sans and
    // Mono available without installing them.
    #[cfg(target_os = "macos")]
    {
        use objc2_core_foundation::CFURL;
        use objc2_core_text::{CTFontManagerRegisterFontsForURL, CTFontManagerScope};
        let exe = std::env::current_exe().ok();
        // Mixtapes.app/Contents/Resources/share/fonts, or the repository's fonts/ for a cargo run.
        let bundled = exe.as_deref().and_then(|e| Some(e.parent()?.parent()?.join("Resources/share/fonts")));
        let source = cfg!(debug_assertions).then(|| std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("fonts"));
        let Some(dir) = bundled.into_iter().chain(source).find(|d| d.is_dir()) else { return };
        let Ok(entries) = std::fs::read_dir(&dir) else { return };
        for path in entries.flatten().map(|e| e.path()).filter(|p| p.extension().is_some_and(|x| x == "ttf")) {
            let Some(url) = CFURL::from_file_path(&path) else { continue };
            // SAFETY: a valid file URL; errors are not collected.
            if !unsafe { CTFontManagerRegisterFontsForURL(&url, CTFontManagerScope::Process, std::ptr::null_mut()) } {
                tracing::debug!(?path, "font not registered");
            }
        }
    }
    #[cfg(windows)]
    {
        let Some(root) = std::env::current_exe().ok().and_then(|exe| exe.parent()?.parent().map(std::path::Path::to_path_buf)) else {
            return;
        };
        let config = root.join("etc").join("fonts").join("fonts.conf");
        if !config.is_file() {
            return;
        }
        for (key, value) in [("PANGOCAIRO_BACKEND", std::ffi::OsString::from("fc")), ("FONTCONFIG_FILE", config.into_os_string())] {
            if std::env::var_os(key).is_none() {
                set_crt_env(key, &value);
            }
        }
    }
}

/// Let GTK render on the GPU on Windows. GTK 4.24's OpenGL and Vulkan renderers
/// need DirectComposition there, which is opt-in through GDK_DEBUG, so without
/// it every window falls back to the CPU renderer and animations crawl.
pub fn enable_gpu_rendering() {
    #[cfg(windows)]
    {
        let current = std::env::var("GDK_DEBUG").unwrap_or_default();
        if current.split([',', ':', ' ']).any(|flag| flag == "dcomp") {
            return;
        }
        let value = if current.is_empty() { "dcomp".to_owned() } else { format!("{current},dcomp") };
        set_crt_env("GDK_DEBUG", std::ffi::OsStr::new(&value));
    }
}

/// Sets a variable in both environments a Windows process has. Pango and
/// fontconfig read the C runtime's copy through getenv, which set_var
/// (SetEnvironmentVariableW) leaves untouched.
#[cfg(windows)]
fn set_crt_env(key: &str, value: &std::ffi::OsStr) {
    use std::os::windows::ffi::OsStrExt;
    unsafe extern "C" {
        fn _wputenv_s(name: *const u16, value: *const u16) -> i32;
    }
    let wide = |text: &std::ffi::OsStr| text.encode_wide().chain(std::iter::once(0)).collect::<Vec<u16>>();
    let (key_wide, value_wide) = (wide(std::ffi::OsStr::new(key)), wide(value));
    // SAFETY: called from main before any other thread exists; both buffers are
    // NUL-terminated and outlive the call.
    unsafe {
        std::env::set_var(key, value);
        _wputenv_s(key_wide.as_ptr(), value_wide.as_ptr());
    }
}

/// Honor the user's GSK renderer choice from prefs.json before GTK loads.
/// An explicit GSK_RENDERER in the environment wins.
pub fn apply_gsk_renderer_pref(paths: &Paths) {
    if std::env::var_os("GSK_RENDERER").is_some() {
        return;
    }
    let prefs = paths.read_prefs();
    let value = prefs.get("gsk_renderer").and_then(|v| v.as_str()).unwrap_or_default();
    if value.is_empty() || value == "default" {
        // Windows: GTK's GL renderer paints the shadow margin around the
        // window black under DirectComposition. Vulkan keeps it transparent.
        #[cfg(windows)]
        // SAFETY: called from main before any other thread exists.
        unsafe {
            std::env::set_var("GSK_RENDERER", "vulkan")
        };
        return;
    }
    // SAFETY: called from main before any other thread exists.
    unsafe { std::env::set_var("GSK_RENDERER", value) };
    tracing::info!(renderer = value, "applied GSK renderer preference");
}

/// Install the tracing subscriber.
/// config.json's debug_logs flag replaces the Python print() override.
/// RUST_LOG still overrides everything.
pub fn init_logging(paths: &Paths) {
    let filter = EnvFilter::try_from_default_env().unwrap_or_else(|_| EnvFilter::new(filter_for(debug_logs(paths))));
    let (filter, handle) = tracing_subscriber::reload::Layer::new(filter);
    let _ = FILTER.set(handle);
    tracing_subscriber::registry()
        .with(filter)
        .with(tracing_subscriber::fmt::layer().with_target(false).compact())
        .with(log_file_layer(paths))
        .init();
}

/// Windows release builds have no console, so the log also goes to
/// %LOCALAPPDATA%\muse\mixtapes.log. Appended to, not recreated: a second
/// launch runs this before handing over to the first, and truncating would
/// cut the running instance's log. It starts over past LOG_FILE_LIMIT.
#[cfg(windows)]
fn log_file_layer<S>(paths: &Paths) -> Option<impl tracing_subscriber::Layer<S>>
where
    S: tracing::Subscriber + for<'a> tracing_subscriber::registry::LookupSpan<'a>,
{
    const LOG_FILE_LIMIT: u64 = 5 * 1024 * 1024;
    // The data folder, not the cache: GLib puts that in INetCache on Windows.
    let path = paths.data_dir.join("mixtapes.log");
    if std::fs::metadata(&path).is_ok_and(|m| m.len() > LOG_FILE_LIMIT) {
        let _ = std::fs::remove_file(&path);
    }
    let file = std::fs::OpenOptions::new().create(true).append(true).open(&path).ok()?;
    Some(tracing_subscriber::fmt::layer().with_target(false).with_ansi(false).compact().with_writer(std::sync::Mutex::new(file)))
}

#[cfg(not(windows))]
fn log_file_layer(_paths: &Paths) -> Option<tracing_subscriber::layer::Identity> {
    None
}

type FilterHandle = tracing_subscriber::reload::Handle<EnvFilter, tracing_subscriber::Registry>;

/// Lets the settings switch change verbosity without a restart.
static FILTER: std::sync::OnceLock<FilterHandle> = std::sync::OnceLock::new();

fn filter_for(debug: bool) -> &'static str {
    if debug { "mixtapes=debug,info" } else { "mixtapes=info,warn" }
}

pub fn debug_logs(paths: &Paths) -> bool {
    paths.read_config().get("debug_logs").and_then(|v| v.as_bool()).unwrap_or(false)
}

/// Port of logger.set_debug_logs: save the flag and apply it now. RUST_LOG still wins.
pub fn set_debug_logs(paths: &Paths, enabled: bool) {
    let mut config = paths.read_config();
    config.insert("debug_logs".into(), enabled.into());
    let write = serde_json::to_vec(&serde_json::Value::Object(config)).map_err(std::io::Error::other).and_then(|bytes| std::fs::write(&paths.config_file, bytes));
    if let Err(err) = write {
        tracing::warn!(%err, "could not save config.json");
    }
    if std::env::var_os("RUST_LOG").is_some() {
        return;
    }
    if let Some(handle) = FILTER.get() {
        let _ = handle.reload(EnvFilter::new(filter_for(enabled)));
    }
}

//! Process-level setup that must run before GTK, GStreamer or any thread exists.
//! Mirrors the top of src/main.py.

use tracing_subscriber::EnvFilter;

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

/// Honor the user's GSK renderer choice from prefs.json before GTK loads.
/// An explicit GSK_RENDERER in the environment wins.
pub fn apply_gsk_renderer_pref(paths: &Paths) {
    if std::env::var_os("GSK_RENDERER").is_some() {
        return;
    }
    let prefs = paths.read_prefs();
    let Some(value) = prefs.get("gsk_renderer").and_then(|v| v.as_str()) else {
        return;
    };
    if value.is_empty() || value == "default" {
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
    let debug = paths
        .read_config()
        .get("debug_logs")
        .and_then(|v| v.as_bool())
        .unwrap_or(false);
    let default = if debug { "mixtapes=debug,info" } else { "mixtapes=info,warn" };
    let filter = EnvFilter::try_from_default_env().unwrap_or_else(|_| EnvFilter::new(default));
    tracing_subscriber::fmt()
        .with_env_filter(filter)
        .with_target(false)
        .compact()
        .init();
}

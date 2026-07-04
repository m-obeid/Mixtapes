import os
import threading
import weakref
import time
import urllib.request
import collections
import re
from gi.repository import Gtk, Gdk, GObject, GLib, GdkPixbuf


# is_online() is called from every bind path that greys out offline rows —
# i.e. hundreds of times during a normal playlist open and on every scroll.
# Previously the TCP probe ran synchronously on the UI thread (with up to a
# 1.5 s timeout!) every couple of seconds when the 2 s TTL expired, which is
# the single biggest source of bind-time stutter. Now the sync entry point
# *never* blocks: it returns the last known value from a background-refreshed
# state, optimistically True if we haven't probed yet.
import threading as _is_online_threading

_IS_ONLINE_LOCK = _is_online_threading.Lock()
_IS_ONLINE_STATE = {
    "value": True,            # optimistic default — callers degrade gracefully if wrong
    "last_probe": 0.0,        # monotonic timestamp of last probe completion
    "probe_in_flight": False,
}
_IS_ONLINE_PROBE_INTERVAL = 15.0   # how stale the cached value is allowed to get

_FORCE_OFFLINE_CACHE = {"value": False, "expires": 0.0}
_FORCE_OFFLINE_TTL = 10.0


def _check_force_offline():
    """Return the force_offline pref, cached for ``_FORCE_OFFLINE_TTL``
    seconds to avoid hammering the disk on every bind."""
    now = time.monotonic()
    if now < _FORCE_OFFLINE_CACHE["expires"]:
        return _FORCE_OFFLINE_CACHE["value"]
    import json
    prefs_path = os.path.join(GLib.get_user_data_dir(), "muse", "prefs.json")
    result = False
    try:
        if os.path.exists(prefs_path):
            with open(prefs_path) as f:
                result = bool(json.load(f).get("force_offline"))
    except Exception:
        pass
    _FORCE_OFFLINE_CACHE["value"] = result
    _FORCE_OFFLINE_CACHE["expires"] = now + _FORCE_OFFLINE_TTL
    return result


def _kick_online_probe(now):
    """Spawn a background probe if it's been a while since the last one
    and no probe is currently in flight."""
    with _IS_ONLINE_LOCK:
        if _IS_ONLINE_STATE["probe_in_flight"]:
            return
        if now - _IS_ONLINE_STATE["last_probe"] < _IS_ONLINE_PROBE_INTERVAL:
            return
        _IS_ONLINE_STATE["probe_in_flight"] = True

    def _probe():
        import socket
        try:
            with socket.create_connection(
                ("music.youtube.com", 443), timeout=2.0
            ):
                result = True
        except OSError:
            result = False
        with _IS_ONLINE_LOCK:
            _IS_ONLINE_STATE["value"] = result
            _IS_ONLINE_STATE["last_probe"] = time.monotonic()
            _IS_ONLINE_STATE["probe_in_flight"] = False

    _is_online_threading.Thread(target=_probe, daemon=True).start()


def is_online():
    """Return the most recently observed network state. **Never blocks
    the calling thread.**

    A short TCP connect to ``music.youtube.com:443`` runs on a background
    thread roughly every ``_IS_ONLINE_PROBE_INTERVAL`` seconds; this
    function returns whatever the last probe set (optimistically True
    until the first probe completes). Callers that need a real-time
    answer should rely on the *actual* network call failing — failing a
    request to YT directly is a better signal than pre-probing anyway.
    """
    if _check_force_offline():
        return False
    _kick_online_probe(time.monotonic())
    with _IS_ONLINE_LOCK:
        return _IS_ONLINE_STATE["value"]


def invalidate_is_online_cache():
    """Force the next ``is_online()`` to re-probe in the background.
    Call after toggling the force_offline pref or after a NetworkMonitor
    state change."""
    _FORCE_OFFLINE_CACHE["expires"] = 0.0
    with _IS_ONLINE_LOCK:
        _IS_ONLINE_STATE["last_probe"] = 0.0

# Bounded LRU Cache to prevent memory leaks. The cache is
# read/written by multiple worker threads and the main thread, so every
# mutation is serialized through IMG_CACHE_LOCK — concurrent check-then-modify
# sequences were corrupting LRU state and evicting pixbufs that other threads
# were still wiring up into textures.
IMG_CACHE = collections.OrderedDict()
# Each cached pixbuf is up to MAX_CACHED_DIM² × 4 bytes. A 64-entry cache at
# 1024px can pin ~256 MB by itself after playlist/queue browsing. Keep enough
# warm artwork for the current view and nearby tracks without letting decoded
# covers dominate the process footprint.
MAX_CACHE_SIZE = 24
MAX_CACHED_DIM = 768
IMG_CACHE_LOCK = threading.Lock()

# Bounded executor for image fetches. Each row's `load_url` used to spawn a
# fresh `threading.Thread`, which costs ~1-2ms apiece — when 25 rows bind at
# once on playlist open, that's a 30-50ms stall on the UI thread *just for
# thread creation*, before any I/O begins. The pool reuses workers and caps
# concurrency, which also stops the flood of simultaneous network/PixbufLoader
# work that was implicated in occasional segfaults.
_FETCH_EXECUTOR = None
_FETCH_EXECUTOR_LOCK = threading.Lock()

# In-flight URL dedup. When a playlist has many rows sharing artwork (album
# views, fallback chains, etc.) we used to fire N concurrent fetches for the
# same URL. Now the first arrival owns the fetch, and later arrivals attach
# their apply callbacks to be fired together when the pixbuf is ready.
_INFLIGHT_FETCHES = {}
_INFLIGHT_LOCK = threading.Lock()


def _get_fetch_executor():
    global _FETCH_EXECUTOR
    if _FETCH_EXECUTOR is not None:
        return _FETCH_EXECUTOR
    with _FETCH_EXECUTOR_LOCK:
        if _FETCH_EXECUTOR is None:
            from concurrent.futures import ThreadPoolExecutor
            # Keep decode concurrency low: GdkPixbuf/PIL bursts can otherwise
            # briefly allocate hundreds of MB while several covers decode and
            # downscale at the same time.
            _FETCH_EXECUTOR = ThreadPoolExecutor(
                max_workers=2, thread_name_prefix="muse-img"
            )
    return _FETCH_EXECUTOR


def submit_fetch(fn, *args, **kwargs):
    """Submit an image fetch onto the shared pool. Returns a Future.
    Falls back to a raw thread if the executor can't be created (shouldn't
    happen in practice, but keeps the UI loading even in weird envs)."""
    try:
        return _get_fetch_executor().submit(fn, *args, **kwargs)
    except Exception as e:
        print(f"[IMG] executor unavailable, falling back to thread: {e}")
        t = threading.Thread(target=fn, args=args, kwargs=kwargs, daemon=True)
        t.start()
        return None

# Resolved local-cover path per video_id ("file://..." or None). _get_local_cover
# used to run mutagen + a write() on every row bind, which froze the UI when
# opening playlists. After the first resolution we keep the answer in memory
# so the bind path is a single dict lookup. Invalidated by the cover-extraction
# code path itself (rare) or by app restart.
_LOCAL_COVER_CACHE = {}
_LOCAL_COVER_CACHE_LOCK = threading.Lock()


def resolve_local_cover(video_id):
    """Return a 'file://' URL for the embedded cover of a downloaded track, or
    None if the track isn't downloaded / has no embedded art.

    Fast path (the common case): O(1) dict hit, or a single stat() if the
    extracted JPEG already exists in the cache dir. Falls back to mutagen
    only on the very first lookup per (track, install) — and only if the
    track is actually downloaded.
    """
    if not video_id:
        return None
    with _LOCAL_COVER_CACHE_LOCK:
        if video_id in _LOCAL_COVER_CACHE:
            return _LOCAL_COVER_CACHE[video_id]

    # If the track isn't downloaded there's no embedded cover to extract.
    # is_downloaded is an in-memory set lookup, so this short-circuits the
    # mass-bind freeze for non-downloaded playlists.
    try:
        from player.downloads import get_download_db
        db = get_download_db()
    except Exception:
        return None
    if not db.is_downloaded(video_id):
        with _LOCAL_COVER_CACHE_LOCK:
            _LOCAL_COVER_CACHE[video_id] = None
        return None

    cache_dir = os.path.join(GLib.get_user_cache_dir(), "muse", "covers")
    cover_path = os.path.join(cache_dir, f"{video_id}.jpg")
    if os.path.exists(cover_path):
        url = f"file://{cover_path}"
        with _LOCAL_COVER_CACHE_LOCK:
            _LOCAL_COVER_CACHE[video_id] = url
        return url

    # Cold path: read the audio file's tags and extract the embedded image.
    try:
        from player.downloads import DownloadManager
        audio_path = db.get_local_path(video_id)
        if audio_path:
            cover_data = DownloadManager.extract_cover_from_file(audio_path)
            if cover_data:
                os.makedirs(cache_dir, exist_ok=True)
                with open(cover_path, "wb") as f:
                    f.write(cover_data)
                url = f"file://{cover_path}"
                with _LOCAL_COVER_CACHE_LOCK:
                    _LOCAL_COVER_CACHE[video_id] = url
                return url
    except Exception:
        pass

    with _LOCAL_COVER_CACHE_LOCK:
        _LOCAL_COVER_CACHE[video_id] = None
    return None


def invalidate_local_cover(video_id):
    """Drop the resolved-cover cache entry for a track (e.g. after a re-download
    or when the user removes the download)."""
    if not video_id:
        return
    with _LOCAL_COVER_CACHE_LOCK:
        _LOCAL_COVER_CACHE.pop(video_id, None)


# ── Persistent thumbnail cache on disk ─────────────────────────────────────
# Avoids the placeholder-icon flash when returning to the library, and makes
# subsequent launches render covers instantly. Files are raw image bytes
# under XDG_CACHE/muse/thumbs/<sha1-of-url>.
def _thumb_cache_dir():
    path = os.path.join(GLib.get_user_cache_dir(), "muse", "thumbs")
    try:
        os.makedirs(path, exist_ok=True)
    except OSError:
        pass
    return path


def _thumb_cache_key(url):
    import hashlib
    return hashlib.sha1(url.encode("utf-8", errors="replace")).hexdigest()


def _thumb_cache_path(url):
    if not url or url.startswith("file://"):
        return None
    return os.path.join(_thumb_cache_dir(), _thumb_cache_key(url))


def read_thumb_cache(url):
    path = _thumb_cache_path(url)
    if not path or not os.path.exists(path):
        return None
    try:
        with open(path, "rb") as f:
            return f.read()
    except OSError:
        return None


def write_thumb_cache(url, data):
    if not url or not data:
        return
    path = _thumb_cache_path(url)
    if not path:
        return
    try:
        # Write to a sibling tmp file then rename so partial writes can never
        # be read as valid cached bytes.
        tmp = path + ".tmp"
        with open(tmp, "wb") as f:
            f.write(data)
        os.replace(tmp, path)
    except OSError:
        pass


def cache_pixbuf(url, pixbuf):
    if not url or not pixbuf:
        return
    # Scale down very large images before caching so the cache can't pin
    # hundreds of MB of pixbufs at full resolution.
    w = pixbuf.get_width()
    h = pixbuf.get_height()
    if w > MAX_CACHED_DIM or h > MAX_CACHED_DIM:
        scale = MAX_CACHED_DIM / max(w, h)
        pixbuf = pixbuf.scale_simple(
            int(w * scale), int(h * scale), GdkPixbuf.InterpType.BILINEAR
        )

    with IMG_CACHE_LOCK:
        if url in IMG_CACHE:
            IMG_CACHE.move_to_end(url)
            return
        IMG_CACHE[url] = pixbuf
        if len(IMG_CACHE) > MAX_CACHE_SIZE:
            IMG_CACHE.popitem(last=False)


def decode_pixbuf_bounded(data, max_dim=MAX_CACHED_DIM):
    """Decode image bytes with a hard dimension cap applied during load."""
    loader = GdkPixbuf.PixbufLoader()

    def _on_size_prepared(loader, width, height):
        if width <= 0 or height <= 0:
            return
        if width <= max_dim and height <= max_dim:
            return
        scale = max_dim / max(width, height)
        loader.set_size(max(1, int(width * scale)), max(1, int(height * scale)))

    loader.connect("size-prepared", _on_size_prepared)
    loader.write(data)
    loader.close()
    return loader.get_pixbuf()


def get_high_res_url(url, target_size=None):
    """Rewrites Google Image URLs to request a high resolution (800x800).
    Also strips sqp and rs parameters which constrain resolution, UNLESS it's a locker track.
    """
    if not url:
        return url

    # 1. Clean up parameters that constrain resolution
    # Strip sqp and rs which are often used to force small/safe thumbnails
    # CRITICAL: Some URL families REQUIRE these params and 404 without them:
    #   - vi_locker (locker tracks)
    #   - pl_c (custom playlist covers — sqp/rs are signed access tokens)
    # Stripping them on pl_c made every library tile fail its primary fetch
    # and fall back to the original URL, doubling the network load when
    # opening anything while the library grid is still hydrating.
    if "vi_locker" not in url and "/pl_c/" not in url:
        clean_url = re.sub(r"([?&])(sqp|rs)=[^&]*&?", r"\1", url)
        clean_url = clean_url.replace("?&", "?").rstrip("?&")
    else:
        clean_url = url

    # 2. Upgrade resolution/quality based on domain
    if "i.ytimg.com" in clean_url:
        for q in _YTIMG_QUALITIES:
            if q in clean_url:
                return clean_url.replace(q, "maxresdefault")
        return clean_url

    if "googleusercontent.com" in clean_url or "ggpht.com" in clean_url:
        # If it has w/h, only update those and ignore s
        if re.search(r"([=-])w\d+-h\d+", clean_url):
            return re.sub(r"([=-])w\d+-h\d+", r"\1w800-h800", clean_url)
        # Otherwise update s
        return re.sub(r"([=-])s\d+(?=-|$)", r"\1s800", clean_url)

    return clean_url


_YTIMG_QUALITIES = ["maxresdefault", "sddefault", "hqdefault", "mqdefault", "default"]


def get_ytimg_fallbacks(url):
    """For YouTube video thumbnail URLs (i.ytimg.com/vi/...), generate
    a fallback chain from the current quality downward.
    Returns a list of fallback URLs (excluding the primary URL).
    """
    if not url or "i.ytimg.com/vi/" not in url:
        return []

    # Find which quality is currently in the URL
    current_idx = -1
    for i, q in enumerate(_YTIMG_QUALITIES):
        if q in url:
            current_idx = i
            break

    if current_idx < 0:
        # If no known quality is in the URL, provide the full chain
        # try to guess where in the path the quality name would be
        # (usually after /vi/VIDEO_ID/)
        match = re.search(r"/vi/[^/]+/", url)
        if match:
            base = url[: match.end()]
            return [f"{base}{q}.jpg" for q in _YTIMG_QUALITIES]
        return []

    # Generate fallbacks from the next quality downward
    fallbacks = []
    current_q = _YTIMG_QUALITIES[current_idx]
    for q in _YTIMG_QUALITIES[current_idx + 1 :]:
        fallbacks.append(url.replace(current_q, q))
    return fallbacks


# In-flight de-duplication so the library grid doesn't fire N parallel
# downloads for the same cover on every refresh.
_COVER_DL_INFLIGHT = set()
_COVER_DL_LOCK = threading.Lock()


_COVER_FRESHNESS_SECONDS = 24 * 60 * 60  # one day


def playlist_cover_path(title):
    """Absolute path of a playlist's locally-cached cover, or None if the
    title is empty / the music dir can't be resolved. Does not touch disk."""
    if not title:
        return None
    try:
        from player.downloads import get_music_dir, _sanitize_filename

        return os.path.join(
            get_music_dir(), "Playlists", f"{_sanitize_filename(title)}.jpg"
        )
    except Exception:
        return None


def local_playlist_cover_url(title):
    """A `file://` URL for a playlist's cached cover, stamped with the file's
    mtime (`?m=<epoch>`), or None when no local copy exists.

    The mtime stamp is load-bearing: the in-memory pixbuf cache and the grid's
    "reload only if the URL changed" check are both keyed on the URL string.
    A bare path stays identical when the bytes are replaced (remote/local
    edit), so the stale art would survive; folding mtime into the URL makes a
    content change look like a new URL — busting the cache and repainting the
    tile. The single os.stat also replaces the os.path.exists check, so this
    adds no extra syscall. `_fetch_image` strips the query before opening."""
    cover_path = playlist_cover_path(title)
    if not cover_path:
        return None
    try:
        st = os.stat(cover_path)
    except OSError:
        return None
    # Nanosecond mtime so two saves in the same wall-clock second still produce
    # distinct URLs (a second-resolution stamp could alias a fresh cover onto
    # the stale pixbuf).
    return f"file://{cover_path}?m={st.st_mtime_ns}"


def _cover_identity(url):
    """The stable part of a cover URL — everything before the query string.

    YT custom-cover URLs (pl_c/...) carry signed `sqp`/`rs` params that rotate
    on every fetch even when the image is unchanged, so the full URL is useless
    as a "did the art change?" signal. The path identifies the image; the query
    only signs access to it."""
    return (url or "").split("?", 1)[0]


def save_playlist_cover_async(player, title, url):
    """Download a playlist's cover to <music_dir>/playlists/<title>.jpg so
    future opens (from anywhere — library grid, playlist page) can render
    it instantly and offline. Silently no-ops on failure.

    Custom-playlist covers (i.ytimg.com/pl_c/...) require YT auth cookies,
    which a naked `requests.get` doesn't carry. We pass the signed-in
    client's Cookie header so those URLs resolve.

    Re-fetches when either (a) the local copy is older than a day, or (b) the
    cover's identity (the URL minus its signing query) differs from what we
    last saved — so an edit on YT, or from Mixtapes, propagates on the next
    library load instead of waiting out the freshness window. A `.url` sidecar
    records the identity of the bytes on disk.

    All disk I/O happens on the worker thread: library rebuilds call this once
    per tile during grid build, and doing the stat/makedirs inline stalled the
    main thread on dozens of syscalls before any tile painted.
    """
    if not title or not url:
        return

    key = (title, url)
    with _COVER_DL_LOCK:
        if key in _COVER_DL_INFLIGHT:
            return
        _COVER_DL_INFLIGHT.add(key)

    def _dl():
        try:
            cover_path = playlist_cover_path(title)
            if not cover_path:
                return
            try:
                os.makedirs(os.path.dirname(cover_path), exist_ok=True)
            except OSError:
                return

            # Freshness gate: skip the re-fetch only when we have a recent copy
            # *and* it was saved from the same cover identity. A changed
            # identity (remote/local edit) always re-downloads; the playlist
            # page's refresh button remains an unconditional escape hatch.
            sidecar = cover_path + ".url"
            try:
                st = os.stat(cover_path)
                fresh = (time.time() - st.st_mtime) < _COVER_FRESHNESS_SECONDS
                if fresh:
                    try:
                        with open(sidecar, "r", encoding="utf-8") as f:
                            saved_identity = f.read().strip()
                    except OSError:
                        saved_identity = None
                    if saved_identity == _cover_identity(url):
                        return
            except OSError:
                pass  # missing or unreadable — proceed to download

            import requests

            headers = {"User-Agent": "Mozilla/5.0"}
            try:
                if player and hasattr(player, "client"):
                    client = player.client
                    if (
                        client
                        and client.is_authenticated()
                        and any(
                            d in url
                            for d in (
                                "ytimg.com",
                                "googleusercontent.com",
                                "ggpht.com",
                            )
                        )
                    ):
                        cookie = client.api.headers.get("Cookie")
                        if cookie:
                            headers["Cookie"] = cookie
            except Exception:
                pass
            # Try the high-res upgrade first, then walk the ytimg quality
            # chain down, and finally the original URL — not every video
            # has a maxresdefault.jpg generated.
            candidates = []
            hi = get_high_res_url(url)
            if hi:
                candidates.append(hi)
            candidates.extend(get_ytimg_fallbacks(hi or url))
            if url not in candidates:
                candidates.append(url)

            saved = False
            last_status = None
            for candidate in candidates:
                try:
                    resp = requests.get(candidate, headers=headers, timeout=15)
                except Exception as e:
                    print(f"[COVER] {candidate} errored: {e}")
                    continue
                last_status = resp.status_code
                if resp.status_code == 200 and len(resp.content) > 1000:
                    with open(cover_path, "wb") as f:
                        f.write(resp.content)
                    # Record the identity of the bytes now on disk so the next
                    # freshness check can tell an unchanged cover from an edit.
                    try:
                        with open(sidecar, "w", encoding="utf-8") as f:
                            f.write(_cover_identity(url))
                    except OSError:
                        pass
                    # The on-disk bytes changed; readers key the in-memory
                    # pixbuf cache on the file's mtime (local_playlist_cover_url),
                    # so the new mtime busts the stale entry automatically.
                    print(
                        f"[COVER] saved {cover_path} from {candidate} "
                        f"({len(resp.content)} bytes)"
                    )
                    saved = True
                    break
            if not saved:
                print(
                    f"[COVER] all candidates failed for {title} "
                    f"(last HTTP {last_status})"
                )
        except Exception as e:
            print(f"[COVER] exception for {title}: {e}")
        finally:
            with _COVER_DL_LOCK:
                _COVER_DL_INFLIGHT.discard(key)

    # Library rebuilds invoke this once per playlist tile — 50 playlists used
    # to mean 50 concurrent threads doing TLS handshakes, which showed up as
    # the dominant load in py-spy and starved the main thread of the GIL.
    # Routing through the shared pool caps concurrency at max_workers.
    submit_fetch(_dl)


def suppress_hover_while_scrolling(scrolled, settle_ms=110):
    """Disable pointer hit-testing on `scrolled`'s content while it is actively
    scrolling, restoring it shortly after motion settles.

    The stutter felt only when the pointer sits over content (empty space is
    smooth) is GTK re-picking the widget under the *stationary* pointer every
    frame as rows slide past, then re-resolving that row's `:hover` style
    against the whole stylesheet. Freezing the CSS fade doesn't help — the cost
    is the per-frame hover hit-test + restyle itself, which CSS can't suppress.

    So we stop the picking outright: set `can-target = False` on the scrolled
    content during the scroll, so no row is ever hovered and GTK does zero
    per-frame hover work; restore it once motion stops. The `.is-scrolling`
    class is still toggled (kept as a CSS-level fallback). The ScrolledWindow
    itself stays targetable, so its own scroll/kinetic gestures and the
    scrollbar are unaffected — only row-level hit-testing pauses. Clicks on
    rows are suppressed for the brief settle after scrolling (and during an
    active fling, where a press conventionally just stops the fling anyway).
    """
    vadjust = scrolled.get_vadjustment()
    if vadjust is None:
        return
    state = {"timeout_id": 0}

    def _set_content_targetable(targetable):
        # Resolved lazily: the content child is often set on the ScrolledWindow
        # after this helper runs, and could be swapped later.
        child = scrolled.get_child()
        if child is not None:
            child.set_can_target(targetable)

    def _settle():
        state["timeout_id"] = 0
        scrolled.remove_css_class("is-scrolling")
        _set_content_targetable(True)
        return GLib.SOURCE_REMOVE

    def _on_value_changed(_adj):
        if state["timeout_id"]:
            GLib.source_remove(state["timeout_id"])
        else:
            scrolled.add_css_class("is-scrolling")
            _set_content_targetable(False)
        state["timeout_id"] = GLib.timeout_add(settle_ms, _settle)

    vadjust.connect("value-changed", _on_value_changed)


def attach_playing_highlight(row_widget, player, video_id):
    """Toggle a `playing` CSS class while `player`'s currently-playing
    track matches `video_id`. Auto-disconnects on widget destroy.

    Targets the enclosing Gtk.ListBoxRow when one exists (so the
    full outer row lights up, not just the inner box — and the inner
    `box.song-row.playing` rule doesn't fight us by double-tinting).
    Falls back to `row_widget` itself if no ListBoxRow ancestor is
    found.

    Use for ad-hoc song rows (Home, Explore, Artist Top Songs, etc.)
    that don't go through SongRowWidget — which already has its own
    highlight machinery. Lightweight: one signal connection per row.
    """
    if not row_widget or not player or not video_id:
        return

    list_row = row_widget.get_parent()
    while list_row is not None and not isinstance(list_row, Gtk.ListBoxRow):
        list_row = list_row.get_parent()
    target = list_row or row_widget

    def _refresh(*_):
        # Match either the player's current id OR the pre-swap source
        # id — when the player auto-swaps an OMV/UGC track to its ATV
        # counterpart, the row's stored videoId would otherwise no
        # longer match.
        source = getattr(player, "_current_source_video_id", None)
        is_playing = video_id in (player.current_video_id, source)
        if is_playing:
            target.add_css_class("playing")
        else:
            target.remove_css_class("playing")

    try:
        handler = player.connect("metadata-changed", _refresh)
    except Exception:
        return

    def _cleanup(*_):
        try:
            player.disconnect(handler)
        except Exception:
            pass

    row_widget.connect("destroy", _cleanup)
    _refresh()


def show_toast(widget, message):
    """Show a toast on the nearest ancestor window that exposes
    `add_toast` (Adw.ApplicationWindow + Adw.ToastOverlay setup).
    Silent no-op if `widget` isn't currently parented to such a window
    yet — happens when a deferred result comes back after the user has
    already navigated away."""
    root = widget.get_root() if widget else None
    if root and hasattr(root, "add_toast"):
        root.add_toast(message)


def copy_to_clipboard(text):
    """Copies the given text to the default system clipboard."""
    if not text:
        return
    display = Gdk.Display.get_default()
    if display:
        clipboard = display.get_clipboard()
        clipboard.set(text)


def get_yt_music_link(item_id, is_album=False, audio_playlist_id=None):
    """
    Constructs a YouTube Music link for a playlist or album.
    Albums use /playlist?list=OLAK... (the audio playlist ID).
    MPRE browse IDs are internal and not shareable.
    """
    if not item_id:
        return ""
    if item_id.startswith("OLAK"):
        return f"https://music.youtube.com/playlist?list={item_id}"
    if is_album or item_id.startswith("MPRE"):
        # MPRE is a browse ID, not a shareable URL.
        # Use the audio_playlist_id if available, otherwise fall back to browse URL.
        if audio_playlist_id:
            return f"https://music.youtube.com/playlist?list={audio_playlist_id}"
        return f"https://music.youtube.com/browse/{item_id}"
    return f"https://music.youtube.com/playlist?list={item_id}"


def parse_item_metadata(item):
    """
    Robustly extracts metadata (year, type, is_explicit) from ytmusicapi item formats.
    Handles standard keys and fallbacks to subtitle runs/badges.
    """
    metadata = {
        "year": str(item.get("year", "")),
        "type": str(item.get("type", "")),
        "is_explicit": bool(item.get("isExplicit") or item.get("explicit")),
    }

    # Fallback for explicit (badges)
    if not metadata["is_explicit"]:
        badges = item.get("badges", [])
        for badge in badges:
            # Check for label in the badge itself or inside a music_inline_badge_renderer
            label = ""
            if isinstance(badge, dict):
                label = badge.get("label", "") or badge.get(
                    "musicInlineBadgeRenderer", {}
                ).get("accessibilityData", {}).get("accessibilityData", {}).get(
                    "label", ""
                )
            if not label and isinstance(badge, str):
                label = badge

            label = str(label).lower()
            if "explicit" in label or label == "e":
                metadata["is_explicit"] = True
                break

    # Fallback for year/type (subtitle runs)
    subtitle = item.get("subtitle", "")
    runs = []
    if isinstance(subtitle, list):
        runs = subtitle
    elif isinstance(item.get("subtitles"), list):
        runs = item.get("subtitles")
    elif isinstance(subtitle, dict) and "runs" in subtitle:
        runs = subtitle["runs"]

    if runs:
        for run in runs:
            if not isinstance(run, dict):
                continue
            text = run.get("text", "")
            if not text:
                continue

            # Look for 4-digit years
            year_match = re.search(r"\d{4}", text)
            if year_match and not metadata["year"]:
                metadata["year"] = year_match.group(0)

            # Common types
            type_lower = text.lower()
            if (
                "single" in type_lower
                or "ep" in type_lower
                or "album" in type_lower
                or "video" in type_lower
            ):
                if not metadata["type"]:
                    metadata["type"] = text

    # Final cleanup: if year is not numeric, it's likely a type
    year_val = metadata["year"]
    is_numeric_year = bool(re.search(r"\d{4}", year_val))
    if year_val and not is_numeric_year:
        if not metadata["type"]:
            metadata["type"] = year_val
        metadata["year"] = ""

    return metadata


class AsyncImage(Gtk.Image):
    def __init__(
        self,
        url=None,
        size=None,
        width=None,
        height=None,
        circular=False,
        player=None,
        **kwargs,
    ):
        super().__init__(**kwargs)
        self.player = player

        # Determine target dimensions
        self.target_w = width if width else size
        self.target_h = height if height else size
        self._is_placeholder = True

        if not self.target_w:
            self.target_w = 48
        if not self.target_h:
            self.target_h = 48

        # Set pixel size if provided (limits size for icons).
        if size:
            self.set_pixel_size(size)
        else:
            # Rely on pixbuf scaling for explicit width/height.
            pass

        # Skip the placeholder icon-name lookup at init time — it's a
        # GtkIconTheme.lookup_icon roundtrip per widget and it adds up when
        # PlaylistPage spins up ~25 row pictures + a header cover all at
        # once. load_url() sets the placeholder itself when it can't show
        # a cached pixbuf, and constructions with a URL go straight to
        # load_url anyway, never seeing the placeholder.
        self._is_placeholder = True
        self.url = url
        self.circular = circular
        self.video_id = None
        self._pending_fetch = None
        self._map_handler_id = None
        self.connect("destroy", self._on_destroy)
        # Remember the desktop size so set_compact() can restore it
        # when compact mode toggles off. Mirrors AsyncPicture.target_size.
        self._base_size = self.target_w

        if url:
            self.load_url(url)

    def _on_destroy(self, *_):
        self._pending_fetch = None
        self.url = None
        self.video_id = None
        handler_id = getattr(self, "_map_handler_id", None)
        if handler_id:
            try:
                self.disconnect(handler_id)
            except Exception:
                pass
            self._map_handler_id = None
        try:
            self.clear()
        except Exception:
            pass

    def set_compact(self, compact):
        """Switch between desktop and mobile sizing. Only applies to
        small thumbnail-sized images (≤80 px base) — larger AsyncImages
        like section cards (160 px) or artist banner thumbs (140 px)
        are full-tile artwork and shouldn't shrink to 44, that would
        ruin the visual hierarchy of the page."""
        if self._base_size is None or self._base_size > 80:
            return
        new = 44 if compact else self._base_size
        if new == self.target_w:
            return
        self.target_w = new
        self.target_h = new
        self.set_pixel_size(new)
        self.queue_resize()

    @staticmethod
    def _get_local_cover(video_id):
        return resolve_local_cover(video_id)

    # Viewport-aware fetch deferral. When a ListView row is bound for a
    # track that isn't currently on-screen (initial layout, prefetch buffer,
    # fast scroll-through), we don't actually want to spend executor time
    # fetching its cover — the row may scroll out before the user ever
    # sees it. Defer the submit_fetch until the widget is mapped; cache
    # hits still paint synchronously so already-loaded rows are instant.
    def _queue_fetch(self, fn, *args):
        if self.get_mapped():
            submit_fetch(fn, *args)
            return
        self._pending_fetch = (fn, args)
        if not getattr(self, "_map_handler_id", None):
            self._map_handler_id = self.connect("map", self._on_mapped_fetch)

    def _on_mapped_fetch(self, _widget):
        pending = getattr(self, "_pending_fetch", None)
        if not pending:
            return
        fn, args = pending
        self._pending_fetch = None
        # The first positional arg to _fetch_image is the URL it was queued
        # for. If load_url has since pointed this widget at a different URL
        # (recycled to a new track), drop the stale fetch.
        if args and args[0] != self.url:
            return
        submit_fetch(fn, *args)



    def load_url(self, url, **kwargs):
        orig_url = url
        url = get_high_res_url(url, self.target_w)
        self.url = url
        # A new URL invalidates any previously deferred fetch.
        self._pending_fetch = None

        vid = getattr(self, 'video_id', None)

        # Fast path: web URL already cached. Paint synchronously and skip the
        # local-cover lookup entirely — keeps the bind path I/O-free.
        cached_pixbuf = IMG_CACHE.get(url) if url else None
        if cached_pixbuf:
            with IMG_CACHE_LOCK:
                if url in IMG_CACHE:
                    IMG_CACHE.move_to_end(url)
            self._apply_pixbuf(cached_pixbuf, url)
            return

        # No cached pixbuf — check for a downloaded copy. resolve_local_cover
        # is O(1) after first resolution and skips entirely for non-downloaded
        # tracks, so this no longer blocks the UI on playlist open.
        #
        # Local covers are authoritative for downloaded tracks: skip the web
        # fetch entirely (online and offline alike). Previously online mode
        # would apply the local cover AND still queue a web fetch per row,
        # which is what made opening a big mostly-downloaded playlist drag.
        local = resolve_local_cover(vid) if vid else None
        if local:
            self.url = local
            if local in IMG_CACHE:
                self._apply_pixbuf(IMG_CACHE[local], local)
                IMG_CACHE.move_to_end(local)
            else:
                self._queue_fetch(self._fetch_image, local, [], None)
            return

        if not url:
            self.set_from_icon_name("image-missing-symbolic")
            return

        if not self.get_paintable() or self._is_placeholder:
            self.set_from_icon_name("image-missing-symbolic")
            self._is_placeholder = True

        fallbacks = kwargs.get("fallbacks") or get_ytimg_fallbacks(url)
        if url != orig_url and orig_url not in fallbacks:
            fallbacks.append(orig_url)

        self.url = url  # Update so _apply_pixbuf accepts the web result
        self._queue_fetch(self._fetch_image, url, fallbacks, None)

    def _fetch_image(self, url, fallbacks=None, cached_pixbuf=None):
        # Skip stale work: if the widget has moved on (fast scroll, re-bind to
        # a different track), don't spend cycles fetching/decoding for it.
        if self.url != url:
            return
        # Another submission for the same URL may have already populated the
        # cache by the time this task is picked up — short-circuit to apply.
        if not cached_pixbuf:
            cached_pixbuf = IMG_CACHE.get(url)
            if cached_pixbuf:
                with IMG_CACHE_LOCK:
                    if url in IMG_CACHE:
                        IMG_CACHE.move_to_end(url)
        try:
            pixbuf = cached_pixbuf
            if not pixbuf:
                if url.startswith("file://"):
                    import os
                    path = url[7:]
                    # Local cover URLs may carry an `?m=<mtime>` cache-buster
                    # (see local_playlist_cover_url) — strip it to get the path.
                    q = path.rfind("?")
                    if q != -1:
                        path = path[:q]
                    if os.path.exists(path):
                        with open(path, "rb") as f:
                            data = f.read()
                    else:
                        return
                else:
                    # Persistent disk cache first — skips the network hop
                    # entirely for covers we've already fetched, which makes
                    # library re-entry flash-free.
                    data = read_thumb_cache(url)
                    if not data:
                        headers = {"User-Agent": "Mozilla/5.0"}
                        if self.player and hasattr(self.player, "client"):
                            client = self.player.client
                            if client and client.is_authenticated():
                                if any(d in url for d in ["youtube.com", "ytimg.com", "googleusercontent.com", "ggpht.com"]):
                                    cookie = client.api.headers.get("Cookie")
                                    if cookie:
                                        headers["Cookie"] = cookie

                        import requests
                        resp = requests.get(url, headers=headers, timeout=10)
                        resp.raise_for_status()
                        data = resp.content
                        write_thumb_cache(url, data)

                pixbuf = decode_pixbuf_bounded(data)

                if pixbuf:
                    cache_pixbuf(url, pixbuf)

            if pixbuf:
                # Now perform the widget-specific scaling and cropping in the background thread
                # To support HiDPI (e.g. 200% scale), we double the target pixel density
                # GTK will scale the texture back down smoothly, keeping it crisp.
                tw = self.target_w * 2
                th = self.target_h * 2

                w = pixbuf.get_width()
                h = pixbuf.get_height()

                # Calculate scale to fill the target size (cover)
                scale = max(tw / w, th / h)
                new_w = int(w * scale)
                new_h = int(h * scale)

                # Scale properly
                scaled = pixbuf.scale_simple(
                    new_w, new_h, GdkPixbuf.InterpType.BILINEAR
                )

                # Center crop to target dimensions
                final_pixbuf = scaled
                if new_w > tw or new_h > th:
                    offset_x = max(0, (new_w - tw) // 2)
                    offset_y = max(0, (new_h - th) // 2)
                    cw = min(tw, new_w - offset_x)
                    ch = min(th, new_h - offset_y)
                    if cw > 0 and ch > 0:
                        try:
                            final_pixbuf = scaled.new_subpixbuf(
                                offset_x, offset_y, cw, ch
                            )
                        except Exception as e:
                            print(f"Pixbuf crop error: {e}")

                # Apply on main thread
                GLib.idle_add(self._apply_pixbuf, final_pixbuf, url)

        except Exception:
            if fallbacks and self.url == url:
                next_url = fallbacks.pop(0)
                self.url = next_url  # Update current URL to match the fallback

                # If we have a player, notify it about the working fallback URL
                # when it finally succeeds. This is handled in _apply_pixbuf.

                print(f"Trying fallback: {next_url}")
                self._fetch_image(next_url, fallbacks)

    def _apply_pixbuf(self, pixbuf, url=None):
        # Race condition check: only apply if the URL hasn't changed since request
        if url and self.url != url:
            return

        # Notify player of working URL if it's different from what we started with
        if self.player and url and "ytimg.com" in url:
            # We only want to notify if this is a fallback that worked
            # or if the URL was resolved from a 404.
            # We'll rely on the player to handle the update logic.
            GLib.idle_add(self._sync_player_url, url)

        # Center-crop to a square aspect ratio when the source is wider than
        # tall (or taller than wide). Required because IMG_CACHE stores the
        # full-aspect pixbuf — without this, cache-hit re-displays of
        # rectangular covers (some YT thumbnails) show up letterboxed in
        # cover slots (player bar, library tiles) that expect a square.
        if pixbuf:
            w = pixbuf.get_width()
            h = pixbuf.get_height()
            if w != h:
                size = min(w, h)
                x_off = (w - size) // 2
                y_off = (h - size) // 2
                try:
                    pixbuf = pixbuf.new_subpixbuf(x_off, y_off, size, size)
                except Exception:
                    pass

        texture = Gdk.Texture.new_for_pixbuf(pixbuf)
        self.set_from_paintable(texture)
        self._is_placeholder = False

    def _sync_player_url(self, url):
        if not self.player or not url:
            return
        # Find current track and update its thumb if it matches
        if hasattr(self.player, "update_track_thumbnail"):
            # We don't know the video_id here easily without storing it,
            # but usually the image loading is for the 'currently playing' or 'item in list'.
            # To be safe, we'll only sync if this widget was explicitly given a video_id.
            video_id = getattr(self, "video_id", None)
            if video_id:
                self.player.update_track_thumbnail(video_id, url)

    def set_from_file(self, file):
        """Optimistically set image from a local file object (GFile)"""
        try:
            # We must load into a pixbuf first to handle scaling correctly
            path = file.get_path()
            # Multiplying by 2 to support HiDPI displays
            pixbuf = GdkPixbuf.Pixbuf.new_from_file_at_scale(
                path, self.target_w * 2, self.target_h * 2, True
            )
            print(f"[IMAGE-LOAD] AsyncImage path={path}")
            self.set_from_pixbuf(pixbuf)
            # Nullify URL so subsequent async loads don't overwrite this immediately
            self.url = f"file://{path}"
        except Exception as e:
            print(f"Error setting from file: {e}")


def subprocess_pixbuf(pixbuf, x, y, w, h):
    # bindings helper
    return pixbuf.new_subpixbuf(x, y, w, h)


class AsyncPicture(Gtk.Picture):
    # Added crop_to_square parameter
    def __init__(
        self,
        url=None,
        crop_to_square=False,
        icon_name=None,
        target_size=None,
        player=None,
        **kwargs,
    ):
        super().__init__(**kwargs)
        self.player = player
        self.set_content_fit(Gtk.ContentFit.COVER)
        self.crop_to_square = crop_to_square
        self.target_size = target_size
        self.url = url
        self.video_id = None
        self._is_placeholder = True
        self._pending_fetch = None
        self._map_handler_id = None
        self.connect("destroy", self._on_destroy)

        # Constrain the picture widget to target_size so it doesn't
        # request more space when a non-square texture is loaded
        if target_size:
            self.set_size_request(target_size, target_size)
            self.set_hexpand(False)
            self.set_vexpand(False)

        if icon_name:
            self.set_from_icon_name(icon_name)
        else:
            # Skip the eager placeholder icon-name lookup; the bind path
            # calls load_url which sets it on miss. ~25 row Pictures × 1
            # icon-theme roundtrip apiece used to fire on every cold
            # playlist render.
            self._is_placeholder = True
            if url:
                self.load_url(url)

    def _on_destroy(self, *_):
        self._pending_fetch = None
        self.url = None
        self.video_id = None
        handler_id = getattr(self, "_map_handler_id", None)
        if handler_id:
            try:
                self.disconnect(handler_id)
            except Exception:
                pass
            self._map_handler_id = None
        try:
            self.set_paintable(None)
        except Exception:
            pass

    def do_measure(self, orientation, for_size):
        """Clamp natural size so the texture doesn't inflate the parent.
        Uses _current_size which is updated by set_compact()."""
        minimum, natural, min_baseline, nat_baseline = Gtk.Picture.do_measure(
            self, orientation, for_size
        )
        size = getattr(self, '_current_size', self.target_size)
        if size and natural > size:
            natural = size
            minimum = min(minimum, size)
        return minimum, natural, -1, -1

    def _get_local_cover(self):
        return resolve_local_cover(self.video_id)

    def set_compact(self, compact):
        """Switch between desktop and mobile sizing."""
        if self.target_size:
            self._current_size = 44 if compact else self.target_size
            self.set_size_request(self._current_size, self._current_size)
            self.queue_resize()

    def set_from_icon_name(self, icon_name):
        if not icon_name:
            self.set_paintable(None)
            return

        display = Gdk.Display.get_default()
        theme = Gtk.IconTheme.get_for_display(display)

        # 256 is a good high-res baseline for icons to be scaled by GTK
        icon_paintable = theme.lookup_icon(
            icon_name, None, 256, 1, Gtk.TextDirection.NONE, Gtk.IconLookupFlags.PRELOAD
        )
        if icon_paintable:
            self.set_paintable(icon_paintable)
            self._is_placeholder = ("image-missing" in icon_name)
        else:
            self.set_paintable(None)
            self._is_placeholder = True

    # Viewport-aware fetch deferral — see AsyncImage._queue_fetch above
    # for rationale. Identical mechanism, separate class because Gtk.Picture
    # and Gtk.Image don't share a base.
    def _queue_fetch(self, fn, *args):
        if self.get_mapped():
            submit_fetch(fn, *args)
            return
        self._pending_fetch = (fn, args)
        if not getattr(self, "_map_handler_id", None):
            self._map_handler_id = self.connect("map", self._on_mapped_fetch)

    def _on_mapped_fetch(self, _widget):
        pending = getattr(self, "_pending_fetch", None)
        if not pending:
            return
        fn, args = pending
        self._pending_fetch = None
        if args and args[0] != self.url:
            return
        submit_fetch(fn, *args)



    def load_url(self, url, **kwargs):
        orig_url = url
        url = get_high_res_url(url, self.target_size)
        self.url = url
        self._pending_fetch = None

        target_size = self.target_size
        crop = self.crop_to_square

        # Fast path: web URL already cached. Paint synchronously and skip the
        # local-cover lookup entirely — keeps the bind path I/O-free.
        if url and url in IMG_CACHE:
            pixbuf = IMG_CACHE[url]
            with IMG_CACHE_LOCK:
                if url in IMG_CACHE:
                    IMG_CACHE.move_to_end(url)
            self._apply_pixbuf(pixbuf, url)
            return

        # No cached pixbuf — check for a downloaded copy. Local cover is
        # authoritative for downloaded tracks; skip the web fetch entirely
        # (matches the AsyncImage fix above — see that comment for context).
        local = resolve_local_cover(self.video_id) if self.video_id else None
        if local:
            self.url = local
            if local in IMG_CACHE:
                self._apply_pixbuf(IMG_CACHE[local], local)
                IMG_CACHE.move_to_end(local)
            else:
                self._queue_fetch(self._fetch_image, local, target_size, crop, [])
            return

        if not url:
            self.set_paintable(None)
            return

        if not self.get_paintable() or self._is_placeholder:
            self.set_from_icon_name("image-missing-symbolic")
            self._is_placeholder = True

        fallbacks = kwargs.get("fallbacks") or get_ytimg_fallbacks(url)
        if url != orig_url and orig_url not in fallbacks:
            fallbacks.append(orig_url)

        self.url = url  # Update so _apply_pixbuf accepts the web result
        self._queue_fetch(self._fetch_image, url, target_size, crop, fallbacks)

    def _fetch_image(self, url, target_size=None, crop=False, fallbacks=None):
        # Skip stale work: if the widget has moved on (fast scroll, re-bind to
        # a different track), don't spend cycles fetching/decoding for it.
        if self.url != url:
            return
        # Another submission for the same URL may have already populated the
        # cache by the time this task is picked up — short-circuit to apply.
        cached_pixbuf = IMG_CACHE.get(url)
        if cached_pixbuf:
            with IMG_CACHE_LOCK:
                if url in IMG_CACHE:
                    IMG_CACHE.move_to_end(url)
            GLib.idle_add(self._apply_pixbuf, cached_pixbuf, url)
            return
        try:
            if url.startswith("file://"):
                # Local file
                import os
                path = url[7:]
                if os.path.exists(path):
                    with open(path, "rb") as f:
                        data = f.read()
                else:
                    return
            else:
                # Persistent disk cache first.
                data = read_thumb_cache(url)
                if not data:
                    # Download image data
                    headers = {"User-Agent": "Mozilla/5.0"}
                    if self.player and hasattr(self.player, "client"):
                        client = self.player.client
                        if client and client.is_authenticated():
                            if any(d in url for d in ["youtube.com", "ytimg.com", "googleusercontent.com", "ggpht.com"]):
                                cookie = client.api.headers.get("Cookie")
                                if cookie:
                                    headers["Cookie"] = cookie

                    import requests
                    resp = requests.get(url, headers=headers, timeout=10)
                    resp.raise_for_status()
                    data = resp.content
                    write_thumb_cache(url, data)

            pixbuf = decode_pixbuf_bounded(data)

            if pixbuf:
                w = pixbuf.get_width()
                h = pixbuf.get_height()

                # Cache the high-res version BEFORE potential thumbnail downscaling
                cache_pixbuf(url, pixbuf)

                if target_size:
                    # Scale to 2x for HiDPI quality (this is the widget-specific version)
                    tw = target_size * 2
                    th = target_size * 2
                    if w > tw or h > th:
                        scale = max(tw / w, th / h)
                        pixbuf = pixbuf.scale_simple(
                            int(w * scale),
                            int(h * scale),
                            GdkPixbuf.InterpType.BILINEAR,
                        )

            GLib.idle_add(self._apply_pixbuf, pixbuf, url)

        except Exception:
            if fallbacks and self.url == url:
                next_url = fallbacks.pop(0)
                self.url = next_url
                self._fetch_image(next_url, target_size, crop, fallbacks)
            else:
                # Last resort: try local cover for downloaded songs
                try:
                    local = self._get_local_cover()
                    if local and local != url:
                        self._fetch_image(local, target_size, crop, [])
                except Exception:
                    pass

    def _apply_pixbuf(self, pixbuf, url=None):
        # Race condition check
        if url and self.url != url:
            return

        if not pixbuf:
            self.set_paintable(None)
            return

        # Notify player of working URL
        if self.player and url and "ytimg.com" in url:
            GLib.idle_add(self._sync_player_url, url)

        # Crop to center square if requested
        if self.crop_to_square and pixbuf:
            w = pixbuf.get_width()
            h = pixbuf.get_height()
            if w != h:
                size = min(w, h)
                x_off = (w - size) // 2
                y_off = (h - size) // 2
                pixbuf = pixbuf.new_subpixbuf(x_off, y_off, size, size)

        # Convert to Texture and paint
        texture = Gdk.Texture.new_for_pixbuf(pixbuf)
        self.set_paintable(texture)
        self._is_placeholder = False

    def _sync_player_url(self, url):
        if not self.player or not url:
            return
        if hasattr(self.player, "update_track_thumbnail"):
            video_id = getattr(self, "video_id", None)
            if video_id:
                self.player.update_track_thumbnail(video_id, url)





class MarqueeLabel(Gtk.ScrolledWindow):
    def __init__(self, **kwargs):
        super().__init__(**kwargs)
        self.set_policy(Gtk.PolicyType.EXTERNAL, Gtk.PolicyType.NEVER)
        self.set_hexpand(True)

        self.box = Gtk.Box(orientation=Gtk.Orientation.HORIZONTAL, spacing=60)
        self.label1 = Gtk.Label()
        self.label2 = Gtk.Label()
        
        self.box.append(self.label1)
        self.box.append(self.label2)
        self.set_child(self.box)

        self._tick_id = 0
        self._loop_spacing = 60
        self._is_animating = False

        self.connect("map", self._start_marquee)
        self.connect("unmap", self._stop_marquee)

    def add_css_class(self, class_name):
        self.label1.add_css_class(class_name)
        self.label2.add_css_class(class_name)

    def _start_marquee(self, *args):
        if self._tick_id == 0:
            self._tick_id = self.add_tick_callback(self._on_tick)

    def _stop_marquee(self, *args):
        if self._tick_id != 0:
            self.remove_tick_callback(self._tick_id)
            self._tick_id = 0

    def _on_tick(self, widget, frame_clock):
        width = self.get_width()
        label_w = self.label1.get_width()

        # If it fits, don't animate and keep centered/start aligned
        if label_w <= width:
            self.label2.set_visible(False)
            self.get_hadjustment().set_value(0)
            self._is_animating = False
            return True

        # Otherwise, animate
        self.label2.set_visible(True)
        self._is_animating = True

        frame_time = frame_clock.get_frame_time()
        if not hasattr(self, "_last_frame_time"):
            self._last_frame_time = frame_time
            return True

        delta = (frame_time - self._last_frame_time) / 1_000_000.0
        self._last_frame_time = frame_time

        adj = self.get_hadjustment()
        speed = 40.0  # px/s
        new_val = adj.get_value() + (speed * delta)

        # Seamless loop point
        loop_point = label_w + self._loop_spacing
        if new_val >= loop_point:
            new_val -= loop_point

        adj.set_value(new_val)
        return True

    def set_label(self, text):
        self.label1.set_label(text)
        self.label2.set_label(text)
        # Reset scroll on text change
        self.get_hadjustment().set_value(0)
        if hasattr(self, "_last_frame_time"):
            delattr(self, "_last_frame_time")


class LikeButton(Gtk.Button):
    def __init__(self, client, video_id, initial_status="INDIFFERENT", **kwargs):
        super().__init__(**kwargs)
        self.client = client
        self.video_id = video_id
        self.status = initial_status

        self.add_css_class("flat")
        self.add_css_class("circular")
        self.set_valign(Gtk.Align.CENTER)

        self.update_icon()
        weak_self = weakref.ref(self)
        self.connect("clicked", lambda btn: weak_self().on_clicked(btn) if weak_self() else None)

    def update_icon(self):
        if self.status == "LIKE":
            self.set_icon_name("starred-symbolic")
            self.add_css_class("liked-button")  # For potential CSS styling
            self.set_tooltip_text("Unlike")
        elif self.status == "DISLIKE":
            self.set_icon_name(
                "view-restore-symbolic"
            )  # Placeholder or specific icon if found
            self.set_tooltip_text("Disliked")
        else:
            self.set_icon_name("non-starred-symbolic")
            self.remove_css_class("liked-button")
            self.set_tooltip_text("Like")

    def on_clicked(self, btn):
        # Toggle: LIKE -> INDIFFERENT, others -> LIKE
        new_status = "INDIFFERENT" if self.status == "LIKE" else "LIKE"

        # Optimistic update
        old_status = self.status
        self.status = new_status
        self.update_icon()

        def do_rate():
            success = self.client.rate_song(self.video_id, new_status)
            if not success:
                # Revert on failure
                GLib.idle_add(self.revert, old_status)
            else:
                # if disliked, invalidate Liked Music playlist cache
                # this avoids the regression skip
                # normal playlists have a similar functionality to invalidate cache when a track is removed
                if new_status == "INDIFFERENT":
                    from player.downloads import get_download_db
                    get_download_db().invalidate_playlist_cache("LM")

        thread = threading.Thread(target=do_rate)
        thread.daemon = True
        thread.start()

    def revert(self, status):
        self.status = status
        self.update_icon()

    def set_data(self, video_id, status):
        self.video_id = video_id
        self.status = status
        self.update_icon()
        self.set_visible(bool(video_id))

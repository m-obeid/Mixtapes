"""Album-cover-derived effects: a heavily-blurred copy for the
Amberol-style background, and a dominant-color extraction for the
dynamic accent. Both are computed on a background thread and cached so a
repeated lookup for the same cover is free."""

import io
import os
import re
import threading
from collections import OrderedDict
from concurrent.futures import ThreadPoolExecutor

from gi.repository import GLib

from ui.utils import (
    read_thumb_cache,
    write_thumb_cache,
    _thumb_cache_key,
)


MAX_BLUR_CACHE_ENTRIES = 16
MAX_COLOR_CACHE_ENTRIES = 64
_cache_lock = threading.Lock()
_blur_cache = OrderedDict()    # cache key -> path to blurred PNG
_color_cache = OrderedDict()   # url -> (r, g, b) normalized 0..1

# Coalesce concurrent _ensure_image_bytes() calls for the same URL. On a
# track change the blur + accent-color workers both run on their own
# thread, and historically both raced to fetch the same image bytes —
# i.e. two HTTP GETs to YouTube's CDN per track. Now the second caller
# waits on the first one's Event and then reads the freshly-populated
# disk cache.
_inflight_lock = threading.Lock()
_inflight = {}  # url -> threading.Event
_effect_executor = ThreadPoolExecutor(
    max_workers=2,
    thread_name_prefix="muse-cover-effects",
)


def _submit_effect_worker(fn):
    _effect_executor.submit(fn)


def _remember_blur_cache(cache_key, path):
    with _cache_lock:
        _blur_cache[cache_key] = path
        _blur_cache.move_to_end(cache_key)
        while len(_blur_cache) > MAX_BLUR_CACHE_ENTRIES:
            _blur_cache.popitem(last=False)


def _get_blur_cache(cache_key):
    with _cache_lock:
        path = _blur_cache.get(cache_key)
        if path is not None:
            _blur_cache.move_to_end(cache_key)
        return path


def _remember_color_cache(url, color):
    with _cache_lock:
        _color_cache[url] = color
        _color_cache.move_to_end(url)
        while len(_color_cache) > MAX_COLOR_CACHE_ENTRIES:
            _color_cache.popitem(last=False)


def _get_color_cache(url):
    with _cache_lock:
        color = _color_cache.get(url)
        if color is not None:
            _color_cache.move_to_end(url)
        return color


def _blur_cache_dir():
    path = os.path.join(GLib.get_user_cache_dir(), "muse", "covers_blurred")
    try:
        os.makedirs(path, exist_ok=True)
    except OSError:
        pass
    return path


_YT_THUMB_RE = re.compile(
    r"^(https?://i\.ytimg\.com/vi/[^/]+/)([^/.?]+)(\.[A-Za-z]+)(\?.*)?$"
)
# YouTube generates these lazily; maxres/sd are only present for videos
# uploaded at sufficient resolution, while hq/mq/default always exist.
# Ordered from highest to lowest quality so we degrade gracefully.
_YT_THUMB_FALLBACKS = [
    "maxresdefault", "sddefault", "hqdefault", "mqdefault", "default",
]


def _yt_thumb_fallback_urls(url):
    """If `url` is a YouTube video thumbnail, yield it followed by the
    progressively lower-resolution variants. Otherwise yield just `url`."""
    m = _YT_THUMB_RE.match(url)
    if not m:
        yield url
        return
    prefix, variant, ext, query = m.group(1), m.group(2), m.group(3), m.group(4) or ""
    seen = set()
    # Try the originally-requested variant first, then walk the fallback
    # list starting at its position (skipping anything higher-res than
    # what was asked for — no point retrying a 404 with a higher-res URL
    # that's even more likely to be missing).
    yield url
    seen.add(variant)
    try:
        start = _YT_THUMB_FALLBACKS.index(variant) + 1
    except ValueError:
        start = 0
    for fb in _YT_THUMB_FALLBACKS[start:]:
        if fb in seen:
            continue
        seen.add(fb)
        yield f"{prefix}{fb}{ext}{query}"


def _ensure_image_bytes(url):
    """Return cached/downloaded bytes for `url`, or None on failure.

    For YouTube thumbnail URLs we transparently fall back to lower-res
    variants (sddefault → hqdefault → ...) when the requested one 404s,
    since YouTube only generates maxres/sd for high-res uploads."""
    if not url:
        return None
    data = read_thumb_cache(url)
    if data:
        return data

    # Coalesce concurrent fetches for the same URL — one leader does the
    # HTTP, everyone else waits and reads the populated cache.
    with _inflight_lock:
        event = _inflight.get(url)
        is_leader = event is None
        if is_leader:
            event = threading.Event()
            _inflight[url] = event
    if not is_leader:
        # Cap the wait so a stuck leader (e.g. dead network) doesn't
        # hang follower threads forever.
        event.wait(timeout=20)
        return read_thumb_cache(url)

    try:
        try:
            import requests
        except Exception as e:
            print(f"[cover_effects] requests import failed: {e}")
            return None
        last_err = None
        for candidate in _yt_thumb_fallback_urls(url):
            try:
                resp = requests.get(
                    candidate, timeout=10, headers={"User-Agent": "Mozilla/5.0"}
                )
                resp.raise_for_status()
                data = resp.content
                # Cache under the originally-requested URL so subsequent
                # lookups for the same key hit the cache, regardless of
                # which fallback actually served the bytes.
                write_thumb_cache(url, data)
                return data
            except Exception as e:
                last_err = e
                # Only walk to the next fallback on 404 — other errors
                # (timeout, DNS, etc.) won't be fixed by a different URL.
                status = getattr(getattr(e, "response", None), "status_code", None)
                if status != 404:
                    break
        print(f"[cover_effects] fetch failed: {last_err}")
        return None
    finally:
        with _inflight_lock:
            _inflight.pop(url, None)
        event.set()


# ─── Blurred background ────────────────────────────────────────────────────


def get_blurred_cover(
    url,
    blur_radius=42,
    output_size=512,
    tint=(0, 0, 0, 150),
    callback=None,
):
    """Asynchronously produce a heavily-blurred, tinted PNG of `url`.

    `tint` is an (R, G, B, A) tuple — black with high alpha for dark mode,
    white with moderate alpha for light mode. The alpha controls how
    aggressively the cover's color is washed toward the tint color.

    Calls `callback(path_or_none)` on the GTK main loop when ready. The
    blurred image is cached per (url, radius, size, tint) tuple so light
    and dark variants don't overwrite each other.
    """
    if not url:
        if callback:
            GLib.idle_add(callback, None)
        return

    cache_key = (url, blur_radius, output_size, tuple(tint))
    cached_path = _get_blur_cache(cache_key)
    if cached_path and os.path.exists(cached_path):
        if callback:
            GLib.idle_add(callback, cached_path)
        return

    tint_tag = f"r{tint[0]}g{tint[1]}b{tint[2]}a{tint[3]}"
    out_path = os.path.join(
        _blur_cache_dir(),
        f"{_thumb_cache_key(url)}_b{blur_radius}_s{output_size}_{tint_tag}.png",
    )
    if os.path.exists(out_path):
        _remember_blur_cache(cache_key, out_path)
        if callback:
            GLib.idle_add(callback, out_path)
        return

    def _worker():
        data = _ensure_image_bytes(url)
        if not data:
            if callback:
                GLib.idle_add(callback, None)
            return
        try:
            from PIL import Image, ImageFilter
            img = Image.open(io.BytesIO(data)).convert("RGB")
            w, h = img.size
            side = min(w, h)
            img = img.crop((
                (w - side) // 2,
                (h - side) // 2,
                (w + side) // 2,
                (h + side) // 2,
            ))
            img = img.resize((output_size, output_size), Image.LANCZOS)
            img = img.filter(ImageFilter.GaussianBlur(radius=blur_radius))
            try:
                from PIL import ImageEnhance
                img = ImageEnhance.Color(img).enhance(1.25)
            except Exception:
                pass
            if tint and tint[3] > 0:
                img = img.convert("RGBA")
                overlay = Image.new("RGBA", img.size, tuple(tint))
                img.alpha_composite(overlay)
                img = img.convert("RGB")
            tmp_path = out_path + ".tmp"
            img.save(tmp_path, "PNG", optimize=True)
            os.replace(tmp_path, out_path)
            _remember_blur_cache(cache_key, out_path)
            if callback:
                GLib.idle_add(callback, out_path)
        except Exception as e:
            print(f"[cover_effects] blur failed for {url}: {e}")
            if callback:
                GLib.idle_add(callback, None)

    _submit_effect_worker(_worker)


# ─── Dominant color extraction ─────────────────────────────────────────────


def get_dominant_color(url, callback=None):
    """Asynchronously extract a "good accent" color from `url`. Calls
    `callback((r, g, b))` with floats 0..1, or `callback(None)` on
    failure. Cached per URL.

    The selection prefers colors that are:
      - saturated enough to be a real accent (not gray)
      - mid-bright (not so dark text dies on top, not so washed-out it
        disappears against a light theme)
      - heavily represented in the image (so the accent feels like it
        comes *from* the cover)
    """
    if not url:
        if callback:
            GLib.idle_add(callback, None)
        return

    cached = _get_color_cache(url)
    if cached is not None:
        if callback:
            GLib.idle_add(callback, cached)
        return

    def _worker():
        data = _ensure_image_bytes(url)
        if not data:
            if callback:
                GLib.idle_add(callback, None)
            return
        try:
            from PIL import Image
            img = Image.open(io.BytesIO(data)).convert("RGB")
            img.thumbnail((96, 96), Image.LANCZOS)
            quant = img.quantize(colors=10, method=Image.MEDIANCUT)
            palette = quant.getpalette() or []
            counts = sorted(quant.getcolors() or [], reverse=True)

            best = None
            best_score = -1.0
            for count, idx in counts:
                r = palette[idx * 3]
                g = palette[idx * 3 + 1]
                b = palette[idx * 3 + 2]
                mx, mn = max(r, g, b), min(r, g, b)
                if mx == 0:
                    continue
                sat = (mx - mn) / mx              # 0..1
                lum = (r + g + b) / (3 * 255)     # 0..1
                if lum < 0.18 or lum > 0.88:
                    continue
                if sat < 0.18:
                    continue
                score = sat * (count ** 0.35)
                if score > best_score:
                    best_score = score
                    best = (r / 255.0, g / 255.0, b / 255.0)

            # Fallback: most-frequent color regardless of saturation/luminance.
            if best is None and counts:
                _, idx = counts[0]
                r = palette[idx * 3]
                g = palette[idx * 3 + 1]
                b = palette[idx * 3 + 2]
                best = (r / 255.0, g / 255.0, b / 255.0)

            if best is not None:
                _remember_color_cache(url, best)
            if callback:
                GLib.idle_add(callback, best)
        except Exception as e:
            print(f"[cover_effects] color extract failed for {url}: {e}")
            if callback:
                GLib.idle_add(callback, None)

    _submit_effect_worker(_worker)

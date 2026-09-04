"""Album-cover-derived effects: a heavily-blurred copy for the
Amberol-style background, and a dominant-color extraction for the
dynamic accent. Both are computed on a background thread and cached so a
repeated lookup for the same cover is free."""

import io
import math
import os
import re
import threading
from collections import OrderedDict
from concurrent.futures import ThreadPoolExecutor

from gi.repository import GLib

from ui import color_utils
from ui.utils import (
    read_thumb_cache,
    write_thumb_cache,
    _thumb_cache_key,
)


MAX_BLUR_CACHE_ENTRIES = 48
MAX_COLOR_CACHE_ENTRIES = 128

# Accent selection, in OkLCh. Below MIN_ACCENT_CHROMA a color reads as
# gray. Accents look best near IDEAL_ACCENT_LIGHTNESS, falling off over
# ACCENT_LIGHTNESS_SPREAD.
MIN_ACCENT_CHROMA = 0.045
IDEAL_ACCENT_LIGHTNESS = 0.62
ACCENT_LIGHTNESS_SPREAD = 0.32
# Monochrome covers get the full treatment, in grays. Featureless ones
# get nothing. MIN_COVER_DETAIL is the OkLCh lightness spread across the
# cover: solid fills measure 0, the lowest real cover in 300 measured
# 0.03. Only colorless covers reach this test. A flat but saturated
# cover has no lightness spread either, and keeps its accent.
MIN_COVER_DETAIL = 0.05
# Cached in place of a color to mean "no usable accent".
_NO_ACCENT = object()

# Blurred-background normalization. A fixed tint alpha left dark covers
# near black and light ones near white, so the album art decided how
# legible the chrome was. Normalize into a band instead. Dark: median
# luminance to 0.025, highlights capped at 0.12, which drops the spread
# across covers from 32x to 4x. Light: blend toward white until the
# 2nd-percentile luminance hits 0.35, which takes body text below 4.5:1
# from 55% of covers to 3%.
BLUR_DARK_MEDIAN = 0.025
BLUR_DARK_HIGHLIGHT_CAP = 0.12
BLUR_LIGHT_FLOOR = 0.35
BLUR_MAX_GAIN = 3.0
# A featureless cover normalizes to a flat gray field, which reads as
# broken chrome. No accent means no blur. See pick_accent.
# Cached in place of a path.
_NO_BLUR = object()


def _blur_luminances(img):
    """Sorted relative luminances of a 48x48 sample."""
    small = img.resize((48, 48))
    return sorted(
        color_utils.relative_luminance(tuple(c / 255.0 for c in px))
        for px in small.getdata()
    )


def _percentile(values, fraction):
    return values[min(len(values) - 1, int(len(values) * fraction))]


def _normalize_blur(img, dark):
    """Bring a blurred cover into the scheme's luminance band."""
    from PIL import Image, ImageEnhance

    values = _blur_luminances(img)
    if dark:
        # Blending toward black is a scale in sRGB. One gain dims a
        # bright cover and lifts a near-black one.
        gain = (BLUR_DARK_MEDIAN / max(_percentile(values, 0.5), 1e-5)) ** (1 / 2.4)
        gain = max(0.05, min(BLUR_MAX_GAIN, gain))
        highlight = _percentile(values, 0.98) * (gain ** 2.4)
        if highlight > BLUR_DARK_HIGHLIGHT_CAP:
            gain *= (BLUR_DARK_HIGHLIGHT_CAP / highlight) ** (1 / 2.4)
        return ImageEnhance.Brightness(img).enhance(gain)

    # Light: lift toward white until the darkest areas clear the floor.
    # Those areas set worst-case text contrast. Search on a 48x48 proxy:
    # blending the full 720x720 image twelve times allocated ~18 MB of
    # throwaway buffers per cover, on the same threads GdkPixbuf decodes
    # covers on.
    proxy = img.resize((48, 48))
    proxy_white = Image.new("RGB", proxy.size, (255, 255, 255))
    lo, hi = 0.0, 1.0
    for _ in range(12):
        mid = (lo + hi) / 2
        blended = Image.blend(proxy, proxy_white, mid)
        if _percentile(_blur_luminances(blended), 0.02) < BLUR_LIGHT_FLOOR:
            lo = mid
        else:
            hi = mid
    return Image.blend(img, Image.new("RGB", img.size, (255, 255, 255)), hi)


_cache_lock = threading.Lock()
_blur_cache = OrderedDict()    # cache key -> path to blurred PNG
_color_cache = OrderedDict()   # url -> (r, g, b) normalized 0..1

# Coalesce concurrent _ensure_image_bytes() calls for the same URL. On a
# track change the blur and accent-color workers each run on their own
# thread, and both used to race to fetch the same image bytes: two HTTP
# GETs to YouTube's CDN per track. The second caller now waits on the
# first one's Event, then reads the populated disk cache.
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
    # Try the requested variant first, then walk the fallback list from
    # its position. Skip anything higher-res than the request. Retrying a
    # 404 with a higher-res URL only finds something missing more often.
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

    # Coalesce concurrent fetches for the same URL. One leader does the
    # HTTP. Everyone else waits, then reads the populated cache.
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
                # Walk to the next fallback on 404 only. A different URL
                # fixes nothing for a timeout or a DNS failure.
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
    output_size=720,
    dark=True,
    callback=None,
):
    """Blur `url` on a worker thread, normalized into `dark`'s
    luminance band. See _normalize_blur.

    Calls `callback(path, backdrop)` on the GTK main loop. `backdrop` is
    the (typical, worst-for-text) luminance pair the image landed on;
    callers derive the colors going on top from it. Failure or a
    declined cover gives `callback(None, None)`. Cached per
    (url, radius, size, dark).
    """
    if not url:
        if callback:
            GLib.idle_add(callback, None, None)
        return

    cache_key = (url, blur_radius, output_size, bool(dark))
    cached = _get_blur_cache(cache_key)
    if cached is _NO_BLUR:
        if callback:
            GLib.idle_add(callback, None, None)
        return
    if cached and os.path.exists(cached[0]):
        if callback:
            GLib.idle_add(callback, cached[0], cached[1])
        return

    scheme_tag = "dark" if dark else "light"
    out_path = os.path.join(
        _blur_cache_dir(),
        f"{_thumb_cache_key(url)}_b{blur_radius}_s{output_size}_{scheme_tag}.png",
    )

    def _worker():
        data = _ensure_image_bytes(url)
        if not data:
            if callback:
                GLib.idle_add(callback, None, None)
            return
        try:
            from PIL import Image, ImageFilter
            img = Image.open(io.BytesIO(data)).convert("RGB")
            # One verdict for both effects. Separate ones let a cover
            # keep its backdrop while its accent fell back to the system
            # accent, mixing two unrelated colors.
            if pick_accent(img) is None:
                _remember_blur_cache(cache_key, _NO_BLUR)
                if callback:
                    GLib.idle_add(callback, None, None)
                return
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
            img = _normalize_blur(img, dark)
            values = _blur_luminances(img)
            # Typical brightness, plus the end worst for text.
            backdrop = (
                _percentile(values, 0.5),
                _percentile(values, 0.98 if dark else 0.02),
            )
            tmp_path = out_path + ".tmp"
            img.save(tmp_path, "PNG", optimize=True)
            os.replace(tmp_path, out_path)
            _remember_blur_cache(cache_key, (out_path, backdrop))
            if callback:
                GLib.idle_add(callback, out_path, backdrop)
        except Exception as e:
            print(f"[cover_effects] blur failed for {url}: {e}")
            if callback:
                GLib.idle_add(callback, None, None)

    _submit_effect_worker(_worker)


# ─── Dominant color extraction ─────────────────────────────────────────────


def _cover_lightnesses(img):
    """Sorted OkLCh lightnesses of a coarse sample of the cover."""
    small = img.convert("RGB").resize((32, 32))
    return sorted(
        color_utils.rgb_to_oklch(tuple(c / 255.0 for c in px))[0]
        for px in small.getdata()
    )


def cover_detail(img):
    """Spread between the cover's dark and light ends, in OkLCh lightness."""
    values = _cover_lightnesses(img)
    if not values:
        return 0.0
    n = len(values)
    return values[int(n * 0.95)] - values[int(n * 0.05)]


def pick_accent(img):
    """The cover's accent as (r, g, b) floats, or None when the cover
    is featureless. See get_dominant_color."""
    from PIL import Image

    img = img.copy()
    # 128px and 32 bins, up from 96px and 10. A coarse palette spends
    # every bin on shades of white on a mostly-white cover.
    img.thumbnail((128, 128), Image.LANCZOS)
    quant = img.quantize(colors=32, method=Image.MEDIANCUT)
    palette = quant.getpalette() or []
    counts = quant.getcolors() or []
    total = sum(count for count, _ in counts) or 1

    best = None
    best_score = 0.0
    for count, idx in counts:
        rgb = (
            palette[idx * 3] / 255.0,
            palette[idx * 3 + 1] / 255.0,
            palette[idx * 3 + 2] / 255.0,
        )
        lightness, chroma, _hue = color_utils.rgb_to_oklch(rgb)
        if lightness < 0.12 or lightness > 0.95:
            continue  # near-black / near-white: not an accent
        if chroma < MIN_ACCENT_CHROMA:
            continue  # gray, however much of the cover it is
        share = count / total
        lightness_weight = math.exp(
            -(((lightness - IDEAL_ACCENT_LIGHTNESS)
               / ACCENT_LIGHTNESS_SPREAD) ** 2)
        )
        # Cap chroma so one neon speck cannot outrank the region that
        # defines the cover.
        score = min(chroma, 0.18) * (share ** 0.3) * lightness_weight
        if score > best_score:
            best_score = score
            best = rgb
    if best is not None:
        return best

    # No chromatic bin. A monochrome cover still gets a neutral; a
    # featureless one gets nothing. Use the median lightness, not the
    # most common one: white line art on black is mostly black, and
    # answering "black" hands the pipeline an extreme.
    if cover_detail(img) < MIN_COVER_DETAIL:
        return None
    values = _cover_lightnesses(img)
    if not values:
        return None
    median = values[len(values) // 2]
    return color_utils.oklch_to_rgb(min(0.85, max(0.35, median)), 0.0, 0.0)


def get_dominant_color(url, callback=None):
    """Extract an accent color from `url` on a worker thread.

    Calls `callback((r, g, b))` with floats 0..1, or `callback(None)`
    for a featureless cover. Cached per URL.

    Scores palette bins in OkLCh on chroma, lightness near the middle,
    and share of the cover damped by share ** 0.3. HSV saturation was the
    old test and calls a muddy brown saturated and a pastel gray.

    Monochrome covers return a neutral. The old fallback took the most
    frequent color and washed the UI out on 15% of a 600-cover sample.
    """
    if not url:
        if callback:
            GLib.idle_add(callback, None)
        return

    cached = _get_color_cache(url)
    if cached is not None:
        if callback:
            GLib.idle_add(callback, None if cached is _NO_ACCENT else cached)
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
            best = pick_accent(img)
            _remember_color_cache(url, best if best is not None else _NO_ACCENT)
            if callback:
                GLib.idle_add(callback, best)
        except Exception as e:
            print(f"[cover_effects] color extract failed for {url}: {e}")
            if callback:
                GLib.idle_add(callback, None)

    _submit_effect_worker(_worker)

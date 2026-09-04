"""User preferences for the lyrics pipeline.

Covers four things, all stored in the shared ``prefs.json``:

- ``lyrics_provider_order`` — the search queue. An ordered list of
  provider display names; the chain tries them top-down.
- ``lyrics_providers_disabled`` — names the user switched off. Kept as a
  separate list (rather than dropping them from the order) so toggling a
  provider back on restores its position instead of appending it last.
- ``lyrics_match_mode`` — ``quality`` walks past a provider that only has
  plain unsynced text when a later one might have synced lyrics, falling
  back to the plain hit if nothing better turns up. ``strict`` takes the
  first provider that returns anything at all.
- ``lyrics_second_line`` / ``lyrics_effects`` — display options consumed
  by the lyrics widget.

A provider added in a later release isn't in the saved order, so
:func:`provider_order` appends unknown-but-known-to-the-app names at the
end rather than losing them.

Reads are mtime-cached: the fetch chain asks for the order on every
track change from a worker thread, and the widget asks per row build.
"""

import json
import os
import threading

from gi.repository import GLib


# Canonical provider names, in the order the chain used before the queue
# was configurable. api/client.py maps these to fetchers; keep the two
# lists in sync.
DEFAULT_PROVIDER_ORDER = [
    "Apple Music",
    "BetterLyrics",
    "BiniLyrics",
    "NetEase",
    "LRCLIB",
    "YouTube Music",
]

MATCH_QUALITY = "quality"
MATCH_STRICT = "strict"

SECOND_LINE_MODES = ("off", "auto", "romanization", "translation", "background")
EFFECTS_LEVELS = ("off", "subtle", "full")
EFFECTS_DEFAULT = "full"

# Multiplier on the lyric column's resting type size.
FONT_SCALE_MIN, FONT_SCALE_MAX, FONT_SCALE_DEFAULT = 0.7, 2.0, 1.0
# How much bigger the active line is drawn than the resting ones. The
# row's own height never changes, and a line with no slack to grow into
# is capped to what fits, so this can be assertive by default.
ACTIVE_SCALE_MIN, ACTIVE_SCALE_MAX, ACTIVE_SCALE_DEFAULT = 1.0, 1.4, 1.20

_lock = threading.Lock()
_cache = None
_cache_mtime = -1.0


def _path():
    return os.path.join(GLib.get_user_data_dir(), "muse", "prefs.json")


def _read():
    """Return the whole prefs dict, re-reading only when the file's
    mtime moved. Preferences are written by the settings dialog on the
    main thread and read from lyric fetch workers, hence the lock."""
    global _cache, _cache_mtime
    path = _path()
    try:
        mtime = os.path.getmtime(path)
    except OSError:
        return {}
    with _lock:
        if _cache is not None and mtime == _cache_mtime:
            return _cache
        try:
            with open(path) as f:
                data = json.load(f)
        except (OSError, json.JSONDecodeError):
            data = {}
        if not isinstance(data, dict):
            data = {}
        _cache = data
        _cache_mtime = mtime
        return data


def _write(key, value):
    global _cache, _cache_mtime
    path = _path()
    data = {}
    try:
        if os.path.exists(path):
            with open(path) as f:
                data = json.load(f) or {}
    except (OSError, json.JSONDecodeError):
        data = {}
    if not isinstance(data, dict):
        data = {}
    data[key] = value
    try:
        os.makedirs(os.path.dirname(path), exist_ok=True)
        with open(path, "w") as f:
            json.dump(data, f)
    except OSError as e:
        print(f"[LYRICS-PREFS] write failed: {e}")
        return
    with _lock:
        _cache = data
        try:
            _cache_mtime = os.path.getmtime(path)
        except OSError:
            _cache_mtime = -1.0


def invalidate():
    """Drop the mtime cache. Only needed when something outside this
    module rewrites prefs.json in the same second the cache was filled."""
    global _cache, _cache_mtime
    with _lock:
        _cache = None
        _cache_mtime = -1.0


def full_provider_order():
    """Every known provider in the user's order, disabled ones included.
    Saved names the app no longer ships are dropped; providers the app
    ships that aren't in the saved order are appended in catalog order."""
    saved = _read().get("lyrics_provider_order")
    if not isinstance(saved, list):
        return list(DEFAULT_PROVIDER_ORDER)
    known = set(DEFAULT_PROVIDER_ORDER)
    out = []
    for name in saved:
        if name in known and name not in out:
            out.append(name)
    for name in DEFAULT_PROVIDER_ORDER:
        if name not in out:
            out.append(name)
    return out


def disabled_providers():
    saved = _read().get("lyrics_providers_disabled")
    if not isinstance(saved, list):
        return set()
    return {n for n in saved if isinstance(n, str)}


def provider_order():
    """The search queue: enabled providers only, in user order. Never
    returns an empty list — switching every provider off would leave the
    lyrics view permanently blank with no way to tell why, so an
    all-disabled config falls back to the catalog default."""
    disabled = disabled_providers()
    order = [n for n in full_provider_order() if n not in disabled]
    return order or list(DEFAULT_PROVIDER_ORDER)


def set_provider_order(order):
    _write("lyrics_provider_order", list(order))


def set_provider_enabled(name, enabled):
    disabled = disabled_providers()
    if enabled:
        disabled.discard(name)
    else:
        disabled.add(name)
    _write("lyrics_providers_disabled", sorted(disabled))


def match_mode():
    val = _read().get("lyrics_match_mode", MATCH_QUALITY)
    return val if val in (MATCH_QUALITY, MATCH_STRICT) else MATCH_QUALITY


def set_match_mode(mode):
    _write("lyrics_match_mode", mode)


def second_line_mode():
    val = _read().get("lyrics_second_line", "auto")
    return val if val in SECOND_LINE_MODES else "auto"


def set_second_line_mode(mode):
    _write("lyrics_second_line", mode)


def line_sweep():
    """Whether a line-synced source gets synthesized per-word timing so
    its highlight travels across the line."""
    return bool(_read().get("lyrics_line_sweep", True))


def set_line_sweep(enabled):
    _write("lyrics_line_sweep", bool(enabled))


def _clamped_float(key, default, low, high):
    try:
        val = float(_read().get(key, default))
    except (TypeError, ValueError):
        return default
    return max(low, min(high, val))


def font_scale():
    return _clamped_float(
        "lyrics_font_scale", FONT_SCALE_DEFAULT, FONT_SCALE_MIN, FONT_SCALE_MAX
    )


def set_font_scale(value):
    _write("lyrics_font_scale", float(value))


def active_scale():
    return _clamped_float(
        "lyrics_active_scale", ACTIVE_SCALE_DEFAULT,
        ACTIVE_SCALE_MIN, ACTIVE_SCALE_MAX,
    )


def set_active_scale(value):
    _write("lyrics_active_scale", float(value))


def effects_level():
    val = _read().get("lyrics_effects", EFFECTS_DEFAULT)
    return val if val in EFFECTS_LEVELS else EFFECTS_DEFAULT


def set_effects_level(level):
    _write("lyrics_effects", level)

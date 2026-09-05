"""Self-contained lyrics widget. Subscribes to the player's metadata and
progression signals, fetches lyrics in a background thread, and renders
them as a scrollable ListBox of tap-to-seek lines. Active line is centered
in the viewport; per-word data, when available, drives a karaoke-style
alpha fade.

Architecture follows Nocturne's (Jeffser/Nocturne) playing lyrics page:
- Every row always renders with ``set_markup`` (never ``set_label``), so
  Pango's layout pipeline is stable across active/inactive transitions.
- Per-row opacity comes from a ``<span fgalpha='N'>`` Pango attribute,
  which is a pure render attribute that does not affect glyph metrics.
- A per-row ``tick_callback`` lerps the alphas toward their target each
  frame and only re-renders markup when ``changed`` is true.
- Scroll target uses the row's own ``get_allocation().y`` (which lives
  in the ListBox's coordinate space — that's exactly what the
  vadjustment is offset against), wrapped in ``GLib.idle_add`` so the
  scroll happens after layout has settled.
"""

import html
import math
import os
import re
import threading
import time
from gi.repository import Gtk, Adw, GObject, GLib, Gdk, Gsk, Pango, Graphene

from ui.widgets.fade_edges_bin import FadeEdgesBin
from ui.util_classes import ScrolledWindow
from player import lyrics_prefs


# Pango's "alpha" attribute takes a 16-bit value where 65535 = fully opaque.
_PANGO_ALPHA_MAX = 65535


# Toggle with MIXTAPES_LYRICS_DEBUG=1 (or =2 for tick-level chatter).
# Logs go to stdout, prefixed with the elapsed seconds since launch and
# a [LYRICS] tag so they're easy to grep.
_DEBUG_LEVEL = int(os.environ.get("MIXTAPES_LYRICS_DEBUG") or "0")
_DEBUG_T0 = time.monotonic()


def _dlog(level, msg):
    if _DEBUG_LEVEL >= level:
        dt = time.monotonic() - _DEBUG_T0
        print(f"[LYRICS {dt:7.3f}] {msg}", flush=True)

# Opacity targets per line state.
_ALPHA_ACTIVE = 1.00
_ALPHA_INACTIVE = 0.32
_ALPHA_FUTURE_WORD = 0.32
# The second line (romanization / translation / background vocals) sits
# under the lead at a lower ceiling so it reads as support, not as a
# second lyric competing for the eye.
_ALPHA_SUB_ACTIVE = 0.78
_ALPHA_SUB_INACTIVE = 0.20

# Per-frame fade speed (fraction of the remaining gap). Effects-off keeps
# the original constant-rate fade; with effects on the per-word target is
# already ramped against the word's own duration, so the lerp only needs
# to be fast enough to smooth out seeks.
_LERP_SPEED = 0.18
_LERP_SPEED_EFFECTS = 0.34

# How long a word takes to reach full brightness, as a fraction of how
# long it's actually held, clamped at both ends. A word held for two
# seconds swells in; a rapid-fire syllable snaps.
_WORD_RAMP_FRACTION = 0.55
_WORD_RAMP_MIN_MS = 90
_WORD_RAMP_MAX_MS = 420
# Synthesized timings are an estimate, so their words ramp faster than
# real ones: a slow swell on a guessed boundary reads as lag, while a
# quicker fill reads as the highlight simply travelling.
_SWEEP_RAMP_FRACTION = 0.3

# What each level draws. The active line growing is the effect that
# actually tracks the singing, so it carries "subtle" on its own; the
# glow and the distance blur are decoration and belong to "full".
#
#   off     — plain fade, nothing else
#   subtle  — duration-aware word fades + the active line grows
#   full    — the above + accent glow + blur on distant lines
_EFFECT_SCALE = ("subtle", "full")
_EFFECT_GLOW = ("full",)
_EFFECT_BLUR = ("full",)

_BLUR_START_DISTANCE = 0
_BLUR_PER_LINE = 0.65
_BLUR_MAX = 3.5
_EFFECT_LERP = 0.16

# Scripts that a romanization actually helps with. Greek is left out on
# purpose: it shows up as stylized Latin far more often than as a lyric
# the listener can't read.
_NON_LATIN_RE = re.compile(
    "["
    "\u0400-\u04FF"   # Cyrillic
    "\u0590-\u05FF"   # Hebrew
    "\u0600-\u06FF"   # Arabic
    "\u0E00-\u0E7F"   # Thai
    "\u3040-\u30FF"   # Hiragana + Katakana
    "\u3400-\u4DBF"   # CJK extension A
    "\u4E00-\u9FFF"   # CJK unified ideographs
    "\uAC00-\uD7AF"   # Hangul syllables
    "]"
)


def _is_non_latin(text):
    return bool(text) and bool(_NON_LATIN_RE.search(text))


def _second_line_for(line, mode):
    """Pick the second line's content for one lyric line.

    Returns ``(text, parts_or_None)``. ``parts`` is only ever set for
    background vocals, which carry their own word timing and so can
    karaoke along with the lead; romanizations and translations are
    line-level and just follow the lead line's state.

    ``auto`` shows a romanization when the lyric is in a script the
    romanization is actually for, and background vocals otherwise — the
    two cases where a second line adds something the listener can't get
    from the first.
    """
    if mode == "off":
        return None, None

    roman = (line.get("romanization") or "").strip()
    translation = (line.get("translation") or "").strip()
    bg_text = (line.get("bg_text") or "").strip()
    bg_parts = line.get("bg")

    if mode == "romanization":
        return (roman or None), None
    if mode == "translation":
        return (translation or None), None
    if mode == "background":
        return (bg_text or None), (bg_parts if bg_text else None)

    # auto
    if roman and _is_non_latin(line.get("text") or ""):
        return roman, None
    if bg_text:
        return bg_text, bg_parts
    return None, None


# The lyric column's type size is a preference, so the rule that sets it
# has to be generated rather than living in style.css. One provider for
# the whole display: both LyricsView instances (mobile expanded player
# and desktop cover view) show the same lyrics at the same size.
_FONT_CSS_PROVIDER = None
_FONT_CSS_SCALE = None

# Matches the resting sizes in style.css, which these multiply.
_BASE_FONT_EM = 1.4
_SUB_FONT_EM = 0.98


def apply_font_scale(force=False):
    """Push the current type-size preference into the display's CSS."""
    global _FONT_CSS_PROVIDER, _FONT_CSS_SCALE

    scale = lyrics_prefs.font_scale()
    if not force and scale == _FONT_CSS_SCALE:
        return
    display = Gdk.Display.get_default()
    if display is None:
        return
    if _FONT_CSS_PROVIDER is None:
        _FONT_CSS_PROVIDER = Gtk.CssProvider()
        # One step above the app stylesheet so it wins over the resting
        # sizes declared there, while still losing to user CSS.
        Gtk.StyleContext.add_provider_for_display(
            display, _FONT_CSS_PROVIDER,
            Gtk.STYLE_PROVIDER_PRIORITY_APPLICATION + 1,
        )
    _FONT_CSS_PROVIDER.load_from_string(
        f".lyrics-line .lyrics-line-label {{ font-size: {_BASE_FONT_EM * scale:.3f}em; }}\n"
        f".lyrics-line .lyrics-line-sub {{ font-size: {_SUB_FONT_EM * scale:.3f}em; }}\n"
    )
    _FONT_CSS_SCALE = scale


def _is_cjk_char(ch):
    """Scripts written without spaces between words."""
    o = ord(ch)
    return (
        0x3040 <= o <= 0x30FF      # Hiragana + Katakana
        or 0x3400 <= o <= 0x4DBF   # CJK extension A
        or 0x4E00 <= o <= 0x9FFF   # CJK unified ideographs
        or 0xAC00 <= o <= 0xD7AF   # Hangul syllables
        or 0xF900 <= o <= 0xFAFF   # CJK compatibility ideographs
    )


def _sweep_tokens(text):
    """Split a line into the chunks a sweep advances over.

    Words for space-separated text, single characters for CJK. Splitting
    Japanese on whitespace would yield one token for the whole line and
    the sweep would have nothing to advance across — which is the case
    that most wants it.

    Returns ``[(text, space_after), ...]``.
    """
    tokens = []
    buf = ""

    def flush():
        nonlocal buf
        if buf:
            tokens.append([buf, False])
            buf = ""

    for ch in text:
        if ch.isspace():
            flush()
            if tokens:
                tokens[-1][1] = True
        elif _is_cjk_char(ch):
            flush()
            tokens.append([ch, False])
        else:
            buf += ch
    flush()
    return [(t, sp) for t, sp in tokens if t]


# A line whose span runs longer than this is almost certainly followed by
# an instrumental rather than being sung for that whole time. Cap the
# sweep so it doesn't crawl across the line for half a minute.
_SWEEP_MAX_MS = 12000
# Below this there's no room for a sweep to read as anything but a flash.
_SWEEP_MIN_MS = 400


def _synthesize_parts(text, start_ms, end_ms):
    """Fake per-word timing for a line-synced lyric.

    Line-level sources give a start per line and nothing within it. The
    line still has a known span, though, so dividing that span across the
    line's own tokens — weighted by how long each one is — produces a
    highlight that advances through the line instead of the whole line
    lighting at once. It isn't real word timing and can drift inside a
    line, but it tracks the singing far better than a single step does.

    Returns ``[]`` when there's nothing sensible to sweep across.
    """
    if start_ms is None or end_ms is None:
        return []
    span = end_ms - start_ms
    if span < _SWEEP_MIN_MS:
        return []
    span = min(span, _SWEEP_MAX_MS)

    tokens = _sweep_tokens(text or "")
    if len(tokens) < 2:
        return []

    total = sum(len(t) for t, _ in tokens) or 1
    parts = []
    cursor = float(start_ms)
    for token, space_after in tokens:
        share = span * (len(token) / total)
        parts.append({
            "start_ms": int(cursor),
            "end_ms": int(cursor + share),
            "text": token,
            "space_after": space_after,
        })
        cursor += share
    return parts


def _normalize_parts(raw_parts):
    """``[{start, end, text, space_after}]`` -> millisecond ints, dropping
    anything without a start time (which we can't schedule)."""
    out = []
    for p in raw_parts or []:
        start = p.get("start")
        text = p.get("text")
        if not text or start is None:
            continue
        end = p.get("end")
        out.append({
            "start_ms": int(start * 1000),
            "end_ms": int((end if end is not None else start) * 1000),
            "text": text,
            # Sources that don't record inter-word spacing (older LRC-shaped
            # payloads) default to space-separated, which is how word-level
            # lines have always rendered.
            "space_after": p.get("space_after", True),
        })
    return out


class LyricRow(Gtk.ListBoxRow):
    """
    A single lyric line. Always rendered with Pango markup so swapping
    between active and inactive doesn't re-layout the label.
    """

    __gtype_name__ = "MixtapesLyricRow"

    def __init__(self, line, line_idx, second_line_mode="auto", 
                 effects=lyrics_prefs.EFFECTS_DEFAULT, sweep_end_ms=None,
                 sweep=True, active_scale=lyrics_prefs.ACTIVE_SCALE_DEFAULT):
        super().__init__()
        self.line_idx = line_idx
        self.is_static = line.get("start") is None
        self.start_ms = int((line.get("start") or 0.0) * 1000)
        self.text = line.get("text") or ""
        self._effects = effects
        self._second_line_mode = second_line_mode
        
        self._can_scale = (effects in _EFFECT_SCALE) and not self.is_static
        self._can_glow = (effects in _EFFECT_GLOW) and not self.is_static
        self._can_blur = (effects in _EFFECT_BLUR) and not self.is_static
        self._active_scale = active_scale
        
        base_lerp = _LERP_SPEED if effects == "off" else _LERP_SPEED_EFFECTS
        self._lerp = base_lerp * 0.6
        
        self.parts = _normalize_parts(line.get("parts"))
        self.swept = False
        
        if not self.parts and sweep and line.get("start") is not None:
            synthetic = _synthesize_parts(self.text, self.start_ms, sweep_end_ms)
            if synthetic:
                self.parts = synthetic
                self.swept = True
                
        self._assign_byte_offsets(self.text, self.parts)

        self.opposite_voice = line.get("align") == "end"
        
        align = Gtk.Align.END if self.opposite_voice else Gtk.Align.START
        justify = Gtk.Justification.RIGHT if self.opposite_voice else Gtk.Justification.LEFT
        xalign = 1.0 if self.opposite_voice else 0.0

        box = Gtk.Box(orientation=Gtk.Orientation.VERTICAL, spacing=2)
        self.set_child(box)

        self.label = Gtk.Label(
            wrap=True, 
            wrap_mode=Pango.WrapMode.WORD_CHAR,
            justify=justify, 
            halign=align, 
            valign=Gtk.Align.CENTER, 
            xalign=xalign,
        )
        self.label.add_css_class("lyrics-line-label")
        box.append(self.label)

        sub_text, sub_parts = _second_line_for(line, second_line_mode)
        self.sub_text = sub_text or ""
        self.sub_parts = _normalize_parts(sub_parts) if sub_parts else []
        
        is_bg = (second_line_mode == "background") or \
                (second_line_mode == "auto" and not line.get("romanization") and line.get("bg_text"))
                
        if not is_bg:
            self.sub_parts = []
        elif not self.sub_parts and self.sub_text and sweep and line.get("start") is not None:
            synthetic_sub = _synthesize_parts(self.sub_text, self.start_ms, sweep_end_ms)
            if synthetic_sub:
                self.sub_parts = synthetic_sub
                
        self._assign_byte_offsets(self.sub_text, self.sub_parts)

        self.sub_label = None
        if self.sub_text:
            self.sub_label = Gtk.Label(
                wrap=True, 
                wrap_mode=Pango.WrapMode.WORD_CHAR,
                justify=justify, 
                halign=align, 
                valign=Gtk.Align.CENTER, 
                xalign=xalign,
            )
            self.sub_label.add_css_class("lyrics-line-sub")
            box.append(self.sub_label)

        self.add_css_class("lyrics-line")
        self.set_selectable(True)
        self.set_activatable(True)
        self.set_can_focus(False)
        self.set_focusable(False)

        self._cursor_ms = -1
        self._internal_cursor_ms = -1
        self._wants_turn_off = False
        
        self._scale = 1.0
        self._scale_target = 1.0
        self._blur = 0.0
        self._blur_target = 0.0
        self._distance = 99

        n = len(self.parts) or 1
        n_sub = len(self.sub_parts) or 1
        
        self._word_alphas = [0.0] * n
        self._word_targets = [0.0] * n
        self._sub_alphas = [0.0] * n_sub
        self._sub_targets = [0.0] * n_sub

        self._recompute_targets()
        self._dirty = True
        self.add_tick_callback(self._on_tick)

    def _assign_byte_offsets(self, text, parts):
        if not text or not parts: 
            return
            
        text_bytes = text.encode('utf-8')
        offset = 0
        
        for p in parts:
            word_bytes = p["text"].encode('utf-8')
            idx = text_bytes.find(word_bytes, offset)
            
            if idx >= 0:
                p["byte_start"] = idx
                p["byte_end"] = idx + len(word_bytes)
                offset = p["byte_end"]
            else:
                p["byte_start"] = -1
                p["byte_end"] = -1

    def set_cursor_ms(self, ms):
        if ms == self._cursor_ms and ms != -1: 
            return
        
        if ms == -1:
            self._wants_turn_off = True
        else:
            self._wants_turn_off = False
            if self._cursor_ms < 0 or abs(self._cursor_ms - ms) > 1000:
                self._internal_cursor_ms = ms
            self._cursor_ms = ms
            
        self._recompute_effect_targets()
        self._recompute_targets()

    def set_distance(self, distance):
        if distance == self._distance: 
            return
        self._distance = distance
        self._recompute_effect_targets()

    def _fitting_scale(self):
        if not self._can_scale: 
            return 1.0
            
        layout = self.label.get_layout()
        
        if layout and layout.get_line_count() > 1:
            return 1.0
            
        width = self.get_width()
        if width <= 0: 
            return self._active_scale
        
        if layout:
            text_w = layout.get_pixel_size()[0]
            alloc = self.label.get_allocation()
            
            if not self.opposite_voice:
                reach = alloc.x + text_w
            else:
                reach = width - (alloc.x + alloc.width - text_w)
                
            if reach > 0:
                return max(1.0, min(self._active_scale, width / reach))
                
        return self._active_scale

    def _recompute_effect_targets(self):
        is_active = self._cursor_ms >= 0
        self._scale_target = self._fitting_scale() if (is_active and self._can_scale) else 1.0
        
        if not self._can_blur:
            self._blur_target = 0.0
            return
            
        if is_active or self._distance < _BLUR_START_DISTANCE:
            self._blur_target = 0.0
        else:
            self._blur_target = min(_BLUR_MAX, (self._distance - _BLUR_START_DISTANCE + 1) * _BLUR_PER_LINE)

    def _word_alpha_for(self, part):
        if self._cursor_ms < part["start_ms"]: 
            return 0.0
            
        if self._effects == "off": 
            return 1.0
            
        held = max(0, part["end_ms"] - part["start_ms"])
        fraction = _SWEEP_RAMP_FRACTION if self.swept else _WORD_RAMP_FRACTION
        ramp = min(_WORD_RAMP_MAX_MS, max(_WORD_RAMP_MIN_MS, held * fraction))
        
        return min(1.0, (self._cursor_ms - part["start_ms"]) / ramp)

    def _recompute_targets(self):
        is_active = self._cursor_ms >= 0
        
        if not self.parts:
            self._word_targets[0] = 1.0 if is_active else 0.0
        else:
            for i, part in enumerate(self.parts):
                self._word_targets[i] = self._word_alpha_for(part) if is_active else 0.0

        if self.sub_label is not None:
            if not self.sub_parts:
                self._sub_targets[0] = 1.0 if is_active else 0.0
            else:
                for i, part in enumerate(self.sub_parts):
                    self._sub_targets[i] = self._word_alpha_for(part) if is_active else 0.0
                    
        self._dirty = True

    def _get_sweep_state(self, cursor_ms, parts, layout):
        if not parts or not layout: 
            return -1, 0.0, Graphene.Rect().init(0, 0, 0, 0), None, 0.0, False
        
        sung_rect = Graphene.Rect().init(0, 0, 0, 0)
        
        MIN_GLOW_MS = 600
        MAX_GLOW_MS = 1000
        
        for i, p in enumerate(parts):
            b_start = p.get("byte_start", -1)
            b_end = p.get("byte_end", -1)
            if b_start < 0: 
                continue
            
            pos_start = layout.index_to_pos(b_start)
            pos_end = layout.index_to_pos(b_end)
            
            x_start = pos_start.x / Pango.SCALE
            y_start = pos_start.y / Pango.SCALE
            height = pos_start.height / Pango.SCALE
            x_end = pos_end.x / Pango.SCALE
            
            if x_end <= x_start:
                x_end = x_start + (pos_start.width / Pango.SCALE) * len(p["text"])
                
            if cursor_ms >= p["end_ms"]:
                sung_rect = Graphene.Rect().init(x_end, y_start, 0, height)
                
            elif cursor_ms >= p["start_ms"]:
                duration = max(1, p["end_ms"] - p["start_ms"])
                progress = (cursor_ms - p["start_ms"]) / duration
                
                sweep_x_rel = x_start + (x_end - x_start) * progress
                active_rect = Graphene.Rect().init(x_start, y_start, x_end - x_start, height)
                
                if duration >= MIN_GLOW_MS:
                    is_long = True
                    weight = min(1.0, (duration - MIN_GLOW_MS) / (MAX_GLOW_MS - MIN_GLOW_MS))
                    
                    smooth_curve = math.sin(progress * math.pi)
                    glow = smooth_curve * (2.0 * weight)
                else:
                    is_long = False
                    glow = 0.0
                    
                return i, sweep_x_rel, sung_rect, active_rect, glow, is_long
            else:
                break
                
        return -1, 0.0, sung_rect, None, 0.0, False

    def _create_isolated_word_layout(self, context, font_desc, text, progress, is_long):
        gl = Pango.Layout.new(context)
        gl.set_text(text)
        gl.set_font_description(font_desc)

        if is_long:
            attrs = Pango.AttrList.new()
            N = len(text)
            curr_byte = 0

            for char_idx, char in enumerate(text):
                char_bytes = char.encode('utf-8')
                char_len = len(char_bytes)

                delay = (char_idx / max(1, N)) * 0.5
                t = progress * 1.5 - delay
                t_clamped = max(0.0, min(1.0, t))
                wave_val = math.sin(t_clamped * math.pi) * 3500

                if wave_val > 5:
                    attr = Pango.attr_rise_new(int(wave_val))
                    attr.start_index = curr_byte
                    attr.end_index = curr_byte + char_len
                    attrs.insert(attr)

                curr_byte += char_len

            gl.set_attributes(attrs)
            
        return gl
        
    def _get_line_baseline(self, layout, byte_index):
        """Finds the exact baseline of the line containing the specified byte."""
        iter = layout.get_iter()
        while True:
            line = iter.get_line_readonly()
            if line is not None and line.start_index <= byte_index < line.start_index + line.length:
                return iter.get_baseline() / Pango.SCALE
            if not iter.next_line():
                break
        return layout.get_baseline() / Pango.SCALE

    def _update_alphas(self):
        changed = False
        is_active = self._cursor_ms >= 0

        if is_active and self._effects != "off":
            if self.parts:
                for i, p in enumerate(self.parts):
                    t = 1.0 if self._internal_cursor_ms > p["end_ms"] else 0.0
                    if abs(self._word_alphas[i] - t) > 0.005:
                        self._word_alphas[i] += (t - self._word_alphas[i]) * self._lerp
                        changed = True
            else:
                t = 1.0
                if abs(self._word_alphas[0] - t) > 0.005:
                    self._word_alphas[0] += (t - self._word_alphas[0]) * self._lerp
                    changed = True

            if self.sub_label:
                if self.sub_parts:
                    for i, p in enumerate(self.sub_parts):
                        t = 1.0 if self._internal_cursor_ms > p["end_ms"] else 0.0
                        if abs(self._sub_alphas[i] - t) > 0.005:
                            self._sub_alphas[i] += (t - self._sub_alphas[i]) * self._lerp
                            changed = True
                else:
                    t = 1.0
                    if abs(self._sub_alphas[0] - t) > 0.005:
                        self._sub_alphas[0] += (t - self._sub_alphas[0]) * self._lerp
                        changed = True

        else:
            for i in range(len(self.parts) if self.parts else 1):
                target = self._word_targets[i]
                if abs(self._word_alphas[i] - target) > 0.005:
                    self._word_alphas[i] += (target - self._word_alphas[i]) * self._lerp
                    changed = True
                    
            if self.sub_label:
                if not self.sub_parts:
                    target = self._sub_targets[0]
                    if abs(self._sub_alphas[0] - target) > 0.005:
                        self._sub_alphas[0] += (target - self._sub_alphas[0]) * self._lerp
                        changed = True
                else:
                    for i in range(len(self.sub_parts)):
                        target = self._sub_targets[i]
                        if abs(self._sub_alphas[i] - target) > 0.005:
                            self._sub_alphas[i] += (target - self._sub_alphas[i]) * self._lerp
                            changed = True

        if changed or getattr(self, "_dirty", False):
            self._render_markup()
            self._render_sub_markup()
            self._dirty = False

    def _on_tick(self, _widget, _frame_clock):
        current_time = GLib.get_monotonic_time() / 1000.0
        delta = current_time - getattr(self, "_last_tick_time", current_time)
        self._last_tick_time = current_time

        changed = False

        if self._wants_turn_off:
            end_bound = 0
            if self.parts: 
                end_bound = max(end_bound, self.parts[-1]["end_ms"])
            if self.sub_parts: 
                end_bound = max(end_bound, self.sub_parts[-1]["end_ms"])
            
            if delta > 250:
                self._internal_cursor_ms = end_bound + 1
            
            if self._internal_cursor_ms > end_bound or self._internal_cursor_ms < self.start_ms:
                self._cursor_ms = -1
                self._internal_cursor_ms = -1
                self._wants_turn_off = False
                self._recompute_effect_targets()
                self._recompute_targets()
                changed = True
            else:
                if delta < 100: 
                    self._internal_cursor_ms += delta
                changed = True
        else:
            if self._cursor_ms >= 0:
                if delta > 250:
                    self._internal_cursor_ms = self._cursor_ms
                elif self._internal_cursor_ms < self._cursor_ms + 250:
                    self._internal_cursor_ms += delta
                changed = True

        self._update_alphas()

        for attr, target_attr in (("_scale", "_scale_target"), ("_blur", "_blur_target")):
            cur = getattr(self, attr)
            target = getattr(self, target_attr)
            if abs(cur - target) > 0.001:
                setattr(self, attr, cur + (target - cur) * (_EFFECT_LERP * 0.6))
                changed = True

        if getattr(self, "_dirty", False):
            self.queue_draw()
        elif changed or self._cursor_ms >= 0:
            self.queue_draw()

        # Keep ticking — cheap when no alphas are in motion.
        return GLib.SOURCE_CONTINUE

    def _get_css_colors(self, label):
        ctx = label.get_style_context()
        c_in = ctx.get_color()
        
        ctx.save()
        ctx.add_class("active")
        c_act = ctx.get_color()
        
        ctx.remove_class("active")
        ctx.add_class("glow")
        c_glow = ctx.get_color()
        ctx.restore()
        
        return c_in, c_act, c_glow

    def _lerp_color(self, c1, c2, t):
        c = Gdk.RGBA()
        c.red = c1.red + (c2.red - c1.red) * t
        c.green = c1.green + (c2.green - c1.green) * t
        c.blue = c1.blue + (c2.blue - c1.blue) * t
        c.alpha = c1.alpha + (c2.alpha - c1.alpha) * t
        return c

    def _color_to_markup(self, color, text):
        r = int(color.red * 255)
        g = int(color.green * 255)
        b = int(color.blue * 255)
        a = max(1, int(color.alpha * 65535))
        return f"<span color='#{r:02x}{g:02x}{b:02x}' fgalpha='{a}'>{html.escape(text)}</span>"

    def _render_markup(self):
        c_in, c_act, _ = self._get_css_colors(self.label)
        
        if self._effects == "off":
            c = c_in if self._cursor_ms < 0 else c_act
            self.label.set_markup(self._color_to_markup(c, self.text))
            return

        if not self.parts:
            t = max(0.0, min(1.0, self._word_alphas[0]))
            c = self._lerp_color(c_in, c_act, t)
            self.label.set_markup(self._color_to_markup(c, self.text))
        else:
            chunks = []
            for i, part in enumerate(self.parts):
                t = max(0.0, min(1.0, self._word_alphas[i]))
                c = self._lerp_color(c_in, c_act, t)
                chunks.append(self._color_to_markup(c, part["text"]))
                if part.get("space_after") and i < len(self.parts) - 1:
                    chunks.append(" ")
                    
            self.label.set_markup("".join(chunks))

    def _render_sub_markup(self):
        if self.sub_label is None: 
            return
            
        c_in, c_act, _ = self._get_css_colors(self.sub_label)
        
        if self._effects == "off":
            c = c_in if self._cursor_ms < 0 else c_act
            self.sub_label.set_markup(self._color_to_markup(c, self.sub_text))
            return

        if not self.sub_parts:
            t = max(0.0, min(1.0, self._sub_alphas[0]))
            c = self._lerp_color(c_in, c_act, t)
            self.sub_label.set_markup(self._color_to_markup(c, self.sub_text))
        else:
            chunks = []
            for i, part in enumerate(self.sub_parts):
                t = max(0.0, min(1.0, self._sub_alphas[i]))
                c = self._lerp_color(c_in, c_act, t)
                chunks.append(self._color_to_markup(c, part["text"]))
                if part.get("space_after") and i < len(self.sub_parts) - 1:
                    chunks.append(" ")
                    
            self.sub_label.set_markup("".join(chunks))

    def _render_layer(self, snapshot, label, text, parts):
        layout = label.get_layout()
        alloc = label.get_allocation()
        row_w = self.get_width()
        
        if not layout or not parts: 
            return
        
        c_in, c_act, c_glow = self._get_css_colors(label)
        active_idx, sweep_x_rel, sung_rect, active_rect, glow_int, is_long = self._get_sweep_state(
            self._internal_cursor_ms, parts, layout
        )
        
        progress = 0.0
        if active_idx >= 0:
            p = parts[active_idx]
            duration = max(1, p["end_ms"] - p["start_ms"])
            progress = (self._internal_cursor_ms - p["start_ms"]) / duration
            
        layout_width, layout_height = layout.get_pixel_size()
        x_offset = alloc.x + (alloc.width - layout_width) * label.get_xalign()
        y_offset = alloc.y + (alloc.height - layout_height) * label.get_yalign()
        
        context = label.get_pango_context()

        base_layout = Pango.Layout.new(context)
        base_layout.set_text(text)
        base_layout.set_font_description(layout.get_font_description())
        base_layout.set_width(layout.get_width())
        base_layout.set_alignment(layout.get_alignment())
        base_layout.set_wrap(layout.get_wrap())

        clip_top = alloc.y - 60
        clip_bottom = alloc.height + 120

        def draw_clipped_base(x, y, w, h, color):
            if w <= 0 or h <= 0: return
            snapshot.push_clip(Graphene.Rect().init(x, y, w, h))
            snapshot.save()
            snapshot.translate(Graphene.Point().init(x_offset, y_offset))
            snapshot.append_layout(base_layout, color)
            snapshot.restore()
            snapshot.pop()

        if not active_rect:
            if self._internal_cursor_ms < parts[0]["start_ms"]:
                draw_clipped_base(0, clip_top, row_w, clip_bottom - clip_top, c_in)
            else:
                sx = x_offset + sung_rect.origin.x
                sy = y_offset + sung_rect.origin.y
                sh = sung_rect.size.height
                
                draw_clipped_base(0, clip_top, row_w, sy - clip_top, c_act)         
                draw_clipped_base(0, sy, sx, sh, c_act)                             
                draw_clipped_base(sx, sy, row_w - sx, sh, c_in)                     
                draw_clipped_base(0, sy + sh, row_w, clip_bottom - (sy + sh), c_in) 
            return

        # Absolute coordinates of the word
        ax = x_offset + active_rect.origin.x
        ay = y_offset + active_rect.origin.y
        aw = active_rect.size.width
        ah = active_rect.size.height
        sweep_abs_x = x_offset + sweep_x_rel

        draw_clipped_base(0, clip_top, row_w, ay - clip_top, c_act)
        draw_clipped_base(0, ay + ah, row_w, clip_bottom - (ay + ah), c_in)

        draw_clipped_base(0, ay, ax, ah, c_act)                         
        draw_clipped_base(ax + aw, ay, row_w - (ax + aw), ah, c_in)     

        active_text = parts[active_idx]["text"]
        iso_layout = self._create_isolated_word_layout(
            context, layout.get_font_description(), active_text, progress, is_long
        )

        active_byte_start = parts[active_idx].get("byte_start", 0)
        base_baseline = self._get_line_baseline(base_layout, active_byte_start)
        base_baseline_y = y_offset + base_baseline
        
        iso_ascent = iso_layout.get_baseline() / Pango.SCALE
        iso_draw_y = base_baseline_y - iso_ascent

        if self._can_glow and glow_int > 0:
            snapshot.save()
            snapshot.translate(Graphene.Point().init(ax, iso_draw_y))
            snapshot.push_blur(14.0 * glow_int)
            snapshot.append_layout(iso_layout, c_glow)
            snapshot.pop()
            snapshot.restore()

            snapshot.save()
            snapshot.translate(Graphene.Point().init(ax, iso_draw_y))
            snapshot.push_blur(4.0 * glow_int)
            snapshot.append_layout(iso_layout, c_in)
            snapshot.pop()
            snapshot.restore()

        clip_y = iso_draw_y - 20
        clip_h = ah + 40

        if sweep_abs_x < ax + aw:
            snapshot.push_clip(Graphene.Rect().init(max(ax, sweep_abs_x), clip_y, (ax + aw) - max(ax, sweep_abs_x), clip_h))
            snapshot.save()
            snapshot.translate(Graphene.Point().init(ax, iso_draw_y))
            snapshot.append_layout(iso_layout, c_in)
            snapshot.restore()
            snapshot.pop()

        if sweep_abs_x > ax:
            snapshot.push_clip(Graphene.Rect().init(ax, clip_y, sweep_abs_x - ax, clip_h))
            snapshot.save()
            snapshot.translate(Graphene.Point().init(ax, iso_draw_y))
            snapshot.append_layout(iso_layout, c_act)
            snapshot.restore()
            snapshot.pop()

    def do_snapshot(self, snapshot):
        scaling = abs(self._scale - 1.0) > 0.002
        blurring = self._blur > 0.02
        is_active = self._internal_cursor_ms >= 0 and self._effects != "off"

        if is_active:
            if self.parts: 
                self.label.set_opacity(0.0)
            if self.sub_label and self.sub_parts: 
                self.sub_label.set_opacity(0.0)
        else:
            self.label.set_opacity(1.0)
            if self.sub_label: 
                self.sub_label.set_opacity(1.0)

        if blurring: 
            snapshot.push_blur(self._blur)
            
        if scaling:
            height = self.get_height()
            anchor_x = float(self.get_width()) if self.opposite_voice else 0.0
            
            snapshot.save()
            snapshot.translate(Graphene.Point().init(anchor_x, height / 2.0))
            snapshot.scale(self._scale, self._scale)
            snapshot.translate(Graphene.Point().init(-anchor_x, -height / 2.0))

        Gtk.ListBoxRow.do_snapshot(self, snapshot)

        if is_active:
            try:
                if self.parts:
                    self._render_layer(snapshot, self.label, self.text, self.parts)
                if self.sub_label and self.sub_parts:
                    self._render_layer(snapshot, self.sub_label, self.sub_text, self.sub_parts)
            except Exception:
                pass

        if scaling: 
            snapshot.restore()
            
        if blurring: 
            snapshot.pop()


# An instrumental stretch shorter than this isn't worth marking — the
# gap between two lines of a normal verse is often 3-4 seconds.
_INTERLUDE_MIN_S = 5.0
# End the marker just before the vocal returns, so the dots clear as the
# singer comes back in rather than on the exact frame of the first word.
# Lifted from beautiful-lyrics, which does the same for the same reason.
_INTERLUDE_END_EARLY_S = 0.25
# Dots stop pulsing and hold full brightness for the last moment before
# the vocal comes back in, so the fill reads as a countdown.
_INTERLUDE_DOTS = 3
# Drawn, not typeset, so these are real pixels rather than a font size.
_DOT_RADIUS = 3.0
_DOT_SPACING = 11.0
# How much a dot grows as its own slice of the gap fills, and the smaller
# wave that keeps travelling along the row underneath that.
_DOT_FOCUS_SWELL = 0.55
_DOT_WAVE_SWELL = 0.14
_DOT_WAVE_PERIOD = 2.2
_DOT_MAX_SWELL = 1.0 + _DOT_FOCUS_SWELL + _DOT_WAVE_SWELL


def _find_interludes(lines):
    """Locate the instrumental stretches in a line list.

    Returns ``[(start_s, end_s), ...]``. Two shapes feed this: providers
    that stamp an empty line at the top of a break (LRCLIB does), and
    providers that just leave a hole in the timeline (Apple Music does,
    since its lines carry an explicit ``end``). Both end up as a gap
    between two lines that actually have words.
    """
    timed = [
        (i, l) for i, l in enumerate(lines)
        if l.get("start") is not None
    ]
    if not timed:
        return []
    sung = [(i, l) for i, l in timed if (l.get("text") or "").strip()]
    if not sung:
        return []

    out = []
    # A long lead-in before the first word is an interlude too.
    first_start = sung[0][1]["start"]
    if first_start >= _INTERLUDE_MIN_S:
        out.append((0.0, first_start - _INTERLUDE_END_EARLY_S))

    for (idx_a, a), (_idx_b, b) in zip(sung, sung[1:]):
        # Prefer the line's own end. Failing that, an empty marker line
        # between the two says where the singing actually stopped.
        gap_start = a.get("end")
        if gap_start is None:
            for j, l in timed:
                if j > idx_a and not (l.get("text") or "").strip():
                    gap_start = l["start"]
                    break
        if gap_start is None:
            continue
        gap_end = b["start"]
        if gap_end - gap_start >= _INTERLUDE_MIN_S:
            out.append((gap_start, gap_end - _INTERLUDE_END_EARLY_S))
    return out


class InterludeRow(Gtk.ListBoxRow):
    """The music-only marker shown during an instrumental stretch.

    Three dots that fill in turn across the gap, so the row doubles as a
    countdown to the next line instead of just saying "nothing here".
    They're drawn rather than typeset: a glyph can only be given an
    opacity through Pango, while drawing them means each dot can also
    swell as its turn comes round and ride a slow wave that travels along
    the row, which is what keeps a twenty-second break from looking
    frozen. Clicking seeks to the start of the break."""

    __gtype_name__ = "MixtapesLyricInterludeRow"

    def __init__(self, start_s, end_s, effects=lyrics_prefs.EFFECTS_DEFAULT):
        super().__init__()
        self.line_idx = -1
        self.start_ms = int(start_s * 1000)
        self.end_ms = int(end_s * 1000)
        self._effects = effects
        self._lerp = _LERP_SPEED if effects == "off" else _LERP_SPEED_EFFECTS

        # An empty box purely to claim the row's space; the dots are
        # painted over its allocation so they line up with the lyric text
        # column above and below.
        self._dots = Gtk.Box()
        self._dots.add_css_class("lyrics-interlude")
        self._dots.set_halign(Gtk.Align.START)
        self._dots.set_valign(Gtk.Align.CENTER)
        self._dots.set_size_request(
            int(_DOT_SPACING * (_INTERLUDE_DOTS - 1) + _DOT_RADIUS * 2),
            int(_DOT_RADIUS * 2 * _DOT_MAX_SWELL),
        )
        self.set_child(self._dots)

        self.add_css_class("lyrics-line")
        self.set_selectable(True)
        self.set_activatable(True)
        self.set_can_focus(False)
        self.set_focusable(False)

        self._alphas = [_ALPHA_FUTURE_WORD] * _INTERLUDE_DOTS
        self._targets = [_ALPHA_FUTURE_WORD] * _INTERLUDE_DOTS
        self._swell = [0.0] * _INTERLUDE_DOTS
        self._cursor_ms = -1
        self._t0 = time.monotonic()
        self.add_tick_callback(self._on_tick)

    def set_cursor_ms(self, ms):
        if ms == self._cursor_ms:
            return
        self._cursor_ms = ms
        self._recompute_targets()

    # Distance blur doesn't apply to the marker; it stays legible.
    def set_distance(self, distance):
        return

    def _progress(self):
        if self._cursor_ms < 0:
            return -1.0
        span = max(1, self.end_ms - self.start_ms)
        return min(1.0, max(0.0, (self._cursor_ms - self.start_ms) / span))

    def _recompute_targets(self):
        progress = self._progress()
        if progress < 0:
            self._targets = [_ALPHA_FUTURE_WORD] * _INTERLUDE_DOTS
            return
        for i in range(_INTERLUDE_DOTS):
            # Each dot owns its slice of the gap and fills across it.
            filled = min(1.0, max(0.0, (progress - i / _INTERLUDE_DOTS)
                                  * _INTERLUDE_DOTS))
            self._targets[i] = (
                _ALPHA_FUTURE_WORD
                + (_ALPHA_ACTIVE - _ALPHA_FUTURE_WORD) * filled
            )

    def _on_tick(self, _widget, _frame_clock):
        changed = False
        for i, target in enumerate(self._targets):
            cur = self._alphas[i]
            if abs(cur - target) > 0.002:
                self._alphas[i] = cur + (target - cur) * self._lerp
                changed = True

        progress = self._progress()
        if progress >= 0 and self._effects != "off":
            # A dot swells as its own slice fills, and a slow wave runs
            # along the row underneath that so the marker keeps moving
            # even while a single dot is holding.
            elapsed = time.monotonic() - self._t0
            for i in range(_INTERLUDE_DOTS):
                filled = min(1.0, max(0.0, (progress - i / _INTERLUDE_DOTS)
                                      * _INTERLUDE_DOTS))
                focus = 1.0 - abs(2.0 * filled - 1.0) if 0 < filled < 1 else 0.0
                wave = math.sin(
                    elapsed * (2 * math.pi / _DOT_WAVE_PERIOD) - i * 0.8
                )
                self._swell[i] = _DOT_FOCUS_SWELL * focus + _DOT_WAVE_SWELL * wave
            changed = True
        elif any(v for v in self._swell):
            self._swell = [0.0] * _INTERLUDE_DOTS
            changed = True

        if changed:
            self.queue_draw()
        return GLib.SOURCE_CONTINUE

    def do_snapshot(self, snapshot):
        Gtk.ListBoxRow.do_snapshot(self, snapshot)

        alloc = self._dots.get_allocation()
        if alloc.width <= 0:
            return
        color = self._dots.get_color()
        cy = alloc.y + alloc.height / 2.0

        for i in range(_INTERLUDE_DOTS):
            radius = _DOT_RADIUS * (1.0 + self._swell[i])
            cx = alloc.x + _DOT_RADIUS + i * _DOT_SPACING
            dot = Gdk.RGBA()
            dot.red, dot.green, dot.blue = color.red, color.green, color.blue
            dot.alpha = color.alpha * max(0.0, min(1.0, self._alphas[i]))

            rect = Graphene.Rect().init(
                cx - radius, cy - radius, radius * 2, radius * 2
            )
            rounded = Gsk.RoundedRect()
            rounded.init_from_rect(rect, radius)
            snapshot.push_rounded_clip(rounded)
            snapshot.append_color(dot, rect)
            snapshot.pop()


class LyricsView(Gtk.Box):
    """A column of lyrics for the currently-playing track."""

    _next_dbg_id = 0

    def __init__(self, player, **kwargs):
        super().__init__(orientation=Gtk.Orientation.VERTICAL, spacing=0, **kwargs)
        self.player = player
        self.add_css_class("lyrics-view")
        self.set_hexpand(True)
        self.set_vexpand(True)
        LyricsView._next_dbg_id += 1
        self._dbg_id = LyricsView._next_dbg_id
        if _DEBUG_LEVEL >= 1:
            _dlog(1, f"LyricsView #{self._dbg_id} init "
                     f"(debug level {_DEBUG_LEVEL})")
        # Re-activate the right line whenever this view becomes mapped, so
        # the queue/lyrics tab user just switched to picks up the current
        # playback position. While unmapped we drop scroll work to keep
        # the hidden duplicate (mobile expanded player vs desktop cover
        # view) from racing the visible one's autoscroll.
        self.connect("map", self._on_map)

        # Display prefs (second-line content, effect level). Cached on the
        # view because every row build reads them; refreshed whenever the
        # settings dialog reports a change.
        self._second_line_mode = lyrics_prefs.second_line_mode()
        self._effects = lyrics_prefs.effects_level()
        self._sweep = lyrics_prefs.line_sweep()
        self._active_scale = lyrics_prefs.active_scale()
        apply_font_scale()

        # Async fetch generation token — invalidates stale in-flight fetches.
        self._fetch_gen = 0
        self._current_video_id = None
        self._lines = []
        self._synced = False
        self._active_idx = -1
        # The row currently carrying a live cursor. This is NOT always
        # ``_active_idx``: while the view is unmapped, progression updates
        # _active_idx without lighting anything, and a rebuild drops every
        # row. Clearing the previous highlight has to follow the row that
        # was really lit, or two lines end up bright at once.
        self._lit_idx = -1
        # line index -> LyricRow, and the standalone instrumental markers.
        self._row_for_line = {}
        self._interlude_rows = []
        self._lit_interlude = None
        self._last_pos = 0.0

        # Suspend autoscroll for a short window after the user manually
        # scrolls so we don't fight them.
        import time as _time
        self._time = _time
        self._user_scrolled_at = 0.0
        self._user_scroll_pause = 4.0
        self._suppress_select_signal = False
        self._scroll_anim_source = 0
        # The line index our most-recent scroll request targets. If it
        # changes before a deferred do_scroll fires, that scroll is
        # superseded and bails out.
        self._scroll_target_idx = -1

        self.stack = Gtk.Stack()
        self.stack.set_transition_type(Gtk.StackTransitionType.CROSSFADE)
        self.stack.set_transition_duration(150)
        self.stack.set_hexpand(True)
        self.stack.set_vexpand(True)
        self._stack_overlay = Gtk.Overlay()
        self._stack_overlay.set_child(self.stack)
        self.append(self._stack_overlay)

        # --- Loading page ---
        loading_box = Gtk.Box(
            orientation=Gtk.Orientation.VERTICAL, spacing=12
        )
        loading_box.set_valign(Gtk.Align.CENTER)
        loading_box.set_halign(Gtk.Align.CENTER)
        loading_box.set_vexpand(True)
        spinner = Adw.Spinner()
        spinner.set_size_request(36, 36)
        loading_box.append(spinner)
        self.stack.add_named(loading_box, "loading")

        # --- Empty / no-lyrics page ---
        self.status_page = Adw.StatusPage()
        # ``emblem-music-symbolic`` doesn't ship with Adwaita; use the same
        # icon as the lyrics toggle so "no lyrics" reads as a dimmed
        # version of the affordance the user just clicked.
        self.status_page.set_icon_name("format-justify-fill-symbolic")
        self.status_page.set_title("No lyrics")
        self.status_page.set_description("No lyrics found for this track.")
        self.status_page.set_vexpand(True)
        self.stack.add_named(self.status_page, "empty")

        # --- Lyrics page ---
        self.scroller = ScrolledWindow()
        self.scroller.set_policy(Gtk.PolicyType.NEVER, Gtk.PolicyType.AUTOMATIC)
        self.scroller.set_hexpand(True)
        self.scroller.set_vexpand(True)
        self.scroller.add_css_class("lyrics-scroller")

        # Gtk.ListBox: each row is a LyricRow. Selecting a row drives both
        # the highlight (via :selected style) and the autoscroll.
        self.lrc_list = Gtk.ListBox()
        self.lrc_list.set_selection_mode(Gtk.SelectionMode.SINGLE)
        self.lrc_list.add_css_class("lyrics-list")
        # Small top margin (just enough to keep the first line out of the
        # short top-edge fade band) and a generous bottom one so the last
        # few lines can still scroll into the viewport center.
        self.lrc_list.set_margin_top(32)
        self.lrc_list.set_margin_bottom(400)
        self.lrc_list.set_margin_start(16)
        self.lrc_list.set_margin_end(16)
        self.lrc_list.connect("row-selected", self._on_row_selected)
        self.lrc_list.connect("row-activated", self._on_row_activated)

        # Wider clamp than the typical Adw.Clamp default so the lyrics
        # column actually uses the sidebar room when it's available.
        # Tightening kicks in earlier so the column compresses gradually
        # rather than snapping at the max.
        clamp = Adw.Clamp()
        clamp.set_maximum_size(820)
        clamp.set_tightening_threshold(640)
        clamp.set_child(self.lrc_list)
        self.scroller.set_child(clamp)

        # Asymmetric fade: barely-there at the top so the first line of
        # lyrics stays fully readable, generous at the bottom for the
        # scroll-out effect against the player bar / chrome below.
        fade = FadeEdgesBin(fade_top_px=20, fade_bottom_px=80)
        fade.set_orientation(Gtk.Orientation.VERTICAL)
        fade.set_hexpand(True)
        fade.set_vexpand(True)
        fade.append(self.scroller)

        # Floating source-picker button in the top-right corner. Opens
        # a popover listing every provider's result for the current
        # track so the user can switch sources when the chain-picked
        # default has bad timing or a wrong-language version.
        self._lyrics_page_overlay = Gtk.Box()
        self._lyrics_page_overlay.append(fade)

        # Small floating menu button. Just an icon — the current source
        # is shown as a checkmark inside the popover. The button needs to
        # work over busy backgrounds (album art on desktop, dark window
        # bg on mobile), so it carries a soft translucent surface.
        self._source_picker_btn = Gtk.MenuButton()
        self._source_picker_btn.set_icon_name("view-more-symbolic")
        self._source_picker_btn.set_tooltip_text("Choose lyrics source")
        # ``osd`` + ``circular`` is the HIG treatment for a control
        # floating over content, and it comes with a full-size hit
        # target. The previous hand-rolled disc zeroed the inner button's
        # padding and min-width/min-height, which shrank the clickable
        # area to roughly the icon itself and made the button feel like
        # it was ignoring clicks.
        self._source_picker_btn.add_css_class("osd")
        self._source_picker_btn.add_css_class("circular")
        self._source_picker_btn.add_css_class("lyrics-osd-btn")
        self._source_picker_btn.set_halign(Gtk.Align.END)
        self._source_picker_btn.set_valign(Gtk.Align.START)
        self._source_picker_btn.set_margin_top(10)
        self._source_picker_btn.set_margin_end(10)
        self._source_picker_btn.set_visible(False)
        # No internal label — kept for compatibility with helpers that
        # update the visible button copy on source change.
        self._source_picker_label = None

        self._source_picker_popover = self._build_source_picker_popover()
        self._source_picker_btn.set_popover(self._source_picker_popover)
        self._source_picker_btn.connect(
            "notify::active", self._on_source_picker_toggled,
        )
        self.stack.add_named(self._lyrics_page_overlay, "lyrics")

        # The button overlays the whole stack, not just the lyrics page,
        # so it stays reachable when a track has no lyrics — that's when
        # switching provider or typing a query actually matters.
        self._stack_overlay.add_overlay(self._source_picker_btn)

        # Detect manual scrolling so autoscroll pauses while the user is
        # interacting with the view. Only the scroll controller — the
        # earlier GestureDrag fired false positives on incidental
        # mouse-button movement inside the lyrics area, suspending the
        # autoscroll for several seconds at random.
        scroll_ctl = Gtk.EventControllerScroll.new(
            Gtk.EventControllerScrollFlags.VERTICAL
        )
        scroll_ctl.connect("scroll", self._on_user_scroll)
        self.scroller.add_controller(scroll_ctl)

        self.stack.set_visible_child_name("empty")

        self.player.connect("metadata-changed", self._on_metadata_changed)
        self.player.connect("progression", self._on_progression)
        self.player.connect("state-changed", self._on_state_changed)

        if self.player.current_video_id:
            self._refresh_for_current_track()

    def _log(self, level, msg):
        _dlog(level, f"#{self._dbg_id} {msg}")

    # ── Source picker ─────────────────────────────────────────────────────

    def _build_source_picker_popover(self):
        pop = Gtk.Popover()
        pop.set_position(Gtk.PositionType.BOTTOM)
        pop.set_size_request(200, -1)

        outer = Gtk.Box(orientation=Gtk.Orientation.VERTICAL, spacing=6)
        # No margins on the container — the popover already provides its
        # own chrome.

        header = Gtk.Label(label="Lyrics source")
        header.add_css_class("heading")
        header.set_halign(Gtk.Align.START)
        header.set_margin_start(10)
        header.set_margin_top(4)
        header.set_margin_bottom(2)
        outer.append(header)

        # Mirrors the add-to-playlist popover: ``navigation-sidebar`` for
        # the row-hover effect without the ``boxed-list`` border/shadow
        # that was making each entry look like its own card.
        self._source_picker_list = Gtk.ListBox()
        self._source_picker_list.set_selection_mode(Gtk.SelectionMode.NONE)
        self._source_picker_list.add_css_class("navigation-sidebar")
        self._source_picker_list.add_css_class("lyrics-source-list")
        self._source_picker_list.connect("row-activated", self._on_source_row_activated)
        outer.append(self._source_picker_list)

        self._source_picker_spinner_row = Gtk.ListBoxRow()
        self._source_picker_spinner_row.set_selectable(False)
        self._source_picker_spinner_row.set_activatable(False)
        sb = Gtk.Box(orientation=Gtk.Orientation.HORIZONTAL, spacing=8)
        sb.set_margin_top(6); sb.set_margin_bottom(6)
        sb.set_margin_start(8); sb.set_margin_end(8)
        sb.set_halign(Gtk.Align.CENTER)
        spinner = Adw.Spinner()
        spinner.set_size_request(16, 16)
        sb.append(spinner)
        lab = Gtk.Label(label="Searching…")
        lab.add_css_class("dim-label")
        sb.append(lab)
        self._source_picker_spinner_row.set_child(sb)

        # ── Second line ───────────────────────────────────────────────
        # Which second-line kinds exist is a property of the track, not of
        # the app, so the choice belongs next to the lyrics rather than
        # only in Settings. Only the kinds this track actually has are
        # offered; the section hides itself when there are none.
        self._matches_source = None
        self._second_line_section = Gtk.Box(
            orientation=Gtk.Orientation.VERTICAL, spacing=6,
        )
        self._second_line_section.append(
            Gtk.Separator(orientation=Gtk.Orientation.HORIZONTAL)
        )
        sl_header = Gtk.Label(label="Second line")
        sl_header.add_css_class("heading")
        sl_header.set_halign(Gtk.Align.START)
        sl_header.set_margin_start(10)
        sl_header.set_margin_bottom(2)
        self._second_line_section.append(sl_header)

        self._second_line_list = Gtk.ListBox()
        self._second_line_list.set_selection_mode(Gtk.SelectionMode.NONE)
        self._second_line_list.add_css_class("navigation-sidebar")
        self._second_line_list.add_css_class("lyrics-source-list")
        self._second_line_list.connect(
            "row-activated", self._on_second_line_row_activated,
        )
        self._second_line_section.append(self._second_line_list)
        self._second_line_section.set_visible(False)
        outer.append(self._second_line_section)

        # Second page: every match one provider has for this track. The
        # chain picks one and the gates try to make it the right one, but
        # catalogs carry re-records and same-title songs that no rule
        # separates reliably, so this is the escape hatch.
        self._picker_stack = Gtk.Stack()
        self._picker_stack.set_transition_type(
            Gtk.StackTransitionType.SLIDE_LEFT_RIGHT
        )
        self._picker_stack.set_transition_duration(150)
        self._picker_stack.add_named(outer, "sources")
        self._picker_stack.add_named(self._build_matches_page(), "matches")
        self._picker_stack.add_named(self._build_search_page(), "search")
        self._picker_stack.set_visible_child_name("sources")

        pop.set_child(self._picker_stack)
        pop.connect("closed", lambda *_: self._show_picker_page("sources"))
        return pop

    # The two pages want different widths: the source list is a handful of
    # short provider names, while a match list has to keep "[A Cappella]"
    # distinguishable from "[Slowed]" — truncating those to the same
    # prefix would defeat the point of showing them.
    _PICKER_WIDTHS = {"sources": 200, "matches": 340, "search": 340}

    def _show_picker_page(self, name):
        self._source_picker_popover.set_size_request(
            self._PICKER_WIDTHS.get(name, 200), -1
        )
        self._picker_stack.set_visible_child_name(name)

    def _build_search_page(self):
        box = Gtk.Box(orientation=Gtk.Orientation.VERTICAL, spacing=6)

        header = Gtk.Box(orientation=Gtk.Orientation.HORIZONTAL, spacing=4)
        back = Gtk.Button(icon_name="go-previous-symbolic")
        back.add_css_class("flat")
        back.set_tooltip_text("Back to sources")
        back.connect("clicked", lambda *_: self._show_picker_page("sources"))
        header.append(back)
        title = Gtk.Label(label="Search by name")
        title.add_css_class("heading")
        title.set_halign(Gtk.Align.START)
        header.append(title)
        box.append(header)

        self._search_entry = Gtk.SearchEntry()
        self._search_entry.set_placeholder_text("Song title")
        self._search_entry.set_margin_start(4)
        self._search_entry.set_margin_end(4)
        self._search_entry.connect("activate", self._on_manual_search)
        self._search_entry.connect("search-changed", lambda *_: None)
        box.append(self._search_entry)

        self._search_list = Gtk.ListBox()
        self._search_list.set_selection_mode(Gtk.SelectionMode.NONE)
        self._search_list.add_css_class("navigation-sidebar")
        self._search_list.add_css_class("lyrics-source-list")
        self._search_list.connect("row-activated", self._on_match_row_activated)
        scroller = ScrolledWindow()
        scroller.set_policy(Gtk.PolicyType.NEVER, Gtk.PolicyType.AUTOMATIC)
        scroller.set_propagate_natural_height(True)
        scroller.set_max_content_height(280)
        scroller.set_child(self._search_list)
        box.append(scroller)
        return box

    def _open_search(self):
        # Seed with the track's title so the common case is a small edit
        # (deleting the bracketed credits) rather than typing it out.
        title, _artist, _dur = self._track_metadata(self._current_video_id)
        if title and not self._search_entry.get_text():
            self._search_entry.set_text(title)
        self._show_picker_page("search")
        self._search_entry.grab_focus()

    def _on_manual_search(self, entry):
        query = entry.get_text().strip()
        if not query:
            return
        self._clear_list(self._search_list)
        self._search_list.append(self._loading_row("Searching…"))
        self._matches_source = None
        gen = self._fetch_gen
        _title, artist, duration = self._track_metadata(self._current_video_id)

        def _worker():
            try:
                matches = self.player.client.search_lyrics_manually(
                    query, artist, duration,
                )
            except Exception as e:
                self._log(1, f"manual search failed: {e}")
                matches = []
            GLib.idle_add(self._show_search_results, gen, matches)

        threading.Thread(target=_worker, daemon=True).start()

    def _show_search_results(self, gen, matches):
        if gen != self._fetch_gen:
            return False
        self._clear_list(self._search_list)
        if not matches:
            self._search_list.append(self._message_row("Nothing found"))
            return False
        for match in matches:
            self._search_list.append(
                self._build_match_row(match, source=match.get("source"))
            )
        return False

    @staticmethod
    def _clear_list(listbox):
        child = listbox.get_first_child()
        while child is not None:
            nxt = child.get_next_sibling()
            listbox.remove(child)
            child = nxt

    @staticmethod
    def _loading_row(text):
        row = Gtk.ListBoxRow()
        row.set_activatable(False)
        box = Gtk.Box(orientation=Gtk.Orientation.HORIZONTAL, spacing=8)
        box.set_halign(Gtk.Align.CENTER)
        box.set_margin_top(8); box.set_margin_bottom(8)
        spinner = Adw.Spinner(); spinner.set_size_request(16, 16)
        box.append(spinner)
        box.append(Gtk.Label(label=text, css_classes=["dim-label"]))
        row.set_child(box)
        return row

    @staticmethod
    def _message_row(text):
        row = Gtk.ListBoxRow()
        row.set_activatable(False)
        label = Gtk.Label(label=text)
        label.add_css_class("dim-label")
        label.set_margin_top(8); label.set_margin_bottom(8)
        row.set_child(label)
        return row

    def _build_match_row(self, match, source=None):
        row = Gtk.ListBoxRow()
        row.set_activatable(True)
        row._match_result = match["result"]
        row._match_source = source or match.get("source")
        text = Gtk.Box(orientation=Gtk.Orientation.VERTICAL, spacing=0)
        name = Gtk.Label(label=match["label"])
        name.set_halign(Gtk.Align.START)
        name.set_ellipsize(Pango.EllipsizeMode.END)
        text.append(name)
        bits = []
        if source:
            bits.append(source)
        if match.get("detail"):
            bits.append(match["detail"])
        lines = match["result"].get("lines") or []
        if any(l.get("parts") for l in lines):
            bits.append("word by word")
        elif match["result"].get("synced"):
            bits.append("line by line")
        else:
            bits.append("no timing")
        detail = Gtk.Label(label=" · ".join(bits))
        detail.set_halign(Gtk.Align.START)
        detail.add_css_class("dim-label")
        detail.add_css_class("caption")
        detail.set_ellipsize(Pango.EllipsizeMode.END)
        text.append(detail)
        row.set_child(text)
        return row

    def _build_matches_page(self):
        box = Gtk.Box(orientation=Gtk.Orientation.VERTICAL, spacing=6)

        header = Gtk.Box(orientation=Gtk.Orientation.HORIZONTAL, spacing=4)
        back = Gtk.Button(icon_name="go-previous-symbolic")
        back.add_css_class("flat")
        back.set_tooltip_text("Back to sources")
        back.connect(
            "clicked",
            lambda *_: self._show_picker_page("sources"),
        )
        header.append(back)
        self._matches_title = Gtk.Label()
        self._matches_title.add_css_class("heading")
        self._matches_title.set_halign(Gtk.Align.START)
        self._matches_title.set_ellipsize(Pango.EllipsizeMode.END)
        header.append(self._matches_title)
        box.append(header)

        self._matches_list = Gtk.ListBox()
        self._matches_list.set_selection_mode(Gtk.SelectionMode.NONE)
        self._matches_list.add_css_class("navigation-sidebar")
        self._matches_list.add_css_class("lyrics-source-list")
        self._matches_list.connect("row-activated", self._on_match_row_activated)

        # Several matches per provider is normal, so this scrolls rather
        # than growing the popover past the window.
        scroller = ScrolledWindow()
        scroller.set_policy(Gtk.PolicyType.NEVER, Gtk.PolicyType.AUTOMATIC)
        scroller.set_propagate_natural_height(True)
        scroller.set_max_content_height(300)
        scroller.set_child(self._matches_list)
        box.append(scroller)
        return box

    # Label for each second-line mode, in the order they're offered.
    _SECOND_LINE_LABELS = [
        ("off", "Off"),
        ("auto", "Auto"),
        ("romanization", "Romanization"),
        ("translation", "Translation"),
        ("background", "Background vocals"),
    ]

    def _available_second_lines(self):
        """The second-line modes this track's current lyrics can actually
        fill. ``off`` and ``auto`` are always offered once anything else
        is; a mode with no data would just render a blank line."""
        if not self._lines:
            return []
        have = set()
        for line in self._lines:
            if (line.get("romanization") or "").strip():
                have.add("romanization")
            if (line.get("translation") or "").strip():
                have.add("translation")
            if (line.get("bg_text") or "").strip():
                have.add("background")
        if not have:
            return []
        return [
            key for key, _label in self._SECOND_LINE_LABELS
            if key in ("off", "auto") or key in have
        ]

    def _refresh_second_line_rows(self):
        child = self._second_line_list.get_first_child()
        while child is not None:
            nxt = child.get_next_sibling()
            self._second_line_list.remove(child)
            child = nxt

        available = self._available_second_lines()
        self._second_line_section.set_visible(bool(available))
        if not available:
            return

        labels = dict(self._SECOND_LINE_LABELS)
        for key in available:
            row = Gtk.ListBoxRow()
            row.set_activatable(True)
            row._second_line_mode = key
            box = Gtk.Box(orientation=Gtk.Orientation.HORIZONTAL, spacing=12)
            label = Gtk.Label(label=labels[key])
            label.set_halign(Gtk.Align.START)
            label.set_hexpand(True)
            box.append(label)
            check = Gtk.Image.new_from_icon_name("object-select-symbolic")
            check.set_valign(Gtk.Align.CENTER)
            check.set_opacity(1.0 if key == self._second_line_mode else 0.0)
            box.append(check)
            row.set_child(box)
            self._second_line_list.append(row)

    def _open_matches(self, source):
        """Show every match ``source`` has for the current track."""
        self._matches_title.set_label(source)
        self._matches_source = source
        self._clear_list(self._matches_list)
        self._matches_list.append(self._loading_row("Searching…"))
        self._show_picker_page("matches")

        vid = self._current_video_id
        title, artist, duration = self._track_metadata(vid)
        gen = self._fetch_gen

        def _worker():
            try:
                matches = self.player.client.fetch_provider_matches(
                    source, title, artist, duration,
                )
            except Exception as e:
                self._log(1, f"match list failed: {e}")
                matches = []
            GLib.idle_add(self._show_matches, gen, source, matches)

        threading.Thread(target=_worker, daemon=True).start()

    def _show_matches(self, gen, source, matches):
        # A track change while we were searching invalidates the list.
        if gen != self._fetch_gen or self._matches_source != source:
            return False
        self._clear_list(self._matches_list)
        if not matches:
            self._matches_list.append(
                self._message_row(f"No other matches from {source}")
            )
            return False
        for match in matches:
            self._matches_list.append(self._build_match_row(match))
        return False

    def _on_match_row_activated(self, listbox, row):
        result = getattr(row, "_match_result", None)
        source = getattr(row, "_match_source", None) or self._matches_source
        if not result or not source or not self._current_video_id:
            return
        client = self.player.client
        # Give it the same second line the automatic pick would have had.
        title, artist, duration = self._track_metadata(self._current_video_id)
        try:
            result = client.augment_result(result, title, artist, duration)
        except Exception as e:
            self._log(1, f"augment failed: {e}")
        # Store it as this provider's result and pin it, so the choice
        # survives the next play of the track.
        client._lyrics_cache.add_result(
            self._current_video_id, result, user_choice=True
        )
        client.set_preferred_lyrics_source(self._current_video_id, source)
        self._switch_to_source(source, result)
        self._source_picker_btn.set_active(False)

    def _on_second_line_row_activated(self, listbox, row):
        mode = getattr(row, "_second_line_mode", None)
        if not mode or mode == self._second_line_mode:
            self._source_picker_btn.set_active(False)
            return
        lyrics_prefs.set_second_line_mode(mode)
        # Both LyricsView instances read the same pref, so push it through
        # the window rather than only rebuilding this one.
        window = self.get_root()
        if window is not None and hasattr(window, "_apply_lyrics_display_prefs"):
            window._apply_lyrics_display_prefs()
        else:
            self.apply_display_prefs()
        self._source_picker_btn.set_active(False)

    def _on_source_picker_toggled(self, btn, *_):
        if not btn.get_active():
            return
        # Repopulate every time the picker opens so the rows reflect the
        # current cache state (in case the user changed tracks).
        self._refresh_source_picker_rows(include_spinner=True)
        self._refresh_second_line_rows()
        if not self._current_video_id:
            return
        # Fire any uncached provider in the background; the callback adds
        # rows as results arrive.
        title, artist, duration = self._track_metadata(self._current_video_id)

        def _on_alt(source, result):
            GLib.idle_add(self._on_alt_result, source, result)

        try:
            self.player.client.fetch_lyrics_alternatives_async(
                self._current_video_id, title, artist, duration, _on_alt,
            )
        except Exception as e:
            self._log(1, f"alternatives fetch failed to start: {e}")

    def _refresh_source_picker_rows(self, include_spinner):
        # Clear out existing rows.
        child = self._source_picker_list.get_first_child()
        while child is not None:
            nxt = child.get_next_sibling()
            self._source_picker_list.remove(child)
            child = nxt

        if not self._current_video_id:
            return

        client = self.player.client
        alts = client.get_lyrics_alternatives(self._current_video_id) or []
        preferred = client.get_preferred_lyrics_source(self._current_video_id)
        active_source = self._active_source_name()

        # Order the rows the way the user ordered their queue in Settings.
        # Providers they've disabled still appear (a cached result from one
        # is fine to switch to by hand) but sort to the bottom.
        queue = lyrics_prefs.full_provider_order()
        disabled = lyrics_prefs.disabled_providers()

        def _rank(item):
            name = item[0]
            pos = queue.index(name) if name in queue else len(queue)
            return (name in disabled, pos, name)

        alts = sorted(alts, key=_rank)

        for source, result in alts:
            self._source_picker_list.append(
                self._build_source_row(
                    source, result,
                    is_active=(source == active_source),
                    is_preferred=(source == preferred),
                )
            )

        if include_spinner:
            self._source_picker_list.append(self._source_picker_spinner_row)

        # Always offered, and the only route in when nothing matched at
        # all: some titles carry so much decoration that no automatic
        # query finds the song.
        search_row = Gtk.ListBoxRow()
        search_row.set_activatable(True)
        search_row._open_search = True
        sbox = Gtk.Box(orientation=Gtk.Orientation.HORIZONTAL, spacing=8)
        sbox.append(Gtk.Image.new_from_icon_name("system-search-symbolic"))
        slabel = Gtk.Label(label="Search by name…")
        slabel.set_halign(Gtk.Align.START)
        slabel.set_hexpand(True)
        sbox.append(slabel)
        search_row.set_child(sbox)
        self._source_picker_list.append(search_row)

        # Only offered once something is pinned — otherwise there'd be a
        # permanent "undo" for a choice nobody made.
        if preferred:
            reset_row = Gtk.ListBoxRow()
            reset_row.set_activatable(True)
            reset_row._reset_choice = True
            rbox = Gtk.Box(orientation=Gtk.Orientation.VERTICAL, spacing=0)
            rtitle = Gtk.Label(label="Use automatic choice")
            rtitle.set_halign(Gtk.Align.START)
            rbox.append(rtitle)
            rsub = Gtk.Label(label=f"Pinned to {preferred}")
            rsub.set_halign(Gtk.Align.START)
            rsub.add_css_class("dim-label")
            rsub.add_css_class("caption")
            rsub.set_ellipsize(Pango.EllipsizeMode.END)
            rbox.append(rsub)
            reset_row.set_child(rbox)
            self._source_picker_list.append(reset_row)

    def _build_source_row(self, source, result, is_active, is_preferred):
        """One row of the source picker.

        Provider name with what its timing is worth underneath, and a
        checkmark for the one being shown. The subtitle used to lead with
        a line count, which is the one fact that can't help you choose
        (every source has roughly the same number of lines — they're the
        same song), and described timing as "word-level" / "plain", which
        names the data format rather than what you'd see."""
        row = Gtk.ListBoxRow()
        row.set_activatable(True)
        # Tag the row with its source name so the activation handler
        # knows what to switch to.
        row._lyrics_source = source

        box = Gtk.Box(orientation=Gtk.Orientation.HORIZONTAL, spacing=12)

        text = Gtk.Box(orientation=Gtk.Orientation.VERTICAL, spacing=0)
        text.set_hexpand(True)
        text.set_valign(Gtk.Align.CENTER)

        name = Gtk.Label(label=source)
        name.set_halign(Gtk.Align.START)
        name.set_ellipsize(Pango.EllipsizeMode.END)
        text.append(name)

        lines = result.get("lines") or []
        if any(l.get("parts") for l in lines):
            timing = "Word by word"
        elif result.get("synced"):
            timing = "Line by line"
        else:
            timing = "No timing"
        tier = Gtk.Label(label=timing)
        tier.add_css_class("dim-label")
        tier.add_css_class("caption")
        tier.set_halign(Gtk.Align.START)
        text.append(tier)

        box.append(text)

        # The checkmark is always in the box, transparent when inactive,
        # so switching source doesn't shift every other row sideways.
        check = Gtk.Image.new_from_icon_name("object-select-symbolic")
        check.set_valign(Gtk.Align.CENTER)
        check.set_opacity(1.0 if is_active else 0.0)
        box.append(check)

        client = self.player.client
        if getattr(client, "provider_supports_matches", lambda _s: False)(source):
            more = Gtk.Button(icon_name="go-next-symbolic")
            more.add_css_class("flat")
            more.set_valign(Gtk.Align.CENTER)
            more.set_tooltip_text(f"Other matches from {source}")
            more.connect("clicked", lambda _b, s=source: self._open_matches(s))
            box.append(more)

        if is_preferred:
            row.set_tooltip_text(f"Pinned to {source} for this track")

        row.set_child(box)
        return row

    def _on_alt_result(self, source, result):
        # A provider finished. Refresh the popover rows so the new
        # source shows up (or the spinner can be hidden when all done).
        if not self._source_picker_btn.get_active():
            return False
        # Determine whether all known providers have reported.
        client = self.player.client
        cached_sources = {s for s, _ in client.get_lyrics_alternatives(
            self._current_video_id or "")}
        all_done = all(
            name in cached_sources or name == source
            for name in client._LYRIC_PROVIDERS
        )
        self._refresh_source_picker_rows(include_spinner=not all_done)
        return False

    def _on_source_row_activated(self, listbox, row):
        if getattr(row, "_open_search", False):
            self._open_search()
            return
        if getattr(row, "_reset_choice", False):
            if self._current_video_id:
                self.player.client.clear_lyrics_preference(self._current_video_id)
                self._refresh_for_current_track(force=True)
            self._source_picker_btn.set_active(False)
            return
        source = getattr(row, "_lyrics_source", None)
        if not source or not self._current_video_id:
            return
        client = self.player.client
        # Pin the choice so future loads of this track return the same
        # source.
        client.set_preferred_lyrics_source(self._current_video_id, source)
        # Switch the currently-displayed lyrics to that source's data.
        alts = dict(client.get_lyrics_alternatives(self._current_video_id))
        data = alts.get(source)
        if data:
            self._switch_to_source(source, data)
        # Close the popover and update its checkmark for next open.
        self._source_picker_btn.set_active(False)

    def _switch_to_source(self, source, data):
        """Re-render the lyrics view with a different provider's data
        for the current track."""
        self._log(1, f"switching to source={source}")
        self._lines = data.get("lines") or []
        self._synced = bool(data.get("synced"))
        self._current_source = source
        self._active_idx = -1
        self._build_rows()
        self.stack.set_visible_child_name("lyrics")
        self._update_source_picker_label()
        self._refresh_second_line_rows()
        if self._synced and getattr(self.player, "duration", 0) > 0:
            idx = self._index_for_position(self._last_pos)
            self._activate_row(idx, cursor_ms=int(self._last_pos * 1000))

    def _active_source_name(self):
        return getattr(self, "_current_source", None)

    def _update_source_picker_label(self):
        # The picker is icon-only now — the active source is shown via
        # the checkmark inside the popover. Kept as a no-op so callers
        # don't need to check whether a label exists.
        name = self._active_source_name()
        if name:
            self._source_picker_btn.set_tooltip_text(f"Lyrics source: {name}")

    # ── Public API ─────────────────────────────────────────────────────────

    def refresh(self):
        """Drop this track's cached lyrics and fetch again from scratch."""
        if self._current_video_id:
            cache = getattr(self.player.client, "_lyrics_cache", None)
            if cache is not None:
                cache.invalidate(self._current_video_id)
            self._refresh_for_current_track(force=True)

    def apply_display_prefs(self):
        """Re-read the second-line and effects prefs and rebuild the rows
        in place. Called by the settings dialog — no refetch, the line
        data already carries every second-line variant the provider had."""
        lyrics_prefs.invalidate()
        apply_font_scale()
        second_line = lyrics_prefs.second_line_mode()
        effects = lyrics_prefs.effects_level()
        sweep = lyrics_prefs.line_sweep()
        active_scale = lyrics_prefs.active_scale()
        if (second_line == self._second_line_mode
                and effects == self._effects
                and sweep == self._sweep
                and active_scale == self._active_scale):
            return
        self._second_line_mode = second_line
        self._effects = effects
        self._sweep = sweep
        self._active_scale = active_scale
        if not self._lines:
            return
        active = self._active_idx
        self._active_idx = -1
        self._build_rows()
        if self._synced:
            self._activate_row(
                active if active >= 0 else self._index_for_position(self._last_pos),
                cursor_ms=int(self._last_pos * 1000),
            )

    # ── Player signal handlers ─────────────────────────────────────────────

    def _on_metadata_changed(self, player, title, artist, thumb, video_id, like):
        if video_id == self._current_video_id and self._lines:
            return
        self._log(1, f"metadata-changed: video_id={video_id!r} title={title!r}")
        self._current_video_id = video_id or None
        self._refresh_for_current_track()

    def _on_progression(self, player, pos, dur):
        self._last_pos = pos
        if not self._synced or not self._lines:
            return
        # Two LyricsView instances exist (mobile expanded player +
        # desktop cover view). Only the mapped one should drive scrolls
        # so they don't race each other and double up signal handlers.
        if not self.get_mapped():
            # Still record the active idx so the next progression after
            # we become visible doesn't re-trigger a stale activation.
            new_idx = self._index_for_position(pos)
            self._active_idx = new_idx
            return
        idx = self._index_for_position(pos)
        ms = int(pos * 1000)

        # An instrumental stretch takes over the view: the dots fill and
        # the last sung line dims, instead of the line before the break
        # sitting there lit for ten seconds as if it were still playing.
        interlude = self._interlude_at(ms)
        if interlude is not None:
            self._enter_interlude(interlude, ms)
            return
        if self._lit_interlude is not None:
            self._lit_interlude.set_cursor_ms(-1)
            self._lit_interlude = None
            # Force the next activation to re-scroll onto the vocal line.
            self._active_idx = -1

        if idx != self._active_idx:
            self._log(1, f"progression pos={pos:.2f}s -> idx changed "
                         f"{self._active_idx} -> {idx}")
            self._activate_row(idx, cursor_ms=ms)
        elif 0 <= idx:
            row = self._row_at(idx)
            if row is not None:
                row.set_cursor_ms(ms)
            self._log(2, f"progression pos={pos:.2f}s same idx={idx}")

    def _interlude_at(self, ms):
        for row in self._interlude_rows:
            if row.start_ms <= ms < row.end_ms:
                return row
        return None

    def _enter_interlude(self, row, ms):
        if self._lit_interlude is not row:
            # Dim whatever lyric line was lit before the break.
            if 0 <= self._lit_idx:
                prev = self._row_at(self._lit_idx)
                if prev is not None:
                    prev.set_cursor_ms(-1)
                self._lit_idx = -1
            if self._lit_interlude is not None:
                self._lit_interlude.set_cursor_ms(-1)
            self._lit_interlude = row
            self._suppress_select_signal = True
            self.lrc_list.select_row(row)
            self._suppress_select_signal = False
            self._scroll_to_row(row)
        row.set_cursor_ms(ms)

    def _on_map(self, *_):
        # Switching into this view: jump straight to the correct line
        # without animation so the user lands on the right spot.
        self._log(1, "view mapped")
        if self._synced and self._lines:
            pos = self._last_pos
            ms = int(pos * 1000)
            # Force a re-activation even if idx didn't change while we
            # were hidden — the row may not have been scrolled to.
            self._active_idx = -1
            interlude = self._interlude_at(ms)
            if interlude is not None:
                # Coming back mid-break: land on the dots, not on the line
                # that stopped being sung ten seconds ago.
                self._lit_interlude = None
                self._enter_interlude(interlude, ms)
            else:
                self._activate_row(self._index_for_position(pos), cursor_ms=ms)

    def _on_state_changed(self, player, state):
        if state == "stopped" and not self.player.current_video_id:
            self._current_video_id = None
            self._lines = []
            self._render_status("empty", title="Not playing")

    # ── Fetch pipeline ─────────────────────────────────────────────────────

    def _refresh_for_current_track(self, force=False):
        vid = self._current_video_id
        if not vid:
            self._lines = []
            self._active_idx = -1
            self._clear_rows()
            self._source_picker_btn.set_visible(False)
            self._render_status("empty", title="Not playing",
                                description="Play a song to see lyrics.")
            return

        title, artist, duration = self._track_metadata(vid)

        self._fetch_gen += 1
        gen = self._fetch_gen
        # Wipe ALL state before kicking off the fetch so a late
        # progression event can't index into stale line data.
        self._lines = []
        self._synced = False
        self._active_idx = -1
        self._current_source = None
        self._source_picker_btn.set_visible(True)
        self._clear_rows()
        self.stack.set_visible_child_name("loading")

        def _worker():
            try:
                data = self.player.client.get_lyrics(
                    vid, title=title, artist=artist, duration=duration,
                )
            except Exception as e:
                print(f"[LYRICS] fetch error: {e}")
                data = None
            GLib.idle_add(self._apply_fetch_result, gen, data)

        threading.Thread(target=_worker, daemon=True).start()

    def _track_metadata(self, video_id):
        title, artist, duration = None, None, None
        try:
            idx = self.player.current_queue_index
            if 0 <= idx < len(self.player.queue):
                track = self.player.queue[idx]
                if track.get("videoId") == video_id:
                    title = track.get("title")
                    artists = track.get("artists") or []
                    if artists and isinstance(artists, list):
                        names = [a.get("name") for a in artists if isinstance(a, dict)]
                        names = [n for n in names if n]
                        if names:
                            artist = names[0]
                    if not artist:
                        artist = track.get("artist")
                    from player.player import _parse_track_duration
                    duration = _parse_track_duration(track) or None
        except Exception:
            pass
        if not duration and getattr(self.player, "duration", 0) > 0:
            duration = int(self.player.duration)
        return title, artist, duration

    def _apply_fetch_result(self, gen, data):
        if gen != self._fetch_gen:
            self._log(1, f"fetch result stale (gen={gen} != {self._fetch_gen}), dropping")
            return False

        if not data or not data.get("lines"):
            self._log(1, "fetch result: no lyrics")
            self._lines = []
            self._synced = False
            self._render_status(
                "empty", title="No lyrics",
                description="Nothing matched automatically. Try another "
                            "source, or search by name.",
            )
            self._source_picker_btn.set_visible(True)
            self._update_source_picker_label()
            return False

        self._lines = data["lines"]
        self._synced = bool(data.get("synced"))
        self._current_source = data.get("source")
        word_lines = sum(1 for l in self._lines if l.get("parts"))
        self._log(1, f"fetch result: source={data.get('source')} "
                 f"synced={self._synced} lines={len(self._lines)} "
                 f"word-level={word_lines}")
        self._build_rows()
        self.stack.set_visible_child_name("lyrics")
        # Reveal the source-picker button now that we have at least one
        # provider's data in the cache.
        self._source_picker_btn.set_visible(True)
        self._update_source_picker_label()

        if self._synced and getattr(self.player, "duration", 0) > 0:
            idx = self._index_for_position(self._last_pos)
            self._log(1, f"initial activate at idx={idx} pos={self._last_pos:.2f}s")
            self._activate_row(idx, cursor_ms=int(self._last_pos * 1000))
        else:
            adj = self.scroller.get_vadjustment()
            if adj:
                adj.set_value(adj.get_lower())
        return False

    # ── Row management ────────────────────────────────────────────────────

    def _clear_rows(self):
        # Every row is about to go, so no row is lit any more. Leaving a
        # stale index here would suppress the next activation's clear.
        self._lit_idx = -1
        self._row_for_line = {}
        self._interlude_rows = []
        self._lit_interlude = None
        # GTK 4.12+ has remove_all; earlier versions need a loop. Use the
        # iterative removal for compatibility.
        child = self.lrc_list.get_first_child()
        while child is not None:
            nxt = child.get_next_sibling()
            self.lrc_list.remove(child)
            child = nxt

    def _build_rows(self):
        self._clear_rows()

        # Interlude markers occupy their own rows, so a row's position in
        # the ListBox stops matching its index in ``_lines``. Everything
        # that looks a row up by line index goes through ``_row_for_line``.
        interludes = (
            _find_interludes(self._lines) if self._synced else []
        )
        pending = list(interludes)

        for i, line in enumerate(self._lines):
            start = line.get("start")
            # Emit any interlude that finishes before this line starts.
            while pending and start is not None and pending[0][1] <= start:
                gap_start, gap_end = pending.pop(0)
                row = InterludeRow(gap_start, gap_end, effects=self._effects)
                self.lrc_list.append(row)
                self._interlude_rows.append(row)

            # A provider's empty marker line has no words to show; the
            # interlude row above already stands in for it.
            if not (line.get("text") or "").strip():
                continue

            row = LyricRow(
                line, i,
                second_line_mode=self._second_line_mode,
                effects=self._effects,
                sweep_end_ms=self._sweep_end_ms(i),
                sweep=self._sweep,
                active_scale=self._active_scale,
            )
            # Plain (unsynced) sources have no cursor to scrub against, so
            # the active-line dim/bright contrast just communicates "this
            # is offline text" if every line stayed at INACTIVE alpha.
            # Activate every row so the whole block reads at full
            # brightness — the visual cue for "no sync info".
            if not self._synced:
                row.set_cursor_ms(0)
            self.lrc_list.append(row)
            self._row_for_line[i] = row

        for gap_start, gap_end in pending:
            row = InterludeRow(gap_start, gap_end, effects=self._effects)
            self.lrc_list.append(row)
            self._interlude_rows.append(row)

        self._log(1, f"built {len(self._row_for_line)} lyric rows + "
                 f"{len(self._interlude_rows)} interludes")

    def _sweep_end_ms(self, idx):
        """When line ``idx`` gives way to the next one, in ms.

        The line's own ``end`` when the source provides one (TTML does),
        otherwise the next timed line's start. Only used to spread a
        synthesized sweep across the line, so an estimate is fine — but
        it has to be the *next line's* start rather than the next row's,
        since a blank marker line between them is where the singing
        actually stops."""
        if not self._synced:
            return None
        line = self._lines[idx]
        end = line.get("end")
        if end is not None:
            return int(end * 1000)
        for nxt in self._lines[idx + 1:]:
            start = nxt.get("start")
            if start is not None:
                return int(start * 1000)
        return None

    def _row_at(self, idx):
        return self._row_for_line.get(idx)

    # ── Activation + autoscroll ───────────────────────────────────────────

    def _index_for_position(self, pos):
        if not self._lines:
            return -1
        active = -1
        for i, line in enumerate(self._lines):
            start = line.get("start")
            if start is None:
                continue
            if start <= pos:
                active = i
            else:
                break
        return active

    def _activate_row(self, idx, cursor_ms):
        # A lyric line becoming current means no instrumental break is.
        # Clearing it here rather than only on the progression path covers
        # the routes that jump straight to a line — remapping the view,
        # switching provider, changing the second-line setting — any of
        # which would otherwise leave the marker lit alongside the line.
        if self._lit_interlude is not None:
            self._lit_interlude.set_cursor_ms(-1)
            self._lit_interlude = None
        # Clear the previously-lit row's cursor so its words fade back.
        if 0 <= self._lit_idx and self._lit_idx != idx:
            prev = self._row_at(self._lit_idx)
            if prev is not None:
                prev.set_cursor_ms(-1)
            self._lit_idx = -1
        self._active_idx = idx
        if idx < 0:
            self._log(1, "activate idx=-1 (no active line yet)")
            return
        row = self._row_at(idx)
        if row is None:
            self._log(1, f"activate idx={idx} but row_at returned None")
            return
        row.set_cursor_ms(cursor_ms)
        self._lit_idx = idx
        self._update_row_distances(idx)
        self._log(1, f"activate idx={idx} cursor_ms={cursor_ms} "
                 f"-> calling select_row")
        # Selecting the row drives both visual state (:selected style) and
        # autoscroll (via _on_row_selected). Suppress the click-to-seek
        # path since this selection isn't user-initiated.
        self._suppress_select_signal = True
        self.lrc_list.select_row(row)
        self._suppress_select_signal = False

    def _update_row_distances(self, active_idx):
        """Tell each row how far it is from the active line. Only the
        ``full`` effect level uses it (for the distance blur), so the
        whole walk is skipped otherwise — it runs on every line change."""
        if self._effects not in _EFFECT_BLUR:
            return
        for i in range(len(self._lines)):
            row = self._row_at(i)
            if row is not None:
                row.set_distance(abs(i - active_idx))

    def _on_row_selected(self, listbox, row):
        if row is None:
            self._log(1, "row-selected fired with row=None")
            return
        self._log(1, f"row-selected fired for idx={row.line_idx} "
                 f"(suppress_flag={self._suppress_select_signal})")
        # Autoscroll, regardless of who selected the row.
        self._scroll_to_row(row)

    def _on_row_activated(self, listbox, row):
        # User clicked a row → seek the player to that line's timestamp.
        if self._suppress_select_signal or row is None:
            return
        if not isinstance(row, (LyricRow, InterludeRow)):
            return
        if not self._synced:
            return
        start = (row.start_ms or 0) / 1000.0
        if self.player.duration > 0:
            self.player.seek(start)

    def _scroll_to_row(self, row):
        # Respect a recent manual scroll for a short grace window.
        since = self._time.monotonic() - self._user_scrolled_at
        if since < self._user_scroll_pause:
            self._log(1, f"scroll_to_row idx={row.line_idx} BLOCKED "
                     f"(user scrolled {since:.2f}s ago)")
            return
        # Capture the row's "generation" — if the active row has moved on
        # by the time the deferred scroll fires, abort. Multiple deferred
        # scrolls queueing in idle order was causing the active line to
        # snap back and forth as out-of-date callbacks executed.
        # Interlude rows all report line_idx -1, which would make every
        # one of them look like the same scroll target. Key off the row
        # itself for those.
        target_idx = row.line_idx if row.line_idx >= 0 else row
        self._scroll_target_idx = target_idx
        self._log(1, f"scroll_to_row idx={target_idx} scheduled")

        def do_scroll(retries=8):
            if self._scroll_target_idx != target_idx:
                self._log(1, f"do_scroll idx={target_idx} SUPERSEDED "
                         f"(now targeting {self._scroll_target_idx})")
                return False  # Superseded by a newer activation.
            adj = self.scroller.get_vadjustment()
            content = self.scroller.get_child()
            if adj is None or content is None:
                self._log(1, f"do_scroll idx={target_idx} no adj/content")
                return False
            alloc = row.get_allocation()
            if alloc.height <= 0:
                self._log(2, f"do_scroll idx={target_idx} row not laid out "
                         f"(retries={retries})")
                if retries > 0:
                    GLib.timeout_add(33, do_scroll, retries - 1)
                return False
            # Use row.get_allocation().y directly. Empirically,
            # compute_point(row, scroller.get_child()) in this widget
            # hierarchy factors in the scroll transform — its result
            # equals `alloc.y - adj.value + padding`. Subtracting vh/2
            # from a scroll-relative y produces a target that drifts
            # by the current adj.value each activation, so the active
            # line creeps toward the bottom of the viewport. alloc.y is
            # invariant of scroll: it's the row's true y inside the
            # ListBox, which (with the ListBox at the top of the Clamp,
            # margin_top=0) equals its y in the scrollable content.
            viewport_h = self.scroller.get_height()
            if viewport_h <= 0:
                self._log(1, f"do_scroll idx={target_idx} viewport not "
                         f"realized (vh={viewport_h}) — likely a hidden "
                         f"LyricsView; skipping")
                return False
            target = alloc.y - (viewport_h / 2) + (alloc.height / 2)
            raw_target = target
            target = max(adj.get_lower(),
                         min(target, adj.get_upper() - adj.get_page_size()))
            self._log(1,
                f"do_scroll idx={target_idx} "
                f"row.alloc=(y={alloc.y},h={alloc.height}) "
                f"vh={viewport_h} "
                f"raw_target={raw_target:.1f} clamped={target:.1f} "
                f"adj=(val={adj.get_value():.1f},lo={adj.get_lower():.1f},"
                f"up={adj.get_upper():.1f},page={adj.get_page_size():.1f})")
            self._animate_to(adj, target)
            return False

        GLib.idle_add(do_scroll)

    def _animate_to(self, adj, target, duration_ms=320):
        """Smoothly scroll the adjustment from its current value to
        ``target`` over ``duration_ms`` with an ease-out curve. Any
        in-flight animation is cancelled first so consecutive calls
        seamlessly retarget."""
        if self._scroll_anim_source:
            self._log(1, f"animate_to: cancelling in-flight animation")
            GLib.source_remove(self._scroll_anim_source)
            self._scroll_anim_source = 0
        start_value = adj.get_value()
        if abs(start_value - target) < 1.0:
            self._log(1, f"animate_to: no-op (start={start_value:.1f} ≈ "
                     f"target={target:.1f})")
            adj.set_value(target)
            return
        self._log(1, f"animate_to: start={start_value:.1f} -> target={target:.1f} "
                 f"delta={target - start_value:+.1f}")
        start_time = GLib.get_monotonic_time()

        def _tick():
            elapsed = (GLib.get_monotonic_time() - start_time) / 1000.0
            t = min(1.0, elapsed / max(1, duration_ms))
            eased = 1.0 - (1.0 - t) ** 3
            adj.set_value(start_value + (target - start_value) * eased)
            if t >= 1.0:
                self._scroll_anim_source = 0
                self._log(2, f"animate_to: done at {adj.get_value():.1f}")
                return False
            return True

        self._scroll_anim_source = GLib.timeout_add(16, _tick)

    def _render_status(self, name, title=None, description=None):
        if title is not None:
            self.status_page.set_title(title)
        if description is not None:
            self.status_page.set_description(description)
        self.stack.set_visible_child_name(name)

    def _on_user_scroll(self, *_):
        self._user_scrolled_at = self._time.monotonic()
        self._log(1, "user-scroll detected -> autoscroll paused")

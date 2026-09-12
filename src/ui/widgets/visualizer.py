import json
import math
import os

from gi.repository import Gtk, GLib


_PREFS_PATH = os.path.join(GLib.get_user_data_dir(), "muse", "prefs.json")


def _load_pref(key, default):
    try:
        if os.path.exists(_PREFS_PATH):
            with open(_PREFS_PATH) as f:
                return json.load(f).get(key, default)
    except Exception:
        pass
    return default


class Visualizer(Gtk.DrawingArea):
    """CAVA-inspired bar visualizer. Pulls spectrum bands from the Player
    via `pull_visualizer_bands()` on every UI tick — the Player owns a
    position-keyed queue of magnitudes posted by a GStreamer spectrum
    element, and returns the entry whose audio running-time the sink has
    actually reached so the bars stay in lock-step with playback.

    Pipeline per spectrum frame:

    1. Raw 128 spectrum bands (linear in frequency, threshold .. 0 dB).
    2. Drop sub-audible top bands so we don't dedicate display real estate
       to >16 kHz where there's almost never any energy in music.
    3. Reduce to N display bars on a log frequency scale (peak per bin) —
       bass and treble each get visible real estate.
    4. Apply per-band amplification that grows toward the high end. Real
       music's energy drops with frequency, so without this the right
       side of the row stays anemic compared to the bass.
    5. Monstercat-style smoothing — peaks bleed into neighbors so the row
       reads as a flowing wave instead of disjoint spikes.

    Rendering pipeline:

    - 60 fps tick interpolates each bar toward its target.
    - Bars snap UP instantly on a new peak (zero attack).
    - Bars fall under accumulating velocity (gravity acceleration) — the
       classic bouncy "drop" feel.
    """

    THRESHOLD_DB = -80.0
    GRAVITY = 0.0028
    MONSTERCAT_DEFAULT = 1.8
    BARS_DEFAULT = 56
    # Skip raw bands above this fraction of the spectrum — 0.6 ≈ 13 kHz
    # for a 44.1/48 kHz source. Past that, most music is essentially
    # silent and we were dedicating display bars to dead air.
    UPPER_FRACTION = 0.6
    # Per-band amplification curve: 1 + HIGH_FREQ_BOOST * (i / N).
    # Real music's energy rolls off sharply with frequency — at 1.8, the
    # rightmost bar gets ~2.8× gain so treble reads as visibly as bass.
    HIGH_FREQ_BOOST = 1.8
    # Contrast expansion. The dB→[0,1] mapping is heavily compressed (a
    # -30 dB band still lands at ~0.6), so without this every bar clusters
    # at a similar mid height — a flat carpet instead of CAVA's tall peaks
    # and deep valleys. Raising the normalized value to this power pushes
    # quiet bands down hard while autosens pulls the loudest back to the
    # top, restoring per-bar contrast. ~2.5 ≈ CAVA's linear-amplitude feel;
    # higher = sparser/spikier, lower = flatter.
    CONTRAST_GAMMA = 2.5
    # Auto-sensitivity: track a slowly-decaying recent peak so quiet
    # passages still fill the row. Without this, bars are stuck low
    # whenever the song isn't at peak loudness.
    AUTO_GAIN_DECAY = 0.997   # decay applied each spectrum frame (~30Hz)
    AUTO_GAIN_TARGET = 0.92   # recent peak should map to ~this height
    AUTO_GAIN_MAX = 3.5       # cap amplification so silence doesn't blow up noise
    AUTO_GAIN_FLOOR = 0.05    # ignore the recent_max if it's below this

    def __init__(self, player, height=80):
        super().__init__()
        self.player = player
        self.set_content_height(height)
        self.set_draw_func(self._draw)

        # Configurable from preferences.
        self.display_bars = max(8, min(100, int(_load_pref("visualizer_bars", self.BARS_DEFAULT))))
        self.smoothing = float(_load_pref("visualizer_smoothing", self.MONSTERCAT_DEFAULT))
        if self.smoothing < 1.05:
            self.smoothing = 1.05
        self.set_visible(bool(_load_pref("visualizer_enabled", True)))

        self._levels = []
        self._velocities = []
        self._weights = []
        self._tick_id = None
        self._active = False
        self._log_bins = None
        self._raw_bands = 0
        # Auto-sensitivity: this tracks the loudest bar seen in recent
        # spectrum frames. Each frame we scale the bars so that the
        # recent peak lands near AUTO_GAIN_TARGET — gives CAVA-style
        # dynamic range that fills the visualizer regardless of overall
        # song loudness.
        self._recent_max = 0.0

        self.connect("map", self._on_map)
        self.connect("unmap", self._on_unmap)
        self.connect("realize", self._on_realize)
        self.connect("unrealize", self._on_unrealize)

    # ─── Public config setters (called from Preferences) ───────────────────

    def set_bar_count(self, n):
        n = max(8, min(100, int(n)))
        if n == self.display_bars:
            return
        self.display_bars = n
        # Force a recompute of bins/weights on the next frame.
        self._log_bins = None
        self._levels = []
        self._velocities = []
        self.queue_draw()

    def set_smoothing(self, intensity):
        try:
            intensity = float(intensity)
        except (TypeError, ValueError):
            return
        self.smoothing = max(1.05, intensity)

    # ─── Lifecycle ─────────────────────────────────────────────────────────

    def _on_realize(self, *_):
        # Pull-driven now: the tick callback queries player.pull_visualizer_bands
        # on every frame, so there's no signal to subscribe to.
        print("[VIZ-WIDGET] realized, will pull spectrum data on tick")

    def _on_unrealize(self, *_):
        pass

    def _on_map(self, *_):
        self._active = True
        if self._tick_id is None:
            self._tick_id = GLib.timeout_add(16, self._on_tick)  # 60 fps

    def _on_unmap(self, *_):
        self._active = False
        if self._tick_id is not None:
            GLib.source_remove(self._tick_id)
            self._tick_id = None
        self._levels = []
        self._velocities = []

    # ─── Log-frequency band reduction ──────────────────────────────────────

    def _recompute_log_bins(self, raw_n):
        bins = []
        weights = []
        n_display = self.display_bars
        lo_min = 1  # skip DC
        # Cap upper frequency so we don't allocate display bars to the
        # near-silent top of the spectrum.
        hi_max = max(lo_min + n_display, int(raw_n * self.UPPER_FRACTION))
        ratio = hi_max / lo_min
        prev = lo_min
        for i in range(n_display):
            hi = int(lo_min * (ratio ** ((i + 1) / n_display)))
            hi = max(prev + 1, hi)
            hi = min(hi, hi_max)
            bins.append((prev, hi))
            # Treble-boost weight (1.0 at left → 1 + boost at right).
            weights.append(1.0 + self.HIGH_FREQ_BOOST * (i / max(1, n_display - 1)))
            prev = hi
        self._log_bins = bins
        self._weights = weights
        self._raw_bands = raw_n

    def _reduce(self, raw):
        n = len(raw)
        if n != self._raw_bands or self._log_bins is None or len(self._log_bins) != self.display_bars:
            self._recompute_log_bins(n)
        out = []
        threshold = self.THRESHOLD_DB
        for idx, (lo, hi) in enumerate(self._log_bins):
            if hi <= lo or lo >= n:
                out.append(0.0)
                continue
            chunk = raw[lo:min(hi, n)]
            if not chunk:
                out.append(0.0)
                continue
            peak_db = max(chunk)
            if peak_db <= threshold:
                out.append(0.0)
            else:
                norm = (peak_db - threshold) / -threshold
                # Expand dynamic range before the treble boost (see
                # CONTRAST_GAMMA): turns the flat mid-height carpet into
                # distinct peaks and valleys.
                norm = norm ** self.CONTRAST_GAMMA
                # Per-band amplification: treble gets a multiplicative boost
                # so the right half of the row isn't constantly anemic
                # compared to the bass. Deliberately NOT clamped to 1.0 here:
                # the auto-sensitivity stage needs to see the true peak (which
                # may exceed 1.0 after the boost) so it can scale the whole row
                # down to fit. Clamping here would hide that headroom and pin
                # loud bands at the top permanently.
                norm *= self._weights[idx]
                out.append(max(0.0, norm))
        return out

    # ─── Monstercat-style cross-bar smoothing ──────────────────────────────

    def _smooth(self, bars):
        n = len(bars)
        if n == 0:
            return bars
        intensity = self.smoothing
        out = list(bars)
        for i in range(n):
            peak = bars[i]
            if peak < 0.01:
                continue
            damped = peak
            for k in range(1, n):
                damped /= intensity
                if damped < 0.01:
                    break
                left = i - k
                right = i + k
                if left >= 0 and damped > out[left]:
                    out[left] = damped
                if right < n and damped > out[right]:
                    out[right] = damped
        return out

    # ─── Pull fresh data + snap up ─────────────────────────────────────────

    def _ingest_magnitudes(self, magnitudes):
        if not magnitudes:
            return
        if not getattr(self, "_viz_first_data_logged", False):
            self._viz_first_data_logged = True
            print(
                f"[VIZ-WIDGET] first data pulled "
                f"(magnitudes={len(magnitudes)}, "
                f"visible={self.get_visible()}, mapped={self.get_mapped()})"
            )
        bars = self._reduce(magnitudes)

        # Auto-sensitivity: scale toward AUTO_GAIN_TARGET based on a
        # slowly-decaying recent peak. Decays so quiet passages amplify
        # over time, capped so silence doesn't explode noise to full.
        frame_peak = max(bars) if bars else 0.0
        decayed = self._recent_max * self.AUTO_GAIN_DECAY
        self._recent_max = max(frame_peak, decayed)
        if self._recent_max > self.AUTO_GAIN_FLOOR:
            # Bidirectional auto-sensitivity (CAVA-style autosens): scale the
            # whole row so the slowly-decaying recent peak lands at
            # AUTO_GAIN_TARGET. This both amplifies quiet passages AND
            # attenuates loud ones — the latter is what keeps the bars from
            # pinning at the top during normal-loudness music. Clamp once,
            # here, after the scaling.
            gain = min(self.AUTO_GAIN_MAX, self.AUTO_GAIN_TARGET / self._recent_max)
            bars = [min(1.0, b * gain) for b in bars]
        else:
            bars = [min(1.0, b) for b in bars]

        bars = self._smooth(bars)

        n = len(bars)
        if not self._levels or len(self._levels) != n:
            self._levels = [0.0] * n
            self._velocities = [0.0] * n

        for i, h in enumerate(bars):
            if h > self._levels[i]:
                self._levels[i] = h
                self._velocities[i] = 0.0

    # ─── Frame tick: pull + gravity + redraw ───────────────────────────────

    def _on_tick(self):
        if not self._active:
            self._tick_id = None
            return False

        # Pull whatever spectrum frame the audio sink has actually reached
        # right now. The player's queue is keyed by running-time so widgets
        # in different windows (main + settings preview) stay independently
        # synced to playback without arguing over a shared cursor.
        magnitudes = None
        if hasattr(self.player, "pull_visualizer_bands"):
            try:
                magnitudes = self.player.pull_visualizer_bands()
            except Exception:
                magnitudes = None
        if magnitudes is not None:
            self._ingest_magnitudes(magnitudes)

        if not self._levels:
            return True

        gravity = self.GRAVITY
        for i in range(len(self._levels)):
            if self._levels[i] > 0.0:
                self._velocities[i] += gravity
                self._levels[i] -= self._velocities[i]
                if self._levels[i] <= 0.0:
                    self._levels[i] = 0.0
                    self._velocities[i] = 0.0

        self.queue_draw()
        return True

    # ─── Drawing ───────────────────────────────────────────────────────────

    def _accent_color(self):
        """Bar color: @visualizer_bar when MainWindow has derived one,
        else the plain accent.

        The derived value is the accent held far enough from the color of
        the transport buttons and time labels, which are drawn on top of
        the bars. A bright cover otherwise puts light text over a peak
        that is almost as light.
        """
        ctx = self.get_style_context()
        for name in ("visualizer_bar", "accent_color"):
            try:
                ok, color = ctx.lookup_color(name)
            except Exception:
                continue
            if ok and color is not None:
                return color.red, color.green, color.blue
        return 0.42, 0.34, 0.85

    # Faint baseline so the row is always visible. Active bars use a wide
    # alpha sweep (ACTIVE_ALPHA_MIN at level ~0 up to 1.0 at peak) so the
    # color fade clearly tracks bar height — short = dim, tall = bright.
    IDLE_ALPHA = 0.08
    ACTIVE_ALPHA_MIN = 0.15
    ACTIVE_ALPHA_MAX = 0.6

    def _draw(self, _area, cr, width, height):
        if width <= 0 or height <= 0:
            return

        n = self.display_bars
        levels = self._levels if (self._levels and len(self._levels) == n) else [0.0] * n

        r, g, b = self._accent_color()

        gap = 2.0
        total_gap = gap * (n - 1)
        bar_w = max(1.0, (width - total_gap) / n)
        min_h = 3.0

        for i, level in enumerate(levels):
            h = max(min_h, level * height)
            x = i * (bar_w + gap)
            y = height - h
            if level > 0.0:
                clamped = min(1.0, level)
                # Square root curve makes the fade visible across the whole
                # range — a bar at level 0.25 already lands at alpha ~0.65,
                # not the ~0.5 a linear curve would give. Means small peaks
                # are clearly distinguishable from the idle baseline.
                alpha = self.ACTIVE_ALPHA_MIN + (self.ACTIVE_ALPHA_MAX - self.ACTIVE_ALPHA_MIN) * (clamped ** 0.5)
            else:
                alpha = self.IDLE_ALPHA
            cr.set_source_rgba(r, g, b, alpha)
            self._rounded_rect(cr, x, y, bar_w, h, radius=min(bar_w / 2, 3))
            cr.fill()

    @staticmethod
    def _rounded_rect(cr, x, y, w, h, radius):
        radius = max(0.0, min(radius, w / 2, h / 2))
        if radius <= 0.5:
            cr.rectangle(x, y, w, h)
            return
        cr.new_sub_path()
        cr.arc(x + w - radius, y + radius, radius, -math.pi / 2, 0)
        cr.arc(x + w - radius, y + h - radius, radius, 0, math.pi / 2)
        cr.arc(x + radius, y + h - radius, radius, math.pi / 2, math.pi)
        cr.arc(x + radius, y + radius, radius, math.pi, 3 * math.pi / 2)
        cr.close_path()

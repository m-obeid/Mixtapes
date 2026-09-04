import os
import json
from gi.repository import Gtk, Adw, GObject, GLib

from ui.utils import AsyncPicture
from ui.widgets.visualizer import Visualizer
from ui.widgets.lyrics_view import LyricsView


_PREFS_PATH = os.path.join(
    os.environ.get("XDG_DATA_HOME") or os.path.expanduser("~/.local/share"),
    "muse",
    "prefs.json",
)


def _load_pref(key, default):
    try:
        if os.path.exists(_PREFS_PATH):
            with open(_PREFS_PATH) as f:
                return json.load(f).get(key, default)
    except Exception:
        pass
    return default


def _save_pref(key, value):
    try:
        os.makedirs(os.path.dirname(_PREFS_PATH), exist_ok=True)
        data = {}
        if os.path.exists(_PREFS_PATH):
            with open(_PREFS_PATH) as f:
                data = json.load(f) or {}
        data[key] = value
        with open(_PREFS_PATH, "w") as f:
            json.dump(data, f)
    except Exception:
        pass


class DesktopCoverView(Adw.Bin):
    """Full-window "cover art" view for desktop. The cover + visualizer
    column lives on the left; an optional lyrics column slides in to the
    right when the user toggles it on.

    Built on Adw.ToolbarView so the page has an opaque background
    without hand-rolled CSS. A plain Gtk.Box with a ``background-color``
    rule was rendering transparently during the OVER_UP slide,
    revealing the browser content behind it.
    """

    __gsignals__ = {
        "dismiss": (GObject.SignalFlags.RUN_FIRST, None, ()),
    }

    def __init__(self, player):
        super().__init__()
        self.player = player

        toolbar = Adw.ToolbarView()
        toolbar.set_hexpand(True)
        toolbar.set_vexpand(True)
        self.set_child(toolbar)

        # Cover art. AspectFrame + obey_child=False keeps it square
        # regardless of the window's aspect ratio. halign/valign must
        # stay at FILL (the default) — using CENTER here made the frame
        # shrink to its child's *natural* size, which for low-res video
        # thumbnails (120×90, 320×180) capped the cover at that tiny
        # size instead of upscaling into the whole available area.
        self.cover_img = AsyncPicture(crop_to_square=True, player=self.player)
        self.cover_img.add_css_class("rounded")
        self.cover_img.set_content_fit(Gtk.ContentFit.COVER)
        self.cover_img.set_hexpand(True)
        self.cover_img.set_vexpand(True)

        # Lyrics toggle floats in the top-right of the cover itself.
        # Built early so we can drop it into the cover_overlay below; the
        # toggled handler is wired up later, after self.split exists.
        #
        # ``osd`` is the HIG's style class for controls that float over
        # content: it paints its own dark scrim with a light foreground
        # regardless of theme or accent colour, which is the only way to
        # stay legible over arbitrary album art. The previous hand-rolled
        # translucent-window-background fill went invisible over pale
        # covers, and its accent-tinted checked state turned the icon
        # yellow-on-yellow whenever dynamic accent picked up a warm cover.
        self.lyrics_toggle = Gtk.ToggleButton()
        self.lyrics_toggle.set_icon_name("format-justify-fill-symbolic")
        self.lyrics_toggle.set_tooltip_text("Show lyrics")
        self.lyrics_toggle.add_css_class("osd")
        self.lyrics_toggle.add_css_class("circular")
        self.lyrics_toggle.add_css_class("lyrics-osd-btn")
        self.lyrics_toggle.set_halign(Gtk.Align.END)
        self.lyrics_toggle.set_valign(Gtk.Align.START)
        self.lyrics_toggle.set_margin_top(10)
        self.lyrics_toggle.set_margin_end(10)

        # Reveal on hover, the way media overlays behave elsewhere in
        # GNOME (Loupe, Videos) and in the YouTube Music web player, so
        # the cover is unobstructed while you're just listening.
        self._lyrics_toggle_revealer = Gtk.Revealer()
        self._lyrics_toggle_revealer.set_transition_type(
            Gtk.RevealerTransitionType.CROSSFADE
        )
        self._lyrics_toggle_revealer.set_transition_duration(150)
        self._lyrics_toggle_revealer.set_child(self.lyrics_toggle)
        self._lyrics_toggle_revealer.set_halign(Gtk.Align.END)
        self._lyrics_toggle_revealer.set_valign(Gtk.Align.START)
        self._lyrics_toggle_revealer.set_reveal_child(False)
        # A revealer that isn't revealed still takes its child's input
        # region in an overlay; make it inert so the hidden button can't
        # swallow clicks meant for the cover.
        self._lyrics_toggle_revealer.set_can_target(False)

        # The overlay must sit *inside* the AspectFrame so it inherits the
        # frame's square allocation — otherwise the button anchors to the
        # outer rectangle (including the empty space the AspectFrame
        # leaves around the square) and floats away from the cover's
        # corner.
        cover_overlay = Gtk.Overlay()
        cover_overlay.set_child(self.cover_img)
        cover_overlay.add_overlay(self._lyrics_toggle_revealer)

        self._pointer_over_cover = False
        motion = Gtk.EventControllerMotion()
        motion.connect("enter", self._on_cover_enter)
        motion.connect("leave", self._on_cover_leave)
        cover_overlay.add_controller(motion)

        # Touch screens never emit a pointer-enter, so hover alone would
        # leave the control unreachable on a tablet. A tap on the cover
        # brings it up for a few seconds, the way media controls behave on
        # touch elsewhere. Touch-only: a mouse press shouldn't start a
        # countdown when hover already handles that case.
        self._touch_reveal = False
        self._touch_reveal_source = 0
        tap = Gtk.GestureClick()
        tap.set_touch_only(True)
        tap.connect("released", self._on_cover_tapped)
        cover_overlay.add_controller(tap)
        # Keyboard users never generate a pointer-enter, so hiding the
        # control outright would put it out of reach. Focus reveals it too.
        self.lyrics_toggle.connect(
            "notify::has-focus", lambda *_: self._sync_lyrics_toggle_reveal()
        )

        cover_frame = Gtk.AspectFrame(ratio=1.0, obey_child=False)
        cover_frame.set_vexpand(True)
        cover_frame.set_hexpand(True)
        cover_frame.set_overflow(Gtk.Overflow.HIDDEN)
        cover_frame.set_child(cover_overlay)

        # Bar visualizer beneath the cover — narrows to the cover's
        # width via the same clamp so it never sticks out into the
        # wide-screen margins.
        self.visualizer = Visualizer(self.player, height=80)
        self.visualizer.add_css_class("cover-visualizer")
        self.visualizer.set_hexpand(True)

        # The cover is the elastic part of the stack: it gets whatever
        # height is left after the visualizer (fixed) and margins, and
        # AspectFrame keeps it square within that allocation. We must NOT
        # set valign=CENTER on the column — that forces the column to its
        # natural height (cover + visualizer), which on shorter windows
        # pushes the visualizer off the bottom of the screen.
        cover_column = Gtk.Box(orientation=Gtk.Orientation.VERTICAL, spacing=12)
        cover_column.set_hexpand(True)
        cover_column.set_vexpand(True)
        cover_column.append(cover_frame)
        cover_column.append(self.visualizer)

        # The cover column on its own clamps to a comfortable 800px so the
        # cover doesn't fill ultra-wide displays. When lyrics are visible we
        # widen the clamp and split the area in half.
        self.cover_clamp = Adw.Clamp()
        self.cover_clamp.set_maximum_size(800)
        self.cover_clamp.set_child(cover_column)
        self.cover_clamp.set_hexpand(True)
        self.cover_clamp.set_vexpand(True)

        # Lyrics column.
        self.lyrics_view = LyricsView(self.player)
        self.lyrics_view.set_hexpand(True)
        self.lyrics_view.set_vexpand(True)

        # Cover + lyrics live in an Adw.OverlaySplitView with the lyrics
        # as the sidebar on the right. Libadwaita handles the smooth
        # show/hide animation, the responsive sidebar sizing (fraction +
        # min/max), and the space reclaim when toggled off — none of
        # which a hand-rolled Gtk.Box + Gtk.Revealer pulled off cleanly.
        cover_outer = Gtk.Box(orientation=Gtk.Orientation.VERTICAL)
        cover_outer.set_hexpand(True)
        cover_outer.set_vexpand(True)
        cover_outer.set_margin_top(32)
        cover_outer.set_margin_bottom(32)
        cover_outer.set_margin_start(48)
        cover_outer.set_margin_end(48)
        cover_outer.append(self.cover_clamp)

        lyrics_outer = Gtk.Box(orientation=Gtk.Orientation.VERTICAL)
        lyrics_outer.set_hexpand(True)
        lyrics_outer.set_vexpand(True)
        lyrics_outer.set_margin_top(32)
        lyrics_outer.set_margin_bottom(32)
        lyrics_outer.set_margin_start(0)
        lyrics_outer.set_margin_end(24)
        lyrics_outer.append(self.lyrics_view)

        self.split = Adw.OverlaySplitView()
        self.split.add_css_class("lyrics-split")
        self.split.set_content(cover_outer)
        self.split.set_sidebar(lyrics_outer)
        self.split.set_sidebar_position(Gtk.PackType.END)
        self.split.set_show_sidebar(False)
        # The user's intent (separate from what libadwaita has the sidebar
        # actually showing). Adw.OverlaySplitView occasionally flips
        # ``show-sidebar`` back to its default on layout transitions —
        # specifically when the collapse breakpoint applies/unapplies on
        # resize, the lyrics column would silently re-open. We watch the
        # property and snap it back to the user's wish if it drifts.
        self._lyrics_intent = False
        self._suppress_intent_sync = False
        self.split.connect("notify::show-sidebar", self._on_show_sidebar_changed)
        # Push layout (sidebar steals real space) instead of overlay.
        # Overlay would float the lyrics on top of the cover, which we
        # don't want at any width above the collapse breakpoint.
        self.split.set_collapsed(False)
        # Lyrics get a generous slice (55%) of the total area, clamped
        # so they don't get unusably thin or absurdly wide.
        self.split.set_sidebar_width_fraction(0.55)
        self.split.set_min_sidebar_width(360)
        self.split.set_max_sidebar_width(900)

        # Below this width the split collapses (lyrics become a floating
        # overlay) — happens when the queue sidebar AND lyrics are both
        # open and the window is small. Avoids the cover squishing past
        # usable size.
        self._bp_bin = Adw.BreakpointBin()
        self._bp_bin.set_size_request(150, 150)
        self._bp_bin.set_child(self.split)
        collapse_bp = Adw.Breakpoint.new(
            Adw.BreakpointCondition.parse("max-width: 560px")
        )
        collapse_bp.add_setter(self.split, "collapsed", True)
        self._bp_bin.add_breakpoint(collapse_bp)

        # The toggle was created up next to the cover frame (it's
        # already overlaid on top of the cover) — just wire its handler
        # now that ``self.split`` exists.
        self.lyrics_toggle.connect("toggled", self._on_lyrics_toggled)

        toolbar.set_content(self._bp_bin)

        # Keep the cover in sync with the currently-playing track.
        self.player.connect("metadata-changed", self._on_metadata_changed)

        # Restore the user's last lyrics-toggle state. The toggled handler
        # does all the work of attaching/detaching the panel.
        if bool(_load_pref("lyrics_shown_desktop", False)):
            self.lyrics_toggle.set_active(True)
        self._sync_lyrics_toggle_reveal()

    def _on_cover_enter(self, *_):
        self._pointer_over_cover = True
        self._sync_lyrics_toggle_reveal()

    def _on_cover_leave(self, *_):
        self._pointer_over_cover = False
        self._sync_lyrics_toggle_reveal()

    # How long the control stays up after a tap before fading away again.
    TOUCH_REVEAL_SECONDS = 4

    def _on_cover_tapped(self, _gesture, _n_press, _x, _y):
        self._touch_reveal = True
        self._sync_lyrics_toggle_reveal()
        self._restart_touch_reveal_timer()

    def _restart_touch_reveal_timer(self):
        if self._touch_reveal_source:
            GLib.source_remove(self._touch_reveal_source)
        self._touch_reveal_source = GLib.timeout_add_seconds(
            self.TOUCH_REVEAL_SECONDS, self._end_touch_reveal
        )

    def _end_touch_reveal(self):
        self._touch_reveal_source = 0
        self._touch_reveal = False
        self._sync_lyrics_toggle_reveal()
        return GLib.SOURCE_REMOVE

    def _sync_lyrics_toggle_reveal(self):
        # Hover, keyboard focus, or a recent tap. The toggle's state is
        # already visible in the window — the lyrics column is either
        # there or it isn't — so keeping the button on screen to report
        # it would just put a permanent widget back on the cover.
        reveal = (
            self._pointer_over_cover
            or self.lyrics_toggle.has_focus()
            or self._touch_reveal
        )
        self._lyrics_toggle_revealer.set_reveal_child(reveal)
        self._lyrics_toggle_revealer.set_can_target(reveal)

    def _on_lyrics_toggled(self, btn):
        shown = btn.get_active()
        self._lyrics_intent = shown
        self._suppress_intent_sync = True
        self.split.set_show_sidebar(shown)
        self._suppress_intent_sync = False
        btn.set_tooltip_text("Hide lyrics" if shown else "Show lyrics")
        if self._touch_reveal:
            self._restart_touch_reveal_timer()
        self._sync_lyrics_toggle_reveal()
        _save_pref("lyrics_shown_desktop", bool(shown))

    def _on_show_sidebar_changed(self, *_):
        # Guard against the round-trip from our own _on_lyrics_toggled
        # setting show-sidebar — we only care about *external* drift.
        if self._suppress_intent_sync:
            return
        actual = self.split.get_show_sidebar()
        if actual != self._lyrics_intent:
            # Libadwaita changed the property on us (typically after a
            # collapse/uncollapse during window resize). Snap it back.
            def _restore():
                self._suppress_intent_sync = True
                self.split.set_show_sidebar(self._lyrics_intent)
                self._suppress_intent_sync = False
                return False
            GLib.idle_add(_restore)

    def _on_metadata_changed(self, player, title, artist, thumb_url,
                             video_id, like_status):
        if thumb_url:
            self.cover_img.video_id = video_id
            self.cover_img.load_url(thumb_url)
        else:
            self.cover_img.set_paintable(None)

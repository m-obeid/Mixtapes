import os
import json
from gi.repository import Gtk, Adw, GObject, GLib, Gdk, Gio, Pango

from ui.context_menu import MenuAction, build_song_menu
from ui.utils import AsyncPicture, LikeButton, MarqueeLabel, show_toast
from ui.widgets.visualizer import Visualizer
from ui.widgets.lyrics_view import LyricsView


_PREFS_PATH = os.path.join(
    GLib.get_user_data_dir(),
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
    __gsignals__ = {
        "dismiss": (GObject.SignalFlags.RUN_FIRST, None, ()),
        "queue-requested": (GObject.SignalFlags.RUN_FIRST, None, ()),
    }

    def __init__(self, player, on_artist_click=None, on_queue_click=None):
        super().__init__()
        self.player = player
        self.on_artist_click = on_artist_click
        self.on_queue_click = on_queue_click
        self._scroll_seek_id = None
        self._is_buffering_spinner = False
        self._load_css()

        toolbar = Adw.ToolbarView()
        toolbar.set_hexpand(True)
        toolbar.set_vexpand(True)
        self.set_child(toolbar)

        # ── Toggle Group (Player / Lyrics) ───────────────────────────
        self._toggle_group_is_adw = hasattr(Adw, "ToggleGroup") and hasattr(Adw, "Toggle")
        self._buttons_by_page = {}
        self._suppress_toggle_sync = False

        if self._toggle_group_is_adw:
            self.toggle_nav = Adw.ToggleGroup()
            self.toggle_nav.add_css_class("round")
            self.toggle_nav.set_halign(Gtk.Align.CENTER)
            self.toggle_nav.set_margin_top(8)
            self.toggle_nav.set_margin_bottom(8)

            t_player = Adw.Toggle(name="player", label="Player", icon_name="folder-music-symbolic")
            t_lyrics = Adw.Toggle(name="lyrics", label="Lyrics", icon_name="format-justify-fill-symbolic")

            self.toggle_nav.add(t_player)
            self.toggle_nav.add(t_lyrics)

            self.toggle_nav.connect("notify::active-name", self._on_toggle_group_changed)
        else:
            self.toggle_nav = Gtk.Box(orientation=Gtk.Orientation.HORIZONTAL)
            self.toggle_nav.add_css_class("linked")
            self.toggle_nav.set_halign(Gtk.Align.CENTER)
            self.toggle_nav.set_margin_top(8)
            self.toggle_nav.set_margin_bottom(8)

            pages = [
                ("player", "folder-music-symbolic", "Player"),
                ("lyrics", "format-justify-fill-symbolic", "Lyrics"),
            ]

            first_btn = None
            for name, icon, label in pages:
                btn = Gtk.ToggleButton(icon_name=icon)
                btn.set_tooltip_text(label)
                if first_btn:
                    btn.set_group(first_btn)
                else:
                    first_btn = btn
                    btn.set_active(True)

                btn.connect(
                    "toggled",
                    lambda b, n=name: self._on_fallback_button_toggled(b, n),
                )
                self.toggle_nav.append(btn)
                self._buttons_by_page[name] = btn

        toolbar.add_top_bar(self.toggle_nav)

        self.cover_img = AsyncPicture(crop_to_square=True, player=self.player)
        self.cover_img.add_css_class("cover-desktop")
        self.cover_img.set_content_fit(Gtk.ContentFit.COVER)
        self.cover_img.set_hexpand(True)
        self.cover_img.set_vexpand(True)

        self._lyrics_toggle_revealer = Gtk.Revealer()
        self._lyrics_toggle_revealer.set_transition_type(
            Gtk.RevealerTransitionType.CROSSFADE
        )
        self._lyrics_toggle_revealer.set_transition_duration(150)
        self._lyrics_toggle_revealer.set_halign(Gtk.Align.END)
        self._lyrics_toggle_revealer.set_valign(Gtk.Align.START)
        self._lyrics_toggle_revealer.set_reveal_child(False)
        self._lyrics_toggle_revealer.set_can_target(False)

        cover_overlay = Gtk.Overlay()
        cover_overlay.set_child(self.cover_img)
        cover_overlay.add_overlay(self._lyrics_toggle_revealer)

        self._pointer_over_cover = False
        motion = Gtk.EventControllerMotion()
        motion.connect("enter", self._on_cover_enter)
        motion.connect("leave", self._on_cover_leave)
        cover_overlay.add_controller(motion)

        self._touch_reveal = False
        self._touch_reveal_source = 0
        tap = Gtk.GestureClick()
        tap.set_touch_only(True)
        tap.connect("released", self._on_cover_tapped)
        cover_overlay.add_controller(tap)

        cover_frame = Gtk.AspectFrame(ratio=1.0, obey_child=False, margin_bottom=16)
        cover_frame.set_vexpand(True)
        cover_frame.set_hexpand(True)
        cover_frame.set_overflow(Gtk.Overflow.HIDDEN)
        cover_frame.set_child(cover_overlay)

        meta_row = Gtk.Box(orientation=Gtk.Orientation.HORIZONTAL, spacing=12)
        meta_row.set_hexpand(True)
        meta_row.set_valign(Gtk.Align.CENTER)
        meta_row.set_margin_bottom(8)

        text_box = Gtk.Box(orientation=Gtk.Orientation.VERTICAL, spacing=2)
        text_box.set_hexpand(True)
        text_box.set_valign(Gtk.Align.CENTER)

        self.title_label = MarqueeLabel()
        self.title_label.set_label("Not Playing")
        self.title_label.add_css_class("title-3")

        self.artists_box = Gtk.Box(orientation=Gtk.Orientation.HORIZONTAL, spacing=2)
        self.artists_box.set_halign(Gtk.Align.START)

        self.artist_label = Gtk.Label(label="")
        self.artist_label.add_css_class("heading")
        self.artist_label.set_opacity(0.7)
        self.artist_label.set_ellipsize(Pango.EllipsizeMode.END)
        self.artist_label.set_halign(Gtk.Align.START)
        self.artists_box.append(self.artist_label)

        text_box.append(self.title_label)
        text_box.append(self.artists_box)

        self.like_btn = LikeButton(self.player.client, None)
        self.like_btn.set_visible(False)
        self.like_btn.set_valign(Gtk.Align.CENTER)

        meta_row.append(text_box)
        meta_row.append(self.like_btn)

        self.visualizer = Visualizer(self.player, height=80)
        self.visualizer.set_hexpand(True)
        self.visualizer.set_vexpand(True)
        self.visualizer.set_can_target(False)
        self.visualizer.add_css_class("cover-visualizer")

        buttons_row = Gtk.Box(orientation=Gtk.Orientation.HORIZONTAL, spacing=16)
        buttons_row.set_halign(Gtk.Align.CENTER)
        buttons_row.set_valign(Gtk.Align.CENTER)

        self.vol_btn = Gtk.MenuButton()
        self.vol_btn.set_icon_name("audio-volume-high-symbolic")
        self.vol_btn.set_direction(Gtk.ArrowType.UP)
        self.vol_btn.add_css_class("flat")
        self.vol_btn.add_css_class("circular")
        self.vol_btn.set_valign(Gtk.Align.CENTER)

        self.vol_popover = Gtk.Popover()
        self.vol_popover.set_position(Gtk.PositionType.TOP)
        self.vol_popover.set_has_arrow(True)
        self.vol_popover.add_css_class("compact-popover")
        self.vol_popover_box = Gtk.Box(orientation=Gtk.Orientation.VERTICAL, spacing=8)
        self.vol_popover_box.set_margin_top(12)
        self.vol_popover_box.set_margin_bottom(12)
        self.vol_popover_box.set_margin_start(8)
        self.vol_popover_box.set_margin_end(8)

        self.volume_scale = Gtk.Scale(orientation=Gtk.Orientation.VERTICAL)
        self.volume_scale.set_range(0, 1.0)
        self.volume_scale.set_inverted(True)
        self.volume_scale.set_size_request(-1, 150)
        self.volume_scale.set_value(self.player.get_volume())
        self.volume_scale.connect("value-changed", self.on_volume_scale_changed)

        self.vol_popover_box.append(self.volume_scale)
        self.vol_popover.set_child(self.vol_popover_box)
        self.vol_btn.set_popover(self.vol_popover)
        buttons_row.append(self.vol_btn)

        self.prev_btn = Gtk.Button(icon_name="media-skip-backward-symbolic")
        self.prev_btn.add_css_class("circular")
        self.prev_btn.set_valign(Gtk.Align.CENTER)
        self.prev_btn.connect("clicked", lambda _: self.player.previous())
        buttons_row.append(self.prev_btn)

        self.play_btn = Gtk.Button()
        self.play_btn.add_css_class("suggested-action")
        self.play_btn.add_css_class("circular")
        self.play_btn.set_valign(Gtk.Align.CENTER)
        self.play_btn.set_size_request(48, 48)
        self.play_btn.connect("clicked", self._on_play_clicked)

        self._play_stack = Gtk.Stack()
        self._play_stack.set_transition_type(Gtk.StackTransitionType.CROSSFADE)
        self._play_stack.set_transition_duration(150)

        self._play_icon = Gtk.Image.new_from_icon_name("media-playback-start-symbolic")
        self._play_icon.set_pixel_size(20)
        self._play_stack.add_named(self._play_icon, "icon")

        self._play_spinner = Adw.Spinner()
        self._play_spinner.set_size_request(24, 24)
        self._play_stack.add_named(self._play_spinner, "spinner")

        self.play_btn.set_child(self._play_stack)
        buttons_row.append(self.play_btn)

        self.next_btn = Gtk.Button(icon_name="media-skip-forward-symbolic")
        self.next_btn.add_css_class("circular")
        self.next_btn.set_valign(Gtk.Align.CENTER)
        self.next_btn.connect("clicked", lambda _: self.player.next())
        buttons_row.append(self.next_btn)

        self.more_menu_model = Gio.Menu()
        self.more_btn = Gtk.MenuButton(icon_name="view-more-symbolic")
        self.more_btn.add_css_class("flat")
        self.more_btn.add_css_class("circular")
        self.more_btn.set_valign(Gtk.Align.CENTER)
        self.more_btn.set_menu_model(self.more_menu_model)
        self._refresh_more_menu()
        buttons_row.append(self.more_btn)

        progress_row = Gtk.Box(orientation=Gtk.Orientation.HORIZONTAL)
        progress_row.set_hexpand(True)

        time_row = Gtk.Box(orientation=Gtk.Orientation.HORIZONTAL)
        time_row.set_hexpand(True)

        self.current_time_label = Gtk.Label(label="0:00")
        self.current_time_label.add_css_class("caption")
        self.current_time_label.add_css_class("numeric")
        self.current_time_label.set_halign(Gtk.Align.START)
        time_row.append(self.current_time_label)

        spacer = Gtk.Box()
        spacer.set_hexpand(True)
        time_row.append(spacer)

        self.total_time_label = Gtk.Label(label="0:00")
        self.total_time_label.add_css_class("caption")
        self.total_time_label.add_css_class("numeric")
        self.total_time_label.set_halign(Gtk.Align.END)
        time_row.append(self.total_time_label)

        self.scale = Gtk.Scale(orientation=Gtk.Orientation.HORIZONTAL)
        self.scale.set_hexpand(True)
        self.scale.set_valign(Gtk.Align.CENTER)
        self.scale.set_range(0, 100)
        self.scale.add_css_class("progress-scale")
        self.scale.connect("change-value", self.on_scale_change_value)

        scroll_controller = Gtk.EventControllerScroll.new(
            Gtk.EventControllerScrollFlags.VERTICAL
        )
        scroll_controller.connect("scroll", self.on_scale_scroll)
        self.scale.add_controller(scroll_controller)
        progress_row.append(self.scale)

        controls_overlay_content = Gtk.Box(
            orientation=Gtk.Orientation.VERTICAL
        )
        controls_overlay_content.set_hexpand(True)
        controls_overlay_content.set_valign(Gtk.Align.CENTER)
        controls_overlay_content.append(progress_row)
        controls_overlay_content.append(time_row)
        controls_overlay_content.append(buttons_row)
        
        self.controls_overlay = Gtk.Overlay()
        self.controls_overlay.set_hexpand(True)
        self.controls_overlay.set_valign(Gtk.Align.CENTER)

        overlay_base = Adw.Bin()
        overlay_base.set_hexpand(True)
        overlay_base.set_size_request(-1, 85)
        self.controls_overlay.set_child(overlay_base)

        self.controls_overlay.add_overlay(self.visualizer)
        self.controls_overlay.add_overlay(controls_overlay_content)

        player_controls_box = Gtk.Box(
            orientation=Gtk.Orientation.VERTICAL, spacing=8, margin_top=16
        )
        player_controls_box.set_hexpand(True)
        player_controls_box.set_valign(Gtk.Align.CENTER)
        player_controls_box.append(cover_frame)
        player_controls_box.append(meta_row)
        player_controls_box.append(self.controls_overlay)

        cover_column = Gtk.Box(orientation=Gtk.Orientation.VERTICAL)
        cover_column.set_hexpand(True)
        cover_column.set_vexpand(True)
        cover_column.append(player_controls_box)

        self.cover_clamp = Adw.Clamp()
        self.cover_clamp.set_maximum_size(512)
        self.cover_clamp.set_child(cover_column)
        self.cover_clamp.set_hexpand(True)
        self.cover_clamp.set_vexpand(True)

        self.lyrics_view = LyricsView(self.player)
        self.lyrics_view.set_hexpand(True)
        self.lyrics_view.set_vexpand(True)

        self.cover_outer = Gtk.Box(orientation=Gtk.Orientation.VERTICAL)
        self.cover_outer.set_hexpand(True)
        self.cover_outer.set_vexpand(True)
        self.cover_outer.set_margin_bottom(32)
        self.cover_outer.set_margin_start(48)
        self.cover_outer.set_margin_end(48)
        self.cover_outer.append(self.cover_clamp)

        lyrics_outer = Gtk.Box(orientation=Gtk.Orientation.VERTICAL)
        lyrics_outer.set_hexpand(True)
        lyrics_outer.set_vexpand(True)
        lyrics_outer.set_margin_top(16)
        lyrics_outer.set_margin_bottom(32)
        lyrics_outer.set_margin_start(0)
        lyrics_outer.set_margin_end(24)
        lyrics_outer.append(self.lyrics_view)

        self.split = Adw.OverlaySplitView()
        self.split.add_css_class("lyrics-split")
        self.split.set_content(self.cover_outer)
        self.split.set_sidebar(lyrics_outer)
        self.split.set_sidebar_position(Gtk.PackType.END)
        self.split.set_show_sidebar(False)
        
        collapse_btn = Gtk.Button(icon_name="go-down-symbolic")
        collapse_btn.set_valign(Gtk.Align.END)
        collapse_btn.set_halign(Gtk.Align.END)
        collapse_btn.add_css_class("flat")
        collapse_btn.add_css_class("circular")
        collapse_btn.set_tooltip_text("Collapse player")
        collapse_btn.set_margin_end(24)
        collapse_btn.set_margin_bottom(24)
        collapse_btn.connect("clicked", lambda _: self.emit("dismiss"))
                
        queue_btn = Gtk.Button(icon_name="music-queue-symbolic")
        queue_btn.set_valign(Gtk.Align.END)
        queue_btn.set_halign(Gtk.Align.END)
        queue_btn.add_css_class("flat")
        queue_btn.add_css_class("circular")
        queue_btn.set_tooltip_text("Queue")
        queue_btn.set_margin_end(66)
        queue_btn.set_margin_bottom(24)
        if self.on_queue_click:
            queue_btn.connect("clicked", lambda _: self.on_queue_click())

        self.view_overlay = Gtk.Overlay()
        self.view_overlay.set_hexpand(True)
        self.view_overlay.set_vexpand(True)
        self.view_overlay.set_child(self.split)
        self.view_overlay.add_overlay(collapse_btn)
        self.view_overlay.add_overlay(queue_btn)

        self._lyrics_intent = False
        self._suppress_intent_sync = False
        self.split.connect("notify::show-sidebar", self._on_show_sidebar_changed)
        self.split.connect("notify::collapsed", self._on_collapsed_changed)

        self.split.set_collapsed(False)
        self.split.set_sidebar_width_fraction(0.55)
        self.split.set_min_sidebar_width(360)
        self.split.set_max_sidebar_width(900)

        self._bp_bin = Adw.BreakpointBin()
        self._bp_bin.set_size_request(150, 150)
        self._bp_bin.set_child(self.view_overlay)
        collapse_bp = Adw.Breakpoint.new(
            Adw.BreakpointCondition.parse("max-width: 735px")
        )
        collapse_bp.add_setter(self.split, "collapsed", True)
        self._bp_bin.add_breakpoint(collapse_bp)

        toolbar.set_content(self._bp_bin)

        self.player.connect("progression", self.on_progression)
        self.player.connect("state-changed", self.on_state_changed)
        self.player.connect("metadata-changed", self._on_metadata_changed)
        self.player.connect("volume-changed", self.on_volume_changed)

        self.connect("map", lambda *_: self.update_visualizer_state())

        initial_lyrics = bool(_load_pref("lyrics_shown_desktop", False))
        if initial_lyrics:
            self.split.set_show_sidebar(True)
        self._sync_nav_toggles(initial_lyrics)
        self._sync_lyrics_toggle_reveal()

    def _on_queue_btn_clicked(self, _btn):
        if callable(self.on_queue_click):
            self.on_queue_click()
        self.emit("queue-requested")

    def update_visualizer_state(self):
        if hasattr(self, "visualizer") and hasattr(self.visualizer, "set_active"):
            is_playing = self.player.get_state_string() == "playing"
            self.visualizer.set_active(is_playing)

    def _on_toggle_group_changed(self, group, _param):
        if self._suppress_toggle_sync:
            return
        name = group.get_active_name()
        self._apply_lyrics_state(name == "lyrics")

    def _on_fallback_button_toggled(self, button, page_name):
        if self._suppress_toggle_sync:
            return
        if button.get_active():
            self._apply_lyrics_state(page_name == "lyrics")

    def _apply_lyrics_state(self, show: bool):
        self._lyrics_intent = show
            
        if hasattr(self, "cover_clamp"):
            if self.split.get_collapsed() and show:
                self.cover_clamp.set_opacity(0.0)
            else:
                self.cover_clamp.set_opacity(1.0)

        self._suppress_intent_sync = True
        self.split.set_show_sidebar(show)
        self._suppress_intent_sync = False
        
        _save_pref("lyrics_shown_desktop", bool(show))

    def _sync_nav_toggles(self, is_lyrics_shown: bool):
        target_name = "lyrics" if is_lyrics_shown else "player"
        self._suppress_toggle_sync = True
        try:
            if self._toggle_group_is_adw:
                if self.toggle_nav.get_active_name() != target_name:
                    self.toggle_nav.set_active_name(target_name)
            else:
                btn = self._buttons_by_page.get(target_name)
                if btn and not btn.get_active():
                    btn.set_active(True)
        finally:
            self._suppress_toggle_sync = False

    def _on_lyrics_toggled(self, btn):
        if self._suppress_toggle_sync:
            return
        shown = btn.get_active()
        self._lyrics_intent = shown
        btn.set_tooltip_text("Hide lyrics" if shown else "Show lyrics")

        if self._touch_reveal:
            self._restart_touch_reveal_timer()
        self._sync_lyrics_toggle_reveal()

        self._suppress_intent_sync = True
        self.split.set_show_sidebar(shown)
        self._suppress_intent_sync = False
        
        self._sync_nav_toggles(shown)
        
        if hasattr(self, "cover_clamp"):
            if self.split.get_collapsed() and shown:
                self.cover_clamp.set_opacity(0.0)
            else:
                self.cover_clamp.set_opacity(1.0)
                
        _save_pref("lyrics_shown_desktop", bool(shown))

    def _on_show_sidebar_changed(self, *_):
        if self._suppress_intent_sync:
            return
        actual = self.split.get_show_sidebar()
        
        if self.split.get_collapsed() and self._lyrics_intent and not actual:
            self._suppress_intent_sync = True
            self.split.set_show_sidebar(True)
            self._suppress_intent_sync = False
            return

        if actual != self._lyrics_intent:
            self._lyrics_intent = actual

        self._sync_nav_toggles(actual)
            
        if hasattr(self, "cover_clamp"):
            self.cover_clamp.set_opacity(0.0 if (self.split.get_collapsed() and actual) else 1.0)

    def _on_collapsed_changed(self, split, _param=None):
        collapsed = split.get_collapsed()
        
        if collapsed and self._lyrics_intent:
            self._suppress_intent_sync = True
            self.split.set_show_sidebar(True)
            self._suppress_intent_sync = False
            
        if hasattr(self, "cover_clamp"):
            if collapsed and self._lyrics_intent:
                self.cover_clamp.set_opacity(0.0)
            else:
                self.cover_clamp.set_opacity(1.0)

    def _on_single_artist_clicked(self, aid, name):
        if self.on_artist_click:
            try:
                self.on_artist_click(aid, name)
            except TypeError:
                self.on_artist_click()
        self.emit("dismiss")

    def _format_time(self, seconds):
        if seconds < 0:
            return "0:00"
        m = int(seconds // 60)
        s = int(seconds % 60)
        return f"{m}:{s:02d}"

    def on_progression(self, player, pos, dur):
        if getattr(self, "_scroll_seek_id", None):
            return
        self.scale.set_range(0, dur)
        self.scale.set_value(pos)
        self.scale.set_sensitive(dur > 0)
        self.current_time_label.set_label(self._format_time(pos))
        self.total_time_label.set_label(self._format_time(dur))

        if self._is_buffering_spinner and dur > 0:
            if self.player.get_state_string() == "playing":
                self._is_buffering_spinner = False
                self._play_stack.set_visible_child_name("icon")
                self._play_icon.set_from_icon_name("media-playback-pause-symbolic")
                self.play_btn.set_sensitive(True)
                self.scale.set_sensitive(True)

    def on_state_changed(self, player, state):
        self.update_visualizer_state()
        if state == "loading":
            self.scale.set_value(0)
            self.scale.set_sensitive(False)
            self.current_time_label.set_label("0:00")
            self.total_time_label.set_label("0:00")
            self._play_stack.set_visible_child_name("spinner")
            self.play_btn.set_sensitive(False)
            self._is_buffering_spinner = True
        elif state == "playing":
            if self.player.duration <= 0:
                self._is_buffering_spinner = True
                self._play_stack.set_visible_child_name("spinner")
                self.play_btn.set_sensitive(False)
                self.scale.set_sensitive(False)
            else:
                self._is_buffering_spinner = False
                self._play_icon.set_from_icon_name("media-playback-pause-symbolic")
                self._play_stack.set_visible_child_name("icon")
                self.play_btn.set_sensitive(True)
                self.scale.set_sensitive(True)
        elif state in ("paused", "stopped"):
            if self._is_buffering_spinner and self.player.duration <= 0:
                return
            self._is_buffering_spinner = False
            self._play_icon.set_from_icon_name("media-playback-start-symbolic")
            self._play_stack.set_visible_child_name("icon")
            self.play_btn.set_sensitive(True)
            if state == "paused":
                self.scale.set_sensitive(True)

    def _on_play_clicked(self, _btn):
        if self.player.get_state_string() == "playing":
            self.player.pause()
        else:
            self.player.play()

    def on_volume_scale_changed(self, scale):
        if getattr(self, "_updating_volume", False):
            return
        self.player.set_volume(scale.get_value())

    def on_volume_changed(self, player, volume, muted):
        display_volume = 0.0 if muted else volume

        self._updating_volume = True
        self.volume_scale.set_value(display_volume)
        self._updating_volume = False

        if muted or volume == 0:
            self.vol_btn.set_icon_name("audio-volume-muted-symbolic")
        elif volume < 0.33:
            self.vol_btn.set_icon_name("audio-volume-low-symbolic")
        elif volume < 0.66:
            self.vol_btn.set_icon_name("audio-volume-medium-symbolic")
        else:
            self.vol_btn.set_icon_name("audio-volume-high-symbolic")

    def _refresh_more_menu(self):
        vid = getattr(self.player, "current_video_id", None)
        idx = getattr(self.player, "current_queue_index", -1)
        queue = getattr(self.player, "queue", []) or []
        track = queue[idx] if 0 <= idx < len(queue) else {"videoId": vid}

        extras = []
        if vid and hasattr(self.player, "get_stream_debug"):
            extras.append(
                MenuAction(
                    "Stream Info (Debug)", self._show_stream_info, section="debug"
                )
            )

        self.more_menu_model = build_song_menu(
            self.more_btn,
            track,
            player=self.player,
            client=self.player.client,
            prefix="dcv",
            video_id=vid,
            hide=("play_next", "add_to_queue", "goto_artist", "goto_album"),
            extras=extras,
        )
        self.more_btn.set_menu_model(self.more_menu_model)

    def _show_stream_info(self):
        try:
            info = self.player.get_stream_debug()
        except Exception as e:
            info = f"Failed to read stream info: {e}"

        label = Gtk.Label(label=info)
        label.set_selectable(True)
        label.set_wrap(True)
        label.set_xalign(0.0)
        label.add_css_class("monospace")
        label.set_margin_top(4)

        dialog = Adw.MessageDialog(
            transient_for=self.get_root(),
            heading="Stream Info",
        )
        dialog.set_extra_child(label)
        dialog.add_response("close", "Close")
        dialog.add_response("copy", "Copy")
        dialog.set_default_response("close")
        dialog.set_close_response("close")

        def on_response(dg, response_id):
            if response_id == "copy":
                try:
                    full = self.player.get_stream_debug(full=True)
                except Exception:
                    full = info
                Gdk.Display.get_default().get_clipboard().set(full)
                show_toast(self, "Stream info copied")
            dg.destroy()

        dialog.connect("response", on_response)
        dialog.present()

    def on_scale_change_value(self, scale, scroll, value):
        if self.player.duration > 0:
            self.player.seek(value)
        return False

    def on_scale_scroll(self, controller, dx, dy):
        if self.player.duration <= 0:
            return False

        adj = self.scale.get_adjustment()
        val = adj.get_value()

        step = 2.0
        new_val = val - (dy * step)
        new_val = max(0, min(new_val, self.player.duration))
        adj.set_value(new_val)

        if self._scroll_seek_id:
            GLib.source_remove(self._scroll_seek_id)

        self._scroll_seek_id = GLib.timeout_add(100, self._do_scroll_seek, new_val)
        return True

    def _do_scroll_seek(self, value):
        self.player.seek(value, flush=True)
        self._scroll_seek_id = None
        return False

    def _load_css(self):
        css = """ 
        .progress-scale {
            padding-left: 0;
            padding-right: 0;
        }
        .progress-scale trough { 
            min-height: 6px; 
            border-radius: 4px; 
            background-color: alpha(@window_fg_color, 0.2); 
        } 
        .progress-scale highlight { 
            min-height: 4px; 
            border-radius: 2px; 
            background-color: @accent_color; 
        } 
        .progress-scale slider {
            border-radius: 50%; 
            background-color: @window_fg_color; 
            box-shadow: 0 1px 3px rgba(0, 0, 0, 0.4); 
            opacity: 0; 
            transition: opacity 150ms ease; 
        } 
        .progress-scale:hover slider { 
            opacity: 1; 
        } 
        .cover-visualizer { 
            transform: translateY(14px); 
        } 
        """
        provider = Gtk.CssProvider()
        provider.load_from_data(css.encode("utf-8"))
        display = Gdk.Display.get_default()
        if display:
            Gtk.StyleContext.add_provider_for_display(
                display, provider, Gtk.STYLE_PROVIDER_PRIORITY_APPLICATION
            )

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
        reveal = (
            self._pointer_over_cover
            or self._touch_reveal
        )
        self._lyrics_toggle_revealer.set_reveal_child(reveal)
        self._lyrics_toggle_revealer.set_can_target(reveal)

    def _on_cover_enter(self, *_):
        self._pointer_over_cover = True
        self._sync_lyrics_toggle_reveal()

    def _on_cover_leave(self, *_):
        self._pointer_over_cover = False
        self._sync_lyrics_toggle_reveal()

    def _on_metadata_changed(
        self, player, title, artist, thumb_url, video_id, like_status
    ):
        track = None
        if 0 <= player.current_queue_index < len(player.queue):
            track = player.queue[player.current_queue_index]

        if not title and track:
            title = track.get("title", "Not Playing")
        if not artist and track:
            artist = track.get("artist", "")
        if not thumb_url and track:
            thumb_url = track.get("thumb") or (track.get("thumbnails", [{}])[-1].get("url") if track.get("thumbnails") else None)
        if not video_id and track:
            video_id = track.get("videoId")
        if (not like_status or like_status == "INDIFFERENT") and track:
            like_status = track.get("likeStatus", "INDIFFERENT")

        self.title_label.set_label(title or "Not Playing")

        while child := self.artists_box.get_first_child():
            self.artists_box.remove(child)

        artists_list = track.get("artists", []) if track else []

        if artists_list and isinstance(artists_list, list):
            for i, art in enumerate(artists_list):
                if isinstance(art, dict):
                    name = art.get("name", "")
                    aid = art.get("id")
                else:
                    name = str(art)
                    aid = None

                btn = Gtk.Button()
                btn.add_css_class("flat")
                btn.add_css_class("link-btn")
                btn.set_has_frame(False)

                lbl = Gtk.Label(label=name)
                lbl.add_css_class("heading")
                lbl.set_opacity(0.7)
                btn.set_child(lbl)

                if aid and self.on_artist_click:
                    btn.connect(
                        "clicked",
                        lambda _b, a_id=aid, a_name=name: self._on_single_artist_clicked(
                            a_id, a_name
                        ),
                    )

                self.artists_box.append(btn)

                if i < len(artists_list) - 1:
                    sep = Gtk.Label(label=", ")
                    sep.add_css_class("heading")
                    sep.set_opacity(0.7)
                    self.artists_box.append(sep)
        else:
            lbl = Gtk.Label(label=artist or "Unknown Artist")
            lbl.add_css_class("heading")
            lbl.set_opacity(0.7)
            self.artists_box.append(lbl)

        if thumb_url:
            self.cover_img.video_id = video_id
            self.cover_img.load_url(thumb_url)
        else:
            self.cover_img.set_paintable(None)

        if video_id:
            self.like_btn.set_data(video_id, like_status or "INDIFFERENT")
            self.like_btn.set_visible(True)
        else:
            self.like_btn.set_visible(False)

        self._refresh_more_menu()

        if video_id and self.player.duration <= 0:
            self._is_buffering_spinner = True
            self._play_stack.set_visible_child_name("spinner")
            self.play_btn.set_sensitive(False)
            self.scale.set_sensitive(False)

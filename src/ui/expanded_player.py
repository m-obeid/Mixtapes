import threading
import weakref
from gi.repository import Gtk, Adw, GObject, GLib, Pango, Gdk, Gio
from ui.utils import AsyncPicture, LikeButton, MarqueeLabel, show_toast
from ui.queue_panel import QueuePanel
from ui.widgets.lyrics_view import LyricsView
from ui.util_classes import ScrolledWindow


MAX_CAROUSEL_COVERS = 31
CAROUSEL_PRELOAD_RADIUS = 5


class ExpandedPlayer(Gtk.Box):
    @GObject.Signal
    def dismiss(self):
        pass

    def _make_cover(self):
        img = AsyncPicture(crop_to_square=True, player=self.player)
        img.add_css_class("rounded")
        img.set_halign(Gtk.Align.FILL)
        img.set_valign(Gtk.Align.FILL)
        img.set_hexpand(False)
        img.set_vexpand(True)
        img.set_content_fit(Gtk.ContentFit.COVER)
        return img

    def __init__(self, player, on_artist_click=None, on_album_click=None, **kwargs):
        super().__init__(orientation=Gtk.Orientation.VERTICAL, **kwargs)
        self.player = player
        self.on_artist_click = on_artist_click
        self.on_album_click = on_album_click
        self._is_buffering_spinner = False

        self.view_stack = Adw.ViewStack()
        self.view_stack.set_vexpand(True)
        # Adw.ViewStack doesn't support transition types in all versions,
        # and it's handled by libadwaita's animation system.

        self.switcher_title = Adw.ViewSwitcherTitle()
        self.switcher_title.set_stack(self.view_stack)

        self.set_margin_top(32)
        self.append(self.view_stack)

        self.switcher = Adw.ViewSwitcher()
        self.switcher.set_stack(self.view_stack)
        self.switcher.set_policy(Adw.ViewSwitcherPolicy.WIDE)
        self.switcher.set_halign(Gtk.Align.CENTER)
        self.switcher.set_margin_top(8)
        self.switcher.set_margin_bottom(8)
        self.append(self.switcher)

        # ==========================================
        # PAGE 1: THE PLAYER VIEW
        # ==========================================
        self.player_scroll = ScrolledWindow()
        self.player_scroll.set_policy(Gtk.PolicyType.NEVER, Gtk.PolicyType.AUTOMATIC)
        self.player_scroll.set_propagate_natural_height(True)

        main_box = Gtk.Box(orientation=Gtk.Orientation.VERTICAL, spacing=12)
        main_box.set_margin_top(12)
        main_box.set_margin_bottom(16)

        self.covers = []
        self._cover_offset = 0
        self.cover_img = self._make_cover()  # fallback center

        self.carousel = Adw.Carousel()
        self.carousel.set_spacing(16)
        self.carousel.set_interactive(True)

        # The frame clips to show only the center cover
        cover_frame = Gtk.AspectFrame(ratio=1.0, obey_child=False)
        cover_frame.set_halign(Gtk.Align.CENTER)
        cover_frame.set_valign(Gtk.Align.CENTER)
        cover_frame.set_vexpand(True)
        cover_frame.set_hexpand(True)
        cover_frame.set_overflow(Gtk.Overflow.HIDDEN)
        cover_frame.set_child(self.carousel)
        cover_frame.set_margin_start(24)
        cover_frame.set_margin_end(24)
        self._cover_frame = cover_frame

        # Add tap gesture for album navigation
        cover_click = Gtk.GestureClick()
        cover_click.connect("pressed", self._on_cover_pressed)
        cover_click.connect("released", self._on_cover_tapped)
        cover_frame.add_controller(cover_click)

        self._ignore_page_change = False
        # The carousel emits notify::position both for user swipes AND for
        # layout settles after we programmatically scroll_to a new page.
        # The two are indistinguishable from the signal alone, so the
        # `_ignore_page_change` window can race with a late settle event
        # and cause `_do_jump(0)` to fire — snapping playback back to the
        # first track right after the user clicked a different one. To
        # reject those late settles, we only act on position-changed when
        # we've seen a real user gesture (drag or click) on the carousel
        # within the recent past.
        import time as _time
        self._carousel_user_input_at = 0.0
        self._carousel_user_input_window = 0.8  # seconds
        self._time = _time
        drag = Gtk.GestureDrag()
        drag.set_propagation_phase(Gtk.PropagationPhase.CAPTURE)
        drag.connect("drag-begin", self._on_carousel_user_input)
        self.carousel.add_controller(drag)
        click = Gtk.GestureClick()
        click.set_propagation_phase(Gtk.PropagationPhase.CAPTURE)
        click.connect("pressed", self._on_carousel_user_input)
        self.carousel.add_controller(click)
        # Trackpad / mouse wheel swipes don't fire the gesture
        # controllers above — they go through EventControllerScroll
        # (which Adw.Carousel uses internally to scroll its pages).
        # Listen on CAPTURE so we stamp the user-input timestamp before
        # the carousel consumes the event.
        scroll = Gtk.EventControllerScroll.new(
            Gtk.EventControllerScrollFlags.BOTH_AXES
        )
        scroll.set_propagation_phase(Gtk.PropagationPhase.CAPTURE)
        scroll.connect("scroll", self._on_carousel_user_input)
        self.carousel.add_controller(scroll)
        self.carousel.connect("notify::position", self._on_carousel_position_changed)
        self.connect("map", self._on_map)

        main_box.append(cover_frame)

        # Metadata & Like
        meta_row = Gtk.Box(orientation=Gtk.Orientation.HORIZONTAL, spacing=12)
        meta_row.set_halign(Gtk.Align.FILL)
        meta_row.set_margin_start(32)
        meta_row.set_margin_end(32)

        text_box = Gtk.Box(orientation=Gtk.Orientation.VERTICAL, spacing=2)
        text_box.set_hexpand(True)
        text_box.set_valign(Gtk.Align.CENTER)

        # --- Marquee Title ---
        self.title_label = MarqueeLabel()
        self.title_label.set_label("Not Playing")
        self.title_label.add_css_class("title-3")

        self.artist_btn = Gtk.Button()
        self.artist_btn.add_css_class("flat")
        self.artist_btn.add_css_class("link-btn")
        self.artist_btn.set_halign(Gtk.Align.START)
        self.artist_btn.set_has_frame(False)
        self.artist_btn.connect("clicked", self._on_artist_btn_clicked)

        self.artist_label = Gtk.Label(label="")
        self.artist_label.add_css_class("heading")
        self.artist_label.set_opacity(0.7)
        self.artist_label.set_ellipsize(Pango.EllipsizeMode.END)
        self.artist_label.set_halign(Gtk.Align.START)

        self.artist_btn.set_child(self.artist_label)

        text_box.append(self.title_label)
        text_box.append(self.artist_btn)

        self.like_btn = LikeButton(self.player.client, None)
        self.like_btn.set_visible(False)
        self.like_btn.set_valign(Gtk.Align.CENTER)

        # More menu (3-dot)
        self.more_menu_model = Gio.Menu()
        self.more_btn = Gtk.MenuButton(icon_name="view-more-symbolic")
        self.more_btn.add_css_class("flat")
        self.more_btn.add_css_class("circular")
        self.more_btn.set_valign(Gtk.Align.CENTER)
        self.more_btn.set_menu_model(self.more_menu_model)

        # Action group for the more menu
        self.ep_action_group = Gio.SimpleActionGroup()
        self.insert_action_group("ep", self.ep_action_group)

        a_add = Gio.SimpleAction.new("add_to_playlist", GLib.VariantType.new("s"))
        a_add.connect("activate", self._on_add_to_playlist)
        self.ep_action_group.add_action(a_add)

        a_show_add = Gio.SimpleAction.new("show_add_to_playlist", None)
        a_show_add.connect("activate", self._on_show_add_to_playlist)
        self.ep_action_group.add_action(a_show_add)

        a_radio = Gio.SimpleAction.new("start_radio", None)
        a_radio.connect("activate", self._on_start_radio)
        self.ep_action_group.add_action(a_radio)

        a_copy = Gio.SimpleAction.new("copy_link", None)
        a_copy.connect("activate", self._on_copy_link)
        self.ep_action_group.add_action(a_copy)

        a_dl = Gio.SimpleAction.new("download", None)
        a_dl.connect("activate", self._on_download)
        self.ep_action_group.add_action(a_dl)

        a_refresh = Gio.SimpleAction.new("refresh_metadata", None)
        a_refresh.connect("activate", self._on_refresh_metadata)
        self.ep_action_group.add_action(a_refresh)

        meta_row.append(text_box)
        meta_row.append(self.like_btn)
        main_box.append(meta_row)

        progress_box = Gtk.Box(orientation=Gtk.Orientation.VERTICAL, spacing=0)
        progress_box.set_margin_start(16)
        progress_box.set_margin_end(16)
        self.scale = Gtk.Scale(orientation=Gtk.Orientation.HORIZONTAL)
        self.scale.set_range(0, 100)
        self.scale.add_css_class("progress-scale")
        self.scale.connect("change-value", self.on_scale_change_value)
        progress_box.append(self.scale)

        timings_box = Gtk.Box(orientation=Gtk.Orientation.HORIZONTAL)
        timings_box.set_margin_start(8)
        timings_box.set_margin_end(8)
        timings_box.set_margin_top(0)
        self.pos_label = Gtk.Label(label="0:00")
        self.pos_label.add_css_class("caption")
        self.pos_label.add_css_class("numeric")

        dur_spacer = Gtk.Box()
        dur_spacer.set_hexpand(True)

        self.dur_label = Gtk.Label(label="0:00")
        self.dur_label.add_css_class("caption")
        self.dur_label.add_css_class("numeric")

        timings_box.append(self.pos_label)
        timings_box.append(dur_spacer)
        timings_box.append(self.dur_label)
        progress_box.append(timings_box)
        main_box.append(progress_box)

        # Media Controls
        controls_box = Gtk.Box(orientation=Gtk.Orientation.HORIZONTAL, spacing=12)
        controls_box.set_halign(Gtk.Align.CENTER)
        controls_box.set_margin_top(20)
        controls_box.set_margin_start(24)
        controls_box.set_margin_end(24)

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

        self.prev_btn = Gtk.Button(icon_name="media-skip-backward-symbolic")
        self.prev_btn.set_size_request(48, 48)
        self.prev_btn.add_css_class("circular")
        self.prev_btn.set_valign(Gtk.Align.CENTER)
        self.prev_btn.connect("clicked", lambda x: self.player.previous())

        # 3-dot menu button (balances vol_btn on the left)
        self.more_btn.set_valign(Gtk.Align.CENTER)

        self.play_btn = Gtk.Button()
        self.play_btn.set_size_request(64, 64)
        self.play_btn.add_css_class("circular")
        self.play_btn.add_css_class("suggested-action")
        self.play_btn.set_valign(Gtk.Align.CENTER)
        self.play_btn.connect("clicked", self.on_play_clicked)

        self.play_btn_stack = Gtk.Stack()
        self.play_btn_stack.set_transition_type(Gtk.StackTransitionType.CROSSFADE)
        self.play_btn_stack.set_transition_duration(200)

        self.play_icon = Gtk.Image.new_from_icon_name("media-playback-start-symbolic")
        self.play_icon.set_pixel_size(24)
        self.play_btn_stack.add_named(self.play_icon, "icon")

        self.play_spinner = Adw.Spinner()
        self.play_spinner.set_size_request(24, 24)
        self.play_btn_stack.add_named(self.play_spinner, "spinner")

        self.play_btn.set_child(self.play_btn_stack)

        self.next_btn = Gtk.Button(icon_name="media-skip-forward-symbolic")
        self.next_btn.set_size_request(48, 48)
        self.next_btn.add_css_class("circular")
        self.next_btn.set_valign(Gtk.Align.CENTER)
        self.next_btn.connect("clicked", lambda x: self.player.next())

        controls_box.append(self.vol_btn)
        controls_box.append(self.prev_btn)
        controls_box.append(self.play_btn)
        controls_box.append(self.next_btn)
        controls_box.append(self.more_btn)
        main_box.append(controls_box)

        self.player_scroll.set_child(main_box)
        self.view_stack.add_titled_with_icon(
            self.player_scroll, "player", "Player", "folder-music-symbolic"
        )

        # ==========================================
        # PAGE 2: THE QUEUE VIEW
        # ==========================================
        queue_box = Gtk.Box(orientation=Gtk.Orientation.VERTICAL, spacing=0)
        queue_box.set_margin_top(0)

        self.queue_panel = QueuePanel(self.player)
        self.queue_panel.set_vexpand(True)

        # Remove the internal header of QueuePanel since it already has one,
        # or maybe we want a dedicated header here?
        # Let's keep it simple for now.

        queue_box.append(self.queue_panel)
        self.view_stack.add_titled_with_icon(
            queue_box, "queue", "Queue", "music-queue-symbolic"
        )

        # ==========================================
        # PAGE 3: THE LYRICS VIEW
        # ==========================================
        self.lyrics_view = LyricsView(self.player)
        self.view_stack.add_titled_with_icon(
            self.lyrics_view, "lyrics", "Lyrics", "format-justify-fill-symbolic"
        )

        # Connect Signals
        self.player.connect("metadata-changed", self.on_metadata_changed)
        self.player.connect("progression", self.on_progression)
        self.player.connect("state-changed", self.on_state_changed)
        self.player.connect("volume-changed", self.on_volume_changed)

        # Initial state sync
        self.on_state_changed(self.player, self.player.get_state_string())

    def set_compact_mode(self, compact):
        """
        True: Mobile mode (tabbed view with Player/Queue)
        False: Desktop mode (Player view only, queue is in sidebar)
        """
        self.switcher.set_visible(compact)
        if not compact:
            self.view_stack.set_visible_child_name("player")
            self.set_margin_top(12)  # Less padding on desktop
        else:
            self.set_margin_top(32)

    def _on_map(self, widget):
        GLib.idle_add(self._center_carousel)
        # Sync like status from current queue track (may have been missed before map)
        if self.player.current_video_id and 0 <= self.player.current_queue_index < len(
            self.player.queue
        ):
            track = self.player.queue[self.player.current_queue_index]
            like_status = track.get("likeStatus", "INDIFFERENT")
            self.like_btn.set_data(self.player.current_video_id, like_status)

    def _center_carousel(self):
        self._ignore_page_change = True
        if self.cover_img and self.cover_img.get_parent() == self.carousel:
            self.carousel.scroll_to(self.cover_img, animate=False)
        self._ignore_page_change = False
        return False

    # --- SIGNAL HANDLERS ---
    def on_metadata_changed(
        self, player, title, artist, thumbnail_url, video_id, like_status
    ):
        self.title_label.set_label(title)
        self.artist_label.set_label(artist)

        if thumbnail_url:
            self.cover_img.video_id = video_id
            self.cover_img.load_url(thumbnail_url)
        else:
            self.cover_img.video_id = None
            self.cover_img.load_url(None)

        if video_id:
            self.like_btn.set_data(video_id, like_status)
            self.like_btn.set_visible(True)
        else:
            self.like_btn.set_visible(False)

        # Refresh the more menu for the new track
        self._refresh_more_menu()

        # Preload neighbor covers and sync queue
        self._sync_carousel_queue()

        # Show spinner when a new track starts loading
        if video_id and self.player.duration <= 0:
            self._is_buffering_spinner = True
            self.play_btn_stack.set_visible_child_name("spinner")
            self.play_btn.set_sensitive(False)

    def _get_track_thumb(self, index):
        """Get a thumbnail URL for a track at the given queue index."""
        if index < 0 or index >= len(self.player.queue):
            return None
        track = self.player.queue[index]
        thumb = track.get("thumb")
        if not thumb and "thumbnails" in track:
            thumbs = track.get("thumbnails", [])
            if thumbs:
                thumb = thumbs[-1]["url"]
        if thumb:
            return thumb
        return None

    def _sync_carousel_queue(self):
        """Keep a bounded carousel window around the current queue item."""
        queue_len = len(self.player.queue)
        idx = self.player.current_queue_index

        if queue_len == 0:
            self._cover_offset = 0
            while self.covers:
                cover = self.covers.pop()
                cover.video_id = None
                cover.load_url(None)
                if cover.get_parent() == self.carousel:
                    self.carousel.remove(cover)
            return
        if idx < 0 or idx >= queue_len:
            idx = 0

        self._ignore_page_change = True
        # Re-armable token: only the most recent sync's timer is allowed to
        # clear the flag. Otherwise rapid syncs (e.g. spamming next) leave
        # earlier 200ms timers in flight; the first one fires and clears the
        # flag while later syncs are still mutating the carousel, letting a
        # spurious position-changed leak through and snap playback to 0.
        self._carousel_sync_token = getattr(self, "_carousel_sync_token", 0) + 1
        token = self._carousel_sync_token

        window_len = min(queue_len, MAX_CAROUSEL_COVERS)
        half_window = window_len // 2
        max_offset = max(0, queue_len - window_len)
        self._cover_offset = min(max(idx - half_window, 0), max_offset)

        while len(self.covers) > window_len:
            cover = self.covers.pop()
            cover.video_id = None
            cover.load_url(None)
            if cover.get_parent() == self.carousel:
                self.carousel.remove(cover)

        while len(self.covers) < window_len:
            cover = self._make_cover()
            self.covers.append(cover)
            self.carousel.append(cover)

        page_idx = idx - self._cover_offset
        if 0 <= page_idx < len(self.covers):
            self.cover_img = self.covers[page_idx]

        self._last_lazy_idx = -1  # Force reload of the new window's covers
        self._lazy_load_covers_around(page_idx)

        if 0 <= page_idx < len(self.covers):
            self.carousel.scroll_to(self.covers[page_idx], animate=False)

        GLib.timeout_add(200, self._allow_page_change, token)

    def _lazy_load_covers_around(self, center_page_idx):
        old_center = getattr(self, "_last_lazy_idx", -1)
        if center_page_idx == old_center:
            return
        self._last_lazy_idx = center_page_idx

        R = CAROUSEL_PRELOAD_RADIUS
        total = len(self.covers)
        if total == 0:
            return
        new_lo = max(0, center_page_idx - R)
        new_hi = min(total - 1, center_page_idx + R)

        def _set_cover(page_idx):
            cover = self.covers[page_idx]
            queue_idx = self._cover_offset + page_idx
            thumb = self._get_track_thumb(queue_idx)
            if thumb:
                if not cover.get_visible():
                    cover.set_visible(True)
                if cover.url != thumb:
                    cover.video_id = self.player.queue[queue_idx].get("videoId")
                    cover.load_url(thumb)
            else:
                if cover.get_visible():
                    cover.set_visible(False)
                cover.video_id = None
                if cover.url is not None:
                    cover.load_url(None)

        def _clear_cover(page_idx):
            cover = self.covers[page_idx]
            cover.video_id = None
            if cover.url is not None:
                cover.load_url(None)

        # Load the new window.
        for i in range(new_lo, new_hi + 1):
            _set_cover(i)

        # Clear what just left the window. When old_center is -1 (forced
        # invalidation from _sync_carousel_queue, or first call), there's
        # nothing to clear.
        if old_center >= 0:
            old_lo = max(0, old_center - R)
            old_hi = min(total - 1, old_center + R)
            for page_idx in range(old_lo, old_hi + 1):
                if new_lo <= page_idx <= new_hi:
                    continue
                _clear_cover(page_idx)

    def _allow_page_change(self, token=None):
        # Only the latest sync's timer may clear the flag.
        if token is not None and token != getattr(self, "_carousel_sync_token", 0):
            return False
        self._ignore_page_change = False
        return False

    def on_progression(self, player, pos, dur):
        self.scale.set_range(0, dur)
        self.scale.set_value(pos)
        self.pos_label.set_label(self._format_time(pos))
        self.dur_label.set_label(self._format_time(dur))

        # Hide spinner once we have valid duration
        if getattr(self, "_is_buffering_spinner", False) and dur > 0:
            if self.player.get_state_string() == "playing":
                self._is_buffering_spinner = False
                self.play_btn.set_sensitive(True)
                self.play_btn_stack.set_visible_child_name("icon")
                self.play_icon.set_from_icon_name("media-playback-pause-symbolic")

    def on_scale_change_value(self, scale, scroll, value):
        if self.player.duration > 0:
            self.player.seek(value)
        return False

    def _format_time(self, seconds):
        if seconds < 0:
            return "0:00"
        m = int(seconds // 60)
        s = int(seconds % 60)
        return f"{m}:{s:02d}"

    def on_play_clicked(self, btn):
        if self.player.get_state_string() == "playing":
            self.player.pause()
        else:
            self.player.play()

    def on_state_changed(self, player, state):
        if state == "queue-updated":
            self._sync_carousel_queue()
            return

        if state == "loading":
            self.play_btn_stack.set_visible_child_name("spinner")
            self.play_btn.set_sensitive(False)
            self._is_buffering_spinner = True
            return

        if state == "playing" and self.player.duration <= 0:
            # We are playing but buffering stream-keep spinner active until duration > 0
            self.play_btn_stack.set_visible_child_name("spinner")
            self.play_btn.set_sensitive(False)
            self._is_buffering_spinner = True
            return

        if (
            getattr(self, "_is_buffering_spinner", False)
            and self.player.duration <= 0
            and state in ("paused", "stopped")
        ):
            # Still buffering-keep spinner visible
            return

        self._is_buffering_spinner = False
        self.play_btn_stack.set_visible_child_name("icon")
        self.play_btn.set_sensitive(True)
        icon = (
            "media-playback-pause-symbolic"
            if state == "playing"
            else "media-playback-start-symbolic"
        )
        self.play_icon.set_from_icon_name(icon)

    def on_volume_scale_changed(self, scale):
        if getattr(self, "_updating_volume", False):
            return
        self.player.set_volume(scale.get_value())

    def on_volume_changed(self, player, volume, muted):
        display_volume = 0.0 if muted else volume

        # Guard against feedback loop: set_value triggers value-changed
        # which calls set_volume which emits volume-changed again
        self._updating_volume = True
        self.volume_scale.set_value(display_volume)
        self._updating_volume = False

        # Update Icon
        if muted or volume == 0:
            self.vol_btn.set_icon_name("audio-volume-muted-symbolic")
        elif volume < 0.33:
            self.vol_btn.set_icon_name("audio-volume-low-symbolic")
        elif volume < 0.66:
            self.vol_btn.set_icon_name("audio-volume-medium-symbolic")
        else:
            self.vol_btn.set_icon_name("audio-volume-high-symbolic")

    def _on_artist_btn_clicked(self, btn):
        if self.on_artist_click:
            self.on_artist_click()
        self.emit("dismiss")

    def _on_cover_pressed(self, gesture, n_press, x, y):
        self._press_x = x
        self._press_y = y

    def _on_cover_tapped(self, gesture, n_press, x, y):
        # Ignore false clicks generated during a swiping drag
        if hasattr(self, "_press_x"):
            if abs(x - self._press_x) > 15 or abs(y - self._press_y) > 15:
                # User was swiping the carousel
                return

        if self.on_album_click:
            self.on_album_click()
        self.emit("dismiss")

    # --- ADW.CAROUSEL GESTURE HANDLERS ---

    # ── More menu (3-dot) handlers ──────────────────────────────────────────

    def _refresh_more_menu(self):
        # Rebuild the menu model from scratch and re-hand it to the button.
        # Mutating the existing model in place causes GtkPopoverMenu's
        # internal GtkStack to accumulate stale submenu pages, producing
        # "duplicate child name in GtkStack" warnings.
        self.more_menu_model = Gio.Menu()
        self.more_btn.set_menu_model(self.more_menu_model)

        vid = self.player.current_video_id

        action_section = Gio.Menu()

        from ui.utils import is_online

        _online = is_online()

        # Start Radio (online only)
        if vid and _online:
            action_section.append("Start Radio", "ep.start_radio")

        # Add to Playlist (online only) — opens the custom popover
        if vid and _online and self.player.client.get_editable_playlists():
            action_section.append("Add to Playlist…", "ep.show_add_to_playlist")

        # Download (online only)
        if vid and _online and not self.player.download_manager.is_downloaded(vid):
            action_section.append("Download", "ep.download")

        # Refresh metadata (online only)
        if vid and _online:
            action_section.append("Refresh Metadata", "ep.refresh_metadata")

        if action_section.get_n_items() > 0:
            self.more_menu_model.append_section(None, action_section)

        # Clipboard (online only)
        if vid and _online:
            clip_section = Gio.Menu()
            clip_section.append("Copy Song Link", "ep.copy_link")
            self.more_menu_model.append_section(None, clip_section)

    def _on_add_to_playlist(self, action, param):
        self._do_add_to_playlist(param.get_string())

    def _on_show_add_to_playlist(self, action, param):
        from ui.widgets.add_to_playlist import AddToPlaylistPopover
        pop = AddToPlaylistPopover(
            self.player, on_select=self._do_add_to_playlist, parent=self.more_btn
        )
        pop.popup()

    def _do_add_to_playlist(self, target_pid):
        vid = self.player.current_video_id
        if not target_pid or not vid:
            return

        from ui.widgets.add_to_playlist import mark_playlist_used
        mark_playlist_used(target_pid)

        weak_self = weakref.ref(self)
        client = self.player.client
        def _thread():
            # add_playlist_items auto-swaps OMV→ATV for single-item
            # adds, so even if the player's own swap hasn't run yet,
            # the audio version still ends up in the playlist.
            success = client.add_playlist_items(target_pid, [vid])
            msg = "Added to playlist" if success else "Failed to add"
            GLib.idle_add(lambda: weak_self()._show_toast(msg) if weak_self() else None)

        threading.Thread(target=_thread, daemon=True).start()

    def _on_start_radio(self, action, param):
        vid = self.player.current_video_id
        if vid:
            self.player.start_radio(video_id=vid)

    def _on_download(self, action, param):
        idx = self.player.current_queue_index
        if 0 <= idx < len(self.player.queue):
            track = self.player.queue[idx]
            root = self.get_root()
            if root and hasattr(root, "download_track"):
                root.download_track(track)

    def _on_copy_link(self, action, param):
        vid = self.player.current_video_id
        if vid:
            Gdk.Display.get_default().get_clipboard().set(
                f"https://music.youtube.com/watch?v={vid}"
            )
            self._show_toast("Link copied")

    def _on_refresh_metadata(self, action, param):
        vid = self.player.current_video_id
        idx = self.player.current_queue_index
        if not vid or idx < 0 or idx >= len(self.player.queue):
            return
        self._show_toast("Refreshing metadata...")

        def _fetch():
            try:
                wp = self.player.client.get_watch_playlist(video_id=vid, limit=1)
                wp_tracks = wp.get("tracks", [])
                if wp_tracks:
                    fresh = wp_tracks[0]
                    track = self.player.queue[idx]
                    if track.get("videoId") != vid:
                        return
                    if fresh.get("title"):
                        track["title"] = fresh["title"]
                    if fresh.get("artists"):
                        track["artists"] = fresh["artists"]
                        track["artist"] = ", ".join(
                            a.get("name", "") for a in fresh["artists"] if a
                        )
                    if fresh.get("album"):
                        track["album"] = fresh["album"]
                    if fresh.get("thumbnail"):
                        thumbs = fresh["thumbnail"]
                        if isinstance(thumbs, list) and thumbs:
                            track["thumb"] = thumbs[-1].get("url", "")
                            track["thumbnails"] = thumbs
                    if getattr(self.player, "discord_rpc", None):
                        self.player.discord_rpc.update()
                    GLib.idle_add(self._show_toast, "Metadata refreshed")
                else:
                    GLib.idle_add(self._show_toast, "No metadata found")
            except Exception as e:
                print(f"Refresh metadata error: {e}")
                GLib.idle_add(self._show_toast, "Failed to refresh metadata")

        threading.Thread(target=_fetch, daemon=True).start()

    def _show_toast(self, message):
        show_toast(self, message)

    # ── Carousel gesture handlers ─────────────────────────────────────────

    def _on_carousel_user_input(self, *_):
        """Stamp the timestamp of the last real user interaction with
        the carousel (drag or click). `_do_jump` requires a recent
        stamp to act, so layout-driven `notify::position` settles can
        no longer be misinterpreted as user swipes."""
        self._carousel_user_input_at = self._time.monotonic()

    def _on_carousel_position_changed(self, carousel, param):
        if getattr(self, "_ignore_page_change", False):
            return

        pos = carousel.get_position()
        page_idx = int(round(pos))

        # Dynamically load array ranges during scroll
        if 0 <= page_idx < len(self.covers):
            self._lazy_load_covers_around(page_idx)

        # Only trigger when the carousel float position essentially reaches the target page
        if abs(pos - page_idx) > 0.001:
            return

        active_page = carousel.get_nth_page(page_idx)

        try:
            page_idx = self.covers.index(active_page)
        except ValueError:
            return

        queue_idx = self._cover_offset + page_idx
        if queue_idx != self.player.current_queue_index:
            self._ignore_page_change = True

            if 0 <= queue_idx < len(self.player.queue):

                def _do_jump(jump_idx):
                    cur = self.player.current_queue_index
                    # Require a real user gesture on the carousel (drag
                    # or click) within the recent window. Programmatic
                    # settles after queue rebuilds don't update this
                    # stamp, so spurious position emits — including the
                    # one that used to snap playback back to track 0
                    # after the user clicked a different track in the
                    # home feed — get rejected here.
                    since_input = (
                        self._time.monotonic() - self._carousel_user_input_at
                    )
                    if since_input > self._carousel_user_input_window:
                        self._ignore_page_change = False
                        return False
                    # Guard: don't override if the player is already loading a different track
                    if self.player._is_loading:
                        self._ignore_page_change = False
                        return False
                    # Real user swipes only ever move one page at a time.
                    if cur >= 0 and abs(jump_idx - cur) > 1:
                        self._ignore_page_change = False
                        return False
                    self.player.current_queue_index = jump_idx
                    self.player._play_current_index()
                    self.player.emit("state-changed", "queue-updated")
                    return False

                GLib.idle_add(_do_jump, queue_idx)

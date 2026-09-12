import time as _time
from gi.repository import Adw, Gdk, Gio, GLib, GObject, Gtk, Pango
from ui.context_menu import MenuAction, build_song_menu
from ui.queue_panel import QueuePanel
from ui.util_classes import ScrolledWindow
from ui.utils import AsyncPicture, LikeButton, MarqueeLabel, show_toast
from ui.widgets.lyrics_view import LyricsView
from ui.widgets.visualizer import Visualizer

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

        self.switcher_title = Adw.ViewSwitcherTitle()
        self.switcher_title.set_stack(self.view_stack)

        self.set_margin_top(32)
        self.append(self.view_stack)

        # ==========================================
        # TOGGLE GROUP
        # ==========================================
        self._toggle_group_is_adw = hasattr(Adw, "ToggleGroup") and hasattr(Adw, "Toggle")
        self._buttons_by_page = {}

        if self._toggle_group_is_adw:
            self.toggle_nav = Adw.ToggleGroup()
            self.toggle_nav.add_css_class("round")
            self.toggle_nav.set_halign(Gtk.Align.CENTER)
            self.toggle_nav.set_margin_top(8)
            self.toggle_nav.set_margin_bottom(8)

            t_player = Adw.Toggle(name="player", label="Player", icon_name="folder-music-symbolic")
            t_queue = Adw.Toggle(name="queue", label="Queue", icon_name="music-queue-symbolic")
            t_lyrics = Adw.Toggle(name="lyrics", label="Lyrics", icon_name="format-justify-fill-symbolic")

            self.toggle_nav.add(t_player)
            self.toggle_nav.add(t_queue)
            self.toggle_nav.add(t_lyrics)

            self.toggle_nav.connect("notify::active-name", self._on_toggle_group_changed)
            self.append(self.toggle_nav)
        else:
            self.toggle_nav = Gtk.Box(orientation=Gtk.Orientation.HORIZONTAL)
            self.toggle_nav.add_css_class("linked")
            self.toggle_nav.set_halign(Gtk.Align.CENTER)
            self.toggle_nav.set_margin_top(8)
            self.toggle_nav.set_margin_bottom(8)

            pages = [
                ("player", "folder-music-symbolic", "Player"),
                ("queue", "music-queue-symbolic", "Queue"),
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

            self.append(self.toggle_nav)

        self.view_stack.connect("notify::visible-child-name", self._on_stack_child_changed)

        # ==========================================
        # PLAYER VIEW
        # ==========================================
        self.player_scroll = ScrolledWindow()
        self.player_scroll.set_policy(Gtk.PolicyType.NEVER, Gtk.PolicyType.AUTOMATIC)
        self.player_scroll.set_propagate_natural_height(True)

        main_box = Gtk.Box(orientation=Gtk.Orientation.VERTICAL, spacing=12)
        main_box.set_margin_top(12)
        main_box.set_margin_bottom(24)

        self.covers = []
        self._cover_offset = 0
        self.cover_img = self._make_cover()

        self.carousel = Adw.Carousel()
        self.carousel.set_spacing(16)
        self.carousel.set_interactive(True)

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

        cover_click = Gtk.GestureClick()
        cover_click.connect("pressed", self._on_cover_pressed)
        cover_click.connect("released", self._on_cover_tapped)
        cover_frame.add_controller(cover_click)

        self._ignore_page_change = False
        self._carousel_user_input_at = 0.0
        self._carousel_user_input_window = 0.8
        self._time = _time

        drag = Gtk.GestureDrag()
        drag.set_propagation_phase(Gtk.PropagationPhase.CAPTURE)
        drag.connect("drag-begin", self._on_carousel_user_input)
        self.carousel.add_controller(drag)

        click = Gtk.GestureClick()
        click.set_propagation_phase(Gtk.PropagationPhase.CAPTURE)
        click.connect("pressed", self._on_carousel_user_input)
        self.carousel.add_controller(click)

        scroll = Gtk.EventControllerScroll.new(Gtk.EventControllerScrollFlags.BOTH_AXES)
        scroll.set_propagation_phase(Gtk.PropagationPhase.CAPTURE)
        scroll.connect("scroll", self._on_carousel_user_input)
        self.carousel.add_controller(scroll)

        self.carousel.connect("notify::position", self._on_carousel_position_changed)
        self.connect("map", self._on_map)

        main_box.append(cover_frame)

        # Metadata & Like
        meta_row = Gtk.Box(orientation=Gtk.Orientation.HORIZONTAL, spacing=12)
        meta_row.set_hexpand(True)
        meta_row.set_valign(Gtk.Align.CENTER)
        meta_row.set_margin_start(24)
        meta_row.set_margin_end(24)
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

        self.more_menu_model = Gio.Menu()
        self.more_btn = Gtk.MenuButton(icon_name="view-more-symbolic")
        self.more_btn.add_css_class("flat")
        self.more_btn.add_css_class("circular")
        self.more_btn.set_valign(Gtk.Align.CENTER)
        self.more_btn.set_menu_model(self.more_menu_model)

        self._refresh_more_menu()

        meta_row.append(text_box)
        meta_row.append(self.like_btn)
        main_box.append(meta_row)

        progress_box = Gtk.Box(orientation=Gtk.Orientation.VERTICAL, spacing=0)
        progress_box.set_margin_start(24)
        progress_box.set_margin_end(24)
        self.scale = Gtk.Scale(orientation=Gtk.Orientation.HORIZONTAL)
        self.scale.set_range(0, 100)
        self.scale.add_css_class("progress-scale")
        self.scale.connect("change-value", self.on_scale_change_value)
        progress_box.append(self.scale)

        timings_box = Gtk.Box(orientation=Gtk.Orientation.HORIZONTAL)
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

        # Bars sit behind the progress bar and transport row, same as the
        # desktop cover view. The visualizer is the overlay's main child so
        # the controls paint on top of the bars, and the 85px band is pinned
        # to the bottom to match the height the bars get on desktop. Letting
        # them fill instead would run them up past the play button, since the
        # mobile transport row is ~35px taller.
        self.visualizer = Visualizer(self.player, height=85)
        self.visualizer.set_hexpand(True)
        self.visualizer.set_valign(Gtk.Align.END)
        self.visualizer.set_can_target(False)
        self.visualizer.add_css_class("player-visualizer")

        controls_content = Gtk.Box(orientation=Gtk.Orientation.VERTICAL)
        controls_content.set_hexpand(True)
        controls_content.append(progress_box)
        controls_content.append(controls_box)

        self.controls_overlay = Gtk.Overlay()
        self.controls_overlay.set_hexpand(True)
        self.controls_overlay.set_child(self.visualizer)
        self.controls_overlay.add_overlay(controls_content)
        # Without this the overlay measures only the bars, and the scrolled
        # player view squeezes it to that 85px minimum when the window is
        # short, clipping the bottom off the transport row.
        self.controls_overlay.set_measure_overlay(controls_content, True)

        main_box.append(self.controls_overlay)

        # AdwBottomSheet sizes the drawer to its child's NATURAL height (it
        # ignores vexpand), so the sheet used to stop wherever the cover and
        # controls happened to end. This probe claims a natural height taller
        # than any window while keeping a near-zero minimum: a scrolled
        # window propagates its child's natural height but not its minimum.
        # A plain height request would raise the minimum too, which pins the
        # window's own minimum size and stops it shrinking.
        #
        # It rides in an overlay rather than in main_box because a box would
        # hand it real space and squeeze the cover; overlay children all get
        # the same allocation, and set_measure_overlay folds its height into
        # the overlay's measurement.
        self._height_probe = Gtk.ScrolledWindow()
        self._height_probe.set_policy(Gtk.PolicyType.NEVER, Gtk.PolicyType.EXTERNAL)
        self._height_probe.set_propagate_natural_height(True)
        self._height_probe.set_can_target(False)
        probe_filler = Gtk.Box()
        probe_filler.set_size_request(-1, 3000)
        self._height_probe.set_child(probe_filler)
        self._height_probe.set_visible(False)

        page_overlay = Gtk.Overlay()
        page_overlay.set_child(main_box)
        page_overlay.add_overlay(self._height_probe)
        page_overlay.set_measure_overlay(self._height_probe, True)

        self.player_scroll.set_child(page_overlay)
        self.view_stack.add_titled_with_icon(
            self.player_scroll, "player", "Player", "folder-music-symbolic"
        )

        # ==========================================
        # QUEUE VIEW
        # ==========================================
        queue_box = Gtk.Box(orientation=Gtk.Orientation.VERTICAL, spacing=0)
        queue_box.set_margin_top(0)

        self.queue_panel = QueuePanel(self.player)
        self.queue_panel.set_vexpand(True)

        queue_box.append(self.queue_panel)
        self.view_stack.add_titled_with_icon(
            queue_box, "queue", "Queue", "music-queue-symbolic"
        )

        # ==========================================
        # LYRICS VIEW
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

        self.on_state_changed(self.player, self.player.get_state_string())

    def _on_toggle_group_changed(self, group, _param):
        name = group.get_active_name()
        if name and self.view_stack.get_visible_child_name() != name:
            self.view_stack.set_visible_child_name(name)

    def _on_fallback_button_toggled(self, button, page_name):
        if button.get_active() and self.view_stack.get_visible_child_name() != page_name:
            self.view_stack.set_visible_child_name(page_name)

    def _on_stack_child_changed(self, stack, _param):
        name = stack.get_visible_child_name()
        if not name:
            return

        if self._toggle_group_is_adw:
            if self.toggle_nav.get_active_name() != name:
                self.toggle_nav.set_active_name(name)
        else:
            btn = self._buttons_by_page.get(name)
            if btn and not btn.get_active():
                btn.set_active(True)

    def set_compact_mode(self, compact):
        self.toggle_nav.set_visible(compact)
        if not compact:
            self.view_stack.set_visible_child_name("player")
            self.set_margin_top(12)
        else:
            self.set_margin_top(32)

    def _sync_height_probe(self):
        """The probe only earns its keep inside the bottom sheet, which is
        the one parent here that sizes to natural height. As the desktop
        stack's page this widget fills its parent, so keep the inflated
        height out of that branch's measurement."""
        parent = self.get_parent()
        in_sheet = False
        while parent is not None:
            if isinstance(parent, Adw.BottomSheet):
                in_sheet = True
                break
            parent = parent.get_parent()
        if self._height_probe.get_visible() != in_sheet:
            self._height_probe.set_visible(in_sheet)

    def _on_map(self, widget):
        self._sync_height_probe()
        GLib.idle_add(self._center_carousel)
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

    def _on_single_artist_clicked(self, aid, name):
        if self.on_artist_click:
            try:
                self.on_artist_click(aid, name)
            except TypeError:
                self.on_artist_click()
        self.emit("dismiss")

    # ── Signal Handlers ───────────────────────────────────────────────────────

    def on_metadata_changed(
        self, player, title, artist, thumbnail_url, video_id=None, like_status=None
    ):
        self.title_label.set_label(title)

        while child := self.artists_box.get_first_child():
            self.artists_box.remove(child)

        track = None
        if 0 <= player.current_queue_index < len(player.queue):
            track = player.queue[player.current_queue_index]

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

        if thumbnail_url:
            self.cover_img.video_id = video_id
            self.cover_img.load_url(thumbnail_url)
        else:
            self.cover_img.video_id = None
            self.cover_img.load_url(None)

        if video_id:
            self.like_btn.set_data(video_id, like_status or "INDIFFERENT")
            self.like_btn.set_visible(True)
        else:
            self.like_btn.set_visible(False)

        self._refresh_more_menu()
        self._sync_carousel_queue()

        if video_id and self.player.duration <= 0:
            self._is_buffering_spinner = True
            self.play_btn_stack.set_visible_child_name("spinner")
            self.play_btn.set_sensitive(False)

    def _get_track_thumb(self, index):
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

        self._last_lazy_idx = -1
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

        for i in range(new_lo, new_hi + 1):
            _set_cover(i)

        if old_center >= 0:
            old_lo = max(0, old_center - R)
            old_hi = min(total - 1, old_center + R)
            for page_idx in range(old_lo, old_hi + 1):
                if new_lo <= page_idx <= new_hi:
                    continue
                _clear_cover(page_idx)

    def _allow_page_change(self, token=None):
        if token is not None and token != getattr(self, "_carousel_sync_token", 0):
            return False
        self._ignore_page_change = False
        return False

    def on_progression(self, player, pos, dur):
        self.scale.set_range(0, dur)
        self.scale.set_value(pos)
        self.pos_label.set_label(self._format_time(pos))
        self.dur_label.set_label(self._format_time(dur))

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
            self.play_btn_stack.set_visible_child_name("spinner")
            self.play_btn.set_sensitive(False)
            self._is_buffering_spinner = True
            return

        if (
            getattr(self, "_is_buffering_spinner", False)
            and self.player.duration <= 0
            and state in ("paused", "stopped")
        ):
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

    def _on_artist_btn_clicked(self, btn):
        if self.on_artist_click:
            self.on_artist_click()
        self.emit("dismiss")

    def _on_cover_pressed(self, gesture, n_press, x, y):
        self._press_x = x
        self._press_y = y

    def _on_cover_tapped(self, gesture, n_press, x, y):
        if hasattr(self, "_press_x"):
            if abs(x - self._press_x) > 15 or abs(y - self._press_y) > 15:
                return

        if self.on_album_click:
            self.on_album_click()
        self.emit("dismiss")

    # ── More menu (3-dot) handlers ──────────────────────────────────────────

    def _refresh_more_menu(self):
        vid = self.player.current_video_id
        idx = self.player.current_queue_index
        queue = self.player.queue or []
        track = queue[idx] if 0 <= idx < len(queue) else {"videoId": vid}

        extras = []
        if vid:
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
            prefix="ep",
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
                self._show_toast("Stream info copied")
            dg.destroy()

        dialog.connect("response", on_response)
        dialog.present()

    def _show_toast(self, message):
        show_toast(self, message)

    # ── Carousel gesture handlers ─────────────────────────────────────────

    def _on_carousel_user_input(self, *_):
        self._carousel_user_input_at = self._time.monotonic()

    def _on_carousel_position_changed(self, carousel, param):
        if getattr(self, "_ignore_page_change", False):
            return

        pos = carousel.get_position()
        page_idx = int(round(pos))

        if 0 <= page_idx < len(self.covers):
            self._lazy_load_covers_around(page_idx)

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
                    since_input = (
                        self._time.monotonic() - self._carousel_user_input_at
                    )
                    if since_input > self._carousel_user_input_window:
                        self._ignore_page_change = False
                        return False
                    if self.player._is_loading:
                        self._ignore_page_change = False
                        return False
                    if cur >= 0 and abs(jump_idx - cur) > 1:
                        self._ignore_page_change = False
                        return False
                    self.player.current_queue_index = jump_idx
                    self.player._play_current_index()
                    self.player.emit("state-changed", "queue-updated")
                    return False

                GLib.idle_add(_do_jump, queue_idx)

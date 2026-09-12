from gi.repository import Gtk, Adw, GObject, Gdk


class PlayerBar(Gtk.Box):
    __gsignals__ = {"expand-requested": (GObject.SignalFlags.RUN_FIRST, None, ())}

    def __init__(
        self, player, on_artist_click=None, on_queue_click=None, on_album_click=None
    ):
        super().__init__(orientation=Gtk.Orientation.VERTICAL, spacing=0)
        self.player = player
        self.on_artist_click = on_artist_click
        self.on_queue_click = on_queue_click
        self.on_album_click = on_album_click
        self.add_css_class("player-bar")
        self._load_css()

        self.scale = Gtk.Scale(orientation=Gtk.Orientation.HORIZONTAL)
        self.scale.set_hexpand(True)
        self.scale.set_range(0, 100)
        self.scale.add_css_class("player-scale")
        self.scale.connect("change-value", self.on_scale_change_value)
        self.append(self.scale)

        scroll_controller = Gtk.EventControllerScroll.new(
            Gtk.EventControllerScrollFlags.VERTICAL
        )
        scroll_controller.connect("scroll", self.on_scale_scroll)
        self.scale.add_controller(scroll_controller)

        content_box = Gtk.Box(orientation=Gtk.Orientation.HORIZONTAL, spacing=12)
        content_box.set_margin_top(8)
        content_box.set_margin_bottom(12)
        content_box.set_margin_start(12)
        content_box.set_margin_end(12)
        self.append(content_box)

        from ui.utils import AsyncImage, LikeButton, MarqueeLabel

        self.cover_btn = Gtk.Button()
        self.cover_btn.add_css_class("flat")
        self.cover_btn.add_css_class("link-btn")
        self.cover_btn.set_has_frame(False)
        self.cover_btn.connect("clicked", self._on_cover_btn_clicked)

        self.cover_img = AsyncImage(size=48, player=self.player)
        self.cover_img.set_pixel_size(48)

        self.cover_wrapper = Gtk.Box()
        self.cover_wrapper.set_overflow(Gtk.Overflow.HIDDEN)
        self.cover_wrapper.add_css_class("player-bar-cover")
        self.cover_wrapper.append(self.cover_img)

        self.cover_btn.set_child(self.cover_wrapper)
        content_box.append(self.cover_btn)

        meta_box = Gtk.Box(orientation=Gtk.Orientation.VERTICAL, spacing=2)
        meta_box.set_valign(Gtk.Align.CENTER)
        meta_box.set_hexpand(True)
        meta_box.set_margin_top(0)

        self.title_label = MarqueeLabel()
        self.title_label.set_label("Not Playing")
        self.title_label.add_css_class("heading")
        self.title_label.label1.set_halign(Gtk.Align.START)
        self.title_label.label2.set_halign(Gtk.Align.START)

        self.artist_btn = Gtk.Button()
        self.artist_btn.add_css_class("flat")
        self.artist_btn.add_css_class("link-btn")
        self.artist_btn.set_halign(Gtk.Align.START)
        self.artist_btn.set_has_frame(False)
        self.artist_btn.connect("clicked", self._on_artist_btn_clicked)

        self.artist_label = Gtk.Label(label="")
        self.artist_label.set_ellipsize(3)
        self.artist_label.set_width_chars(1)
        self.artist_label.add_css_class("caption")

        self.artists_box = Gtk.Box(orientation=Gtk.Orientation.HORIZONTAL, spacing=2)
        self.artists_box.set_halign(Gtk.Align.START)
        
        meta_box.append(self.title_label)
        meta_box.append(self.artists_box)

        content_box.append(meta_box)

        controls_box = Gtk.Box(orientation=Gtk.Orientation.HORIZONTAL, spacing=6)
        controls_box.set_valign(Gtk.Align.CENTER)

        self.timings_label = Gtk.Label(label="0:00 / 0:00")
        self.timings_label.add_css_class("caption")
        self.timings_label.set_valign(Gtk.Align.CENTER)
        self.timings_label.add_css_class("numeric")
        controls_box.append(self.timings_label)

        self.prev_btn = Gtk.Button(icon_name="media-skip-backward-symbolic")
        self.prev_btn.set_valign(Gtk.Align.CENTER)
        self.prev_btn.add_css_class("flat")
        self.prev_btn.connect("clicked", lambda x: self.player.previous())
        controls_box.append(self.prev_btn)

        self.play_btn = Gtk.Button()
        self.play_btn.set_valign(Gtk.Align.CENTER)
        self.play_btn.add_css_class("circular")
        self.play_btn.connect("clicked", self.on_play_clicked)

        self._play_stack = Gtk.Stack()
        self._play_stack.set_transition_type(Gtk.StackTransitionType.CROSSFADE)
        self._play_stack.set_transition_duration(150)

        self._play_icon = Gtk.Image.new_from_icon_name("media-playback-start-symbolic")
        self._play_stack.add_named(self._play_icon, "icon")

        self._play_spinner = Adw.Spinner()
        self._play_spinner.set_size_request(16, 16)
        self._play_stack.add_named(self._play_spinner, "spinner")

        self.play_btn.set_child(self._play_stack)
        controls_box.append(self.play_btn)

        # Next
        self.next_btn = Gtk.Button(icon_name="media-skip-forward-symbolic")
        self.next_btn.set_valign(Gtk.Align.CENTER)
        self.next_btn.add_css_class("flat")
        self.next_btn.connect("clicked", lambda x: self.player.next())
        controls_box.append(self.next_btn)

        self.volume_btn = Gtk.Button(icon_name="audio-volume-high-symbolic")
        self.volume_btn.add_css_class("flat")
        self.volume_btn.connect("clicked", self.on_volume_btn_clicked)

        self.volume_revealer = Gtk.Revealer(
            transition_type=Gtk.RevealerTransitionType.SLIDE_RIGHT
        )
        self.volume_revealer.set_transition_duration(250)

        self.volume_scale = Gtk.Scale(orientation=Gtk.Orientation.HORIZONTAL)
        self.volume_scale.set_range(0, 1.0)
        self.volume_scale.set_value(self.player.get_volume())
        self.volume_scale.set_size_request(80, -1)
        self.volume_scale.connect("value-changed", self.on_volume_scale_changed)
        self.volume_revealer.set_child(self.volume_scale)

        self.volume_container = Gtk.Box(
            orientation=Gtk.Orientation.HORIZONTAL, spacing=0
        )
        self.volume_container.set_valign(Gtk.Align.CENTER)
        self.volume_container.append(self.volume_btn)
        self.volume_container.append(self.volume_revealer)

        # Volume Hover Logic
        volume_hover_controller = Gtk.EventControllerMotion()
        volume_hover_controller.connect(
            "enter", lambda *args: self.volume_revealer.set_reveal_child(True)
        )
        volume_hover_controller.connect(
            "leave", lambda *args: self.volume_revealer.set_reveal_child(False)
        )
        self.volume_container.add_controller(volume_hover_controller)

        controls_box.append(self.volume_container)

        self.queue_btn = Gtk.ToggleButton(icon_name="music-queue-symbolic")
        self.queue_btn.set_valign(Gtk.Align.CENTER)
        self.queue_btn.add_css_class("flat")
        self.queue_btn.set_tooltip_text("Toggle Queue")

        if self.on_queue_click:
            self.queue_btn.connect("clicked", lambda x: self.on_queue_click())

        controls_box.append(self.queue_btn)

        self.like_btn = LikeButton(self.player.client, None)
        self.like_btn.remove_css_class("circular")
        self.like_btn.set_visible(False)
        self.like_btn.set_valign(Gtk.Align.CENTER)
        controls_box.append(self.like_btn)

        self.overflow_btn = Gtk.MenuButton(icon_name="view-more-symbolic")
        self.overflow_btn.set_valign(Gtk.Align.CENTER)
        self.overflow_btn.add_css_class("flat")
        self.overflow_btn.set_tooltip_text("More")
        self.overflow_btn.set_visible(False)
        self._overflow_popover = Gtk.Popover()
        self.overflow_btn.set_popover(self._overflow_popover)
        self._overflow_box = Gtk.Box(
            orientation=Gtk.Orientation.VERTICAL, spacing=6
        )
        self._overflow_box.set_margin_top(6)
        self._overflow_box.set_margin_bottom(6)
        self._overflow_box.set_margin_start(6)
        self._overflow_box.set_margin_end(6)
        self._overflow_popover.set_child(self._overflow_box)
        controls_box.append(self.overflow_btn)

        # Expand/collapse button — desktop equivalent of tapping the bar
        # on mobile. Sits at the far right (after the overflow) to mirror
        # YT Music's own chevron placement. Hidden on compact because the
        # bar itself is tappable there. Icon flips between up- and
        # down-chevron via `set_expanded`.
        self.expand_btn = Gtk.Button(icon_name="go-up-symbolic")
        self.expand_btn.set_valign(Gtk.Align.CENTER)
        self.expand_btn.add_css_class("flat")
        self.expand_btn.set_tooltip_text("Expand player")
        self.expand_btn.connect(
            "clicked", lambda _b: self.emit("expand-requested")
        )
        controls_box.append(self.expand_btn)

        content_box.append(controls_box)
        self.content_box = content_box
        self.controls_box = controls_box

        self.player.connect("state-changed", self.on_state_changed)
        self.player.connect("progression", self.on_progression)
        self.player.connect("metadata-changed", self.on_metadata_changed)
        self.player.connect("volume-changed", self.on_volume_changed)

        self._is_buffering_spinner = False
        self.on_state_changed(self.player, self.player.get_state_string())

        self.is_compact = False
        self._sheet_bar = False

        drag = Gtk.GestureDrag()
        drag.set_propagation_phase(Gtk.PropagationPhase.BUBBLE)
        drag.connect("drag-update", self.on_drag_update)
        self.content_box.add_controller(drag)

        click = Gtk.GestureClick()
        click.set_propagation_phase(Gtk.PropagationPhase.BUBBLE)
        click.connect("released", self.on_bar_tapped)
        self.content_box.add_controller(click)

        swipe = Gtk.GestureSwipe()
        swipe.set_propagation_phase(Gtk.PropagationPhase.BUBBLE)
        swipe.connect("swipe", self._on_swipe)
        self.content_box.add_controller(swipe)
        self._skip_cooldown = False

        # Responsive control hiding for the in-between desktop widths where
        # set_compact(True) hasn't kicked in yet but the controls are
        # squeezing the title/artist meta box to zero. We monitor our own
        # allocated width via a tick callback and progressively hide
        # non-essential controls (timings, volume, queue) so the meta box
        # always has room for at least the title.
        self._last_responsive_width = -1
        self.add_tick_callback(self._responsive_tick)

    def set_queue_active(self, active):
        if self.queue_btn.get_active() != active:
            self.queue_btn.set_active(active)

    def set_compact(self, compact):
        self.is_compact = compact
        # _responsive_tick stands down while compact, so anything it folded
        # into the 3-dot popover at desktop widths would be stranded there
        # (the like button, most visibly). Reset the width memo too, or the
        # tick skips the frame we land back on a width it has already seen.
        self._last_responsive_width = -1
        if compact:
            for control in self._responsive_order():
                self._set_control_location(control, inline=True)
            self.overflow_btn.set_visible(False)

            self.add_css_class("compact")
            self.timings_label.set_visible(False)
            self.prev_btn.set_visible(False)
            self.next_btn.set_visible(False)
            self.volume_container.set_visible(False)
            self.queue_btn.set_visible(False)
            self.expand_btn.set_visible(False)
            self.scale.add_css_class("compact")

            self.content_box.set_margin_start(10)
            self.content_box.set_margin_end(10)
            self.content_box.set_margin_top(10)
            self.content_box.set_margin_bottom(10)
            self.content_box.set_spacing(10)
            self.controls_box.set_spacing(10)
        else:
            self.remove_css_class("compact")
            self.timings_label.set_visible(True)
            self.prev_btn.set_visible(True)
            self.next_btn.set_visible(True)
            self.volume_container.set_visible(True)
            self.queue_btn.set_visible(True)
            self.expand_btn.set_visible(True)
            self.scale.remove_css_class("compact")
            self.like_btn.set_visible(bool(self.player.current_video_id))

            self.content_box.set_margin_start(10)
            self.content_box.set_margin_end(10)
            self.content_box.set_margin_top(10)
            self.content_box.set_margin_bottom(10)
            self.content_box.set_spacing(10)
            self.controls_box.set_spacing(10)

    def set_sheet_bar(self, enabled):
        """Tell the bar it is now AdwBottomSheet's bottom bar. The sheet
        opens on click and follows the finger on a pull up by itself, so
        our tap and vertical-drag handlers stand down rather than race its
        swipe tracker. Horizontal swipe-to-skip stays: the tracker only
        claims drags that go vertical.

        A full-width bottom bar is flush to both window edges, so Adwaita
        squares off the sheet's top corners and the bar keeps the shape it
        already has. The class is only there for the seek bar (see CSS)."""
        self._sheet_bar = enabled
        if enabled:
            self.add_css_class("sheet-bar")
        else:
            self.remove_css_class("sheet-bar")

    def set_expanded(self, expanded):
        """Flip the expand button's chevron — down when the cover view
        is open (next click collapses), up when it's not."""
        if expanded:
            self.expand_btn.set_icon_name("go-down-symbolic")
            self.expand_btn.set_tooltip_text("Collapse player")
        else:
            self.expand_btn.set_icon_name("go-up-symbolic")
            self.expand_btn.set_tooltip_text("Expand player")

    def _responsive_tick(self, widget, frame_clock):
        """Move non-essential controls (like / queue / volume) into the 3-dot
        overflow popover as the bar narrows. Timings stay inline. Only kicks
        in when the bar is in desktop (non-compact) mode — set_compact owns
        the mobile layout independently."""
        if self.is_compact:
            return True
        width = self.get_width()
        if width <= 1 or width == self._last_responsive_width:
            return True
        self._last_responsive_width = width

        # Priority of overflowing: like first, then queue, then volume.
        # Thresholds picked so each hidden control gives ~40-60px back to
        # the meta box. Tweak in one place if the bar's metrics change.
        candidates = [
            (self.like_btn, width >= 720),
            (self.queue_btn, width >= 640),
            (self.volume_container, width >= 560),
        ]
        overflow = [control for control, inline in candidates if not inline]

        # Overflowing a single control saves no space: the 3-dot button we'd
        # add is the same width as the control we'd remove, so a 1-item
        # dropdown is a pure 1:1 swap. Only fold things away once at least
        # two controls would move — then hiding N gives a net N-1 back.
        if len(overflow) < 2:
            overflow = []

        for control, _ in candidates:
            self._set_control_location(control, inline=control not in overflow)

        self.overflow_btn.set_visible(bool(overflow))
        return True

    # Canonical left-to-right order of the controls that overflow, matching
    # the order they're appended in __init__ (volume → queue → like). Used to
    # re-inline them in the right place regardless of the order they happen to
    # come back in as the bar widens.
    def _responsive_order(self):
        return [self.volume_container, self.queue_btn, self.like_btn]

    def _inline_anchor_for(self, control):
        """The sibling a re-inlined control should sit after: the nearest
        canonically-preceding control that's already inline, or next_btn if
        none of its predecessors are inline yet."""
        order = self._responsive_order()
        idx = order.index(control)
        for prev in reversed(order[:idx]):
            if prev.get_parent() is self.controls_box:
                return prev
        return self.next_btn

    def _set_control_location(self, control, inline):
        """Move `control` between the inline controls_box and the overflow
        popover. inline=True puts it back in the bar; False relocates it
        into the popover (so the user can still reach it via the 3-dot)."""
        parent = control.get_parent()
        if inline and parent is self._overflow_box:
            self._overflow_box.remove(control)
            # Re-insert after its canonical predecessor so the original
            # ordering (volume → queue → like → overflow → expand) survives
            # being torn apart and reassembled as the bar resizes.
            self.controls_box.insert_child_after(
                control, self._inline_anchor_for(control)
            )
        elif (not inline) and parent is self.controls_box:
            self.controls_box.remove(control)
            self._overflow_box.append(control)

    def _on_artist_btn_clicked(self, btn):
        if self.on_artist_click:
            self.on_artist_click()

    def _on_cover_btn_clicked(self, btn):
        if self.on_album_click:
            self.on_album_click()

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

        if hasattr(self, "_scroll_seek_id") and self._scroll_seek_id:
            from gi.repository import GLib

            GLib.source_remove(self._scroll_seek_id)

        from gi.repository import GLib

        self._scroll_seek_id = GLib.timeout_add(100, self._do_scroll_seek, new_val)
        return True

    def _do_scroll_seek(self, value):
        self.player.seek(value, flush=True)
        self._scroll_seek_id = None
        return False

    def _load_css(self):
        css = """
        .player-bar {
            padding: 0px;
            background-color: @headerbar_bg_color;
            border-top: 1px solid @borders;
        }
        .link-btn {
            padding: 0px;
            margin: 0px;
            min-height: 0px;
            background: transparent;
            box-shadow: none;
        }
        .link-btn:hover {
            color: @accent_color;
        }
        .player-scale {
            margin-top: -1px; /* Desktop: Align with top border */
            margin-bottom: 2px; 
            min-height: 4px;
            padding: 0px;
        }
        .player-scale.compact {
            margin-top: -4px; /* Mobile: Pull even higher to remove perceived gap */
        }
        /* AdwBottomSheet clips its bottom bar to its own bounds, so the
           negative margin above cut 3 of the seek bar's 4px away and left
           it near-invisible and hard to grab. There is no gap to close
           here anyway: the bar's top edge is the sheet's top edge. */
        .player-bar.sheet-bar .player-scale.compact {
            margin-top: 0px;
        }
        .player-scale trough {
            min-height: 4px; /* Slightly thicker */
            margin-top: 0px;
            margin-bottom: 0px;
            padding: 0px;
        }
        .player-scale slider {
            min-height: 0px;
            min-width: 0px;
            margin: 0px;
            background-color: transparent; /* Hide slider by default */
        }
        .player-scale:hover slider {
            min-height: 12px;
            min-width: 12px;
            margin: -5px; 
            background-color: white;
            box-shadow: 0 0 4px rgba(0,0,0,0.3);
        }
        .player-bar-cover {
            border-radius: 6px;
        }
        """
        provider = Gtk.CssProvider()
        provider.load_from_data(css.encode("utf-8"))

        display = Gdk.Display.get_default()
        if display:
            Gtk.StyleContext.add_provider_for_display(
                display, provider, Gtk.STYLE_PROVIDER_PRIORITY_APPLICATION
            )

    def on_metadata_changed(
        self, player, title, artist, thumbnail_url, video_id, like_status
    ):
        self.current_title = title
        self.current_artist = artist
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
                btn.set_cursor(Gdk.Cursor.new_from_name("pointer", None))

                lbl = Gtk.Label(label=name)
                lbl.add_css_class("caption")
                btn.set_child(lbl)

                if self.on_artist_click:
                    btn.connect(
                        "clicked",
                        lambda _b, a_id=aid, a_name=name: self.on_artist_click(a_id, a_name),
                    )

                self.artists_box.append(btn)

                if i < len(artists_list) - 1:
                    sep = Gtk.Label(label=", ")
                    sep.add_css_class("caption")
                    self.artists_box.append(sep)
        else:
            name = artist or "Unknown Artist"
            btn = Gtk.Button()
            btn.add_css_class("flat")
            btn.add_css_class("link-btn")
            btn.set_has_frame(False)
            btn.set_cursor(Gdk.Cursor.new_from_name("pointer", None))

            lbl = Gtk.Label(label=name)
            lbl.add_css_class("caption")
            btn.set_child(lbl)

            if self.on_artist_click:
                btn.connect(
                    "clicked",
                    lambda _b, a_name=name: self.on_artist_click(None, a_name),
                )

            self.artists_box.append(btn)

        if thumbnail_url:
            self.cover_img.video_id = video_id
            self.cover_img.load_url(thumbnail_url)
        else:
            self.cover_img.video_id = None
            self.cover_img.load_url(None)

        if video_id:
            self.like_btn.set_data(video_id, like_status)
        else:
            self.like_btn.set_visible(False)

        if video_id and self.player.duration <= 0:
            self._is_buffering_spinner = True
            self._play_stack.set_visible_child_name("spinner")
            self.play_btn.set_sensitive(False)

    def on_play_clicked(self, btn):
        if self.player.get_state_string() == "playing":
            self.player.pause()
        else:
            self.player.play()

    def on_state_changed(self, player, state):
        if state == "loading":
            self.scale.set_value(0)
            self.scale.set_sensitive(False)
            self.timings_label.set_label("0:00 / 0:00")
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
                self.scale.set_sensitive(True)
                self._play_icon.set_from_icon_name("media-playback-pause-symbolic")
                self._play_stack.set_visible_child_name("icon")
                self.play_btn.set_sensitive(True)
        elif state in ("paused", "stopped"):
            if self._is_buffering_spinner and self.player.duration <= 0:

                return
            if state == "paused":
                self.scale.set_sensitive(True)
            self._play_icon.set_from_icon_name("media-playback-start-symbolic")
            self._play_stack.set_visible_child_name("icon")
            self.play_btn.set_sensitive(True)
            self._is_buffering_spinner = False

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
        t = f"{self._format_time(pos)} / {self._format_time(dur)}"
        self.timings_label.set_label(t)

        if getattr(self, "_is_buffering_spinner", False) and dur > 0:
            if self.player.get_state_string() == "playing":
                self._is_buffering_spinner = False
                self.scale.set_sensitive(True)
                self.play_btn.set_sensitive(True)
                self._play_stack.set_visible_child_name("icon")
                self._play_icon.set_from_icon_name("media-playback-pause-symbolic")

    def on_volume_btn_clicked(self, btn):
        is_muted = not self.player.get_mute()
        self.player.set_mute(is_muted)

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
            self.volume_btn.set_icon_name("audio-volume-muted-symbolic")
        elif volume < 0.33:
            self.volume_btn.set_icon_name("audio-volume-low-symbolic")
        elif volume < 0.66:
            self.volume_btn.set_icon_name("audio-volume-medium-symbolic")
        else:
            self.volume_btn.set_icon_name("audio-volume-high-symbolic")

        if muted or volume == 0:
            self.volume_btn.set_icon_name("audio-volume-muted-symbolic")
        elif volume < 0.33:
            self.volume_btn.set_icon_name("audio-volume-low-symbolic")
        elif volume < 0.66:
            self.volume_btn.set_icon_name("audio-volume-medium-symbolic")
        else:
            self.volume_btn.set_icon_name("audio-volume-high-symbolic")

    def _on_swipe(self, gesture, vx, vy):
        if not self.is_compact or self._skip_cooldown:
            return

        if abs(vy) > 100 or abs(vy) > abs(vx) * 0.5:
            return

        if abs(vx) > 350:
            self._skip_cooldown = True
            if vx < 0:
                self.player.next()
            else:
                self.player.previous()
            gesture.set_state(Gtk.EventSequenceState.CLAIMED)
            from gi.repository import GLib

            GLib.timeout_add(500, self._clear_skip_cooldown)

    def _clear_skip_cooldown(self):
        self._skip_cooldown = False
        return False

    def on_drag_update(self, gesture, offset_x, offset_y):
        if self._sheet_bar:
            return
        if self.is_compact and offset_y < -15:
            self.emit("expand-requested")
            gesture.set_state(Gtk.EventSequenceState.CLAIMED)

    def on_bar_tapped(self, gesture, n_press, x, y):
        if self._sheet_bar:
            return
        if self.is_compact:
            self.emit("expand-requested")

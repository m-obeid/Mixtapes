import gi

gi.require_version("Gtk", "4.0")
gi.require_version("Adw", "1")
from gi.repository import Gtk, Adw, GLib, Pango
from ui.utils import AsyncPicture, LikeButton, show_toast
from ui.context_menu import show_song_menu

# AlbumPage is resolved lazily because ui.pages.album imports BasePlaylistPage,
# which transitively imports this module — a top-level import would cycle.
# Caching the class reference avoids re-importing inside every bind().
_ALBUM_PAGE_CLS = None


def _is_album_page(page):
    global _ALBUM_PAGE_CLS
    if _ALBUM_PAGE_CLS is None:
        from ui.pages.album import AlbumPage
        _ALBUM_PAGE_CLS = AlbumPage
    return isinstance(page, _ALBUM_PAGE_CLS)


class SongRowWidget(Gtk.Box):
    def __init__(self, player, client):
        super().__init__(orientation=Gtk.Orientation.HORIZONTAL)
        self.player = player
        self.client = client
        self.model_item = None
        self._notify_handler_id = None
        self._player_handler_id = None
        self._start_x = 0
        self._start_y = 0

        # Keep the downloaded indicator in sync across any view showing this
        # song — react to both completions and removals from elsewhere in the UI.
        dm = self.player.download_manager
        self._dm_done_handler = dm.connect("item-done", self._on_dm_item_done)
        self._dm_removed_handler = dm.connect("download-removed", self._on_dm_download_removed)
        self.connect("destroy", self._on_destroy)

        self.row = Gtk.Box(orientation=Gtk.Orientation.HORIZONTAL, spacing=12)
        self.row.set_hexpand(True)
        self.row.add_css_class("song-row")
        self.append(self.row)

        # Image with playing indicator overlay
        self.img = AsyncPicture(crop_to_square=True, target_size=56, player=self.player)
        self.img.add_css_class("song-img")

        self.img_overlay = Gtk.Overlay()
        self.img_overlay.set_child(self.img)
        self.img_overlay.set_valign(Gtk.Align.CENTER)

        # Track number label (for album view)
        self.track_num_label = Gtk.Label()
        self.track_num_label.add_css_class("dim-label")
        self.track_num_label.add_css_class("caption")
        self.track_num_label.set_valign(Gtk.Align.CENTER)
        self.track_num_label.set_halign(Gtk.Align.CENTER)
        self.track_num_label.set_size_request(40, 40)
        self.track_num_label.set_visible(False)

        # Playing indicator: 3 animated bars
        self.playing_indicator = Gtk.Box(orientation=Gtk.Orientation.HORIZONTAL)
        self.playing_indicator.set_halign(Gtk.Align.CENTER)
        self.playing_indicator.set_valign(Gtk.Align.CENTER)
        self.playing_indicator.add_css_class("playing-indicator")
        self.playing_indicator.set_visible(False)

        self.bar1 = Gtk.Box()
        self.bar1.add_css_class("playing-bar")
        self.bar1.add_css_class("playing-bar-1")
        self.bar2 = Gtk.Box()
        self.bar2.add_css_class("playing-bar")
        self.bar2.add_css_class("playing-bar-2")
        self.bar3 = Gtk.Box()
        self.bar3.add_css_class("playing-bar")
        self.bar3.add_css_class("playing-bar-3")

        self.playing_indicator.append(self.bar1)
        self.playing_indicator.append(self.bar2)
        self.playing_indicator.append(self.bar3)

        self._anim_timer_id = None
        self._anim_state = False

        self.img_overlay.add_overlay(self.playing_indicator)
        self.row.append(self.track_num_label)
        self.row.append(self.img_overlay)

        # Main Title / Subtitle Box
        self.vbox = Gtk.Box(orientation=Gtk.Orientation.VERTICAL, spacing=2)
        self.vbox.set_valign(Gtk.Align.CENTER)
        self.vbox.set_hexpand(True)

        # Title + badges live in `title_box`. We want the badges to sit
        # directly next to the title text, not at the far right of the row.
        # `title_label` has hexpand=False (so it doesn't gobble all leftover
        # space and push the badges away) but width_chars=1 + ellipsize=END
        # so it can still shrink when the row is narrow. The trailing
        # `_lv_title_spacer` is the element that absorbs leftover space,
        # keeping the trio left-aligned within title_box.
        self.title_box = Gtk.Box(orientation=Gtk.Orientation.HORIZONTAL, spacing=6)
        self.title_label = Gtk.Label()
        self.title_label.set_halign(Gtk.Align.START)
        self.title_label.set_xalign(0.0)
        self.title_label.set_hexpand(False)
        self.title_label.set_ellipsize(Pango.EllipsizeMode.END)
        self.title_label.set_lines(1)
        self.title_label.set_width_chars(1)

        self.explicit_badge = Gtk.Label(label="E")
        self.explicit_badge.add_css_class("explicit-badge")
        self.explicit_badge.set_valign(Gtk.Align.CENTER)
        self.explicit_badge.set_halign(Gtk.Align.CENTER)
        self.explicit_badge.set_justify(Gtk.Justification.CENTER)
        self.explicit_badge.set_visible(False)

        self.dl_icon = Gtk.Image.new_from_icon_name("folder-download-symbolic")
        self.dl_icon.set_pixel_size(14)
        self.dl_icon.add_css_class("dim-label")
        self.dl_icon.set_valign(Gtk.Align.CENTER)
        self.dl_icon.set_visible(False)

        self.title_box.append(self.title_label)
        self.title_box.append(self.explicit_badge)
        self.title_box.append(self.dl_icon)

        # Soak up any leftover horizontal space so the title + badges stay
        # packed at the start of the row.
        title_spacer = Gtk.Box()
        title_spacer.set_hexpand(True)
        self.title_box.append(title_spacer)

        self.subtitle_label = Gtk.Label()
        self.subtitle_label.set_halign(Gtk.Align.START)
        self.subtitle_label.set_ellipsize(Pango.EllipsizeMode.END)
        self.subtitle_label.set_lines(1)
        self.subtitle_label.add_css_class("dim-label")
        self.subtitle_label.add_css_class("caption")

        self.vbox.append(self.title_box)
        self.vbox.append(self.subtitle_label)
        self.row.append(self.vbox)

        # Suffixes: Duration, Like

        self.dur_lbl = Gtk.Label()
        self.dur_lbl.add_css_class("caption")
        self.dur_lbl.set_valign(Gtk.Align.CENTER)
        self.dur_lbl.set_margin_end(6)
        self.row.append(self.dur_lbl)

        self.like_btn = LikeButton(self.client, None)
        self.like_btn.set_valign(Gtk.Align.CENTER)
        self.row.append(self.like_btn)

        # Gesture for Right Click (Context Menu)
        gesture = Gtk.GestureClick()
        gesture.set_button(3)  # Right click
        gesture.connect("released", self.on_right_click)
        self.row.add_controller(gesture)

        # Long Press for touch
        lp = Gtk.GestureLongPress()
        lp.connect("pressed", lambda g, x, y: self.on_right_click(g, 1, x, y))
        self.row.add_controller(lp)

        # Gesture for Left Click (Activation)
        left_click = Gtk.GestureClick()
        left_click.set_button(1)
        left_click.connect("pressed", self._on_left_pressed)
        left_click.connect("released", self._on_left_released)
        self.row.add_controller(left_click)

    def _current_video_id(self):
        return self.model_item.video_id if self.model_item else None

    def _on_dm_item_done(self, dm, video_id, success, message):
        if success and video_id and video_id == self._current_video_id():
            self.dl_icon.set_visible(True)

    def _on_dm_download_removed(self, dm, video_id):
        if video_id and video_id == self._current_video_id():
            self.dl_icon.set_visible(False)

    def _on_destroy(self, widget):
        dm = self.player.download_manager
        for hid_attr in ("_dm_done_handler", "_dm_removed_handler"):
            hid = getattr(self, hid_attr, None)
            if hid is not None:
                try:
                    dm.disconnect(hid)
                except Exception:
                    pass
                setattr(self, hid_attr, None)

    def bind(self, item, page):
        # Disconnect previous player signal handler
        if self._player_handler_id is not None:
            self.player.disconnect(self._player_handler_id)
            self._player_handler_id = None
        # Disconnect previous item notify handler
        if self._notify_handler_id is not None and self.model_item is not None:
            try:
                self.model_item.disconnect(self._notify_handler_id)
            except Exception:
                pass
            self._notify_handler_id = None

        self.model_item = item
        self.page = page

        self.title_label.set_label(item.title)
        self.title_label.set_tooltip_text(item.title)
        self.subtitle_label.set_label(item.artist)
        self.subtitle_label.set_tooltip_text(item.artist)

        self.dur_lbl.set_label(item.duration)
        self.explicit_badge.set_visible(item.is_explicit)

        # Check if this is an album view
        is_album = _is_album_page(page)

        if is_album:
            # Show track number instead of thumbnail
            self.track_num_label.set_label(str(item.index + 1))
            self.track_num_label.set_visible(True)
            self.img_overlay.set_visible(False)
        else:
            self.track_num_label.set_visible(False)
            self.img_overlay.set_visible(True)
            self.img.video_id = item.video_id
            self.img.load_url(item.thumbnail_url)

        self.like_btn.set_data(item.video_id, item.like_status)

        # Downloaded indicator
        if item.video_id:
            self.dl_icon.set_visible(self.player.download_manager.is_downloaded(item.video_id))
        else:
            self.dl_icon.set_visible(False)

        if not item.video_id:
            self.row.set_sensitive(False)
        else:
            self.row.set_sensitive(True)

        # CSS handles responsiveness and size limits natively now

        # Set initial playing state. Check both the player's current
        # video id AND the swap-source id so the row lights up even
        # when the player has already swapped the queued OMV track to
        # its ATV counterpart before this bind happened.
        _source_vid = getattr(self.player, "_current_source_video_id", None)
        self._apply_playing_state(
            bool(item.video_id and (
                item.video_id == self.player.current_video_id
                or item.video_id == _source_vid
            ))
        )

        # Connect directly to the player metadata signal (reliable than GObject property notify)
        self._player_handler_id = self.player.connect(
            "metadata-changed", self._on_player_metadata_changed
        )

    def _on_player_metadata_changed(self, player, *args):
        if self.model_item:
            vid = self.model_item.video_id
            # Match the swap-from id too: when the player auto-swaps
            # an OMV/UGC track to its audio (ATV) counterpart, the
            # `current_video_id` drifts away from what an album/
            # playlist row holds. `_current_source_video_id` is the
            # pre-swap id, so the highlight stays on the right row.
            source_vid = getattr(player, "_current_source_video_id", None)
            is_playing = bool(
                vid and (vid == player.current_video_id or vid == source_vid)
            )
            self._apply_playing_state(is_playing)

    def stop_handlers(self):
        """Disconnect all signal handlers. Called on factory unbind."""
        if self._player_handler_id is not None:
            try:
                self.player.disconnect(self._player_handler_id)
            except Exception:
                pass
            self._player_handler_id = None
        if self._notify_handler_id is not None and self.model_item is not None:
            try:
                self.model_item.disconnect(self._notify_handler_id)
            except Exception:
                pass
            self._notify_handler_id = None
        self._stop_animation()
        # Release the thumbnail texture so GTK can free it — the widget
        # may be recycled for a different track with a different cover,
        # and leaving the old paintable attached keeps the texture ref
        # alive until the bind for the new track completes. Doing this
        # explicitly bounds peak memory when scrolling through long lists.
        try:
            if hasattr(self, "img") and self.img is not None:
                self.img.set_paintable(None)
                self.img.url = None
                self.img._is_placeholder = True
        except Exception:
            pass

    def _apply_playing_state(self, is_playing):
        if is_playing:
            self.row.add_css_class("playing")
            self.playing_indicator.set_visible(True)
            self._start_animation()
        else:
            self.row.remove_css_class("playing")
            self.playing_indicator.set_visible(False)
            self._stop_animation()

    def _start_animation(self):
        if self._anim_timer_id is not None:
            return  # Already running
        self._anim_state = False
        self._anim_timer_id = GLib.timeout_add(350, self._tick_animation)

    def _stop_animation(self):
        if self._anim_timer_id is not None:
            GLib.source_remove(self._anim_timer_id)
            self._anim_timer_id = None
        # Reset bars to default state
        self.bar1.remove_css_class("bar-up")
        self.bar2.remove_css_class("bar-up")
        self.bar3.remove_css_class("bar-up")

    def _tick_animation(self):
        self._anim_state = not self._anim_state
        if self._anim_state:
            self.bar1.add_css_class("bar-up")
            self.bar3.add_css_class("bar-up")
            self.bar2.remove_css_class("bar-up")
        else:
            self.bar2.add_css_class("bar-up")
            self.bar1.remove_css_class("bar-up")
            self.bar3.remove_css_class("bar-up")
        return GLib.SOURCE_CONTINUE

    def _on_left_pressed(self, gesture, n_press, x, y):
        self._start_x = x
        self._start_y = y

    def _on_left_released(self, gesture, n_press, x, y):
        # Displacement check
        dx = abs(x - self._start_x)
        dy = abs(y - self._start_y)
        if dx > 10 or dy > 10:
            return

        if self.model_item and self.page:
            # Trigger page activation logic
            if hasattr(self.page, "on_song_activated"):
                # Pass the SongItem itself, not its stored .index — that index
                # is the original position in the underlying store and goes
                # stale once the user sorts or filters the view, which would
                # send the wrong position to the player.
                self.page.on_song_activated(None, self.model_item)

    def on_right_click(self, gesture, n_press, x, y):
        if not self.model_item:
            return

        item = self.model_item
        show_song_menu(
            self.row,
            x,
            y,
            item.track_data,
            player=self.player,
            client=self.client,
            prefix="row",
            video_id=item.video_id,
            album_title=getattr(self.page, "playlist_title_text", None),
            album_id=getattr(self.page, "playlist_id", None),
        )

    def _show_toast(self, message):
        show_toast(self, message)

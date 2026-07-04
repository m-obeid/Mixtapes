import gi
import weakref

gi.require_version("Gtk", "4.0")
gi.require_version("Adw", "1")
import threading
from gi.repository import Gtk, Adw, GLib, Gdk, Gio, Pango
from ui.utils import AsyncPicture, LikeButton, show_toast

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
        weak_self = weakref.ref(self)
        self._dm_done_handler = dm.connect(
            "item-done", lambda dm, *args: weak_self()._on_dm_item_done(dm, *args) if weak_self() else None
        )
        self._dm_removed_handler = dm.connect(
            "download-removed", lambda dm, *args: weak_self()._on_dm_download_removed(dm, *args) if weak_self() else None
        )
        self.connect(
            "destroy", lambda w: weak_self()._on_destroy(w) if weak_self() else None
        )

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
        weak_self = weakref.ref(self)
        gesture.connect(
            "released", lambda g, n, x, y: weak_self().on_right_click(g, n, x, y) if weak_self() else None
        )
        self.row.add_controller(gesture)

        # Long Press for touch
        lp = Gtk.GestureLongPress()
        lp.connect(
            "pressed", lambda g, x, y: weak_self().on_right_click(g, 1, x, y) if weak_self() else None
        )
        self.row.add_controller(lp)

        # Gesture for Left Click (Activation)
        left_click = Gtk.GestureClick()
        left_click.set_button(1)
        left_click.connect(
            "pressed", lambda g, n, x, y: weak_self()._on_left_pressed(g, n, x, y) if weak_self() else None
        )
        left_click.connect(
            "released", lambda g, n, x, y: weak_self()._on_left_released(g, n, x, y) if weak_self() else None
        )
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
        weak_self = weakref.ref(self)
        self._player_handler_id = self.player.connect(
            "metadata-changed", lambda player, *args: weak_self()._on_player_metadata_changed(player, *args) if weak_self() else None
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
        self.page = None
        self.model_item = None

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
        group = Gio.SimpleActionGroup()
        self.row.insert_action_group("row", group)

        # Copy Link
        def copy_link_action(action, param):
            vid = item.video_id
            if vid:
                url = f"https://music.youtube.com/watch?v={vid}"
                clipboard = Gdk.Display.get_default().get_clipboard()
                clipboard.set(url)
                self._show_toast("Link copied to clipboard")

        def goto_artist_action(action, param):
            # We need to find the artist ID. It's in item.track_data
            artists = item.track_data.get("artists", [])
            if artists:
                artist = artists[0]
                aid = artist.get("id")
                name = artist.get("name")
                if aid:
                    root = self.get_root()
                    if hasattr(root, "open_artist"):
                        root.open_artist(aid, name)

        def goto_album_action(action, param):
            album = item.track_data.get("album")
            if not isinstance(album, dict):
                return
            album_id = album.get("id")
            if not album_id:
                return
            root = self.get_root()
            if hasattr(root, "open_playlist"):
                root.open_playlist(
                    album_id, {"title": album.get("name", "Album")}
                )

        action_copy = Gio.SimpleAction.new("copy_link", None)
        action_copy.connect("activate", copy_link_action)
        group.add_action(action_copy)

        action_goto = Gio.SimpleAction.new("goto_artist", None)
        action_goto.connect("activate", goto_artist_action)
        group.add_action(action_goto)

        action_goto_album = Gio.SimpleAction.new("goto_album", None)
        action_goto_album.connect("activate", goto_album_action)
        group.add_action(action_goto_album)

        # Add to Playlist — opens a custom popover with covers + search +
        # recents-first ordering. The Gio.Menu can't host those affordances,
        # so the menu item just triggers the popover instead of nesting a
        # plain text submenu.
        def add_to_playlist(target_pid):
            target_vid = item.video_id
            if not (target_pid and target_vid):
                return

            from ui.widgets.add_to_playlist import mark_playlist_used
            mark_playlist_used(target_pid)

            def thread_func():
                # add_playlist_items handles the OMV→ATV swap itself
                # (auto-enabled for single-item adds).
                success = self.client.add_playlist_items(target_pid, [target_vid])
                if success:
                    GLib.idle_add(self._show_toast, "Added track to playlist")
                else:
                    GLib.idle_add(self._show_toast, "Failed to add track")

            threading.Thread(target=thread_func, daemon=True).start()

        def show_add_to_playlist_action(action, param):
            from ui.widgets.add_to_playlist import AddToPlaylistPopover
            pop = AddToPlaylistPopover(
                self.player, on_select=add_to_playlist, parent=self.row
            )
            pop.popup()

        action_add = Gio.SimpleAction.new("show_add_to_playlist", None)
        action_add.connect("activate", show_add_to_playlist_action)
        group.add_action(action_add)

        # Start Radio
        def start_radio_action(action, param):
            vid = item.video_id
            if vid:
                self.player.start_radio(video_id=vid)
                self._show_toast("Starting radio...")

        action_radio = Gio.SimpleAction.new("start_radio", None)
        action_radio.connect("activate", start_radio_action)
        group.add_action(action_radio)

        # Play Next / Add to Queue
        def play_next_action(action, param):
            self.player.add_to_queue(dict(item.track_data), next=True)
            self._show_toast("Playing next")

        def add_to_queue_action(action, param):
            self.player.add_to_queue(dict(item.track_data), next=False)
            self._show_toast("Added to queue")

        a_play_next = Gio.SimpleAction.new("play_next", None)
        a_play_next.connect("activate", play_next_action)
        group.add_action(a_play_next)

        a_add_queue = Gio.SimpleAction.new("add_to_queue", None)
        a_add_queue.connect("activate", add_to_queue_action)
        group.add_action(a_add_queue)

        menu_model = Gio.Menu()

        # Queue section
        if item.video_id:
            queue_section = Gio.Menu()
            queue_section.append("Play Next", "row.play_next")
            queue_section.append("Add to Queue", "row.add_to_queue")
            menu_model.append_section(None, queue_section)

        # Navigation section
        nav_section = Gio.Menu()
        artists = item.track_data.get("artists", [])
        if artists and artists[0].get("id"):
            nav_section.append("Go to Artist", "row.goto_artist")
        album_data = item.track_data.get("album")
        if isinstance(album_data, dict) and album_data.get("id"):
            nav_section.append("Go to Album", "row.goto_album")
        if nav_section.get_n_items() > 0:
            menu_model.append_section(None, nav_section)

        # Actions section
        from ui.utils import is_online
        _online = is_online()
        action_section = Gio.Menu()
        if item.video_id and _online:
            action_section.append("Start Radio", "row.start_radio")
            if self.client.get_editable_playlists():
                action_section.append(
                    "Add to Playlist…", "row.show_add_to_playlist"
                )
        # Download / Remove Download
        if item.video_id:
            root = self.get_root()
            is_dl = root and hasattr(root, 'player') and root.player.download_manager.is_downloaded(item.video_id)
            if is_dl:
                action_section.append("Remove Download", "row.remove_download")
                def remove_download_action(action, param):
                    r = self.get_root()
                    if not (r and hasattr(r, "player")):
                        return
                    r.player.download_manager.delete_download(item.video_id)
                    self.dl_icon.set_visible(False)
                a_rd = Gio.SimpleAction.new("remove_download", None)
                a_rd.connect("activate", remove_download_action)
                group.add_action(a_rd)
            elif _online:
                action_section.append("Download", "row.download")
                def download_action(action, param):
                    r = self.get_root()
                    if r and hasattr(r, "download_track"):
                        r.download_track(item.track_data)
                a_dl = Gio.SimpleAction.new("download", None)
                a_dl.connect("activate", download_action)
                group.add_action(a_dl)

        if action_section.get_n_items() > 0:
            menu_model.append_section(None, action_section)

        # Refresh metadata
        if item.video_id and _online:
            def refresh_metadata_action(action, param):
                vid = item.video_id
                if not vid:
                    return
                self._show_toast("Refreshing metadata...")

                def _fetch():
                    try:
                        wp = self.client.get_watch_playlist(video_id=vid, limit=1)
                        wp_tracks = wp.get("tracks", [])
                        if wp_tracks:
                            fresh = wp_tracks[0]
                            td = item.track_data
                            if fresh.get("title"):
                                td["title"] = fresh["title"]
                            if fresh.get("artists"):
                                td["artists"] = fresh["artists"]
                                td["artist"] = ", ".join(
                                    a.get("name", "") for a in fresh["artists"] if a
                                )
                            if fresh.get("album"):
                                td["album"] = fresh["album"]
                            if fresh.get("thumbnail"):
                                thumbs = fresh["thumbnail"]
                                if isinstance(thumbs, list) and thumbs:
                                    td["thumb"] = thumbs[-1].get("url", "")
                                    td["thumbnails"] = thumbs
                            # Update queue track if it matches
                            for t in self.player.queue:
                                if t.get("videoId") == vid:
                                    t.update(td)
                                    break
                            if getattr(self.player, "discord_rpc", None):
                                self.player.discord_rpc.update()
                            GLib.idle_add(self._show_toast, "Metadata refreshed")
                        else:
                            GLib.idle_add(self._show_toast, "No metadata found")
                    except Exception as e:
                        print(f"Refresh metadata error: {e}")
                        GLib.idle_add(self._show_toast, "Failed to refresh metadata")

                threading.Thread(target=_fetch, daemon=True).start()

            a_refresh = Gio.SimpleAction.new("refresh_metadata", None)
            a_refresh.connect("activate", refresh_metadata_action)
            group.add_action(a_refresh)
            action_section.append("Refresh Metadata", "row.refresh_metadata")

        if action_section.get_n_items() > 0:
            menu_model.append_section(None, action_section)

        # Clipboard section
        clip_section = Gio.Menu()
        if item.video_id and _online:
            clip_section.append("Copy Link", "row.copy_link")
        if clip_section.get_n_items() > 0:
            menu_model.append_section(None, clip_section)

        if menu_model.get_n_items() > 0:
            popover = Gtk.PopoverMenu.new_from_model(menu_model)
            popover.set_parent(self.row)
            popover.set_has_arrow(False)

            rect = Gdk.Rectangle()
            rect.x = int(x)
            rect.y = int(y)
            rect.width = 1
            rect.height = 1
            popover.set_pointing_to(rect)

            popover.popup()

    def _show_toast(self, message):
        show_toast(self, message)

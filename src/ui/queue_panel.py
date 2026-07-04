import gi
import threading
import weakref

gi.require_version("Gtk", "4.0")
gi.require_version("Adw", "1")
from gi.repository import Gtk, Adw, GObject, Pango, Gdk, Gio, GLib

from ui.utils import show_toast
from ui.util_classes import ScrolledWindow


class QueueItem(GObject.Object):
    __gtype_name__ = "QueueItem"

    @GObject.Property(type=bool, default=False)
    def is_playing(self):
        return self._is_playing

    @is_playing.setter
    def is_playing(self, value):
        self._is_playing = value

    @GObject.Property(type=bool, default=False)
    def is_paused(self):
        return self._is_paused

    @is_paused.setter
    def is_paused(self, value):
        self._is_paused = value

    def __init__(self, track, index, is_playing, is_paused=False):
        super().__init__()
        self.track = track
        self.index = index
        self._is_playing = is_playing
        self._is_paused = is_paused


class QueueRowWidget(Gtk.Box):
    def __init__(self):
        super().__init__(orientation=Gtk.Orientation.HORIZONTAL, spacing=12)
        self.add_css_class("queue-row")

        self.model_item = None  # QueueItem
        self.panel = None  # QueuePanel reference

        # Drag Handle
        self.handle = Gtk.Image.new_from_icon_name("list-drag-handle-symbolic")
        self.handle.add_css_class("dim-label")
        self.handle.add_css_class("drag-handle")
        self.handle.set_margin_start(6)
        self.handle.set_margin_end(4)
        self.append(self.handle)

        # Setup Drag Source
        drag_source = Gtk.DragSource()
        drag_source.set_actions(Gdk.DragAction.MOVE)
        drag_source.connect("prepare", self.on_drag_prepare)
        drag_source.connect("drag-begin", self.on_drag_begin)
        self.handle.add_controller(drag_source)

        # Indicator / Index
        self.indicator_stack = Gtk.Stack()
        self.indicator_lbl = Gtk.Label()
        self.indicator_lbl.add_css_class("dim-label")
        self.indicator_lbl.set_width_chars(3)
        self.indicator_icon = Gtk.Image.new_from_icon_name(
            "media-playback-start-symbolic"  # Default
        )
        self.indicator_icon.add_css_class("accent")

        self.indicator_stack.add_named(self.indicator_lbl, "index")
        self.indicator_stack.add_named(self.indicator_icon, "playing")
        self.append(self.indicator_stack)

        # Info Box
        info_box = Gtk.Box(orientation=Gtk.Orientation.VERTICAL, spacing=2)
        info_box.set_hexpand(True)

        self.title_lbl = Gtk.Label()
        self.title_lbl.set_halign(Gtk.Align.START)
        self.title_lbl.set_ellipsize(Pango.EllipsizeMode.END)
        self.title_lbl.add_css_class("body")

        self.artist_lbl = Gtk.Label()
        self.artist_lbl.set_halign(Gtk.Align.START)
        self.artist_lbl.set_ellipsize(Pango.EllipsizeMode.END)
        self.artist_lbl.add_css_class("caption")
        self.artist_lbl.add_css_class("dim-label")

        info_box.append(self.title_lbl)
        info_box.append(self.artist_lbl)
        self.append(info_box)

        # Drop Target
        drop_target = Gtk.DropTarget.new(GObject.TYPE_STRING, Gdk.DragAction.MOVE)
        drop_target.connect("drop", self.on_drop)
        self.add_controller(drop_target)

    def bind(self, item, panel):
        if self.model_item:
            try:
                self.model_item.disconnect_by_func(self._on_item_property_changed)
            except Exception:
                pass

        self.model_item = item
        self.panel = panel

        item.connect("notify::is-playing", self._on_item_property_changed)
        item.connect("notify::is-paused", self._on_item_property_changed)
        self._update_playing_ui()

    def unbind(self):
        if self.model_item:
            try:
                self.model_item.disconnect_by_func(self._on_item_property_changed)
            except Exception:
                pass
            self.model_item = None
        self.panel = None

    def _on_item_property_changed(self, item, pspec):
        self._update_playing_ui()

    def _update_playing_ui(self):
        item = self.model_item
        if not item:
            return

        track = item.track
        self.title_lbl.set_label(track.get("title", "Unknown"))

        artist_txt = track.get("artist")
        if isinstance(artist_txt, list):
            artist_txt = ", ".join([a.get("name", "") for a in artist_txt])
        elif not artist_txt and "artists" in track:
            artist_txt = ", ".join(
                [a.get("name", "") for a in track.get("artists", [])]
            )
        if not artist_txt:
            artist_txt = "Unknown"
        self.artist_lbl.set_label(artist_txt)

        if item.is_playing:
            self.add_css_class("playing")
            if item.is_paused:
                self.indicator_icon.set_from_icon_name("media-playback-start-symbolic")
            else:
                self.indicator_icon.set_from_icon_name("media-playback-pause-symbolic")
            self.indicator_stack.set_visible_child_name("playing")
        else:
            self.remove_css_class("playing")
            # For non-playing items, we just show the index
            self.indicator_lbl.set_label(str(item.index + 1))
            self.indicator_stack.set_visible_child_name("index")

    def on_drag_prepare(self, source, x, y):
        if self.model_item:
            value = GObject.Value(str, str(self.model_item.index))
            return Gdk.ContentProvider.new_for_value(value)
        return None

    def on_drag_begin(self, source, drag):
        paintable = Gtk.WidgetPaintable.new(self)
        source.set_icon(paintable, 0, 0)

    def on_drop(self, target, value, x, y):
        try:
            source_index = int(value)
            if self.model_item and source_index != self.model_item.index:
                if self.panel:
                    self.panel._on_row_move(source_index, self.model_item.index)
            return True
        except ValueError:
            return False


class QueuePanel(Gtk.Box):
    def __init__(self, player):
        super().__init__(orientation=Gtk.Orientation.VERTICAL, spacing=0)
        self.player = player
        self.set_size_request(200, -1)  # Relaxed minimum width
        self.add_css_class("background")
        self.add_css_class("queue-panel")  # For potential styling

        # Use Adw.ToolbarView and Adw.HeaderBar for a native look
        self.toolbar_view = Adw.ToolbarView()
        self.header_bar = Adw.HeaderBar()
        self.header_bar.add_css_class("flat")
        self.header_bar.set_show_end_title_buttons(False)
        self.header_bar.set_show_start_title_buttons(False)

        # Title/Subtitle in HeaderBar
        title_box = Gtk.Box(orientation=Gtk.Orientation.VERTICAL, spacing=0)
        title_box.set_valign(Gtk.Align.CENTER)

        self.title_label = Gtk.Label(label="Queue")
        self.title_label.add_css_class("sidebar-title")
        self.title_label.set_halign(Gtk.Align.CENTER)

        self.count_label = Gtk.Label(label="0 tracks")
        self.count_label.add_css_class("caption-2")
        self.count_label.add_css_class("dim-label")
        self.count_label.set_halign(Gtk.Align.CENTER)
        self.count_label.set_opacity(0.6)

        title_box.append(self.title_label)
        title_box.append(self.count_label)
        self.header_bar.set_title_widget(title_box)

        # Shuffle Toggle
        self.shuffle_btn = Gtk.ToggleButton(icon_name="media-playlist-shuffle-symbolic")
        self.shuffle_btn.set_tooltip_text("Shuffle Queue")
        self.shuffle_btn.connect("clicked", self._on_shuffle_clicked)
        self.header_bar.pack_start(self.shuffle_btn)

        # Repeat Toggle
        self.repeat_btn = Gtk.Button(icon_name="media-playlist-consecutive-symbolic")
        self.repeat_btn.set_tooltip_text("Repeat Mode")
        self.repeat_btn.connect("clicked", self._on_repeat_clicked)
        self.header_bar.pack_start(self.repeat_btn)

        # Clear Button
        clear_btn = Gtk.Button(label="Clear")
        clear_btn.connect("clicked", self._on_clear_clicked)
        self.header_bar.pack_end(clear_btn)

        # More Menu
        self.action_group = Gio.SimpleActionGroup()
        self.insert_action_group("queue", self.action_group)
        action_add = Gio.SimpleAction.new(
            "add_all_to_playlist", GLib.VariantType.new("s")
        )
        action_add.connect("activate", self._on_add_all_to_playlist)
        self.action_group.add_action(action_add)

        action_show_add = Gio.SimpleAction.new("show_add_all_to_playlist", None)
        action_show_add.connect("activate", self._on_show_add_all_to_playlist)
        self.action_group.add_action(action_show_add)

        self.more_btn = Gtk.MenuButton(icon_name="view-more-symbolic")
        self.more_btn.set_tooltip_text("More Options")
        self.more_menu_model = Gio.Menu()
        self.playlist_menu = Gio.Menu()
        self.more_btn.set_menu_model(self.more_menu_model)
        self.header_bar.pack_end(self.more_btn)

        self.toolbar_view.add_top_bar(self.header_bar)
        self.append(self.toolbar_view)

        # ListView Setup - use NoSelection to avoid selection-changed race conditions.
        # User clicks are handled via explicit gesture in factory setup.
        self.store = Gio.ListStore(item_type=QueueItem)
        self.selection_model = Gtk.NoSelection(model=self.store)

        factory = Gtk.SignalListItemFactory()
        factory.connect("setup", self._on_factory_setup)
        factory.connect("bind", self._on_factory_bind)
        factory.connect("unbind", self._on_factory_unbind)

        self.list_view = Gtk.ListView(model=self.selection_model, factory=factory)

        scrolled = ScrolledWindow()
        scrolled.set_vexpand(True)
        scrolled.set_child(self.list_view)
        self.append(scrolled)

        # Signals
        weak_self = weakref.ref(self)
        self._player_state_handler = self.player.connect(
            "state-changed", lambda player, *args: weak_self()._on_player_update(player, *args) if weak_self() else None
        )
        self._player_metadata_handler = self.player.connect(
            "metadata-changed", lambda player, *args: weak_self()._on_player_update(player, *args) if weak_self() else None
        )
        self.connect("destroy", lambda w: weak_self()._on_destroy(w) if weak_self() else None)
        self.connect("map", self._on_map)  # Refresh when visible

        # Initial Populate
        self._populate()
        self._update_shuffle_state()
        self._update_repeat_state()

    def _refresh_playlists_menu(self):
        # The popover handles its own list — we just toggle whether the menu
        # exposes the Add-to-Playlist item at all (no playlists => hide).
        self.more_menu_model = Gio.Menu()
        self.more_btn.set_menu_model(self.more_menu_model)
        from ui.utils import is_online

        if not is_online():
            return
        if self.player.client.get_editable_playlists():
            self.more_menu_model.append(
                "Add all to Playlist…", "queue.show_add_all_to_playlist"
            )

    def _on_add_all_to_playlist(self, action, param):
        self._do_add_all_to_playlist(param.get_string())

    def _on_show_add_all_to_playlist(self, action, param):
        from ui.widgets.add_to_playlist import AddToPlaylistPopover
        pop = AddToPlaylistPopover(
            self.player,
            on_select=self._do_add_all_to_playlist,
            parent=self.more_btn,
        )
        pop.popup()

    def _do_add_all_to_playlist(self, playlist_id):
        video_ids = [t.get("videoId") for t in self.player.queue if t.get("videoId")]
        if not playlist_id or not video_ids:
            return

        from ui.widgets.add_to_playlist import mark_playlist_used
        mark_playlist_used(playlist_id)

        weak_self = weakref.ref(self)
        client = self.player.client
        def thread_func():
            success = client.add_playlist_items(playlist_id, video_ids)
            if success:
                msg = f"Added {len(video_ids)} tracks to playlist"
                print(msg)
                GLib.idle_add(lambda: weak_self()._show_toast(msg) if weak_self() else None)
            else:
                GLib.idle_add(lambda: weak_self()._show_toast("Failed to add tracks") if weak_self() else None)

        threading.Thread(target=thread_func, daemon=True).start()

    def _on_map(self, *args):
        # Refresh list when sidebar becomes visible - but only if count changed
        if self.store.get_n_items() != len(self.player.queue):
            self._populate()
        else:
            self._update_item_states()

        self._refresh_playlists_menu()
        self._update_shuffle_state()
        self._update_repeat_state()
        GLib.idle_add(self._scroll_to_current)

    def _scroll_to_current(self):
        idx = self.player.current_queue_index
        if idx >= 0 and idx < self.store.get_n_items():
            self.list_view.scroll_to(idx, Gtk.ListScrollFlags.FOCUS, None)

    def _on_shuffle_clicked(self, btn):
        self.player.shuffle_queue()
        self._scroll_to_current()

    def _update_shuffle_state(self):
        if self.player.shuffle_mode != self.shuffle_btn.get_active():
            self.shuffle_btn.set_active(self.player.shuffle_mode)

        if self.player.shuffle_mode:
            self.shuffle_btn.add_css_class("accent")
        else:
            self.shuffle_btn.remove_css_class("accent")

    def _update_repeat_state(self):
        mode = getattr(self.player, "repeat_mode", "none")
        if mode == "track":
            self.repeat_btn.set_icon_name("media-playlist-repeat-song-symbolic")
            self.repeat_btn.add_css_class("accent")
        elif mode == "all":
            self.repeat_btn.set_icon_name("media-playlist-repeat-symbolic")
            self.repeat_btn.add_css_class("accent")
        else:
            self.repeat_btn.set_icon_name("media-playlist-consecutive-symbolic")
            self.repeat_btn.remove_css_class("accent")

    def _on_repeat_clicked(self, btn):
        mode = getattr(self.player, "repeat_mode", "none")
        if mode == "none":
            self.player.set_repeat_mode("all")
        elif mode == "all":
            self.player.set_repeat_mode("track")
        else:
            self.player.set_repeat_mode("none")

    def _populate(self):
        queue = self.player.queue
        current_idx = self.player.current_queue_index

        items = []
        player_state = self.player.get_state_string()
        is_paused = player_state in ("paused", "stopped")

        for i, track in enumerate(queue):
            items.append(QueueItem(track, i, i == current_idx, is_paused))

        self.store.splice(0, self.store.get_n_items(), items)
        self.count_label.set_label(f"{len(queue)} tracks")

        if self.get_mapped():
            GLib.idle_add(self._scroll_to_current)

    def _on_factory_setup(self, factory, list_item):
        widget = QueueRowWidget()
        list_item.set_child(widget)

        # Left click for activation
        gesture = Gtk.GestureClick()
        gesture.set_button(1)
        gesture.connect("released", self._on_row_clicked, list_item)
        widget.add_controller(gesture)

        # Right click for context menu
        right_click = Gtk.GestureClick()
        right_click.set_button(3)
        right_click.connect("released", self._on_row_right_click, list_item)
        widget.add_controller(right_click)

        # Long press for touch
        lp = Gtk.GestureLongPress()
        lp.connect(
            "pressed",
            lambda g, x, y, li=list_item: self._on_row_right_click(g, 1, x, y, li),
        )
        widget.add_controller(lp)

    def _on_factory_bind(self, factory, list_item):
        widget = list_item.get_child()
        item = list_item.get_item()
        widget.bind(item, self)

    def _on_factory_unbind(self, factory, list_item):
        widget = list_item.get_child()
        if widget and hasattr(widget, "unbind"):
            widget.unbind()

    def _on_destroy(self, widget):
        if hasattr(self, "_player_state_handler") and self._player_state_handler is not None:
            try:
                self.player.disconnect(self._player_state_handler)
            except Exception:
                pass
            self._player_state_handler = None
        if hasattr(self, "_player_metadata_handler") and self._player_metadata_handler is not None:
            try:
                self.player.disconnect(self._player_metadata_handler)
            except Exception:
                pass
            self._player_metadata_handler = None

    def _on_row_clicked(self, gesture, n_press, x, y, list_item):
        item = list_item.get_item()
        if item and item.index != self.player.current_queue_index:
            self.player.play_queue_index(item.index)

    def _on_row_right_click(self, gesture, n_press, x, y, list_item):
        item = list_item.get_item()
        if not item:
            return

        track = item.track
        vid = track.get("videoId")
        idx = item.index
        widget = list_item.get_child()

        group = Gio.SimpleActionGroup()
        widget.insert_action_group("q", group)

        menu = Gio.Menu()

        from ui.utils import is_online

        _online = is_online()

        # Actions section
        action_section = Gio.Menu()

        # Start Radio (online only)
        if vid and _online:
            action_section.append("Start Radio", "q.start_radio")
            a_radio = Gio.SimpleAction.new("start_radio", None)
            a_radio.connect(
                "activate", lambda a, p, v=vid: self.player.start_radio(video_id=v)
            )
            group.add_action(a_radio)

        # Add to Playlist (online only) via the custom popover
        if vid and _online and self.player.client.get_editable_playlists():
            action_section.append("Add to Playlist…", "q.show_add_to_playlist")

            def _show_popover(act, param, v=vid, w=widget):
                from ui.widgets.add_to_playlist import (
                    AddToPlaylistPopover, mark_playlist_used,
                )

                weak_self = weakref.ref(self)
                def _on_select(target_pid):
                    if not target_pid:
                        return
                    mark_playlist_used(target_pid)

                    client = self.player.client
                    def _thread():
                        # add_playlist_items auto-swaps OMV→ATV for
                        # single-item adds.
                        success = client.add_playlist_items(
                            target_pid, [v]
                        )
                        if success:
                            GLib.idle_add(lambda: weak_self()._show_toast("Added to playlist") if weak_self() else None)
                        else:
                            GLib.idle_add(lambda: weak_self()._show_toast("Failed to add") if weak_self() else None)

                    threading.Thread(target=_thread, daemon=True).start()

                pop = AddToPlaylistPopover(
                    self.player, on_select=_on_select, parent=w
                )
                pop.popup()

            a_show = Gio.SimpleAction.new("show_add_to_playlist", None)
            a_show.connect("activate", _show_popover)
            group.add_action(a_show)

        # Remove from Queue
        action_section.append("Remove from Queue", "q.remove")
        a_remove = Gio.SimpleAction.new("remove", None)
        a_remove.connect(
            "activate", lambda a, p, i=idx: self.player.remove_from_queue(i)
        )
        group.add_action(a_remove)

        menu.append_section(None, action_section)

        # Clipboard section (online only)
        if vid and _online:
            clip_section = Gio.Menu()
            clip_section.append("Copy Link", "q.copy_link")
            a_copy = Gio.SimpleAction.new("copy_link", None)

            def _copy_link(a, p, v=vid):
                Gdk.Display.get_default().get_clipboard().set(
                    f"https://music.youtube.com/watch?v={v}"
                )
                self._show_toast("Link copied")

            a_copy.connect("activate", _copy_link)
            group.add_action(a_copy)
            menu.append_section(None, clip_section)

        popover = Gtk.PopoverMenu.new_from_model(menu)
        popover.set_parent(widget)
        popover.set_has_arrow(False)
        rect = Gdk.Rectangle()
        rect.x = int(x)
        rect.y = int(y)
        rect.width = 1
        rect.height = 1
        popover.set_pointing_to(rect)
        popover.popup()

    def _on_row_move(self, old_index, new_index):
        if self.player.move_queue_item(old_index, new_index):
            pass

    def _on_player_update(self, player, *args):
        self._update_shuffle_state()
        self._update_repeat_state()

        # Determine if this is a state-changed or metadata-changed signal
        # state-changed (player, state) -> args = (state,)
        # metadata-changed (player, title, artist, thumb, ...) -> args = (title, ...)

        state = args[0] if len(args) == 1 else None

        # If queue length changed OR structural update requested, we MUST repopulate.
        # Defer to idle: this signal can fire synchronously from inside a row's
        # click gesture handler (via play_queue_index → stop → state-changed),
        # and splicing the ListStore mid-gesture frees the GtkListItem the
        # gesture is still unwinding from, causing a segfault.
        if state == "queue-updated" or self.store.get_n_items() != len(
            self.player.queue
        ):
            GLib.idle_add(self._populate)
        else:
            # Otherwise, just update indicators (very efficient!)
            GLib.idle_add(self._update_item_states)

    def _update_item_states(self):
        current_idx = self.player.current_queue_index
        n = self.store.get_n_items()

        player_state = self.player.get_state_string()
        is_paused_global = player_state in ("paused", "stopped")

        for i in range(n):
            item = self.store.get_item(i)
            is_playing = i == current_idx

            if item.is_playing != is_playing or item.is_paused != is_paused_global:
                item.is_playing = is_playing
                item.is_paused = is_paused_global

        if self.get_mapped():
            GLib.idle_add(self._scroll_to_current)

    def _show_toast(self, message):
        show_toast(self, message)

    def _on_clear_clicked(self, btn):
        self.player.clear_queue()
        root = self.get_root()
        if hasattr(root, "toggle_queue"):
            # If we are clearing, we might want to hide it
            if not self.player.queue:
                root.toggle_queue()

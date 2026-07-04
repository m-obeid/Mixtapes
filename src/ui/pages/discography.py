from gi.repository import Gtk, Adw, GObject, GLib, Gio, Pango, Gdk
import threading
import weakref
import json
from api.client import MusicClient
from ui.utils import AsyncImage, parse_item_metadata
from ui.util_classes import ScrolledWindow


class DiscographyPage(Adw.Bin):
    __gsignals__ = {
        "header-title-changed": (GObject.SignalFlags.RUN_FIRST, None, (str,))
    }

    def __init__(self, player, open_playlist_callback, *args, **kwargs):
        super().__init__(*args, **kwargs)
        self.player = player
        weak_self = weakref.ref(self)
        self.open_playlist_callback = open_playlist_callback
        self.client = MusicClient()
        self.channel_id = None
        self.browse_id = None
        self.params = None
        self.title = ""
        self.items = []
        self._is_loading = False
        self._has_more = True
        self._next_continuation = None

        # Main Layout
        self.main_box = Gtk.Box(orientation=Gtk.Orientation.VERTICAL)

        # Header Bar removed because Adw.NavigationPage handles it

        # Scrolled Window
        self.scrolled = ScrolledWindow()
        self.scrolled.set_policy(Gtk.PolicyType.NEVER, Gtk.PolicyType.AUTOMATIC)
        self.scrolled.set_vexpand(True)

        vadjust = self.scrolled.get_vadjustment()
        weak_self = weakref.ref(self)
        self._scroll_handler_id = vadjust.connect(
            "value-changed", lambda adj: weak_self()._on_scroll(adj) if weak_self() else None
        )

        # Content Box
        self.content_box = Gtk.Box(orientation=Gtk.Orientation.VERTICAL, spacing=16)
        self.content_box.set_margin_top(24)
        self.content_box.set_margin_bottom(24)
        self.content_box.set_margin_start(24)
        self.content_box.set_margin_end(24)

        # FlowBox for Grid
        self.flow_box = Gtk.FlowBox()
        self.flow_box.set_valign(Gtk.Align.START)
        self.flow_box.set_max_children_per_line(5)
        self.flow_box.set_min_children_per_line(2)
        self.flow_box.set_selection_mode(Gtk.SelectionMode.NONE)
        self.flow_box.set_column_spacing(0)
        self.flow_box.set_row_spacing(0)
        self.flow_box.set_homogeneous(True)
        self.flow_box.set_activate_on_single_click(True)
        weak_self = weakref.ref(self)
        self.flow_box.connect(
            "child-activated", lambda fb, child: weak_self().on_grid_child_activated(fb, child) if weak_self() else None
        )

        self.content_box.append(self.flow_box)

        # Loading Spinner. Wrapped in a vexpanding box whose natural size
        # is small, so `valign=CENTER` lands the spinner vertically
        # centered in the viewport when the grid is still empty (initial
        # load). Once items populate, the grid pushes the spinner down
        # and load-more renders it at the bottom, same as before.
        self._loading_wrap = Gtk.Box(orientation=Gtk.Orientation.VERTICAL)
        self._loading_wrap.set_vexpand(True)
        self._loading_wrap.set_valign(Gtk.Align.CENTER)
        self._loading_wrap.set_halign(Gtk.Align.CENTER)
        self._loading_wrap.set_margin_top(32)
        self._loading_wrap.set_margin_bottom(32)
        self.loading_spinner = Adw.Spinner()
        self.loading_spinner.set_halign(Gtk.Align.CENTER)
        self.loading_spinner.set_size_request(48, 48)
        self._loading_wrap.append(self.loading_spinner)
        self._loading_wrap.set_visible(False)
        self.content_box.append(self._loading_wrap)

        # Clamp for consistent width
        self.clamp = Adw.Clamp()
        self.clamp.set_maximum_size(1024)
        self.clamp.set_tightening_threshold(600)
        self.clamp.set_child(self.content_box)

        self.scrolled.set_child(self.clamp)
        self.main_box.append(self.scrolled)

        self.set_child(self.main_box)

    def set_compact_mode(self, compact):
        if compact:
            self.add_css_class("compact")
            self.content_box.set_spacing(12)
            self.content_box.set_margin_start(12)
            self.content_box.set_margin_end(12)
            self.flow_box.set_max_children_per_line(3)
        else:
            self.remove_css_class("compact")
            self.content_box.set_spacing(16)
            self.content_box.set_margin_start(24)
            self.content_box.set_margin_end(24)
            self.flow_box.set_max_children_per_line(5)

    def load_discography(
        self, channel_id, title, browse_id=None, params=None, initial_items=None
    ):
        self.channel_id = channel_id
        self.title = title
        self.browse_id = browse_id
        self.params = params

        # Clear existing items
        self.items = []
        child = self.flow_box.get_first_child()
        while child:
            next_child = child.get_next_sibling()
            self.flow_box.remove(child)
            child = next_child

        self._has_more = True
        self._next_continuation = None

        if initial_items:
            self.items.extend(initial_items)
            self._render_items(initial_items)

        self.emit("header-title-changed", title)
        self._load_more()

    def filter_content(self, text):
        query = text.lower().strip()
        child = self.flow_box.get_first_child()
        while child:
            if hasattr(child, "item_data"):
                title = child.item_data.get("title", "").lower()
                child.set_visible(not query or query in title)
            child = child.get_next_sibling()

    def _on_scroll(self, adjustment):
        if self._is_loading or not self._has_more:
            return

        value = adjustment.get_value()
        upper = adjustment.get_upper()
        page_size = adjustment.get_page_size()

        # Load more when we are near the bottom
        if upper - (value + page_size) < 200:
            self._load_more()

    def _load_more(self):
        if self._is_loading or not self._has_more:
            return

        self._is_loading = True
        self._loading_wrap.set_visible(True)

        client = self.client
        bid = self.browse_id
        p = self.params
        t_title = self.title
        def fetch_func():
            try:
                new_items = []
                if bid and "Top Songs" in t_title:
                    pass
                elif bid and p:
                    # Albums, singles, videos - get_artist_albums with raw parser fallback
                    new_items = client.get_artist_albums(
                        bid, p, limit=100
                    )
                    self._has_more = False
                elif bid:
                    # Fallback: try as playlist, then raw parse
                    try:
                        res = client.get_playlist(bid)
                        new_items = res.get("tracks", []) if res else []
                    except Exception:
                        new_items = client._raw_parse_channel_content(
                            bid, None
                        )
                    self._has_more = False

                def update_cb():
                    if new_items:
                        # Filter out items we already have
                        existing_ids = set()
                        for item in getattr(self, "items", []) or []:
                            for key in ("browseId", "videoId", "playlistId"):
                                if item.get(key):
                                    existing_ids.add(item[key])
                        filtered_items = [
                            item
                            for item in new_items
                            if not any(
                                item.get(k) in existing_ids
                                for k in ("browseId", "videoId", "playlistId")
                                if item.get(k)
                            )
                        ]

                        self.items.extend(filtered_items)
                        self._render_items(filtered_items)

                    self._is_loading = False
                    self._loading_wrap.set_visible(False)

                    if not new_items:
                        self._has_more = False

                GLib.idle_add(update_cb)
            except Exception as e:
                print(f"Error loading discography: {e}")
                GLib.idle_add(lambda: self._loading_wrap.set_visible(False))
                self._is_loading = False

        threading.Thread(target=fetch_func, daemon=True).start()

    def _render_items(self, items):
        for item in items:
            thumb_url = (
                item.get("thumbnails", [])[-1]["url"]
                if item.get("thumbnails")
                else None
            )

            item_box = Gtk.Box(orientation=Gtk.Orientation.VERTICAL, spacing=4)
            item_box.item_data = item

            img = AsyncImage(url=thumb_url, size=140)

            wrapper = Gtk.Box()
            wrapper.set_overflow(Gtk.Overflow.HIDDEN)
            wrapper.add_css_class("card")
            wrapper.set_halign(Gtk.Align.CENTER)
            wrapper.append(img)

            item_box.append(wrapper)

            lbl = Gtk.Label(label=item.get("title", ""))
            lbl.set_ellipsize(Pango.EllipsizeMode.END)
            lbl.set_wrap(True)
            lbl.set_wrap_mode(Pango.WrapMode.WORD_CHAR)
            lbl.set_lines(2)
            lbl.set_justify(Gtk.Justification.LEFT)
            lbl.set_halign(Gtk.Align.START)

            text_clamp = Adw.Clamp(maximum_size=140)
            text_clamp.set_child(lbl)
            item_box.append(text_clamp)

            # Subtitle (Year / Type / Explicit)
            meta = parse_item_metadata(item)
            parts = []
            if meta["year"]:
                parts.append(meta["year"])
            if meta["type"] and meta["type"].lower() not in [p.lower() for p in parts]:
                parts.append(meta["type"])

            subtitle_text = " • ".join(parts)

            subtitle_box = Gtk.Box(orientation=Gtk.Orientation.HORIZONTAL, spacing=4)
            subtitle_box.set_halign(Gtk.Align.START)

            if subtitle_text:
                subtitle_lbl = Gtk.Label(label=subtitle_text)
                subtitle_lbl.add_css_class("caption")
                subtitle_lbl.add_css_class("dim-label")
                subtitle_lbl.set_ellipsize(Pango.EllipsizeMode.END)
                subtitle_box.append(subtitle_lbl)

            if meta["is_explicit"]:
                explicit_lbl = Gtk.Label(label="E")
                explicit_lbl.set_justify(Gtk.Justification.CENTER)
                explicit_lbl.set_halign(Gtk.Align.CENTER)
                explicit_lbl.add_css_class("explicit-badge")
                subtitle_box.append(explicit_lbl)

            if subtitle_text or meta["is_explicit"]:
                subtitle_clamp = Adw.Clamp(maximum_size=140)
                subtitle_clamp.set_child(subtitle_box)
                item_box.append(subtitle_clamp)

            self.flow_box.append(item_box)

            gesture = Gtk.GestureClick()
            gesture.set_button(3)
            weak_self = weakref.ref(self)
            gesture.connect(
                "pressed", lambda g, n, x, y: weak_self().on_grid_right_click(g, n, x, y, g.get_widget()) if weak_self() and g.get_widget() is not None else None
            )
            item_box.add_controller(gesture)

            # Long Press for touch
            lp = Gtk.GestureLongPress()
            lp.connect(
                "pressed", lambda g, x, y: weak_self().on_grid_right_click(g, 1, x, y, g.get_widget()) if weak_self() and g.get_widget() is not None else None
            )
            item_box.add_controller(lp)

    def on_grid_child_activated(self, flowbox, child):
        box = child.get_child()
        if not hasattr(box, "item_data"):
            return

        item = box.item_data
        browse_id = item.get("browseId")
        video_id = item.get("videoId")

        if browse_id:
            # Check if it's a playlist or album based on ID prefix or other metadata
            # Most albums/singles returned by get_artist_albums have a browseId starting with MPREb
            self.open_playlist_callback(browse_id)
        elif video_id:
            # It's a video!
            app = Gtk.Application.get_default()
            window = app.get_active_window()
            if window and hasattr(window, "player"):
                window.player.play_tracks([item])

    def on_grid_right_click(self, gesture, n_press, x, y, item_box):
        if not hasattr(item_box, "item_data"):
            return
        data = item_box.item_data
        group = Gio.SimpleActionGroup()
        item_box.insert_action_group("item", group)

        # Play Action
        weak_self = weakref.ref(self)
        play_action = Gio.SimpleAction.new("play", None)
        play_action.connect(
            "activate", lambda a, p, d=data: weak_self()._on_play_item(a, p, d) if weak_self() else None
        )
        group.add_action(play_action)

        # Queue Action
        queue_action = Gio.SimpleAction.new("queue", None)
        queue_action.connect(
            "activate", lambda a, p, d=data: weak_self()._on_queue_item(a, p, d) if weak_self() else None
        )
        group.add_action(queue_action)

        def copy_json_action(action, param):
            try:
                json_str = json.dumps(data, indent=2)
                clipboard = Gdk.Display.get_default().get_clipboard()
                clipboard.set(json_str)
            except Exception:
                pass

        action_json = Gio.SimpleAction.new("copy_json", None)
        action_json.connect("activate", copy_json_action)
        group.add_action(action_json)

        menu = Gio.Menu()
        menu.append("Play", "item.play")
        menu.append("Add to queue", "item.queue")
        menu.append("Copy JSON (Debug)", "item.copy_json")

        popover = Gtk.PopoverMenu.new_from_model(menu)
        popover.set_parent(item_box)
        popover.set_has_arrow(False)
        rect = Gdk.Rectangle()
        rect.x = int(x)
        rect.y = int(y)
        rect.width = 1
        rect.height = 1
        popover.set_pointing_to(rect)
        popover.popup()

    def _on_play_item(self, action, param, data):
        app = Gtk.Application.get_default()
        window = app.get_active_window()
        if not window or not hasattr(window, "player"):
            return

        video_id = data.get("videoId")
        if video_id:
            GLib.idle_add(window.player.play_tracks, [data])
            return

        browse_id = data.get("browseId")
        if not browse_id:
            return

        # Play the album/single
        def thread_func():
            playlist_data = self.client.get_playlist(browse_id)
            tracks = playlist_data.get("tracks", [])
            if tracks:
                GLib.idle_add(window.player.play_tracks, tracks)

        threading.Thread(target=thread_func, daemon=True).start()

    def _on_queue_item(self, action, param, data):
        app = Gtk.Application.get_default()
        window = app.get_active_window()
        if not window or not hasattr(window, "player"):
            return

        video_id = data.get("videoId")
        if video_id:
            GLib.idle_add(window.player.extend_queue, [data])
            return

        browse_id = data.get("browseId")
        if not browse_id:
            return

        def thread_func():
            playlist_data = self.client.get_playlist(browse_id)
            tracks = playlist_data.get("tracks", [])
            if tracks:
                GLib.idle_add(window.player.extend_queue, tracks)

        threading.Thread(target=thread_func, daemon=True).start()

from gi.repository import Gtk, Adw, GObject, GLib, Gio, Pango, Gdk
import threading
import weakref
from api.client import MusicClient
from ui.utils import AsyncImage
from ui.util_classes import ScrolledWindow


class MoodPage(Adw.Bin):
    __gsignals__ = {
        "header-title-changed": (GObject.SignalFlags.RUN_FIRST, None, (str,))
    }

    def __init__(self, player, open_playlist_callback, *args, **kwargs):
        super().__init__(*args, **kwargs)
        self.player = player
        weak_self = weakref.ref(self)
        self.open_playlist_callback = open_playlist_callback
        self.client = MusicClient()
        self.params = None
        self.title = ""
        self.items = []
        self._is_loading = False

        # Main Layout
        self.main_box = Gtk.Box(orientation=Gtk.Orientation.VERTICAL)

        # Scrolled Window
        self.scrolled = ScrolledWindow()
        self.scrolled.set_policy(Gtk.PolicyType.NEVER, Gtk.PolicyType.AUTOMATIC)
        self.scrolled.set_vexpand(True)

        # Content Box
        self.content_box = Gtk.Box(orientation=Gtk.Orientation.VERTICAL, spacing=16)
        self.content_box.set_margin_top(24)
        self.content_box.set_margin_bottom(24)
        self.content_box.set_margin_start(24)
        self.content_box.set_margin_end(24)

        # FlowBox for Grid (Reusing Discography aesthetics)
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

        # Loading Spinner — centered vertically when the grid is empty.
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

    def load_mood(self, params, title):
        self.params = params
        self.title = title

        # Clear existing items
        self.items = []
        child = self.flow_box.get_first_child()
        while child:
            next_child = child.get_next_sibling()
            self.flow_box.remove(child)
            child = next_child

        self.emit("header-title-changed", title)
        self._load_data()

    def filter_content(self, text):
        query = text.lower().strip()
        child = self.flow_box.get_first_child()
        while child:
            if hasattr(child, "item_data"):
                title = child.item_data.get("title", "").lower()
                child.set_visible(not query or query in title)
            child = child.get_next_sibling()


    def _load_data(self):
        if self._is_loading:
            return

        self._is_loading = True
        self._loading_wrap.set_visible(True)

        client = self.client
        p = self.params
        def fetch_func():
            try:
                new_items = client.get_mood_playlists(p)

                def update_cb():
                    if new_items:
                        self.items.extend(new_items)
                        self._render_items(new_items)

                    self._is_loading = False
                    self._loading_wrap.set_visible(False)

                GLib.idle_add(update_cb)
            except Exception as e:
                print(f"Error loading mood playlists: {e}")
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
        playlist_id = item.get("playlistId")

        if playlist_id:
             self.open_playlist_callback(playlist_id)

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

        menu = Gio.Menu()
        menu.append("Play", "item.play")
        menu.append("Add to queue", "item.queue")

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

        playlist_id = data.get("playlistId")
        if not playlist_id:
            return

        def thread_func():
            playlist_data = self.client.get_playlist(playlist_id)
            tracks = playlist_data.get("tracks", [])
            if tracks:
                GLib.idle_add(window.player.play_tracks, tracks)

        threading.Thread(target=thread_func, daemon=True).start()

    def _on_queue_item(self, action, param, data):
        app = Gtk.Application.get_default()
        window = app.get_active_window()
        if not window or not hasattr(window, "player"):
            return

        playlist_id = data.get("playlistId")
        if not playlist_id:
            return

        def thread_func():
            playlist_data = self.client.get_playlist(playlist_id)
            tracks = playlist_data.get("tracks", [])
            if tracks:
                GLib.idle_add(window.player.extend_queue, tracks)

        threading.Thread(target=thread_func, daemon=True).start()

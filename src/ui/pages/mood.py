import json
import threading
from gi.repository import Adw, GLib, GObject, Gtk, Pango
from api.client import MusicClient
from ui.context_menu import MenuAction, show_item_menu
from ui.util_classes import ScrolledWindow
from ui.utils import AsyncImage, copy_to_clipboard, parse_item_metadata
from ui.widgets.media_card import (
    MediaCardWidget,
    CardWrapLayout,
    GRID_SPACING,
    GRID_LINE_SPACING,
)


class MoodPage(Adw.Bin):
    __gsignals__ = {
        "header-title-changed": (GObject.SignalFlags.RUN_FIRST, None, (str,))
    }

    def __init__(self, player, open_playlist_callback, *args, **kwargs):
        super().__init__(*args, **kwargs)
        self.player = player
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

        vadjust = self.scrolled.get_vadjustment()
        vadjust.connect("value-changed", self._on_scroll)

        # Content Box
        self.content_box = Gtk.Box(orientation=Gtk.Orientation.VERTICAL, spacing=16)
        self.content_box.set_margin_top(24)
        self.content_box.set_margin_bottom(24)
        self.content_box.set_margin_start(24)
        self.content_box.set_margin_end(24)

        # WrapBox centralizado
        self.flow_box = Adw.WrapBox()
        # Same layout as the library and discography grids.
        self.flow_box.set_layout_manager(CardWrapLayout())
        self.flow_box.set_valign(Gtk.Align.START)
        self.flow_box.set_halign(Gtk.Align.FILL)
        self.flow_box.set_align(0.5)
        self.flow_box.set_line_homogeneous(False)
        self.flow_box.set_line_spacing(GRID_LINE_SPACING)
        self.flow_box.set_child_spacing(GRID_SPACING)

        self.content_box.append(self.flow_box)

        # Loading Spinner
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
        else:
            self.remove_css_class("compact")
            self.content_box.set_spacing(16)
            self.content_box.set_margin_start(24)
            self.content_box.set_margin_end(24)
    
        child = self.flow_box.get_first_child()
        while child:
            if hasattr(child, "set_compact_mode"):
                child.set_compact_mode(compact)
            child = child.get_next_sibling()

    def load_mood(self, params, title):
        self.params = params
        self.title = title

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
            data = getattr(child, "item_data", None)
            if data:
                title = data.get("title", "").lower()
                child.set_visible(not query or query in title)
            child = child.get_next_sibling()

    def _on_scroll(self, vadjust):
        if vadjust.get_value() > 50:
            self.emit("header-title-changed", self.title)
        else:
            self.emit("header-title-changed", "")

    def _load_data(self):
        if self._is_loading:
            return

        self._is_loading = True
        self._loading_wrap.set_visible(True)

        def fetch_func():
            try:
                new_items = self.client.get_mood_playlists(self.params)

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

    def _make_card(self, item):
        card = MediaCardWidget(
            item,
            player=self.player,
            title_lines=1,
            on_clicked=lambda btn, it: self.on_card_clicked(btn)
        )
    
        gesture = Gtk.GestureClick()
        gesture.set_button(3)
        gesture.connect("pressed", self.on_grid_right_click, child if 'child' in locals() else card)
        card.add_controller(gesture)
    
        lp = Gtk.GestureLongPress()
        lp.connect(
            "pressed",
            lambda g, x, y, c=card: self.on_grid_right_click(g, 1, x, y, c),
        )
        card.add_controller(lp)
        return card

    def _render_items(self, items):
        for item in items:
            card = self._make_card(item)
            self.flow_box.append(card)

    def on_card_clicked(self, button):
        if hasattr(button, "item_data"):
            self._activate_item_data(button.item_data)

    def _activate_item_data(self, item):
        playlist_id = item.get("playlistId") or item.get("browseId")
        video_id = item.get("videoId")

        if playlist_id:
            initial_data = {
                "title": item.get("title", ""),
                "thumb": (item.get("thumbnails", [{}])[-1] or {}).get("url")
                if item.get("thumbnails")
                else None,
            }
            try:
                self.open_playlist_callback(playlist_id, initial_data)
            except TypeError:
                self.open_playlist_callback(playlist_id)
        elif video_id:
            app = Gtk.Application.get_default()
            window = app.get_active_window()
            if window and hasattr(window, "player"):
                window.player.play_tracks([item])

    def on_grid_right_click(self, gesture, n_press, x, y, item_box):
        if not hasattr(item_box, "item_data"):
            return
        data = item_box.item_data
        show_item_menu(
            item_box,
            x,
            y,
            data,
            player=self.player,
            client=self.client,
            prefix="item",
            extras=[
                MenuAction(
                    "Copy JSON (Debug)",
                    lambda d=data: copy_to_clipboard(json.dumps(d, indent=2)),
                    section="debug",
                )
            ],
        )

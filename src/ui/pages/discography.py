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

class DiscographyPage(Adw.Bin):
    __gsignals__ = {
        "header-title-changed": (GObject.SignalFlags.RUN_FIRST, None, (str,))
    }

    def __init__(self, player, open_playlist_callback, *args, **kwargs):
        super().__init__(*args, **kwargs)
        self.player = player
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

        self.flow_box = Adw.WrapBox()
        # Same layout as the library grid. Without it the wrap was decided
        # from the cards' desktop size, so a compact window could show one
        # column where two fit and only sort itself out on the next resize.
        self.flow_box.set_layout_manager(CardWrapLayout())
        self.flow_box.set_valign(Gtk.Align.START)
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

    def load_discography(
        self, channel_id, title, browse_id=None, params=None, initial_items=None
    ):
        self.channel_id = channel_id
        self.title = title
        self.browse_id = browse_id
        self.params = params

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
            target = child.get_child() if hasattr(child, "get_child") else child
            if hasattr(target, "item_data"):
                title = target.item_data.get("title", "").lower()
                child.set_visible(not query or query in title)
            child = child.get_next_sibling()

    def _on_scroll(self, adjustment):
        if self._is_loading or not self._has_more:
            return

        value = adjustment.get_value()
        upper = adjustment.get_upper()
        page_size = adjustment.get_page_size()

        if upper - (value + page_size) < 200:
            self._load_more()

    def _load_more(self):
        if self._is_loading or not self._has_more:
            return

        self._is_loading = True
        self._loading_wrap.set_visible(True)

        def fetch_func():
            try:
                new_items = []
                if self.browse_id and "Top Songs" in self.title:
                    pass
                elif self.browse_id and self.params:
                    new_items = self.client.get_artist_albums(
                        self.browse_id, self.params, limit=100
                    )
                    self._has_more = False
                elif self.browse_id:
                    try:
                        res = self.client.get_playlist(self.browse_id)
                        new_items = res.get("tracks", []) if res else []
                    except Exception:
                        new_items = self.client._raw_parse_channel_content(
                            self.browse_id, None
                        )
                    self._has_more = False

                def update_cb():
                    if new_items:
                        existing_ids = set()
                        for item in self.items:
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

    def _make_card(self, item):
        card = MediaCardWidget(
            item,
            player=self.player,
            title_lines=1,
            on_clicked=lambda btn, it: self.on_card_clicked(btn)
        )
    
        gesture = Gtk.GestureClick()
        gesture.set_button(3)
        gesture.connect("pressed", self.on_grid_right_click, card)
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

    def on_grid_child_activated(self, flowbox, child):
        target = child.get_child() if hasattr(child, "get_child") else child
        if hasattr(target, "item_data"):
            self._activate_item_data(target.item_data)

    def _activate_item_data(self, item):
        browse_id = item.get("browseId")
        video_id = item.get("videoId")

        if browse_id:
            self.open_playlist_callback(browse_id)
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

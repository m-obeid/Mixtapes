import json
import threading
from gi.repository import Adw, GLib, GObject, Gtk, Pango
from api.client import MusicClient
from ui.context_menu import MenuAction, show_item_menu, show_song_menu
from ui.util_classes import ScrolledWindow
from ui.utils import AsyncImage, AsyncPicture, LikeButton, copy_to_clipboard, parse_item_metadata
from ui.widgets.scroll_box import HorizontalScrollBox
from ui.widgets.media_card import MediaCardWidget


class CategoryPage(Adw.Bin):
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
        self._is_loading = False
        self._section_limits = {}
        self._cached_sections = None
        self._cached_params = None

        # Main Layout
        self.main_box = Gtk.Box(orientation=Gtk.Orientation.VERTICAL)

        # Scrolled Window
        self.scrolled = ScrolledWindow()
        self.scrolled.set_policy(Gtk.PolicyType.NEVER, Gtk.PolicyType.AUTOMATIC)
        self.scrolled.set_vexpand(True)

        vadjust = self.scrolled.get_vadjustment()
        vadjust.connect("value-changed", self._on_scroll)

        # Content Box
        self.content_box = Gtk.Box(orientation=Gtk.Orientation.VERTICAL, spacing=32)
        self.content_box.set_margin_top(24)
        self.content_box.set_margin_bottom(24)
        self.content_box.set_margin_start(16)
        self.content_box.set_margin_end(16)

        # Title Label
        self.page_title_label = Gtk.Label(label="")
        self.page_title_label.add_css_class("title-1")
        self.page_title_label.set_halign(Gtk.Align.START)
        self.page_title_label.set_margin_bottom(16)
        self.content_box.append(self.page_title_label)

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

        # Clamp for consistent width
        self.clamp = Adw.Clamp()
        self.clamp.set_maximum_size(1024)
        self.clamp.set_tightening_threshold(600)
        self.clamp.set_child(self.content_box)

        self.scrolled.set_child(self.clamp)
        self.main_box.append(self.scrolled)

        self.set_child(self.main_box)

    def _on_scroll(self, vadjust):
        if vadjust.get_value() > 50:
            self.emit("header-title-changed", self.title)
        else:
            self.emit("header-title-changed", "")

    def set_compact_mode(self, compact):
        self._compact = compact
        self._propagate_compact(self.content_box, compact)

        if compact:
            self.add_css_class("compact")
            self.content_box.set_spacing(16)
        else:
            self.remove_css_class("compact")
            self.content_box.set_spacing(32)

    def _propagate_compact(self, widget, compact):
        if hasattr(widget, "set_compact") and hasattr(widget, "target_size"):
            widget.set_compact(compact)
        child = widget.get_first_child() if hasattr(widget, "get_first_child") else None
        while child:
            self._propagate_compact(child, compact)
            child = child.get_next_sibling()

    def load_category(self, params, title):
        self.params = params
        self.title = title
        self.page_title_label.set_label(title)

        child = self.content_box.get_first_child()
        while child:
            next_child = child.get_next_sibling()
            if child != self._loading_wrap and child != self.page_title_label:
                self.content_box.remove(child)
            child = next_child

        self.emit("header-title-changed", title)
        self._load_data()

    def _load_data(self):
        if self._is_loading:
            return

        self._is_loading = True
        self._loading_wrap.set_visible(True)

        if self._cached_sections is not None and self._cached_params == self.params:
            GLib.idle_add(self._render_sections, self._cached_sections)
            return

        def fetch_func():
            try:
                sections = self.client.get_category_page(self.params)
                self._cached_sections = sections
                self._cached_params = self.params
                GLib.idle_add(self._render_sections, sections)
            except Exception as e:
                print(f"Error loading category page: {e}")
                GLib.idle_add(lambda: self._loading_wrap.set_visible(False))
                self._is_loading = False

        threading.Thread(target=fetch_func, daemon=True).start()

    def _render_sections(self, sections):
        child = self.content_box.get_first_child()
        while child:
            next_child = child.get_next_sibling()
            if child != self._loading_wrap and child != self.page_title_label:
                self.content_box.remove(child)
            child = next_child

        if sections:
            for section in sections:
                is_video_section = "video" in section["title"].lower()
                is_song_section = section["title"].lower() == "songs" or (
                    not is_video_section
                    and all(not i.get("browseId") and i.get("videoId") for i in section["items"][:3])
                )

                if is_song_section:
                    self._add_songs_list(section["title"], section["items"])
                else:
                    self._add_carousel(section["title"], section["items"])

        self._is_loading = False
        self._loading_wrap.set_visible(False)

    def _make_card(self, item):
        card = MediaCardWidget(
            item,
            player=self.player,
            title_lines=1,
            on_clicked=lambda btn, it: self._on_item_clicked(None, 1, 0, 0, it)
        )
    
        gesture = Gtk.GestureClick()
        gesture.set_button(3)
        gesture.connect("released", self.on_grid_right_click, card)
        card.add_controller(gesture)
    
        lp = Gtk.GestureLongPress()
        lp.connect(
            "pressed",
            lambda g, x, y, c=card: self.on_grid_right_click(g, 1, x, y, c),
        )
        card.add_controller(lp)
        return card

    def _add_carousel(self, title, items):
        if not items:
            return

        section_box = Gtk.Box(orientation=Gtk.Orientation.VERTICAL, spacing=16)
        self.content_box.append(section_box)

        label = Gtk.Label(label=title)
        label.add_css_class("heading")
        label.set_halign(Gtk.Align.START)
        section_box.append(label)

        scroll_box = HorizontalScrollBox()
        inner_box = Gtk.Box(orientation=Gtk.Orientation.HORIZONTAL, spacing=16)

        for item in items:
            card = self._make_card(item)
            inner_box.append(card)

        scroll_box.set_content(inner_box)
        section_box.append(scroll_box)

    def _add_songs_list(self, title, items):
        if not items:
            return

        section_box = Gtk.Box(orientation=Gtk.Orientation.VERTICAL, spacing=8)
        self.content_box.append(section_box)

        label = Gtk.Label(label=title)
        label.add_css_class("heading")
        label.set_halign(Gtk.Align.START)
        label.set_margin_bottom(8)
        section_box.append(label)

        list_box = Gtk.ListBox()
        list_box.add_css_class("boxed-list")
        list_box.add_css_class("songs-list")
        list_box.set_selection_mode(Gtk.SelectionMode.NONE)

        limit = self._section_limits.get(title, 5)
        showing_items = items[:limit]

        for item in showing_items:
            row = Gtk.ListBoxRow()
            box = Gtk.Box(orientation=Gtk.Orientation.HORIZONTAL, spacing=12)
            box.add_css_class("song-row")
            row.set_child(box)

            thumbnails = item.get("thumbnails", [])
            thumb_url = thumbnails[-1]["url"] if thumbnails else None

            img = AsyncPicture(
                url=thumb_url,
                target_size=56,
                crop_to_square=True,
                player=self.player,
            )
            img.video_id = item.get("videoId")
            img.add_css_class("song-img")
            root = self.get_root()
            img.set_compact(getattr(root, "_is_compact", False) if root else False)
            box.append(img)

            song_title = item.get("title", "Unknown")

            artist_list = item.get("artists", [])
            subtitle = ""
            if isinstance(artist_list, list):
                subtitle = ", ".join(
                    [
                        a.get("name", "Unknown")
                        for a in artist_list
                        if isinstance(a, dict)
                    ]
                )
            else:
                subtitle = artist_list or ""

            vbox = Gtk.Box(orientation=Gtk.Orientation.VERTICAL, spacing=2)
            vbox.set_valign(Gtk.Align.CENTER)
            vbox.set_hexpand(True)

            title_label = Gtk.Label(label=song_title)
            title_label.set_halign(Gtk.Align.START)
            title_label.set_ellipsize(Pango.EllipsizeMode.END)
            title_label.set_lines(1)
            title_label.set_width_chars(1)
            title_label.set_xalign(0.0)

            subtitle_label = Gtk.Label(label=subtitle)
            subtitle_label.set_halign(Gtk.Align.START)
            subtitle_label.set_ellipsize(Pango.EllipsizeMode.END)
            subtitle_label.set_lines(1)
            subtitle_label.set_width_chars(1)
            subtitle_label.set_xalign(0.0)
            subtitle_label.add_css_class("dim-label")
            subtitle_label.add_css_class("caption")

            title_box = Gtk.Box(orientation=Gtk.Orientation.HORIZONTAL, spacing=6)
            title_box.append(title_label)

            meta = parse_item_metadata(item)
            if meta["is_explicit"]:
                explicit_badge = Gtk.Label(label="E")
                explicit_badge.add_css_class("explicit-badge")
                explicit_badge.set_valign(Gtk.Align.CENTER)
                title_box.append(explicit_badge)

            vbox.append(title_box)
            vbox.append(subtitle_label)
            box.append(vbox)

            if item.get("videoId"):
                like_btn = LikeButton(
                    self.client, item["videoId"], item.get("likeStatus", "INDIFFERENT")
                )
                like_btn.set_valign(Gtk.Align.CENTER)
                box.append(like_btn)

            row.item_data = item
            list_box.append(row)

            click_gesture = Gtk.GestureClick()
            click_gesture.set_button(1)
            click_gesture.connect("released", self._on_item_clicked, item)
            row.add_controller(click_gesture)

            right_click = Gtk.GestureClick()
            right_click.set_button(3)
            right_click.connect("released", self.on_song_right_click, row)
            row.add_controller(right_click)

        section_box.append(list_box)

        if len(items) > limit:
            show_all_btn = Gtk.Button(label="View All")
            show_all_btn.add_css_class("pill")
            show_all_btn.set_halign(Gtk.Align.CENTER)
            show_all_btn.set_margin_top(12)

            btn_box = Gtk.Box(orientation=Gtk.Orientation.HORIZONTAL, spacing=8)
            btn_box.set_halign(Gtk.Align.CENTER)
            btn_box.append(show_all_btn)

            show_all_btn.connect("clicked", lambda btn, t=title: self.on_show_all_songs_clicked(t))
            section_box.append(btn_box)

    def on_show_all_songs_clicked(self, title):
        self._section_limits[title] = 1000
        if self._cached_sections:
            self._render_sections(self._cached_sections)

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

    def on_song_right_click(self, gesture, n_press, x, y, row):
        if not hasattr(row, "item_data"):
            return
        show_song_menu(
            row,
            x,
            y,
            row.item_data,
            player=self.player,
            client=self.client,
            prefix="row",
        )

    def _on_item_clicked(self, gesture, n_press, x, y, item):
        video_id = item.get("videoId")
        browse_id = item.get("browseId") or item.get("playlistId")

        if video_id:
            self.player.play_tracks([item])
        elif browse_id:
            initial_data = {
                "title": item.get("title", ""),
                "thumb": (item.get("thumbnails", [{}])[-1] or {}).get("url")
                if item.get("thumbnails")
                else None,
            }
            try:
                self.open_playlist_callback(browse_id, initial_data)
            except TypeError:
                self.open_playlist_callback(browse_id)

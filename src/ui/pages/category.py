import json
import weakref
from gi.repository import Gtk, Adw, GObject, GLib, Pango, Gio, Gdk
import threading
from api.client import MusicClient
from ui.utils import AsyncImage, parse_item_metadata
from ui.widgets.scroll_box import HorizontalScrollBox
from ui.util_classes import ScrolledWindow

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

    def set_compact_mode(self, compact):
        self._compact = compact
        # Propagate compact to all song row images
        self._propagate_compact(self.content_box, compact)

        if compact:
            self.add_css_class("compact")
            self.content_box.set_spacing(16)
        else:
            self.remove_css_class("compact")
            self.content_box.set_spacing(32)

    def _propagate_compact(self, widget, compact):
        if hasattr(widget, 'set_compact') and hasattr(widget, 'target_size'):
            widget.set_compact(compact)
        child = widget.get_first_child() if hasattr(widget, 'get_first_child') else None
        while child:
            self._propagate_compact(child, compact)
            child = child.get_next_sibling()

    def load_category(self, params, title):
        self.params = params
        self.title = title
        self.page_title_label.set_label(title)

        # Clear existing items
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

        client = self.client
        p = self.params
        def fetch_func():

            try:
                sections = client.get_category_page(p)

                self._cached_sections = sections
                self._cached_params = p
                GLib.idle_add(self._render_sections, sections)
            except Exception as e:
                print(f"Error loading category page: {e}")
                GLib.idle_add(lambda: self._loading_wrap.set_visible(False))
                self._is_loading = False

        threading.Thread(target=fetch_func, daemon=True).start()

    def _render_sections(self, sections):
        # Clear existing items again just in case
        child = self.content_box.get_first_child()
        while child:
            next_child = child.get_next_sibling()
            if child != self.loading_spinner and child != self.page_title_label:
                self.content_box.remove(child)
            child = next_child

        if sections:
            for section in sections:
                is_video_section = "video" in section["title"].lower()
                is_song_section = section["title"].lower() == "songs" or (not is_video_section and all(not i.get("browseId") and i.get("videoId") for i in section["items"][:3]))
                
                if is_song_section:
                    self._add_songs_list(section["title"], section["items"])
                else:
                    self._add_carousel(section["title"], section["items"])

        self._is_loading = False
        self._loading_wrap.set_visible(False)

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
            thumb_url = (
                item.get("thumbnails", [])[-1]["url"]
                if item.get("thumbnails")
                else None
            )

            item_box = Gtk.Box(orientation=Gtk.Orientation.VERTICAL, spacing=4)
            item_box.add_css_class("artist-horizontal-item")
            item_box.item_data = item
            
            img = AsyncImage(url=thumb_url, size=140, player=self.player)
            
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

            meta = parse_item_metadata(item)
            parts = []
            if meta["year"]:
                parts.append(meta["year"])
            if meta["type"]:
                parts.append(meta["type"])

            subtitle_text = " • ".join(parts)
            if not subtitle_text and item.get("artists"):
                artists = item.get("artists")
                if isinstance(artists, list):
                    subtitle_text = ", ".join([a.get("name", "") for a in artists])
                else:
                    subtitle_text = artists

            if subtitle_text or meta["is_explicit"]:
                subtitle_box = Gtk.Box(orientation=Gtk.Orientation.HORIZONTAL, spacing=4)
                subtitle_box.set_halign(Gtk.Align.START)
                
                if meta["is_explicit"]:
                    explicit_lbl = Gtk.Label(label="E")
                    explicit_lbl.set_justify(Gtk.Justification.CENTER)
                    explicit_lbl.set_halign(Gtk.Align.CENTER)
                    explicit_lbl.add_css_class("explicit-badge")
                    subtitle_box.append(explicit_lbl)

                if subtitle_text:
                    subtitle_lbl = Gtk.Label(label=subtitle_text)
                    subtitle_lbl.add_css_class("caption")
                    subtitle_lbl.add_css_class("dim-label")
                    subtitle_lbl.set_ellipsize(Pango.EllipsizeMode.END)
                    subtitle_box.append(subtitle_lbl)

                subtitle_clamp = Adw.Clamp(maximum_size=140)
                subtitle_clamp.set_child(subtitle_box)
                item_box.append(subtitle_clamp)

            inner_box.append(item_box)

            click_gesture = Gtk.GestureClick()
            click_gesture.set_button(1)
            weak_self = weakref.ref(self)
            click_gesture.connect(
                "released", lambda g, n, x, y, it=item: weak_self()._on_item_clicked(g, n, x, y, it) if weak_self() else None
            )
            item_box.add_controller(click_gesture)

            # Right Click context menu
            right_click = Gtk.GestureClick()
            right_click.set_button(3)
            right_click.connect(
                "released", lambda g, n, x, y: weak_self().on_grid_right_click(g, n, x, y, g.get_widget()) if weak_self() and g.get_widget() is not None else None
            )
            item_box.add_controller(right_click)

            lp = Gtk.GestureLongPress()
            lp.connect(
                "pressed", lambda g, x, y: weak_self().on_grid_right_click(g, 1, x, y, g.get_widget()) if weak_self() and g.get_widget() is not None else None
            )
            item_box.add_controller(lp)

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
        list_box.set_selection_mode(Gtk.SelectionMode.NONE)

        limit = self._section_limits.get(title, 5)
        showing_items = items[:limit]

        for item in showing_items:
            row = Gtk.ListBoxRow()
            box = Gtk.Box(orientation=Gtk.Orientation.HORIZONTAL, spacing=12)
            box.add_css_class("song-row")
            row.set_child(box)

            # Thumbnail
            thumbnails = item.get("thumbnails", [])
            thumb_url = thumbnails[-1]["url"] if thumbnails else None
            from ui.utils import AsyncPicture

            img = AsyncPicture(
                url=thumb_url,
                target_size=56,
                crop_to_square=True,
                player=self.player,
            )
            img.video_id = item.get("videoId")
            img.add_css_class("song-img")
            root = self.get_root()
            img.set_compact(getattr(root, '_is_compact', False) if root else False)
            box.append(img)

            song_title = item.get("title", "Unknown")

            # Artists
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

            subtitle_label = Gtk.Label(label=subtitle)
            subtitle_label.set_halign(Gtk.Align.START)
            subtitle_label.set_ellipsize(Pango.EllipsizeMode.END)
            subtitle_label.set_lines(1)
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

            from ui.utils import LikeButton
            if item.get("videoId"):
                like_btn = LikeButton(
                    self.client, item["videoId"], item.get("likeStatus", "INDIFFERENT")
                )
                like_btn.set_valign(Gtk.Align.CENTER)
                box.append(like_btn)
                row._like_btn = like_btn

            row.item_data = item
            list_box.append(row)

            # Left Click Activation
            click_gesture = Gtk.GestureClick()
            click_gesture.set_button(1)
            weak_self = weakref.ref(self)
            click_gesture.connect(
                "released", lambda g, n, x, y, it=item: weak_self()._on_item_clicked(g, n, x, y, it) if weak_self() else None
            )
            row.add_controller(click_gesture)

            # Right Click context menu
            right_click = Gtk.GestureClick()
            right_click.set_button(3)
            right_click.connect(
                "released", lambda g, n, x, y: weak_self().on_song_right_click(g, n, x, y, g.get_widget()) if weak_self() and g.get_widget() is not None else None
            )
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

            show_all_btn.connect(
                "clicked", lambda btn, t=title: weak_self().on_show_all_songs_clicked(t) if weak_self() else None
            )
            section_box.append(btn_box)

    def on_show_all_songs_clicked(self, title):
        self._section_limits[title] = 1000
        if self._cached_sections:
            self._render_sections(self._cached_sections)

    def on_grid_right_click(self, gesture, n_press, x, y, item_box):
        if not hasattr(item_box, "item_data"):
            return
        data = item_box.item_data
        group = Gio.SimpleActionGroup()
        item_box.insert_action_group("item", group)

        url = None
        if "videoId" in data:
            url = f"https://music.youtube.com/watch?v={data['videoId']}"
        elif "audioPlaylistId" in data:
            url = f"https://music.youtube.com/playlist?list={data['audioPlaylistId']}"
        elif "browseId" in data:
            bid = data["browseId"]
            if bid.startswith("MPRE") or bid.startswith("OLAK"):
                url = f"https://music.youtube.com/playlist?list={bid}"
            else:
                url = f"https://music.youtube.com/browse/{bid}"

        def copy_link_action(action, param):
            if url:
                try:
                    clipboard = Gdk.Display.get_default().get_clipboard()
                    clipboard.set(url)
                    root = self.get_root()
                    if root and hasattr(root, "add_toast"):
                        root.add_toast("Link copied")
                except Exception:
                    pass

        def add_to_playlist(target_pid):
            target_vid = data.get("videoId")
            if not (target_pid and target_vid):
                return
            from ui.widgets.add_to_playlist import mark_playlist_used
            mark_playlist_used(target_pid)

            def thread_func():
                self.client.add_playlist_items(target_pid, [target_vid])

            threading.Thread(target=thread_func, daemon=True).start()

        def show_add_to_playlist_action(action, param):
            from ui.widgets.add_to_playlist import AddToPlaylistPopover
            pop = AddToPlaylistPopover(
                self.player, on_select=add_to_playlist, parent=item_box
            )
            pop.popup()

        action_add = Gio.SimpleAction.new("show_add_to_playlist", None)
        action_add.connect("activate", show_add_to_playlist_action)
        group.add_action(action_add)

        action_copy = Gio.SimpleAction.new("copy_link", None)
        action_copy.connect("activate", copy_link_action)
        group.add_action(action_copy)

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
        if url:
            menu.append("Copy Link", "item.copy_link")
        menu.append("Copy JSON (Debug)", "item.copy_json")

        if "videoId" in data and self.client.get_editable_playlists():
            menu.append("Add to Playlist…", "item.show_add_to_playlist")

        if menu.get_n_items() > 0:
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

    def on_song_right_click(self, gesture, n_press, x, y, row):
        if not hasattr(row, "item_data"):
            return
        data = row.item_data
        group = Gio.SimpleActionGroup()
        row.insert_action_group("row", group)

        url = None
        if "videoId" in data:
            url = f"https://music.youtube.com/watch?v={data['videoId']}"

        def copy_link_action(action, param):
            if url:
                try:
                    clipboard = Gdk.Display.get_default().get_clipboard()
                    clipboard.set(url)
                    root = self.get_root()
                    if root and hasattr(root, "add_toast"):
                        root.add_toast("Link copied")
                except Exception:
                    pass

        def add_to_playlist(target_pid):
            target_vid = data.get("videoId")
            if not (target_pid and target_vid):
                return
            from ui.widgets.add_to_playlist import mark_playlist_used
            mark_playlist_used(target_pid)

            def thread_func():
                self.client.add_playlist_items(target_pid, [target_vid])

            threading.Thread(target=thread_func, daemon=True).start()

        def show_add_to_playlist_action(action, param):
            from ui.widgets.add_to_playlist import AddToPlaylistPopover
            pop = AddToPlaylistPopover(
                self.player, on_select=add_to_playlist, parent=row
            )
            pop.popup()

        action_add = Gio.SimpleAction.new("show_add_to_playlist", None)
        action_add.connect("activate", show_add_to_playlist_action)
        group.add_action(action_add)

        action_copy = Gio.SimpleAction.new("copy_link", None)
        action_copy.connect("activate", copy_link_action)
        group.add_action(action_copy)

        menu = Gio.Menu()
        if url:
            menu.append("Copy Link", "row.copy_link")

        if "videoId" in data and self.client.get_editable_playlists():
            menu.append("Add to Playlist…", "row.show_add_to_playlist")

        if menu.get_n_items() > 0:
            popover = Gtk.PopoverMenu.new_from_model(menu)
            popover.set_parent(row)
            popover.set_has_arrow(False)
            rect = Gdk.Rectangle()
            rect.x = int(x)
            rect.y = int(y)
            rect.width = 1
            rect.height = 1
            popover.set_pointing_to(rect)
            popover.popup()

    def _on_item_clicked(self, gesture, n_press, x, y, item):
        video_id = item.get("videoId")
        browse_id = item.get("browseId") or item.get("playlistId")
        
        if video_id:
            self.player.play_tracks([item])
        elif browse_id:
            self.open_playlist_callback(browse_id)


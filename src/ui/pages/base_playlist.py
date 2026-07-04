from gi.repository import Gtk, Adw, GObject, GLib, Pango, Gdk, Gio
import threading
import weakref
import re
from api.client import MusicClient
from ui.utils import AsyncImage, LikeButton, get_yt_music_link, show_toast
from ui.models.song import SongItem
from ui.widgets.song_row import SongRowWidget
from ui.util_classes import ScrolledWindow


class BasePlaylistPage(Adw.Bin):
    __gsignals__ = {
        "header-title-changed": (GObject.SignalFlags.RUN_FIRST, None, (str,))
    }

    def __init__(self, player, *args, **kwargs):
        super().__init__(*args, **kwargs)
        self.player = player
        weak_self = weakref.ref(self)
        self.connect("map", lambda w: weak_self()._on_map(w) if weak_self() else None)
        self.connect("unmap", lambda w: weak_self()._on_unmap(w) if weak_self() else None)
        self.client = MusicClient()
        self.playlist_id = None
        self.playlist_title_text = ""

        # Main Layout
        self.main_box = Gtk.Box(orientation=Gtk.Orientation.VERTICAL)

        # Content Scrolled Window
        scrolled = ScrolledWindow()
        scrolled.set_policy(Gtk.PolicyType.NEVER, Gtk.PolicyType.AUTOMATIC)
        scrolled.set_vexpand(True)

        # Monitor scroll for title
        vadjust = scrolled.get_vadjustment()
        self.vadjust = vadjust
        self._scroll_handler_id = vadjust.connect(
            "value-changed", lambda adj: weak_self()._on_scroll(adj) if weak_self() else None
        )

        # Clamp for content
        clamp = Adw.Clamp()

        content_box = Gtk.Box(orientation=Gtk.Orientation.VERTICAL, spacing=24)
        content_box.set_margin_top(24)
        content_box.set_margin_bottom(24)
        content_box.set_margin_start(12)
        content_box.set_margin_end(12)

        self.main_content_box = content_box

        # 1. Header Info (Cover + Details)
        self.header_info_box = Gtk.Box(
            orientation=Gtk.Orientation.HORIZONTAL, spacing=24
        )
        self.header_info_box.set_valign(Gtk.Align.START)

        # Cover Art
        self.cover_img = AsyncImage(size=200)
        self.cover_img.set_valign(Gtk.Align.START)

        # Wrapper for rounding
        self.cover_wrapper = Gtk.Box()
        self.cover_wrapper.set_overflow(Gtk.Overflow.HIDDEN)
        self.cover_wrapper.add_css_class("rounded")
        self.cover_wrapper.set_valign(Gtk.Align.START)
        self.cover_wrapper.append(self.cover_img)

        # Clamp for Header
        header_clamp = Adw.Clamp()
        header_clamp.set_maximum_size(1024)
        header_clamp.set_tightening_threshold(600)
        header_clamp.set_child(self.header_info_box)

        self.header_info_box.append(self.cover_wrapper)

        # Details Column
        self.details_col = Gtk.Box(orientation=Gtk.Orientation.VERTICAL, spacing=6)
        self.details_col.set_valign(Gtk.Align.CENTER)
        self.details_col.set_hexpand(True)

        self.playlist_name_label = Gtk.Label(label="Playlist Title")
        self.playlist_name_label.add_css_class("title-1")
        self.playlist_name_label.set_wrap(True)
        self.playlist_name_label.set_wrap_mode(Pango.WrapMode.WORD_CHAR)
        self.playlist_name_label.set_justify(Gtk.Justification.LEFT)
        self.playlist_name_label.set_halign(Gtk.Align.START)
        self.playlist_name_label.set_ellipsize(Pango.EllipsizeMode.END)
        self.playlist_name_label.set_lines(3)
        self.details_col.append(self.playlist_name_label)

        self.description_label = Gtk.Label(label="")
        self.description_label.add_css_class("body")
        self.description_label.set_wrap(True)
        self.description_label.set_wrap_mode(Pango.WrapMode.WORD_CHAR)
        self.description_label.set_justify(Gtk.Justification.LEFT)
        self.description_label.set_halign(Gtk.Align.START)
        self._description_expanded = False
        self._full_description = ""

        self.read_more_btn = Gtk.Label()
        self.read_more_btn.set_use_markup(True)
        self.read_more_btn.set_markup("<a href='toggle'>Read more</a>")
        self.read_more_btn.add_css_class("caption")
        self.read_more_btn.set_halign(Gtk.Align.START)
        self.read_more_btn.set_visible(False)
        self.read_more_btn.connect(
            "activate-link", lambda label, uri: weak_self()._on_read_more_clicked(label, uri) if weak_self() else None
        )

        self.desc_box = Gtk.Box(orientation=Gtk.Orientation.VERTICAL, spacing=0)
        self.desc_box.append(self.description_label)
        self.desc_box.append(self.read_more_btn)
        self.desc_box.set_visible(False)
        self.details_col.append(self.desc_box)

        self.meta_label = Gtk.Label(label="")
        self.meta_label.add_css_class("caption")
        self.meta_label.set_wrap(True)
        self.meta_label.set_halign(Gtk.Align.START)
        self.meta_label.set_use_markup(True)
        self.meta_label.connect(
            "activate-link", lambda label, uri: weak_self().on_meta_link_activated(label, uri) if weak_self() else None
        )
        self.details_col.append(self.meta_label)

        self.stats_label = Gtk.Label(label="")
        self.stats_label.add_css_class("caption")
        self.stats_label.set_halign(Gtk.Align.START)
        self.details_col.append(self.stats_label)

        # Actions Row
        actions_box = Gtk.Box(orientation=Gtk.Orientation.HORIZONTAL, spacing=12)
        actions_box.set_margin_top(12)
        self.actions_box = actions_box

        play_btn = Gtk.Button(label="Play")
        play_btn.add_css_class("suggested-action")
        play_btn.add_css_class("pill")
        play_btn.connect("clicked", lambda btn: weak_self().on_play_clicked(btn) if weak_self() else None)
        actions_box.append(play_btn)

        shuffle_btn = Gtk.Button()
        shuffle_btn.set_icon_name("media-playlist-shuffle-symbolic")
        shuffle_btn.add_css_class("circular")
        shuffle_btn.set_size_request(48, 48)
        shuffle_btn.connect("clicked", lambda btn: weak_self().on_shuffle_clicked(btn) if weak_self() else None)
        actions_box.append(shuffle_btn)

        # Simplified Actions (Play/Shuffle only)
        # self.copy_link_btn is no longer in the main actions_box

        self.more_btn = Gtk.MenuButton(icon_name="view-more-symbolic")
        self.more_btn.add_css_class("circular")
        self.more_btn.set_size_request(48, 48)
        self.more_btn.set_tooltip_text("More Options")

        self.more_menu_model = Gio.Menu()
        self.playlist_menu = Gio.Menu()
        # The following will be populated by _refresh_more_menu()
        self.more_btn.set_menu_model(self.more_menu_model)
        actions_box.append(self.more_btn)

        # Actions Row
        self.action_group = Gio.SimpleActionGroup()
        self.insert_action_group("page", self.action_group)

        action_add = Gio.SimpleAction.new(
            "add_all_to_playlist", GLib.VariantType.new("s")
        )
        action_add.connect(
            "activate", lambda a, p: weak_self()._on_add_all_to_playlist(a, p) if weak_self() else None
        )
        self.action_group.add_action(action_add)

        action_show_add = Gio.SimpleAction.new("show_add_all_to_playlist", None)
        action_show_add.connect(
            "activate", lambda a, p: weak_self()._on_show_add_all_to_playlist(a, p) if weak_self() else None
        )
        self.action_group.add_action(action_show_add)

        action_copy = Gio.SimpleAction.new("copy_link", None)
        action_copy.connect(
            "activate", lambda a, p: weak_self().on_copy_link_clicked(a, p) if weak_self() else None
        )
        self.action_group.add_action(action_copy)

        self.sort_dropdown = Gtk.DropDown.new_from_strings(
            [
                "Default",
                "Title (A-Z)",
                "Artist (A-Z)",
                "Album (A-Z)",
                "Duration",
                "Year",
            ]
        )
        self.sort_dropdown.set_valign(Gtk.Align.CENTER)
        self.sort_dropdown.add_css_class("pill")
        self.sort_dropdown.add_css_class("sort-dropdown")
        self.sort_dropdown.connect(
            "notify::selected", lambda dd, spec: weak_self().on_sort_changed(dd, spec) if weak_self() else None
        )

        self.details_col.append(actions_box)
        content_box.append(header_clamp)

        # Track section
        track_section = Gtk.Box(orientation=Gtk.Orientation.VERTICAL, spacing=6)

        sort_row = Gtk.Box(orientation=Gtk.Orientation.HORIZONTAL, spacing=8)
        sort_row.append(self.sort_dropdown)
        sort_row.set_visible(False)
        self.sort_row = sort_row
        track_section.append(sort_row)

        # ListView Setup
        self.store = Gio.ListStore(item_type=SongItem)
        self.filter_model = Gtk.FilterListModel(model=self.store)
        self.custom_filter = Gtk.CustomFilter.new(
            lambda item: weak_self()._filter_func(item) if weak_self() else False
        )
        self.filter_model.set_filter(self.custom_filter)

        self.sort_model = Gtk.SortListModel(model=self.filter_model)
        # We'll set the sorter in subclasses or on sort change

        self.selection_model = Gtk.SingleSelection(model=self.sort_model)
        self.selection_model.set_autoselect(False)

        factory = Gtk.SignalListItemFactory()
        factory.connect(
            "setup", lambda f, li: weak_self()._on_factory_setup(f, li) if weak_self() else None
        )
        factory.connect(
            "bind", lambda f, li: weak_self()._on_factory_bind(f, li) if weak_self() else None
        )
        factory.connect(
            "unbind", lambda f, li: weak_self()._on_factory_unbind(f, li) if weak_self() else None
        )

        self.songs_view = Gtk.ListView(model=self.selection_model, factory=factory)
        self.songs_view.add_css_class("boxed-list")
        # Keyboard activation: Enter/Space on the focused row plays it. The
        # ListView's "activate" signal passes an int position, which
        # on_song_activated already handles (alongside the click-gesture path).
        self.songs_view.connect(
            "activate", lambda lv, pos: weak_self().on_song_activated(lv, pos) if weak_self() else None
        )
        self.songs_view.set_visible(False)
        track_section.append(self.songs_view)

        self.empty_label = Gtk.Label(label="This playlist has no songs")
        self.empty_label.add_css_class("dim-label")
        self.empty_label.set_margin_top(24)
        self.empty_label.set_visible(False)
        track_section.append(self.empty_label)

        content_box.append(track_section)

        # Spinners
        self.content_spinner = Adw.Spinner()
        self.content_spinner.set_size_request(32, 32)
        self.content_spinner.set_halign(Gtk.Align.CENTER)
        self.content_spinner.set_visible(False)
        content_box.append(self.content_spinner)

        self.load_more_spinner = Adw.Spinner()
        self.load_more_spinner.set_size_request(24, 24)
        self.load_more_spinner.set_halign(Gtk.Align.CENTER)
        self.load_more_spinner.set_visible(False)
        content_box.append(self.load_more_spinner)

        clamp.set_child(content_box)
        scrolled.set_child(clamp)
        self.main_box.append(scrolled)

        self.stack = Adw.ViewStack()
        loading_box = Gtk.Box(orientation=Gtk.Orientation.VERTICAL, spacing=12)
        loading_box.set_valign(Gtk.Align.CENTER)
        loading_box.set_halign(Gtk.Align.CENTER)
        self.spinner = Adw.Spinner()
        self.spinner.set_size_request(32, 32)
        loading_box.append(self.spinner)

        self.stack.add_named(loading_box, "loading")
        self.stack.add_named(self.main_box, "content")
        self.set_child(self.stack)

        self.current_tracks = []
        self.original_tracks = []
        self.is_loading_more = False
        self.current_filter_text = ""
        self.is_fully_loaded = False
        self.is_fully_fetched = False
        self._is_background_fetching = False

        # Signal for current playing item. The player outlives this page, so
        # we MUST disconnect on destroy — otherwise the bound method keeps a
        # strong ref to self and the entire page (with its 1000-item ListStore,
        # widget tree, track lists, etc.) leaks every time the user opens a
        # playlist. After heavy navigation that pinned hundreds of MB.
        self._player_metadata_handler = self.player.connect(
            "metadata-changed", lambda player, *args: weak_self()._update_playing_indicator(player, *args) if weak_self() else None
        )
        self.connect("destroy", lambda w: weak_self()._on_page_destroy(w) if weak_self() else None)

    def _on_page_destroy(self, widget):
        self.cleanup()

    def cleanup(self):
        """Disconnect long-lived signal handlers that prevent GC."""
        self._cleaned_up = True
        hid = getattr(self, "_player_metadata_handler", None)
        if hid is not None:
            try:
                self.player.disconnect(hid)
            except Exception:
                pass
            self._player_metadata_handler = None

    def _on_factory_setup(self, factory, list_item):
        widget = SongRowWidget(self.player, self.client)
        list_item.set_child(widget)

    def _on_factory_bind(self, factory, list_item):
        widget = list_item.get_child()
        item = list_item.get_item()
        widget.bind(item, self)
        if hasattr(widget, 'img') and hasattr(widget.img, 'set_compact'):
            root = self.get_root()
            widget.img.set_compact(getattr(root, '_is_compact', False) if root else False)

    def _on_factory_unbind(self, factory, list_item):
        widget = list_item.get_child()
        if widget and hasattr(widget, "stop_handlers"):
            widget.stop_handlers()

    def _filter_func(self, item):
        if not self.current_filter_text:
            return True
        text = self.current_filter_text.lower()
        return text in item.title.lower() or text in item.artist.lower()

    def filter_content(self, text):
        self.current_filter_text = text.strip()
        query = self.current_filter_text.lower()

        # Search original_tracks directly and repopulate with matching results
        if hasattr(self, "original_tracks") and self.original_tracks:
            # Disable GTK filter during manual rebuilds to avoid double-filtering
            self.filter_model.set_filter(None)

            if query:
                matches = []
                for i, t in enumerate(self.original_tracks):
                    title = (t.get("title") or "").lower()
                    artist = (t.get("artist") or "").lower()
                    if not artist:
                        artists = t.get("artists", [])
                        artist = ", ".join(a.get("name", "") for a in artists).lower()
                    if query in title or query in artist:
                        matches.append(SongItem(t, i))
                self.store.splice(0, self.store.get_n_items(), matches)
            else:
                # Filter cleared: restore the lazy-loaded state from current_tracks
                items = [SongItem(t, i) for i, t in enumerate(self.current_tracks)]
                self.store.splice(0, self.store.get_n_items(), items)

            # Always re-enable the GTK filter so any tracks added later are also filtered
            self.filter_model.set_filter(self.custom_filter)
            return

        # Fallback: use the GTK filter for stores without original_tracks
        self.custom_filter.changed(Gtk.FilterChange.DIFFERENT)

    def _on_scroll(self, vadjust):
        val = vadjust.get_value()
        if val > 100:
            self.emit("header-title-changed", self.playlist_title_text)
        else:
            self.emit("header-title-changed", "")

        max_val = vadjust.get_upper() - vadjust.get_page_size()
        if max_val > 0 and val >= max_val - 200:
            if (
                not self.is_loading_more
                and self.playlist_id
                and not self.is_fully_loaded
            ):
                self.load_more()

    def load_more(self):
        # Implementation in subclasses if needed
        pass

    def _on_map(self, widget):
        if hasattr(self, "vadjust"):
            if self.vadjust.get_value() > 100:
                self.emit("header-title-changed", self.playlist_title_text)
            else:
                self.emit("header-title-changed", "")
        self._refresh_more_menu()

    def _refresh_more_menu(self):
        self.more_menu_model.remove_all()
        # 1. Add All to Playlist — opens the custom popover
        if self.client.get_editable_playlists():
            self.more_menu_model.append(
                "Add all to Playlist…", "page.show_add_all_to_playlist"
            )

        # 2. Copy Link (Always shown)
        self.more_menu_model.append("Copy Link", "page.copy_link")

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
        video_ids = [
            t.get("videoId") for t in self.current_tracks if t.get("videoId")
        ]
        if not playlist_id or not video_ids:
            return

        from ui.widgets.add_to_playlist import mark_playlist_used
        mark_playlist_used(playlist_id)

        weak_self = weakref.ref(self)
        client = self.client
        def thread_func():
            success = client.add_playlist_items(playlist_id, video_ids)
            if success:
                msg = f"Added {len(video_ids)} tracks to playlist"
                print(msg)
                GLib.idle_add(lambda: weak_self()._show_toast(msg) if weak_self() else None)
            else:
                GLib.idle_add(lambda: weak_self()._show_toast("Failed to add tracks") if weak_self() else None)

        threading.Thread(target=thread_func, daemon=True).start()

    def _show_toast(self, message):
        show_toast(self, message)

    def _on_unmap(self, widget):
        self.emit("header-title-changed", "")

    def update_ui(
        self,
        title,
        description,
        meta1,
        meta2,
        thumbnails,
        tracks,
        append=False,
        total_tracks=None,
    ):

        self.stack.set_visible_child_name("content")
        self.content_spinner.set_visible(False)
        self.playlist_title_text = title
        self.playlist_name_label.set_label(title)

        if description and description.strip():
            self._full_description = description
            self._description_expanded = False
            self.read_more_btn.set_markup("<a href='toggle'>Read more</a>")
            if len(description) > 200:
                truncated = description[:200].rsplit(" ", 1)[0] + "..."
                self.description_label.set_label(truncated)
                self.read_more_btn.set_visible(True)
            else:
                self.description_label.set_label(description)
                self.read_more_btn.set_visible(False)
            self.desc_box.set_visible(True)
        else:
            self.desc_box.set_visible(False)

        self.meta_label.set_markup(meta1)
        self.stats_label.set_label(meta2)

        has_tracks = bool(tracks)
        self.songs_view.set_visible(has_tracks)
        self.empty_label.set_visible(not has_tracks)

        if thumbnails and not append:
            self.cover_img.load_url(thumbnails[-1]["url"])

        # Update Store
        new_items = []
        start_idx = len(self.current_tracks) if append else 0
        for i, t in enumerate(tracks[start_idx:]):
            item = SongItem(t, start_idx + i)
            # Check if playing
            if self.player.current_video_id == item.video_id:
                item.is_playing = True
            new_items.append(item)

        if append:
            self.store.splice(self.store.get_n_items(), 0, new_items)
            self.current_tracks.extend(tracks[start_idx:])
        else:
            self.store.splice(0, self.store.get_n_items(), new_items)
            self.current_tracks = list(tracks)

    def _update_playing_indicator(self, *args):
        current_id = self.player.current_video_id
        for i in range(self.store.get_n_items()):
            item = self.store.get_item(i)
            is_playing = item.video_id == current_id
            if item.is_playing != is_playing:
                item.is_playing = is_playing

    def on_song_activated(self, list_view, position_or_item):
        # Accepts either an int position (Gtk.ListView "activate" signal) or a
        # SongItem (row-click gesture in SongRowWidget). A SongItem's stored
        # .index reflects the underlying store, not the sorted/filtered view,
        # so we resolve the live position by identity.
        if isinstance(position_or_item, SongItem):
            item = position_or_item
            position = -1
            for i in range(self.sort_model.get_n_items()):
                if self.sort_model.get_item(i) is item:
                    position = i
                    break
            if position == -1:
                return
        else:
            position = position_or_item
            item = self.sort_model.get_item(position)
            if not item:
                return

        # Get tracks in current order (sorted & filtered)
        tracks_to_queue = []
        for i in range(self.sort_model.get_n_items()):
            tracks_to_queue.append(self.sort_model.get_item(i).track_data)

        is_inf = self._is_infinite()
        self.player.set_queue(
            tracks_to_queue, position, source_id=self.playlist_id, is_infinite=is_inf
        )

    def _is_infinite(self):
        return False

    def on_play_clicked(self, btn):
        if self.sort_model.get_n_items() == 0:
            return

        tracks_to_queue = []
        for i in range(self.sort_model.get_n_items()):
            tracks_to_queue.append(self.sort_model.get_item(i).track_data)

        self.player.set_queue(
            tracks_to_queue,
            0,
            source_id=self.playlist_id,
            is_infinite=self._is_infinite(),
        )

    def on_shuffle_clicked(self, btn):
        if self.sort_model.get_n_items() == 0:
            return

        tracks_to_queue = []
        for i in range(self.sort_model.get_n_items()):
            tracks_to_queue.append(self.sort_model.get_item(i).track_data)

        self.player.set_queue(
            tracks_to_queue,
            -1,
            shuffle=True,
            source_id=self.playlist_id,
            is_infinite=self._is_infinite(),
        )

    def on_copy_link_clicked(self, btn):
        if not self.playlist_id:
            return
        is_album = self.playlist_id.startswith("MPRE") or self.playlist_id.startswith(
            "OLAK"
        )
        link = get_yt_music_link(self.playlist_id, is_album=is_album)
        if link:
            clipboard = Gdk.Display.get_default().get_clipboard()
            clipboard.set(link)
            self._show_toast("Link copied to clipboard")
            print(f"Copied link: {link}")

    def on_sort_changed(self, dropdown, pspec):
        self.reorder_playlist(dropdown.get_selected())

    def reorder_playlist(self, sort_type):
        if not self.current_tracks:
            return

        if sort_type == 0:
            if hasattr(self, "original_tracks") and self.original_tracks:
                self.current_tracks = list(self.original_tracks)
            else:
                return
        elif sort_type == 1:
            self.current_tracks.sort(key=lambda x: x.get("title", "").lower())
        elif sort_type == 2:
            self.current_tracks.sort(
                key=lambda x: (
                    x.get("artists", [{}])[0].get("name", "").lower()
                    if x.get("artists")
                    else "",
                    x.get("title", "").lower(),
                )
            )
        elif sort_type == 3:
            self.current_tracks.sort(
                key=lambda x: (
                    x.get("album", {}).get("name", "").lower()
                    if isinstance(x.get("album"), dict)
                    else str(x.get("album") or "").lower(),
                    x.get("title", "").lower(),
                )
            )
        elif sort_type == 4:  # Duration
            self.current_tracks.sort(key=lambda x: x.get("duration_seconds", 0))
        elif sort_type == 5:  # Year
            self.current_tracks.sort(
                key=lambda x: (
                    str(x.get("year", "0"))
                    if "year" in x
                    else str(x.get("album", {}).get("year", "0"))
                ),
                reverse=True,
            )

        self.store.remove_all()
        new_items = []
        for i, t in enumerate(self.current_tracks):
            item = SongItem(t, i)
            if self.player.current_video_id == item.video_id:
                item.is_playing = True
            new_items.append(item)
        self.store.splice(0, 0, new_items)

    def on_meta_link_activated(self, label, uri):
        if uri.startswith("artist:"):
            aid = uri.split(":", 1)[1]
            root = self.get_root()
            if hasattr(root, "open_artist"):
                root.open_artist(aid, "Artist")
            return True
        return False

    def _move_to_top(self, set_video_id, video_id):
        # Same logic as before
        pass

    def _on_read_more_clicked(self, label, uri):
        GLib.idle_add(self._toggle_description)
        return True

    def _toggle_description(self):
        self._description_expanded = not self._description_expanded
        if self._description_expanded:
            self.description_label.set_label(self._full_description)
            text = "Show less"
        else:
            truncated = self._full_description[:200].rsplit(" ", 1)[0] + "..."
            self.description_label.set_label(truncated)
            text = "Read more"
        parent = self.read_more_btn.get_parent()
        parent.remove(self.read_more_btn)
        self.read_more_btn = Gtk.Label()
        self.read_more_btn.set_use_markup(True)
        self.read_more_btn.set_markup(f"<a href='toggle'>{text}</a>")
        self.read_more_btn.add_css_class("caption")
        self.read_more_btn.set_halign(Gtk.Align.START)
        self.read_more_btn.connect("activate-link", self._on_read_more_clicked)
        parent.append(self.read_more_btn)
        return False

    def set_compact_mode(self, compact):
        # Propagate compact to all song row images
        self._compact = compact
        child = self.songs_view.get_first_child()
        while child:
            row_widget = child.get_first_child()
            if row_widget and hasattr(row_widget, 'img') and hasattr(row_widget.img, 'set_compact'):
                row_widget.img.set_compact(compact)
            child = child.get_next_sibling()

        if compact:
            self.header_info_box.set_orientation(Gtk.Orientation.VERTICAL)
            self.header_info_box.set_halign(Gtk.Align.CENTER)
            self.cover_wrapper.set_halign(Gtk.Align.CENTER)
            self.details_col.set_halign(Gtk.Align.CENTER)
            self.playlist_name_label.set_halign(Gtk.Align.CENTER)
            self.playlist_name_label.set_justify(Gtk.Justification.CENTER)
            self.description_label.set_halign(Gtk.Align.CENTER)
            self.description_label.set_justify(Gtk.Justification.CENTER)
            self.meta_label.set_halign(Gtk.Align.CENTER)
            self.stats_label.set_halign(Gtk.Align.CENTER)
            self.actions_box.set_halign(Gtk.Align.CENTER)
        else:
            self.header_info_box.set_orientation(Gtk.Orientation.HORIZONTAL)
            self.header_info_box.set_halign(Gtk.Align.START)
            self.cover_wrapper.set_halign(Gtk.Align.START)
            self.details_col.set_halign(Gtk.Align.FILL)
            self.playlist_name_label.set_halign(Gtk.Align.START)
            self.playlist_name_label.set_justify(Gtk.Justification.LEFT)
            self.description_label.set_halign(Gtk.Align.START)
            self.description_label.set_justify(Gtk.Justification.LEFT)
            self.meta_label.set_halign(Gtk.Align.START)
            self.stats_label.set_halign(Gtk.Align.START)
            self.actions_box.set_halign(Gtk.Align.START)

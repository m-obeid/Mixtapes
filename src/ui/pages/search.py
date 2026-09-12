from gi.repository import Gtk, Adw, GObject, GLib, Pango, Gdk
import threading
from api.client import MusicClient
from ui.utils import AsyncPicture, LikeButton, parse_item_metadata, attach_playing_highlight
from ui.context_menu import show_item_menu
from ui.util_classes import ScrolledWindow
from ui.widgets.scroll_box import HorizontalScrollBox
from ui.widgets.media_card import MediaCardWidget

CARD_SIZE = 150

def _is_video_thumbnail(item):
    thumbs = item.get("thumbnails") or []
    for t in thumbs:
        url = (t.get("url") or "") if isinstance(t, dict) else ""
        if "/vi/" in url or "/vi_webp/" in url:
            return True
    return False

def _detect_kind(item, default_kind=None):
    if not isinstance(item, dict):
        return None

    res_type = item.get("resultType")
    if res_type == "video":
        return "video"
    if res_type == "song":
        return "song"
    if res_type == "artist":
        return "artist"
    if res_type in ("album", "single", "ep"):
        return "album"
    if res_type == "playlist":
        return "playlist"

    if item.get("videoId"):
        vtype = (item.get("videoType") or "").upper()
        if vtype:
            if vtype == "MUSIC_VIDEO_TYPE_ATV":
                return "song"
            if "PODCAST" in vtype or "EPISODE" in vtype:
                return None
            return "video"
        if _is_video_thumbnail(item):
            return "video"
        return "song"

    if item.get("playlistId"):
        return "playlist"
    browse_id = item.get("browseId") or ""
    if browse_id.startswith(("MPRE", "OLAK")):
        return "album"
    if browse_id.startswith(("UC", "FEmusic_library_privately_owned")):
        return "artist"
    if item.get("subscribers") is not None:
        return "artist"
    return default_kind

def _kind_word(kind, item):
    if kind == "album":
        return item.get("type") or "Album"
    return {
        "song": "Song",
        "video": "Video",
        "playlist": "Playlist",
        "artist": "Artist",
    }.get(kind, "")

def _kind_icon(kind):
    return {
        "song": "audio-x-generic-symbolic",
        "video": "video-x-generic-symbolic",
        "album": "media-optical-symbolic",
        "playlist": "view-list-symbolic",
        "artist": "avatar-default-symbolic",
    }.get(kind)

class SearchPage(Adw.Bin):
    def __init__(self, player, open_playlist_callback, *args, **kwargs):
        super().__init__(*args, **kwargs)
        self.player = player
        self.client = MusicClient()
        self.open_playlist_callback = open_playlist_callback

        box = Gtk.Box(orientation=Gtk.Orientation.VERTICAL, spacing=0)

        self.stack = Gtk.Stack()
        self.stack.set_vexpand(True)

        results_page = Gtk.Box(orientation=Gtk.Orientation.VERTICAL, spacing=0)

        self.toggle_group_container = Gtk.Box(orientation=Gtk.Orientation.HORIZONTAL)
        self.toggle_group_container.set_halign(Gtk.Align.CENTER)
        self.toggle_group_container.set_margin_start(12)
        self.toggle_group_container.set_margin_end(12)

        # Scroll the tab strip sideways instead of ellipsizing labels on phones.
        # NATURAL policy makes the viewport hand the strip its full width.
        toggle_viewport = Gtk.Viewport()
        toggle_viewport.set_hscroll_policy(Gtk.ScrollablePolicy.NATURAL)
        toggle_viewport.set_child(self.toggle_group_container)

        self.toggle_scroller = ScrolledWindow()
        self.toggle_scroller.set_policy(Gtk.PolicyType.EXTERNAL, Gtk.PolicyType.NEVER)
        self.toggle_scroller.set_child(toggle_viewport)
        self.toggle_scroller.set_margin_top(16)
        self.toggle_scroller.set_margin_bottom(8)

        results_page.append(self.toggle_scroller)

        self.results_stack = Gtk.Stack()
        self.results_stack.set_transition_type(Gtk.StackTransitionType.CROSSFADE)

        self.results_stack.set_vhomogeneous(False)
        self.results_stack.set_hhomogeneous(False)
        self.results_stack.set_valign(Gtk.Align.START)

        results_scrolled = ScrolledWindow()
        results_scrolled.set_policy(Gtk.PolicyType.NEVER, Gtk.PolicyType.AUTOMATIC)
        results_scrolled.set_vexpand(True)

        results_clamp = Adw.Clamp()
        results_clamp.set_maximum_size(1024)
        results_clamp.set_tightening_threshold(600)
        results_clamp.set_child(self.results_stack)

        results_scrolled.set_child(results_clamp)
        results_page.append(results_scrolled)

        self.stack.add_named(results_page, "results")

        loading_box = Gtk.Box(orientation=Gtk.Orientation.VERTICAL, spacing=12)
        loading_box.set_valign(Gtk.Align.CENTER)
        loading_box.set_halign(Gtk.Align.CENTER)

        self.spinner = Adw.Spinner()
        self.spinner.set_size_request(32, 32)
        loading_box.append(self.spinner)

        loading_label = Gtk.Label(label="Searching...")
        loading_label.add_css_class("dim-label")
        loading_box.append(loading_label)

        self.stack.add_named(loading_box, "loading")

        explore_scrolled = ScrolledWindow()
        explore_scrolled.set_policy(Gtk.PolicyType.NEVER, Gtk.PolicyType.AUTOMATIC)

        explore_clamp = Adw.Clamp()
        explore_clamp.set_maximum_size(1024)
        explore_clamp.set_tightening_threshold(600)

        self.explore_box = Gtk.Box(orientation=Gtk.Orientation.VERTICAL, spacing=24)
        self.explore_box.set_margin_top(24)
        self.explore_box.set_margin_bottom(24)
        self.explore_box.set_margin_start(12)
        self.explore_box.set_margin_end(12)

        explore_clamp.set_child(self.explore_box)
        explore_scrolled.set_child(explore_clamp)

        self.stack.add_named(explore_scrolled, "explore")

        box.append(self.stack)

        self.set_child(box)
        self.search_timer = None
        self._result_toggles = []

        self.stack.set_visible_child_name("explore")

        self._charts_country = self._load_charts_country()

        self._explore_loaded = False
        self._explore_loading = False
        self._explore_retry_count = 0

        GLib.idle_add(self.load_explore_data)

        self.loading_row_spinner = None
        self.player.connect("state-changed", self.on_player_state_changed)

        self.connect("notify::visible", self._on_page_visible_changed)
        self.connect("map", self._on_page_mapped)

    def _build_kind_subtitle(self, item, kind, subtitle_text="", dim=True):
        row = Gtk.Box(orientation=Gtk.Orientation.HORIZONTAL, spacing=4)
        row.set_halign(Gtk.Align.START)

        icon_name = _kind_icon(kind)
        if icon_name:
            icon = Gtk.Image.new_from_icon_name(icon_name)
            icon.set_pixel_size(12)
            icon.set_valign(Gtk.Align.CENTER)
            if dim:
                icon.add_css_class("dim-label")
            row.append(icon)

        parts = []
        kw = _kind_word(kind, item)
        if kw:
            parts.append(kw)
        if subtitle_text:
            parts.append(subtitle_text)

        text = " • ".join(parts) if parts else ""

        label = Gtk.Label(label=text)
        label.set_halign(Gtk.Align.START)
        label.set_ellipsize(Pango.EllipsizeMode.END)
        label.set_lines(1)
        label.set_width_chars(1)
        label.set_xalign(0.0)
        label.add_css_class("caption")
        if dim:
            label.add_css_class("dim-label")
        row.append(label)

        row._label = label
        row._kind = kind
        row._item = item
        return row

    def _on_page_visible_changed(self, widget, pspec):
        if self.get_visible():
            self._check_and_reset_if_empty()

    def _on_page_mapped(self, widget):
        self._check_and_reset_if_empty()

    def _check_and_reset_if_empty(self):
        entry = getattr(self, "search_entry", None)
        text = ""
        if entry and hasattr(entry, "get_text"):
            text = entry.get_text().strip()
        else:
            root = self.get_root()
            if root and hasattr(root, "search_entry"):
                text = root.search_entry.get_text().strip()

        if not text:
            if self.search_timer:
                GObject.source_remove(self.search_timer)
                self.search_timer = None
            self.stack.set_visible_child_name("explore")
            if not self._explore_loaded:
                self.load_explore_data()

    def set_compact_mode(self, compact):
        self._compact = compact
        for toggle, full_name, compact_name in self._result_toggles:
            toggle.set_label(compact_name if compact else full_name)
        if compact:
            self.add_css_class("compact")
            self.explore_box.set_spacing(16)
            child = self.results_stack.get_first_child()
            while child:
                child.set_spacing(16)
                child = child.get_next_sibling()
        else:
            self.remove_css_class("compact")
            self.explore_box.set_spacing(24)
            child = self.results_stack.get_first_child()
            while child:
                child.set_spacing(24)
                child = child.get_next_sibling()

        self._propagate_compact(self.results_stack, compact)
        self._propagate_compact(self.explore_box, compact)

        if not self._explore_loaded:
            self.load_explore_data()

    def _propagate_compact(self, widget, compact):
        """Recursively find AsyncPicture children and set compact mode."""
        if hasattr(widget, 'set_compact') and hasattr(widget, 'target_size'):
            widget.set_compact(compact)
        child = widget.get_first_child() if hasattr(widget, 'get_first_child') else None
        while child:
            self._propagate_compact(child, compact)
            child = child.get_next_sibling()

    def on_key_pressed(self, controller, keyval, keycode, state):
        if hasattr(self, "search_entry") and not self.search_entry.is_focus():
            if keyval < 65000:
                self.search_entry.grab_focus()
                return controller.forward(self.search_entry)
        return False

    def _retry_explore_fetch(self):
        if not self._explore_loaded:
            self.load_explore_data(force=True)
        return False

    def _show_explore_retry_placeholder(self):
        child = self.explore_box.get_first_child()
        while child:
            next_child = child.get_next_sibling()
            self.explore_box.remove(child)
            child = next_child

        box = Gtk.Box(orientation=Gtk.Orientation.VERTICAL, spacing=12)
        box.set_valign(Gtk.Align.CENTER)
        box.set_halign(Gtk.Align.CENTER)
        box.set_vexpand(True)

        icon = Gtk.Image.new_from_icon_name("dialog-warning-symbolic")
        icon.set_pixel_size(48)
        icon.add_css_class("dim-label")
        box.append(icon)

        label = Gtk.Label(label="Couldn't load Explore")
        label.add_css_class("title-3")
        box.append(label)

        retry_btn = Gtk.Button(label="Retry")
        retry_btn.add_css_class("pill")
        retry_btn.add_css_class("suggested-action")
        retry_btn.set_halign(Gtk.Align.CENTER)
        retry_btn.connect(
            "clicked",
            lambda _b: (
                setattr(self, "_explore_retry_count", 0),
                self.load_explore_data(force=True),
            ),
        )
        box.append(retry_btn)

        self.explore_box.append(box)

    def load_explore_data(self, force=False):
        if self._explore_loading:
            return
        if self._explore_loaded and not force:
            return
        if force:
            self._explore_loaded = False
        self._explore_loading = True
        thread = threading.Thread(target=self._fetch_explore)
        thread.daemon = True
        thread.start()

    def refresh_explore(self):
        self.load_explore_data(force=True)

    def _fetch_explore(self):
        from ui.utils import is_online
        if not is_online():
            GObject.idle_add(self.update_explore_ui, None)
            return

        country = getattr(self, "_charts_country", "ZZ")
        results = {"categories": None, "charts": None}

        def _fetch_categories():
            try:
                results["categories"] = self.client.get_mood_categories()
            except Exception as e:
                print(f"Error fetching mood categories: {e}")

        def _fetch_charts():
            try:
                results["charts"] = self.client.get_charts(country)
            except Exception as e:
                print(f"Error fetching charts: {e}")

        cat_t = threading.Thread(target=_fetch_categories, daemon=True)
        ch_t = threading.Thread(target=_fetch_charts, daemon=True)
        cat_t.start()
        ch_t.start()

        try:
            explore = self.client.get_explore()
        except Exception as e:
            print(f"Error fetching explore data: {e}")
            GObject.idle_add(self.update_explore_ui, None)
            return

        cat_t.join()
        ch_t.join()
        if results["categories"]:
            explore["separated_categories"] = results["categories"]
        if results["charts"]:
            explore["_charts"] = results["charts"]
        GObject.idle_add(self.update_explore_ui, explore)

    def update_explore_ui(self, data):
        self._explore_loading = False
        if not data:
            from ui.utils import is_online
            if not is_online():
                child = self.explore_box.get_first_child()
                while child:
                    next_child = child.get_next_sibling()
                    self.explore_box.remove(child)
                    child = next_child
                offline_box = Gtk.Box(orientation=Gtk.Orientation.VERTICAL, spacing=12)
                offline_box.set_valign(Gtk.Align.CENTER)
                offline_box.set_halign(Gtk.Align.CENTER)
                offline_box.set_vexpand(True)
                offline_icon = Gtk.Image.new_from_icon_name("network-offline-symbolic")
                offline_icon.set_pixel_size(48)
                offline_icon.add_css_class("dim-label")
                offline_box.append(offline_icon)
                offline_label = Gtk.Label(label="You're offline")
                offline_label.add_css_class("title-3")
                offline_box.append(offline_label)
                offline_sub = Gtk.Label(label="Explore requires an internet connection.\nYour downloaded songs are still available.")
                offline_sub.add_css_class("dim-label")
                offline_sub.set_justify(Gtk.Justification.CENTER)
                offline_box.append(offline_sub)
                self.explore_box.append(offline_box)
                return
            if self._explore_retry_count < 3:
                self._explore_retry_count += 1
                delay = 1500 * self._explore_retry_count
                GLib.timeout_add(delay, self._retry_explore_fetch)
            else:
                self._show_explore_retry_placeholder()
            return

        child = self.explore_box.get_first_child()
        while child:
            next_child = child.get_next_sibling()
            self.explore_box.remove(child)
            child = next_child

        self._explore_loaded = True
        self._explore_retry_count = 0

        if "separated_categories" in data:
            cats = data["separated_categories"]
            moods = cats.get("Moods & moments", [])
            genres = cats.get("Genres", [])

            if moods:
                self.add_horizontal_section(self.explore_box, "Moods & Moments", moods, is_category=True)

            if genres:
                for g in genres:
                    g["is_genre"] = True
                self.add_horizontal_section(self.explore_box, "Genres", genres, is_category=True)
        elif "moods_and_genres" in data and isinstance(data["moods_and_genres"], list):
            self.add_horizontal_section(self.explore_box, "Moods & Genres", data["moods_and_genres"], is_category=True)

        if "new_releases" in data and isinstance(data["new_releases"], list):
            self.add_section(self.explore_box, "New Albums & Singles", data["new_releases"][:10])

        if "new_videos" in data and isinstance(data["new_videos"], list):
            self.add_section(self.explore_box, "New Music Videos", data["new_videos"][:5])

        if "trending" in data and data["trending"] and "items" in data["trending"]:
            self.add_section(self.explore_box, "Trending", data["trending"]["items"][:5])

        charts = data.get("_charts")
        if charts:
            self._add_charts_sections(charts)

    def add_horizontal_section(self, parent_box, title, items, is_category=False):
        if not items:
            return

        section_box = Gtk.Box(orientation=Gtk.Orientation.VERTICAL, spacing=8)
        parent_box.append(section_box)

        label = Gtk.Label(label=title)
        label.add_css_class("heading")
        label.set_halign(Gtk.Align.START)
        section_box.append(label)

        scroll_box = HorizontalScrollBox()
        h_box = Gtk.Box(orientation=Gtk.Orientation.HORIZONTAL, spacing=12)
        # Outside the scrolled window: a margin within it is empty space for
        # the overlay scrollbar to draw a line in.
        scroll_box.set_margin_bottom(12)

        display_items = items
        if is_category:
            display_items = items[:20]

        for item in display_items:
            btn_label = Gtk.Label(label=item.get("title", "Unknown"))
            btn_label.set_hexpand(False)

            button = Gtk.Button()
            button.set_child(btn_label)
            button.item_data = item
            button.connect("clicked", self.on_grid_button_clicked)
            button.add_css_class("pill")
            h_box.append(button)

        if is_category and len(items) > 20:
            view_all_btn = Gtk.Button(label="View All")
            view_all_btn.add_css_class("pill")
            view_all_btn.add_css_class("flat")
            view_all_btn.connect("clicked", lambda b, i=items, t=title: self.on_view_all_clicked(i, t))
            h_box.append(view_all_btn)

        scroll_box.set_content(h_box)
        section_box.append(scroll_box)

    def _add_charts_sections(self, charts):
        charts_header = Gtk.Box(orientation=Gtk.Orientation.HORIZONTAL, spacing=8)

        charts_label = Gtk.Label(label="Charts")
        charts_label.add_css_class("title-3")
        charts_label.set_halign(Gtk.Align.START)
        charts_header.append(charts_label)

        spacer = Gtk.Box()
        spacer.set_hexpand(True)
        charts_header.append(spacer)

        countries = charts.get("countries", {})
        options = countries.get("options", [])

        if options:
            _COUNTRY_NAMES = {
                "ZZ": "Global", "AR": "Argentina", "AU": "Australia", "AT": "Austria",
                "BE": "Belgium", "BO": "Bolivia", "BR": "Brazil", "CA": "Canada",
                "CL": "Chile", "CO": "Colombia", "CR": "Costa Rica", "CZ": "Czechia",
                "DK": "Denmark", "DO": "Dominican Republic", "EC": "Ecuador",
                "EG": "Egypt", "SV": "El Salvador", "EE": "Estonia", "FI": "Finland",
                "FR": "France", "DE": "Germany", "GT": "Guatemala", "HN": "Honduras",
                "HU": "Hungary", "IS": "Iceland", "IN": "India", "ID": "Indonesia",
                "IE": "Ireland", "IL": "Israel", "IT": "Italy", "JP": "Japan",
                "KE": "Kenya", "LU": "Luxembourg", "MX": "Mexico", "NL": "Netherlands",
                "NZ": "New Zealand", "NI": "Nicaragua", "NG": "Nigeria", "NO": "Norway",
                "PA": "Panama", "PY": "Paraguay", "PE": "Peru", "PH": "Philippines",
                "PL": "Poland", "PT": "Portugal", "RO": "Romania", "RU": "Russia",
                "SA": "Saudi Arabia", "RS": "Serbia", "ZA": "South Africa",
                "KR": "South Korea", "ES": "Spain", "SE": "Sweden", "CH": "Switzerland",
                "TZ": "Tanzania", "TR": "Turkey", "UG": "Uganda", "UA": "Ukraine",
                "AE": "UAE", "GB": "United Kingdom", "US": "United States",
                "UY": "Uruguay", "VE": "Venezuela", "VN": "Vietnam", "ZW": "Zimbabwe",
            }
            display_names = [_COUNTRY_NAMES.get(c, c) for c in options]
            paired = list(zip(display_names, options))
            paired.sort(key=lambda x: ("" if x[1] == "ZZ" else x[0]))
            display_names = [p[0] for p in paired]
            sorted_codes = [p[1] for p in paired]

            country_items = Gtk.StringList.new(display_names)
            country_dropdown = Gtk.DropDown(model=country_items)
            country_dropdown.add_css_class("flat")

            current = getattr(self, '_charts_country', 'ZZ')
            for i, code in enumerate(sorted_codes):
                if code == current:
                    country_dropdown.set_selected(i)
                    break

            country_dropdown.connect("notify::selected", self._on_charts_country_changed, sorted_codes)
            charts_header.append(country_dropdown)

        self.explore_box.append(charts_header)

        videos = charts.get("videos", [])
        if videos:
            self._add_chart_playlists("Trending", videos)

        genres = charts.get("genres", [])
        if genres:
            self._add_chart_playlists("Genre Charts", genres)

        artists = charts.get("artists", [])
        if artists:
            self._add_chart_artists("Top Artists", artists)

    def _make_chart_card(self, item, on_clicked=None):
        subtitle = item.get("subtitle") or item.get("description") or ""
        card = MediaCardWidget(
            item,
            player=self.player,
            title_lines=2,
            subtitle_text=subtitle,
            on_clicked=lambda btn, it: on_clicked(btn, it) if on_clicked else None
        )
        return card

    def _add_chart_playlists(self, title, items):
        section_box = Gtk.Box(orientation=Gtk.Orientation.VERTICAL, spacing=8)
        self.explore_box.append(section_box)

        label = Gtk.Label(label=title)
        label.add_css_class("heading")
        label.set_halign(Gtk.Align.START)
        section_box.append(label)

        scroll_box = HorizontalScrollBox()
        h_box = Gtk.Box(orientation=Gtk.Orientation.HORIZONTAL, spacing=12)
        scroll_box.set_margin_bottom(8)

        for item in items:
            card = self._make_chart_card(
                item,
                on_clicked=lambda btn, it: self._handle_chart_click(it)
            )
            h_box.append(card)

        scroll_box.set_content(h_box)
        section_box.append(scroll_box)

    def _handle_chart_click(self, item):
        try:
            self._on_chart_playlist_clicked(None, 1, 0, 0, item)
        except TypeError:
            self._on_chart_playlist_clicked(item)

    def _add_chart_artists(self, title, artists):
        section_box = Gtk.Box(orientation=Gtk.Orientation.VERTICAL, spacing=8)
        self.explore_box.append(section_box)

        label = Gtk.Label(label=title)
        label.add_css_class("heading")
        label.set_halign(Gtk.Align.START)
        section_box.append(label)

        list_box = Gtk.ListBox()
        list_box.add_css_class("boxed-list")
        list_box.add_css_class("songs-list")
        list_box.set_selection_mode(Gtk.SelectionMode.NONE)
        list_box.connect("row-activated", self._on_chart_artist_activated)

        for artist in artists[:20]:
            row = Gtk.ListBoxRow()
            box = Gtk.Box(orientation=Gtk.Orientation.HORIZONTAL, spacing=12)
            box.add_css_class("song-row")
            row.set_child(box)

            rank = artist.get("rank", "")
            rank_label = Gtk.Label(label=str(rank))
            rank_label.set_width_chars(3)
            rank_label.add_css_class("heading")
            rank_label.set_valign(Gtk.Align.CENTER)
            box.append(rank_label)

            trend = artist.get("trend", "neutral")
            if trend == "up":
                trend_icon = Gtk.Image.new_from_icon_name("go-up-symbolic")
                trend_icon.add_css_class("success")
            elif trend == "down":
                trend_icon = Gtk.Image.new_from_icon_name("go-down-symbolic")
                trend_icon.add_css_class("error")
            else:
                trend_icon = Gtk.Image.new_from_icon_name("go-next-symbolic")
                trend_icon.add_css_class("dim-label")
            trend_icon.set_valign(Gtk.Align.CENTER)
            trend_icon.set_pixel_size(12)
            box.append(trend_icon)

            thumb_url = artist.get("thumbnails", [{}])[-1].get("url") if artist.get("thumbnails") else None
            img = AsyncPicture(
                url=thumb_url, target_size=56, crop_to_square=True, player=self.player,
            )
            img.add_css_class("song-img")
            root = self.get_root()
            img.set_compact(getattr(root, '_is_compact', False) if root else False)
            if not thumb_url:
                img.set_from_icon_name("avatar-default-symbolic")
            box.append(img)

            vbox = Gtk.Box(orientation=Gtk.Orientation.VERTICAL, spacing=2)
            vbox.set_valign(Gtk.Align.CENTER)
            vbox.set_hexpand(True)

            name_label = Gtk.Label(label=artist.get("title", "Unknown"))
            name_label.set_halign(Gtk.Align.START)
            name_label.set_ellipsize(Pango.EllipsizeMode.END)
            name_label.set_lines(1)
            name_label.set_width_chars(1)
            name_label.set_xalign(0.0)
            vbox.append(name_label)

            subs = artist.get("subscribers", "")
            if subs:
                sub_label = Gtk.Label(label=subs)
                sub_label.set_halign(Gtk.Align.START)
                sub_label.set_ellipsize(Pango.EllipsizeMode.END)
                sub_label.set_lines(1)
                sub_label.set_width_chars(1)
                sub_label.set_xalign(0.0)
                sub_label.add_css_class("dim-label")
                sub_label.add_css_class("caption")
                vbox.append(sub_label)

            box.append(vbox)

            row.artist_data = artist
            row.set_activatable(True)
            list_box.append(row)

        section_box.append(list_box)

    def _on_charts_country_changed(self, dropdown, pspec, options):
        idx = dropdown.get_selected()
        if 0 <= idx < len(options):
            self._charts_country = options[idx]
            self._save_charts_country(options[idx])
            self.load_explore_data()

    @staticmethod
    def _get_prefs_path():
        import os
        return os.path.join(GLib.get_user_data_dir(), "muse", "prefs.json")

    def _save_charts_country(self, code):
        import json, os
        path = self._get_prefs_path()
        prefs = {}
        try:
            if os.path.exists(path):
                with open(path) as f:
                    prefs = json.load(f)
        except Exception:
            pass
        prefs["charts_country"] = code
        os.makedirs(os.path.dirname(path), exist_ok=True)
        with open(path, "w") as f:
            json.dump(prefs, f)

    def _load_charts_country(self):
        import json, os
        path = self._get_prefs_path()
        try:
            if os.path.exists(path):
                with open(path) as f:
                    prefs = json.load(f)
                return prefs.get("charts_country", "ZZ")
        except Exception:
            pass
        return "ZZ"

    def _on_chart_playlist_clicked(self, *args, **kwargs):
        item = args[-1] if args else kwargs.get("item")
        if not item:
            return

        pid = item.get("playlistId")
        if pid and hasattr(self, "open_playlist_callback"):
            initial_data = {
                "title": item.get("title", ""),
                "thumb": (item.get("thumbnails", [{}])[-1] or {}).get("url"),
            }
            self.open_playlist_callback(pid, initial_data)

    def _on_chart_artist_activated(self, listbox, row):
        if hasattr(row, "artist_data"):
            browse_id = row.artist_data.get("browseId")
            name = row.artist_data.get("title")
            if browse_id:
                root = self.get_root()
                if root and hasattr(root, "open_artist"):
                    root.open_artist(browse_id, name)

    def on_view_all_clicked(self, items, title):
        root = self.get_root()
        if hasattr(root, "open_all_moods"):
            root.open_all_moods(items, title)

    def on_grid_button_clicked(self, button):
        if hasattr(button, "item_data"):
            data = button.item_data
            if "params" in data:
                root = self.get_root()
                if hasattr(root, "open_category"):
                    nav_title = data.get("title", "Category")
                    root.open_category(data["params"], nav_title)

    def add_section(self, parent_box, title, items):
        if not items:
            return

        section_box = Gtk.Box(orientation=Gtk.Orientation.VERTICAL, spacing=8)
        parent_box.append(section_box)

        label = Gtk.Label(label=title)
        label.add_css_class("heading")
        label.set_halign(Gtk.Align.START)
        section_box.append(label)

        list_box = Gtk.ListBox()
        list_box.add_css_class("boxed-list")
        list_box.add_css_class("songs-list")
        list_box.set_selection_mode(Gtk.SelectionMode.NONE)
        list_box.connect("row-activated", self.on_row_activated)

        for item in items:
            row = Gtk.ListBoxRow()
            box = Gtk.Box(orientation=Gtk.Orientation.HORIZONTAL, spacing=12)
            box.add_css_class("song-row")
            row.set_child(box)

            if item.get("videoId"):
                attach_playing_highlight(box, self.player, item["videoId"])

            subtitle = ""

            if item.get("resultType") == "artist":
                count = item.get("subscribers", "")
                if count and not count[-1].isdigit() and "listeners" not in count and "subscribers" not in count:
                    subtitle = f"{count} monthly listeners"
                elif count:
                    subtitle = count
            elif item.get("artists"):
                artists = item["artists"]
                if artists:
                    subtitle = ", ".join([a.get("name", "") for a in artists if isinstance(a, dict)])
                    if not subtitle:
                        subtitle = ", ".join([str(a) for a in artists if a])
                if not subtitle and "author" in item:
                    author = item.get("author")
                    if isinstance(author, list):
                        subtitle = ", ".join([a.get("name", str(a)) for a in author])
                    elif isinstance(author, dict):
                        subtitle = author.get("name", str(author))
                    elif author:
                        subtitle = str(author)

                album_data = item.get("album")
                if album_data:
                    album_name = album_data.get("name", "") if isinstance(album_data, dict) else str(album_data)
                    if album_name and subtitle:
                        subtitle += f" • {album_name}"
                    elif album_name:
                        subtitle = album_name
            elif "subscribers" in item:
                subtitle = item.get("subscribers", "")
            elif "itemCount" in item and item["itemCount"]:
                count = str(item["itemCount"])
                subtitle = count if "songs" in count else f"{count} views"

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
            img.set_compact(getattr(root, '_is_compact', False) if root else False)
            if not thumb_url:
                img.set_from_icon_name("media-optical-symbolic")

            box.append(img)

            vbox = Gtk.Box(orientation=Gtk.Orientation.VERTICAL, spacing=2)
            vbox.set_valign(Gtk.Align.CENTER)
            vbox.set_hexpand(True)

            title_label = Gtk.Label(label=item.get("title", "Unknown"))
            title_label.set_halign(Gtk.Align.START)
            title_label.set_ellipsize(Pango.EllipsizeMode.END)
            title_label.set_lines(1)
            title_label.set_width_chars(1)
            title_label.set_xalign(0.0)

            kind = _detect_kind(item)
            sub_row = self._build_kind_subtitle(item, kind, subtitle_text=subtitle, dim=True)
            subtitle_label = sub_row._label

            if not subtitle:
                browse_id = item.get("browseId") or ""
                audio_pid = item.get("audioPlaylistId") or ""
                album_id = browse_id if browse_id.startswith("MPRE") else None
                if not album_id and audio_pid.startswith("OLAK"):
                    album_id = audio_pid
                if album_id:
                    item_type = item.get("type", "")
                    def _fetch_artist_info(aid, itype, lbl, k, itm, client):
                        try:
                            if aid.startswith("OLAK"):
                                bid = client.get_album_browse_id(aid)
                                if bid:
                                    aid = bid
                            data = client.get_album(aid)
                            if data:
                                artists = data.get("artists", [])
                                text = ", ".join(a.get("name", "") for a in artists if isinstance(a, dict))
                                if itype:
                                    text = f"{text} • {itype}" if text else itype
                                if text:
                                    kw = _kind_word(k, itm)
                                    final_txt = f"{kw} • {text}" if kw else text
                                    GLib.idle_add(lbl.set_label, final_txt)
                        except Exception:
                            pass
                    threading.Thread(
                        target=_fetch_artist_info,
                        args=(album_id, item_type, subtitle_label, kind, item, self.client),
                        daemon=True,
                    ).start()

            vid_for_album = item.get("videoId")
            has_album = item.get("album")
            if vid_for_album and not has_album and item.get("resultType") in ("song", "video", None):
                def _fetch_album(vid, itm, lbl, k, client):
                    try:
                        wp = client.get_watch_playlist(video_id=vid, limit=1)
                        wp_tracks = wp.get("tracks", [])
                        if wp_tracks and wp_tracks[0].get("album"):
                            album = wp_tracks[0]["album"]
                            album_name = album.get("name", "") if isinstance(album, dict) else str(album)
                            if album_name:
                                itm["album"] = album
                                def _update_label():
                                    cur = lbl.get_label()
                                    if cur:
                                        lbl.set_label(f"{cur} • {album_name}")
                                    else:
                                        kw = _kind_word(k, itm)
                                        lbl.set_label(f"{kw} • {album_name}" if kw else album_name)
                                GLib.idle_add(_update_label)
                    except Exception:
                        pass
                threading.Thread(
                    target=_fetch_album,
                    args=(vid_for_album, item, subtitle_label, kind, self.client),
                    daemon=True,
                ).start()

            title_box = Gtk.Box(orientation=Gtk.Orientation.HORIZONTAL, spacing=6)
            title_box.append(title_label)

            meta = parse_item_metadata(item)
            if meta["is_explicit"]:
                explicit_badge = Gtk.Label(label="E")
                explicit_badge.add_css_class("explicit-badge")
                explicit_badge.set_valign(Gtk.Align.CENTER)
                title_box.append(explicit_badge)

            vbox.append(title_box)
            vbox.append(sub_row)
            box.append(vbox)

            if item.get("videoId"):
                like_btn = LikeButton(
                    self.client, item["videoId"], item.get("likeStatus", "INDIFFERENT")
                )
                like_btn.set_valign(Gtk.Align.CENTER)
                box.append(like_btn)

            row.item_data = item
            row.set_activatable(True)

            gesture = Gtk.GestureClick()
            gesture.set_button(3)
            gesture.connect("released", self.on_row_right_click, row)
            row.add_controller(gesture)

            lp = Gtk.GestureLongPress()
            lp.connect(
                "pressed", lambda g, x, y, r=row: self.on_row_right_click(g, 1, x, y, r)
            )
            row.add_controller(lp)

            list_box.append(row)

        section_box.append(list_box)

    def on_search_changed(self, entry):
        text = entry.get_text() if hasattr(entry, "get_text") else str(entry)
        self.on_external_search(text)

    def on_external_search(self, text):
        if self.search_timer:
            GObject.source_remove(self.search_timer)
            self.search_timer = None

        text = (text or "").strip()

        if len(text) == 0:
            self.stack.set_visible_child_name("explore")
            if not self._explore_loaded:
                self.load_explore_data()
            return

        if len(text) > 2:
            self.search_timer = GObject.timeout_add(600, self.perform_search, text)

    def perform_search(self, query):
        self.search_timer = None
        self.stack.set_visible_child_name("loading")
        thread = threading.Thread(target=self._search_thread, args=(query,))
        thread.daemon = True
        thread.start()
        return False

    def _search_thread(self, query):
        from ui.utils import is_online
        import concurrent.futures

        if is_online():
            try:
                results = self.client.search(query)
            except Exception:
                results = []

            def fetch_filter(f):
                try:
                    return self.client.search(query, filter=f)
                except Exception:
                    return []

            with concurrent.futures.ThreadPoolExecutor(max_workers=4) as executor:
                f_songs = executor.submit(fetch_filter, "songs")
                f_artists = executor.submit(fetch_filter, "artists")
                f_playlists = executor.submit(fetch_filter, "playlists")
                f_albums = executor.submit(fetch_filter, "albums")

                more_songs = f_songs.result()
                more_artists = f_artists.result()
                more_playlists = f_playlists.result()
                more_albums = f_albums.result()

            seen = set()
            merged_results = []

            def add_items(items):
                if not items:
                    return
                for item in items:
                    vid = item.get("videoId") or item.get("browseId") or item.get("playlistId")
                    if vid:
                        if vid in seen:
                            continue
                        seen.add(vid)
                    merged_results.append(item)

            add_items(results)
            add_items(more_songs)
            add_items(more_artists)
            add_items(more_playlists)
            add_items(more_albums)

            GObject.idle_add(self.update_results, merged_results)
        else:
            results = self._search_local(query)
            GObject.idle_add(self.update_results, results)

    def _search_local(self, query):
        from player.downloads import get_download_db
        db = get_download_db()
        all_downloads = db.get_all_downloads()
        query_lower = query.lower()
        results = []
        for d in all_downloads:
            title = (d.get("title") or "").lower()
            artist = (d.get("artist") or "").lower()
            album = (d.get("album") or "").lower()
            if query_lower in title or query_lower in artist or query_lower in album:
                results.append({
                    "resultType": "song",
                    "title": d.get("title", ""),
                    "videoId": d.get("video_id", ""),
                    "artists": [{"name": d.get("artist", ""), "id": None}],
                    "album": {"name": d.get("album", "")},
                    "thumbnails": [{"url": d.get("thumbnail_url", "")}] if d.get("thumbnail_url") else [],
                    "duration_seconds": d.get("duration_seconds", 0),
                    "isExplicit": False,
                })
        return results

    def _on_toggle_changed(self, toggle_group, param):
        selected_name = toggle_group.get_active_name()
        if selected_name and self.results_stack.get_child_by_name(selected_name):
            self.results_stack.set_visible_child_name(selected_name)

    def update_results(self, results):
        self.stack.set_visible_child_name("results")

        while child := self.results_stack.get_first_child():
            self.results_stack.remove(child)

        while child := self.toggle_group_container.get_first_child():
            self.toggle_group_container.remove(child)

        self._result_toggles = []

        if not results:
            return

        self.results_toggle_group = Adw.ToggleGroup()
        self.results_toggle_group.add_css_class("round")
        self.toggle_group_container.append(self.results_toggle_group)

        top_result = None
        artists = []
        songs = []
        albums = []
        videos = []
        playlists = []
        others = []

        for r in results:
            if "title" not in r:
                if "artist" in r:
                    r["title"] = r["artist"]
                elif "artists" in r and r["artists"]:
                    r["title"] = r["artists"][0]["name"]

            if "browseId" not in r and "artists" in r and r["artists"]:
                r["browseId"] = r["artists"][0]["id"]

            r_type = r.get("resultType")
            category = r.get("category")

            if category == "Top result" and not top_result:
                top_result = r
                continue

            if r_type == "artist":
                artists.append(r)
            elif r_type == "song":
                songs.append(r)
            elif r_type == "album":
                albums.append(r)
                others.append(r)
            elif r_type == "video":
                videos.append(r)
                others.append(r)
            elif r_type == "playlist" or category == "Community playlists":
                playlists.append(r)
            else:
                others.append(r)

        first_page_id = None

        def create_tab(name, page_id, compact_name=None):
            nonlocal first_page_id
            compact = getattr(self, "_compact", False)
            page_box = Gtk.Box(
                orientation=Gtk.Orientation.VERTICAL,
                spacing=16 if compact else 24
            )
            page_box.set_margin_top(16)
            page_box.set_margin_bottom(24)
            page_box.set_margin_start(12)
            page_box.set_margin_end(12)

            self.results_stack.add_named(page_box, page_id)

            compact_name = compact_name or name
            toggle = Adw.Toggle(label=compact_name if compact else name, name=page_id)
            self._result_toggles.append((toggle, name, compact_name))
            self.results_toggle_group.add(toggle)

            if first_page_id is None:
                first_page_id = page_id

            return page_box

        main_tab = create_tab("Main", "main")
        if top_result:
            self.add_section(main_tab, "Top Result", [top_result])

        relevant = [r for r in results if r != top_result]
        if relevant:
            self.add_section(main_tab, "Relevant Results", relevant)

        if songs:
            songs_tab = create_tab("Songs", "songs")
            self.add_section(songs_tab, "Songs", songs)

        if artists:
            artists_tab = create_tab("Artists", "artists")
            self.add_section(artists_tab, "Artists", artists)

        if playlists:
            playlists_tab = create_tab("Community Playlists", "playlists", "Playlists")
            self.add_section(playlists_tab, "Playlists", playlists)

        if others:
            others_tab = create_tab("Other results", "others", "Other")
            if albums:
                self.add_section(others_tab, "Albums", albums)
            if videos:
                self.add_section(others_tab, "Videos", videos)
            rem = [o for o in others if o not in albums and o not in videos]
            if rem:
                self.add_section(others_tab, "More results", rem)

        self.results_toggle_group.connect("notify::active-name", self._on_toggle_changed)

        if first_page_id:
            self.results_toggle_group.set_active_name(first_page_id)
            self.results_stack.set_visible_child_name(first_page_id)

        if getattr(self, "_compact", False):
            self._propagate_compact(self.results_stack, True)

    def on_player_state_changed(self, player, state):
        if state == "playing" or state == "rec-started":
            if hasattr(self, "loading_row_spinner") and self.loading_row_spinner:
                try:
                    parent = self.loading_row_spinner.get_parent()
                    if parent:
                        parent.remove(self.loading_row_spinner)
                except Exception:
                    pass
                self.loading_row_spinner = None

    def on_row_activated(self, listbox, row):
        if hasattr(row, "playlist_data"):
            data = row.playlist_data
            if data["browseId"].startswith("VL"):
                data["browseId"] = data["browseId"][2:]

            initial_data = {
                "title": data.get("title"),
                "thumb": data["thumbnails"][-1]["url"] if data.get("thumbnails") else None,
                "author": data.get("runs", [{}])[0].get("text") if "runs" in data else None,
            }

            self.open_playlist_callback(data["browseId"], initial_data)

        elif hasattr(row, "item_data"):
            data = row.item_data
            title = data.get("title", "Unknown")
            res_type = data.get("resultType")

            def open_pid(pid):
                initial_data = {
                    "title": title,
                    "thumb": data["thumbnails"][-1]["url"] if data.get("thumbnails") else None,
                    "author": ", ".join([a.get("name", "") for a in data.get("artists", [])]) if "artists" in data else data.get("count", ""),
                }
                self.open_playlist_callback(pid, initial_data)

            if res_type in ["song", "video"]:
                if "videoId" in data:
                    queue_tracks = []
                    start_index = 0

                    child = listbox.get_first_child()
                    idx = 0
                    while child:
                        if hasattr(child, "item_data"):
                            s_data = child.item_data
                            if "videoId" in s_data:
                                s_title = s_data.get("title", "Unknown")

                                s_thumb = ""
                                if s_data.get("thumbnails"):
                                    s_thumb = s_data["thumbnails"][-1]["url"]

                                s_artist = ""
                                if "artists" in s_data:
                                    s_artist = ", ".join([a.get("name", "") for a in s_data["artists"]])
                                elif "artist" in s_data:
                                    s_artist = s_data["artist"]

                                qt = {
                                    "videoId": s_data["videoId"],
                                    "title": s_title,
                                    "artist": s_artist,
                                    "thumb": s_thumb,
                                }
                                if s_data.get("album"):
                                    qt["album"] = s_data["album"]
                                queue_tracks.append(qt)

                                if s_data.get("videoId") == data.get("videoId"):
                                    start_index = idx
                                idx += 1

                        child = child.get_next_sibling()

                    if queue_tracks:
                        self.player.set_queue(queue_tracks, start_index)
                    else:
                        thumb_url = data.get("thumbnails", [])[-1]["url"] if data.get("thumbnails") else None
                        artist_name = ", ".join([a.get("name", "") for a in data.get("artists", [])]) if "artists" in data else data.get("artist", "")
                        self.player.load_video(data["videoId"], title, artist_name, thumb_url)
                    return

            elif res_type in ["album", "single", "ep"]:
                if "browseId" in data and data["browseId"].startswith("MPRE"):
                    open_pid(data["browseId"])
                    return
                elif "audioPlaylistId" in data:
                    open_pid(data["audioPlaylistId"])
                    return
                elif "browseId" in data:
                    open_pid(data["browseId"])
                    return

            elif res_type == "playlist":
                if "playlistId" in data:
                    open_pid(data["playlistId"])
                    return
                elif "browseId" in data:
                    open_pid(data["browseId"])
                    return

            if "videoId" in data and res_type not in ["album", "single", "ep", "playlist", "artist"]:
                thumb_url = ""
                thumbnails = data.get("thumbnails", [])
                if thumbnails:
                    thumb_url = thumbnails[-1]["url"]

                artists_list = data.get("artists", [])
                if isinstance(artists_list, list):
                    artist_name = ", ".join([a.get("name", "") for a in artists_list])
                else:
                    artist_name = data.get("artist", "")

                self.player.load_video(data["videoId"], title, artist_name, thumb_url)

            elif "audioPlaylistId" in data:
                open_pid(data["audioPlaylistId"])
            elif "playlistId" in data:
                open_pid(data["playlistId"])
            elif "browseId" in data:
                if res_type in ["playlist", "album"] or data["browseId"].startswith(("VL", "PL", "RD", "OL", "MPRE")):
                    open_pid(data["browseId"])
                else:
                    root = self.get_root()
                    if root and hasattr(root, "open_artist"):
                        root.open_artist(data["browseId"], title)

    def on_row_right_click(self, gesture, n_press, x, y, row):
        if not hasattr(row, "item_data"):
            return
        show_item_menu(
            row,
            x,
            y,
            row.item_data,
            player=self.player,
            client=self.client,
            prefix="row",
        )

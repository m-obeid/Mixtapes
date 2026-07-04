import weakref
from gi.repository import Gtk, Adw, GObject, GLib, Pango, Gio, Gdk
import threading
from api.client import MusicClient
from ui.utils import AsyncPicture, LikeButton, parse_item_metadata, attach_playing_highlight
from ui.util_classes import ScrolledWindow


class SearchPage(Adw.Bin):
    def __init__(self, player, open_playlist_callback, *args, **kwargs):
        super().__init__(*args, **kwargs)
        self.player = player
        self.client = MusicClient()
        self.open_playlist_callback = open_playlist_callback

        # Layout
        box = Gtk.Box(orientation=Gtk.Orientation.VERTICAL, spacing=0)

        # Search Bar removed (Global in MainWindow)

        # Content Stack

        # Content Stack
        self.stack = Gtk.Stack()
        self.stack.set_vexpand(True)

        # 1. Results View
        results_scrolled = ScrolledWindow()
        results_scrolled.set_policy(Gtk.PolicyType.NEVER, Gtk.PolicyType.AUTOMATIC)

        results_clamp = Adw.Clamp()
        results_clamp.set_maximum_size(1024)
        results_clamp.set_tightening_threshold(600)

        self.results_box = Gtk.Box(orientation=Gtk.Orientation.VERTICAL, spacing=24)
        self.results_box.set_margin_top(24)
        self.results_box.set_margin_bottom(24)
        self.results_box.set_margin_start(12)
        self.results_box.set_margin_end(12)

        results_clamp.set_child(self.results_box)
        results_scrolled.set_child(results_clamp)

        self.stack.add_named(results_scrolled, "results")

        # 2. Loading View
        loading_box = Gtk.Box(orientation=Gtk.Orientation.VERTICAL, spacing=12)
        loading_box.set_valign(Gtk.Align.CENTER)
        loading_box.set_halign(Gtk.Align.CENTER)

        self.spinner = Adw.Spinner()
        self.spinner.set_size_request(32, 32)
        # self.spinner.set_spinning(True) # Adw.Spinner spins by default
        loading_box.append(self.spinner)

        loading_label = Gtk.Label(label="Searching...")
        loading_label.add_css_class("dim-label")
        loading_box.append(loading_label)

        self.stack.add_named(loading_box, "loading")

        # 3. Explore View (Default)
        explore_scrolled = ScrolledWindow()
        explore_scrolled.set_policy(Gtk.PolicyType.NEVER, Gtk.PolicyType.AUTOMATIC)

        # Clamp for Explore
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

        # Show explore initially
        self.stack.set_visible_child_name("explore")

        # Load saved chart country preference
        self._charts_country = self._load_charts_country()

        # Explore load state
        self._explore_loaded = False
        self._explore_loading = False
        self._explore_retry_count = 0

        # Kick off the Explore fetch on an idle tick so it doesn't
        # compete with the rest of the UI's initial layout. The fetch
        # itself runs on a worker thread, so this only buys us the
        # widget-construction breathing room — but means the user lands
        # on a populated Explore tab the first time they click into it
        # rather than an empty screen.
        GLib.idle_add(self.load_explore_data)

        # Player listeners
        self.loading_row_spinner = None
        self.player.connect("state-changed", self.on_player_state_changed)

    def set_compact_mode(self, compact):
        self._compact = compact
        if compact:
            self.add_css_class("compact")
            self.results_box.set_spacing(16)
            self.explore_box.set_spacing(16)
        else:
            self.remove_css_class("compact")
            self.results_box.set_spacing(24)
            self.explore_box.set_spacing(24)

        # Update song row images on both the search-results pane and the
        # explore pane. Previously only results_box was walked, so the
        # trending/top-song rows on Explore kept their 56px thumbnails
        # in compact mode.
        self._propagate_compact(self.results_box, compact)
        self._propagate_compact(self.explore_box, compact)

        # Only fetch if explore hasn't loaded yet; otherwise just keep
        # the existing content (spacing was updated above).
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
        # If user types and entry is not focused, focus it
        # We ignore modifier keys to avoid grabbing shortcuts
        # We permit normal typing
        if not self.search_entry.is_focus():
            # Basic check to see if it's a printable character or backspace
            # We want to allow the event to propagate so the entry handles it
            # Focus the search entry to allow typing. Forwarding the event handles the first character.

            if keyval < 65000:  # Rough check for non-special keys?
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
        # Three independent HTTP calls — fire in parallel. ``explore`` is
        # required (the page can't render without it); ``categories`` and
        # ``charts`` are optional enrichment that get merged in. We run
        # the optional ones on side threads while the main thread waits
        # for the explore call, then join.
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

        # Wait for the side fetches before rendering so the page lays
        # out once with everything in place instead of re-flowing.
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
            # Check if offline
            from ui.utils import is_online
            online = is_online()
            if not online:
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
            # Online but fetch failed/empty: retry a few times with backoff,
            # then fall back to a manual retry placeholder.
            if self._explore_retry_count < 3:
                self._explore_retry_count += 1
                delay = 1500 * self._explore_retry_count
                GLib.timeout_add(delay, self._retry_explore_fetch)
            else:
                self._show_explore_retry_placeholder()
            return

        # Clear existing explore content
        child = self.explore_box.get_first_child()
        while child:
            next_child = child.get_next_sibling()
            self.explore_box.remove(child)
            child = next_child

        self._explore_loaded = True
        self._explore_retry_count = 0

        # Separated categories (Moods and Genres)
        if "separated_categories" in data:
            cats = data["separated_categories"]
            moods = cats.get("Moods & moments", [])
            genres = cats.get("Genres", [])
            
            if moods:
                self.add_horizontal_section(
                    self.explore_box, "Moods & Moments", moods, is_category=True
                )
            
            if genres:
                for g in genres:
                    g["is_genre"] = True
                self.add_horizontal_section(
                    self.explore_box, "Genres", genres, is_category=True
                )
        elif "moods_and_genres" in data and isinstance(data["moods_and_genres"], list):
            self.add_horizontal_section(
                self.explore_box, "Moods & Genres", data["moods_and_genres"], is_category=True
            )

        # New Releases (Albums/Singles)
        if "new_releases" in data and isinstance(data["new_releases"], list):
            self.add_section(
                self.explore_box, "New Albums & Singles", data["new_releases"][:10]
            )

        # New Music Videos
        if "new_videos" in data and isinstance(data["new_videos"], list):
            self.add_section(
                self.explore_box, "New Music Videos", data["new_videos"][:5]
            )

        # Trending
        if "trending" in data and data["trending"] and "items" in data["trending"]:
            self.add_section(
                self.explore_box, "Trending", data["trending"]["items"][:5]
            )

        # Charts
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

        from ui.widgets.scroll_box import HorizontalScrollBox
        scroll_box = HorizontalScrollBox()

        h_box = Gtk.Box(orientation=Gtk.Orientation.HORIZONTAL, spacing=12)
        h_box.set_margin_bottom(12)  # Space for scrollbar if it appears
        
        display_items = items
        if is_category:
            display_items = items[:20]

        for item in display_items:
            btn_label = Gtk.Label(label=item.get("title", "Unknown"))
            # No truncation needed in a scrolled window
            btn_label.set_hexpand(False)

            button = Gtk.Button()
            button.set_child(btn_label)
            button.item_data = item
            button.connect("clicked", self.on_grid_button_clicked)
            button.add_css_class("pill") # Adwaita pill style for genres
            h_box.append(button)

        if is_category and len(items) > 20:
            view_all_btn = Gtk.Button(label="View All")
            view_all_btn.add_css_class("pill")
            view_all_btn.add_css_class("flat")
            weak_self = weakref.ref(self)
            view_all_btn.connect("clicked", lambda b, i=items, t=title, ws=weak_self: ws().on_view_all_clicked(i, t) if ws() else None)
            h_box.append(view_all_btn)

        scroll_box.set_content(h_box)
        section_box.append(scroll_box)

    def _add_charts_sections(self, charts):
        """Add charts sections: country selector, trending playlists, genre charts, top artists."""

        # ── Country selector + Charts header ──
        charts_header = Gtk.Box(orientation=Gtk.Orientation.HORIZONTAL, spacing=8)

        charts_label = Gtk.Label(label="Charts")
        charts_label.add_css_class("title-3")
        charts_label.set_halign(Gtk.Align.START)
        charts_header.append(charts_label)

        spacer = Gtk.Box()
        spacer.set_hexpand(True)
        charts_header.append(spacer)

        # Country dropdown
        countries = charts.get("countries", {})
        options = countries.get("options", [])

        if options:
            # Map country codes to display names
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
            # Sort by display name but keep Global first
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

        # ── Trending playlists (horizontal scroll) ──
        videos = charts.get("videos", [])
        if videos:
            self._add_chart_playlists("Trending", videos)

        # ── Genre charts (horizontal scroll of pills) ──
        genres = charts.get("genres", [])
        if genres:
            self._add_chart_playlists("Genre Charts", genres)

        # ── Top Artists (ranked list) ──
        artists = charts.get("artists", [])
        if artists:
            self._add_chart_artists("Top Artists", artists)

    def _add_chart_playlists(self, title, items):
        """Add a horizontal scroll section of chart playlist cards."""
        from ui.utils import AsyncImage

        section_box = Gtk.Box(orientation=Gtk.Orientation.VERTICAL, spacing=8)
        self.explore_box.append(section_box)

        label = Gtk.Label(label=title)
        label.add_css_class("heading")
        label.set_halign(Gtk.Align.START)
        section_box.append(label)

        from ui.widgets.scroll_box import HorizontalScrollBox
        scroll_box = HorizontalScrollBox()

        h_box = Gtk.Box(orientation=Gtk.Orientation.HORIZONTAL, spacing=12)
        h_box.set_margin_bottom(8)

        for item in items:
            thumb_url = item.get("thumbnails", [{}])[-1].get("url") if item.get("thumbnails") else None

            card = Gtk.Box(orientation=Gtk.Orientation.VERTICAL, spacing=4)
            card.set_cursor(Gdk.Cursor.new_from_name("pointer", None))
            card.add_css_class("artist-horizontal-item")
            card.item_data = item

            # Matches the 160px cover size used by library/search grids so
            # the Charts row doesn't tower over everything else on Explore.
            img = AsyncImage(url=thumb_url, size=160, player=self.player)
            wrapper = Gtk.Box()
            wrapper.set_overflow(Gtk.Overflow.HIDDEN)
            wrapper.add_css_class("card")
            wrapper.set_halign(Gtk.Align.CENTER)
            wrapper.append(img)
            card.append(wrapper)

            name_label = Gtk.Label(label=item.get("title", ""))
            name_label.set_ellipsize(Pango.EllipsizeMode.END)
            name_label.set_wrap(True)
            name_label.set_wrap_mode(Pango.WrapMode.WORD_CHAR)
            name_label.set_lines(2)
            name_label.set_halign(Gtk.Align.START)
            clamp = Adw.Clamp(maximum_size=160)
            clamp.set_child(name_label)
            card.append(clamp)

            click = Gtk.GestureClick()
            click.set_button(1)
            click.connect("released", self._on_chart_playlist_clicked, item)
            card.add_controller(click)

            h_box.append(card)

        scroll_box.set_content(h_box)
        section_box.append(scroll_box)

    def _add_chart_artists(self, title, artists):
        """Add a ranked list of chart artists."""
        from ui.utils import AsyncPicture

        section_box = Gtk.Box(orientation=Gtk.Orientation.VERTICAL, spacing=8)
        self.explore_box.append(section_box)

        label = Gtk.Label(label=title)
        label.add_css_class("heading")
        label.set_halign(Gtk.Align.START)
        section_box.append(label)

        list_box = Gtk.ListBox()
        list_box.add_css_class("boxed-list")
        list_box.set_selection_mode(Gtk.SelectionMode.NONE)
        list_box.connect("row-activated", self._on_chart_artist_activated)

        # Show top 20
        for artist in artists[:20]:
            row = Gtk.ListBoxRow()
            box = Gtk.Box(orientation=Gtk.Orientation.HORIZONTAL, spacing=12)
            box.add_css_class("song-row")
            row.set_child(box)

            # Rank number
            rank = artist.get("rank", "")
            rank_label = Gtk.Label(label=str(rank))
            rank_label.set_width_chars(3)
            rank_label.add_css_class("heading")
            rank_label.set_valign(Gtk.Align.CENTER)
            box.append(rank_label)

            # Trend indicator
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

            # Thumbnail
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

            # Name + subscribers
            vbox = Gtk.Box(orientation=Gtk.Orientation.VERTICAL, spacing=2)
            vbox.set_valign(Gtk.Align.CENTER)
            vbox.set_hexpand(True)

            name_label = Gtk.Label(label=artist.get("title", "Unknown"))
            name_label.set_halign(Gtk.Align.START)
            name_label.set_ellipsize(Pango.EllipsizeMode.END)
            name_label.set_lines(1)
            vbox.append(name_label)

            subs = artist.get("subscribers", "")
            if subs:
                sub_label = Gtk.Label(label=subs)
                sub_label.set_halign(Gtk.Align.START)
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

    def _on_chart_playlist_clicked(self, gesture, n_press, x, y, item):
        playlist_id = item.get("playlistId")
        if playlist_id:
            root = self.get_root()
            if root and hasattr(root, "open_playlist"):
                root.open_playlist(playlist_id, {
                    "title": item.get("title", ""),
                    "thumb": item.get("thumbnails", [{}])[-1].get("url") if item.get("thumbnails") else None,
                })

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

        # Wrap label and listbox in a box to control spacing
        section_box = Gtk.Box(orientation=Gtk.Orientation.VERTICAL, spacing=8)
        parent_box.append(section_box)

        label = Gtk.Label(label=title)
        label.add_css_class("heading")
        label.set_halign(Gtk.Align.START)
        section_box.append(label)

        list_box = Gtk.ListBox()
        list_box.add_css_class("boxed-list")
        list_box.set_selection_mode(Gtk.SelectionMode.NONE)
        list_box.connect("row-activated", self.on_row_activated)

        for item in items:
            row = Gtk.ListBoxRow()
            box = Gtk.Box(orientation=Gtk.Orientation.HORIZONTAL, spacing=12)
            box.add_css_class("song-row")
            row.set_child(box)
            # Highlight the row while its track is playing — Explore
            # rows don't use SongRowWidget so they need an explicit
            # subscription.
            if item.get("videoId"):
                attach_playing_highlight(box, self.player, item["videoId"])

            # Subtitle
            subtitle = ""

            # Special handling for Artist results to avoid redundant name
            if item.get("resultType") == "artist":
                if "subscribers" in item and item.get("subscribers"):
                    count = item.get("subscribers", "")
                    if (
                        count
                        and count[-1].isdigit() is False
                        and "listeners" not in count
                        and "subscribers" not in count
                    ):
                        subtitle = f"{count} monthly listeners"
                    else:
                        subtitle = count
                if not subtitle:
                    subtitle = "Artist"
            elif item.get("artists"):
                artists = item["artists"]
                if artists:
                    subtitle = ", ".join([a.get("name", "") for a in artists if isinstance(a, dict)])
                    if not subtitle:
                        # Artists might be plain strings
                        subtitle = ", ".join([str(a) for a in artists if a])
                # Fallback to author field
                if not subtitle and "author" in item:
                    author = item.get("author")
                    if isinstance(author, list):
                        subtitle = ", ".join([a.get("name", str(a)) for a in author])
                    elif isinstance(author, dict):
                        subtitle = author.get("name", str(author))
                    elif author:
                        subtitle = str(author)

                # Append album name if available
                album_data = item.get("album")
                if album_data:
                    album_name = album_data.get("name", "") if isinstance(album_data, dict) else str(album_data)
                    if album_name and subtitle:
                        subtitle += f" • {album_name}"
                    elif album_name:
                        subtitle = album_name

                # Check for Album type
                if "type" in item and subtitle:
                    subtitle += f" • {item['type']}"
                elif "type" in item:
                    subtitle = item["type"]
            elif "subscribers" in item:
                subtitle = item.get("subscribers", "")
            elif "itemCount" in item and item["itemCount"]:
                count = str(item["itemCount"])
                if "songs" not in count:
                    subtitle = f"{count} views"
                else:
                    subtitle = count

            # Cover Art
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

            subtitle_label = Gtk.Label(label=subtitle or "")
            subtitle_label.set_halign(Gtk.Align.START)
            subtitle_label.set_ellipsize(Pango.EllipsizeMode.END)
            subtitle_label.set_lines(1)
            subtitle_label.add_css_class("dim-label")
            subtitle_label.add_css_class("caption")
            subtitle_label.set_visible(bool(subtitle))

            # Fetch missing artist info in background for albums with empty artists
            if not subtitle:
                browse_id = item.get("browseId") or ""
                audio_pid = item.get("audioPlaylistId") or ""
                album_id = browse_id if browse_id.startswith("MPRE") else None
                if not album_id and audio_pid.startswith("OLAK"):
                    # Convert OLAK to browse ID via API
                    album_id = audio_pid
                if album_id:
                    item_type = item.get("type", "")
                    def _fetch_artist_info(aid, itype, lbl, client):
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
                                    GLib.idle_add(lbl.set_label, text)
                                    GLib.idle_add(lbl.set_visible, True)
                        except Exception:
                            pass
                    threading.Thread(
                        target=_fetch_artist_info,
                        args=(album_id, item_type, subtitle_label, self.client),
                        daemon=True,
                    ).start()

            # Fetch missing album name in background for songs
            vid_for_album = item.get("videoId")
            has_album = item.get("album")
            if vid_for_album and not has_album and item.get("resultType") in ("song", "video", None):
                def _fetch_album(vid, itm, lbl, client):
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
                                        lbl.set_label(album_name)
                                    lbl.set_visible(True)
                                GLib.idle_add(_update_label)
                    except Exception:
                        pass
                threading.Thread(
                    target=_fetch_album,
                    args=(vid_for_album, item, subtitle_label, self.client),
                    daemon=True,
                ).start()

            title_box = Gtk.Box(orientation=Gtk.Orientation.HORIZONTAL, spacing=6)
            title_box.append(title_label)

            # Explicit Badge
            meta = parse_item_metadata(item)
            if meta["is_explicit"]:
                explicit_badge = Gtk.Label(label="E")
                explicit_badge.add_css_class("explicit-badge")
                explicit_badge.set_valign(Gtk.Align.CENTER)
                title_box.append(explicit_badge)

            vbox.append(title_box)
            vbox.append(subtitle_label)
            box.append(vbox)

            # Like Button
            if item.get("videoId"):
                like_btn = LikeButton(
                    self.client, item["videoId"], item.get("likeStatus", "INDIFFERENT")
                )
                like_btn.set_valign(Gtk.Align.CENTER)
                box.append(like_btn)
                row._like_btn = like_btn

            row.item_data = item
            row.set_activatable(True)

            # Context Menu (Right Click)
            gesture = Gtk.GestureClick()
            gesture.set_button(3)  # Right click
            gesture.connect("released", self.on_row_right_click, row)
            row.add_controller(gesture)

            # Long Press for touch
            lp = Gtk.GestureLongPress()
            lp.connect(
                "pressed", lambda g, x, y, ws=weakref.ref(self): ws().on_row_right_click(g, 1, x, y, g.get_widget()) if ws() else None
            )
            row.add_controller(lp)

            list_box.append(row)

        section_box.append(list_box)

    def on_external_search(self, text):
        if self.search_timer:
            GObject.source_remove(self.search_timer)

        if len(text) > 2:
            self.search_timer = GObject.timeout_add(600, self.perform_search, text)
        elif len(text) == 0:
            self.stack.set_visible_child_name("explore")

    def on_search_changed(self, entry):
        # Deprecated local handler
        pass

    def perform_search(self, query):
        self.search_timer = None

        # Show loading
        self.stack.set_visible_child_name("loading")
        # self.spinner.start()

        thread = threading.Thread(target=self._search_thread, args=(query,))
        thread.daemon = True
        thread.start()
        return False

    def _search_thread(self, query):
        from ui.utils import is_online
        if is_online():
            results = self.client.search(query)
            GObject.idle_add(self.update_results, results)
        else:
            # Offline: search local downloads
            results = self._search_local(query)
            GObject.idle_add(self.update_results, results)

    def _search_local(self, query):
        """Search downloaded songs in the local database."""
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

    def update_results(self, results):
        # self.spinner.stop()
        self.stack.set_visible_child_name("results")

        # Clear existing results
        # Removing children from Box
        child = self.results_box.get_first_child()
        while child:
            next_child = child.get_next_sibling()
            self.results_box.remove(child)
            child = next_child

        if not results:
            return

        # Group Results
        top_result = None
        artists = []
        songs = []
        albums = []
        videos = []
        playlists = []

        for r in results:
            # Normalize title for artists
            if "title" not in r:
                if "artist" in r:
                    r["title"] = r["artist"]
                elif "artists" in r and r["artists"]:
                    r["title"] = r["artists"][0]["name"]

            # Normalize browseId for Top Result (Artist)
            if "browseId" not in r and "artists" in r and r["artists"]:
                r["browseId"] = r["artists"][0]["id"]

            r_type = r.get("resultType")
            category = r.get("category")

            # Extract Top Result
            if category == "Top result" and not top_result:
                top_result = r
                continue

            if r_type == "artist":
                artists.append(r)
            elif r_type == "song":
                songs.append(r)
            elif r_type == "album":
                albums.append(r)
            elif r_type == "video":
                videos.append(r)
            elif r_type == "playlist" or category == "Community playlists":
                playlists.append(r)

        # Display Top Result first
        if top_result:
            self.add_section(self.results_box, "Top Result", [top_result])

        # Allow user specified order: Artists > Songs > Albums > Videos > Playlists
        self.add_section(self.results_box, "Artists", artists)
        self.add_section(self.results_box, "Songs", songs)
        self.add_section(self.results_box, "Albums", albums)
        self.add_section(self.results_box, "Videos", videos)
        self.add_section(self.results_box, "Community Playlists", playlists)

    def on_player_state_changed(self, player, state):
        if state == "playing" or state == "rec-started":
            # We need to find the row with the spinner and remove it.
            # Iterating through list box children is one way.
            # Or just keep a reference.
            if hasattr(self, "loading_row_spinner") and self.loading_row_spinner:
                # Remove suffix? AdwActionRow.remove(widget) works if it's a child.
                # Suffixes are children.
                try:
                    # We need the parent row of the spinner?
                    # or just: row.remove(spinner)
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
                "thumb": data["thumbnails"][-1]["url"]
                if data.get("thumbnails")
                else None,
                "author": data.get("runs", [{}])[0].get("text")
                if "runs" in data
                else None,  # basic guess
            }
            # For search results, author might be in subtitle or runs.
            # Pass title and thumb for immediate visual feedback.

            self.open_playlist_callback(data["browseId"], initial_data)

        elif hasattr(row, "item_data"):
            data = row.item_data
            title = data.get("title", "Unknown")
            res_type = data.get("resultType")

            # Helper to open playlist/album
            def open_pid(pid):
                initial_data = {
                    "title": title,
                    "thumb": data["thumbnails"][-1]["url"]
                    if data.get("thumbnails")
                    else None,
                    "author": ", ".join(
                        [a.get("name", "") for a in data.get("artists", [])]
                    )
                    if "artists" in data
                    else data.get("count", ""),
                }
                self.open_playlist_callback(pid, initial_data)

            # 1. Check resultType first (Robust for Search Results)
            if res_type in ["song", "video"]:
                if "videoId" in data:
                    # Build queue from the listbox (siblings)
                    queue_tracks = []
                    start_index = 0

                    # listbox is passed as argument
                    child = listbox.get_first_child()
                    idx = 0
                    while child:
                        if hasattr(child, "item_data"):
                            s_data = child.item_data
                            if "videoId" in s_data:
                                # Normalize
                                s_title = s_data.get("title", "Unknown")

                                s_thumb = ""
                                if s_data.get("thumbnails"):
                                    s_thumb = s_data["thumbnails"][-1]["url"]

                                s_artist = ""
                                if "artists" in s_data:
                                    s_artist = ", ".join(
                                        [a.get("name", "") for a in s_data["artists"]]
                                    )
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
                        # Fallback to single (shouldn't happen if we are here)
                        thumb_url = (
                            data.get("thumbnails", [])[-1]["url"]
                            if data.get("thumbnails")
                            else None
                        )
                        artist_name = (
                            ", ".join(
                                [a.get("name", "") for a in data.get("artists", [])]
                            )
                            if "artists" in data
                            else data.get("artist", "")
                        )
                        self.player.load_video(
                            data["videoId"], title, artist_name, thumb_url
                        )
                    return

            elif res_type in ["album", "single", "ep"]:
                # Prefer browseId (MPRE) or audioPlaylistId (OLAK)
                if "browseId" in data and data["browseId"].startswith("MPRE"):
                    open_pid(data["browseId"])
                    return
                elif "audioPlaylistId" in data:
                    open_pid(data["audioPlaylistId"])  # conversion will handle it
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

            # 2. Fallback to Key-Based logic (for items without explicit resultType)
            if "videoId" in data and res_type not in [
                "album",
                "single",
                "ep",
                "playlist",
                "artist",
            ]:
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
                # Check if it's a playlist or artist
                if res_type in ["playlist", "album"] or data["browseId"].startswith(
                    ("VL", "PL", "RD", "OL", "MPRE", "FEmusic_release_")
                ):
                    open_pid(data["browseId"])
                else:
                    print(f"Open BrowseID (Artist?): {data['browseId']}")
                    # Check if we can navigate
                    root = self.get_root()
                    if hasattr(root, "open_artist"):
                        root.open_artist(data["browseId"], title)

    
    def _on_radio_action(self, data):
        # if its a song or video, it has vid
        vid = data.get("videoId")

        if vid:
            self.player.start_radio(video_id=vid)
            self.get_root().add_toast("Starting radio...")
            return

        # if no vid, then it can be a single, album, playlist, or artist
        #   if its in a search page, then the data can be identified by 'resultType' field
        #   if its in the explore page, then the data can be identified by 'type' field
        #   NOTE: some data in search page can have both 'resultType' and 'type', explore page will always only have 'type'
        from_search_page = "resultType" in data
        data_type = data.get("resultType") if from_search_page else data.get("type", None)

        # in most cases, this shouldn't happen, but if it does, the toast will help identify a fix easier
        if data_type == None:
            self.get_root().add_toast("Radio unsupported")
            print(f"[Search Radio] Failed to find item type: {data}")
            return

        # search page has album, playlist, and artist
        #   album's "resultType" is "album"
        #   playlist's "resultType" is "playlist"
        #   artist's "resultType" is "artist"

        # explore page has album and single
        #   album's "type" field is "Album"
        #   single's "type" field is "Single"

        # NOTE: some data "artist" resultType have radioId field, some doesn't
        # if not, we have to do fallback to searching artist from api
        # searching is slow, so we use thread for this case

        # breaking the search process below to avoid using threads when not necessary
        use_thread = from_search_page and data_type == "artist" and "radioId" not in data
        if use_thread:
            def thread_func():
                channel_id = data.get("browseId")
                # NOTE: api call to get artist is slow, thats why we use thread
                artist = self.client.get_artist(channel_id)
                if "radioId" in artist:
                    playlist_id = artist.get("radioId")
                    self.player.start_radio(playlist_id=playlist_id)
                    self.get_root().add_toast("Starting radio...")
                    return
                else:
                    songs = artist.get("songs", {}).get("results", [])
                    first_song = songs[0] if len(songs) > 0 else None
                    first_song_id = first_song.get("videoId") if first_song != None else None

                    if first_song_id != None:
                        self.player.start_radio(video_id=first_song_id)
                        self.get_root().add_toast("Starting radio...")
                    else:
                        self.get_root().add_toast("Radio unsupported")
                        print(f"[Search Radio] {data_type} ({"search" if from_search_page else "explore"} page) item unprocessed or doesn't have supported playlist Id")
                        print(f"[Search Radio] Item data: {data}")
            
            threading.Thread(target=thread_func, daemon=True).start()
            return

        playlist_id = None
        # search page
        if from_search_page:
            if data_type == "album":
                playlist_id = data.get("playlistId", None)
                playlist_id = ("RDAMPL" + playlist_id) if not playlist_id == None else None
            if data_type == "playlist":
                playlist_id = data.get("browseId", None)
                playlist_id = ("RDAMPL" + playlist_id) if not playlist_id == None else None
            if data_type == "artist":
                playlist_id = data.get("radioId", None)
        # explore page
        else:
            if data_type == "Album" or data_type == "Single":
                playlist_id = data.get("audioPlaylistId", None)
                playlist_id = ("RDAMPL" + playlist_id) if not playlist_id == None else None

        if playlist_id == None:
            self.get_root().add_toast("Radio unsupported")
            print(f"[Search Radio] {data_type} ({"search" if from_search_page else "explore"} page) item unprocessed or doesn't have supported playlist Id")
            print(f"[Search Radio] Item data: {data}")
        else:
            self.player.start_radio(playlist_id=playlist_id)
            self.get_root().add_toast("Starting radio...")
        
                        
    def on_row_right_click(self, gesture, n_press, x, y, row):
        if not hasattr(row, "item_data"):
            return

        data = row.item_data

        # Create Action Group for this row
        group = Gio.SimpleActionGroup()
        # Insert into ROW, so the popover (child of row) finds it
        row.insert_action_group("row", group)

        # Determine URL
        url = None
        if "videoId" in data:
            url = f"https://music.youtube.com/watch?v={data['videoId']}"
        elif "audioPlaylistId" in data:  # OLAK ID (Album) - Prioritize this!
            url = f"https://music.youtube.com/playlist?list={data['audioPlaylistId']}"
        elif "playlistId" in data:
            url = f"https://music.youtube.com/playlist?list={data['playlistId']}"
        elif "browseId" in data:
            bid = data["browseId"]
            if bid.startswith("MPRE") or bid.startswith("OLAK"):  # Album Fallback
                url = f"https://music.youtube.com/playlist?list={bid}"
            elif bid.startswith("UC") or bid.startswith("U"):  # Artist
                url = f"https://music.youtube.com/channel={bid}"
            elif bid.startswith("VL"):  # Playlist
                url = f"https://music.youtube.com/playlist?list={bid[2:]}"
            else:  # Fallback
                url = f"https://music.youtube.com/browse/{bid}"

        # Copy Link
        def copy_link_action(action, param):
            if url:
                try:
                    clipboard = Gdk.Display.get_default().get_clipboard()
                    clipboard.set(url)
                    print(f"Copied to clipboard: {url}")
                    root = self.get_root()
                    if root and hasattr(root, "add_toast"):
                        root.add_toast("Link copied")
                except Exception as e:
                    print(f"Clipboard error: {e}")

        # Go To Artist
        def goto_artist_action(action, param):
            # Check for artist in data
            # Determine main artist
            artist_item = None
            if "artists" in data and data["artists"]:
                artist_item = data["artists"][0]
            elif "artist" in data:
                # sometimes simple string, sometimes dict? In search results usually list of dicts.
                pass
            elif data.get("resultType") == "artist":
                # It IS an artist
                artist_item = {
                    "id": data.get("browseId"),
                    "name": data.get("artist") or data.get("title"),
                }

            if artist_item and artist_item.get("id"):
                aid = artist_item.get("id")
                name = artist_item.get("name")
                root = self.get_root()
                if hasattr(root, "open_artist"):
                    root.open_artist(aid, name)

        # Add Actions
        action_copy = Gio.SimpleAction.new("copy_link", None)
        action_copy.connect("activate", copy_link_action)
        group.add_action(action_copy)

        action_goto = Gio.SimpleAction.new("goto_artist", None)
        action_goto.connect("activate", goto_artist_action)
        group.add_action(action_goto)

        # Add to Playlist via the custom popover (covers + search + recents).
        def add_to_playlist(target_pid):
            target_vid = data.get("videoId")
            if not (target_pid and target_vid):
                return
            from ui.widgets.add_to_playlist import mark_playlist_used
            mark_playlist_used(target_pid)

            def thread_func():
                success = self.client.add_playlist_items(target_pid, [target_vid])
                if success:
                    print(f"Added {target_vid} to {target_pid}")
                else:
                    print(f"Failed to add {target_vid} to {target_pid}")

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

        # Start Radio
        action_radio = Gio.SimpleAction.new("start_radio", None)
        action_radio.connect("activate", lambda a, p, ws=weakref.ref(self): ws()._on_radio_action(data) if ws() else None)
        group.add_action(action_radio)

        # Build Menu Model
        menu_model = Gio.Menu()

        # Navigation
        nav_section = Gio.Menu()
        has_artist = False
        if "artists" in data and data["artists"] and data["artists"][0].get("id"):
            has_artist = True
        elif data.get("resultType") == "artist" and data.get("browseId"):
            has_artist = True
        if has_artist:
            nav_section.append("Go to Artist", "row.goto_artist")
        if nav_section.get_n_items() > 0:
            menu_model.append_section(None, nav_section)

        # Queue
        queue_section = Gio.Menu()

        if "videoId" in data:
            def play_next_action(action, param):
                self.player.add_to_queue(dict(data), next=True)
                self.get_root().add_toast("Playing next")

            def add_to_queue_action(action, param):
                self.player.add_to_queue(dict(data), next=False)
                self.get_root().add_toast("Added to queue")
            
            a_pn = Gio.SimpleAction.new("play_next", None)
            a_pn.connect("activate", play_next_action)
            group.add_action(a_pn)
            queue_section.append("Play Next", "row.play_next")

            a_aq = Gio.SimpleAction.new("add_to_queue", None)
            a_aq.connect("activate", add_to_queue_action)
            group.add_action(a_aq)
            queue_section.append("Add to Queue", "row.add_to_queue")

        if queue_section.get_n_items() > 0:
            menu_model.append_section(None, queue_section)

        # Actions
        action_section = Gio.Menu()
        action_section.append("Start Radio", "row.start_radio")
        if "videoId" in data and self.client.get_editable_playlists():
            action_section.append("Add to Playlist…", "row.show_add_to_playlist")
        menu_model.append_section(None, action_section)

        # Clipboard
        clip_section = Gio.Menu()
        if url:
            clip_section.append("Copy Link", "row.copy_link")
        if clip_section.get_n_items() > 0:
            menu_model.append_section(None, clip_section)

        if menu_model.get_n_items() > 0:
            popover = Gtk.PopoverMenu.new_from_model(menu_model)
            popover.set_parent(row)
            popover.set_has_arrow(False)

            # Set rectangle to click position
            rect = Gdk.Rectangle()
            rect.x = int(x)
            rect.y = int(y)
            rect.width = 1
            rect.height = 1
            popover.set_pointing_to(rect)

            popover.popup()

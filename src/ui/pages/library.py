import weakref
import os
import json as _json
from gi.repository import Gtk, Adw, GObject, GLib, Gdk, Gio, Pango
import threading
from api.client import MusicClient
from ui.utils import show_toast
from ui.util_classes import ScrolledWindow


LIBRARY_VIEW_MODES = ("list", "grid")
DEFAULT_LIBRARY_VIEW_MODE = "grid"


def _prefs_path():
    return os.path.join(GLib.get_user_data_dir(), "muse", "prefs.json")


def _load_prefs():
    path = _prefs_path()
    try:
        if os.path.exists(path):
            with open(path) as f:
                return _json.load(f)
    except Exception:
        pass
    return {}


def _save_prefs(prefs):
    path = _prefs_path()
    os.makedirs(os.path.dirname(path), exist_ok=True)
    try:
        with open(path, "w") as f:
            _json.dump(prefs, f)
    except Exception:
        pass


def _get_library_view_mode_pref():
    val = _load_prefs().get("library_view_mode", DEFAULT_LIBRARY_VIEW_MODE)
    return val if val in LIBRARY_VIEW_MODES else DEFAULT_LIBRARY_VIEW_MODE


def _set_library_view_mode_pref(mode):
    if mode not in LIBRARY_VIEW_MODES:
        return
    prefs = _load_prefs()
    prefs["library_view_mode"] = mode
    _save_prefs(prefs)


def _make_flow_grid():
    """Shared FlowBox config matching DiscographyPage's card grid.
    Spacing is 0 so the gutter comes purely from each card's own margins
    + the `.card` wrapper padding."""
    grid = Gtk.FlowBox()
    grid.set_valign(Gtk.Align.START)
    grid.set_selection_mode(Gtk.SelectionMode.NONE)
    grid.set_homogeneous(True)
    grid.set_max_children_per_line(5)
    grid.set_min_children_per_line(2)
    grid.set_row_spacing(0)
    grid.set_column_spacing(0)
    grid.set_activate_on_single_click(True)
    grid.set_visible(False)
    return grid


def _make_library_card(player, title, subtitle, thumb_url, fallback_icon):
    """Vertical cover-on-top card matching DiscographyPage. Returns the
    FlowBoxChild plus the inner AsyncImage so the caller can attach
    identifiers or reload the cover later."""
    from ui.utils import AsyncImage

    child = Gtk.FlowBoxChild()
    child.add_css_class("library-card")

    box = Gtk.Box(orientation=Gtk.Orientation.VERTICAL, spacing=4)
    box.set_halign(Gtk.Align.CENTER)
    # Vertical breathing room so rows don't run into each other. Horizontal
    # gutter is contributed by the FlowBox cell width; this balances that
    # out with a matching gap between rows.
    box.set_margin_top(8)
    box.set_margin_bottom(8)

    img = AsyncImage(url=thumb_url, size=160, player=player)
    if not thumb_url and fallback_icon:
        img.set_from_icon_name(fallback_icon)

    wrapper = Gtk.Box()
    wrapper.set_overflow(Gtk.Overflow.HIDDEN)
    wrapper.add_css_class("card")
    wrapper.set_halign(Gtk.Align.CENTER)
    wrapper.append(img)
    box.append(wrapper)

    title_label = Gtk.Label(label=title)
    title_label.set_halign(Gtk.Align.START)
    title_label.set_ellipsize(Pango.EllipsizeMode.END)
    title_label.set_wrap(True)
    title_label.set_wrap_mode(Pango.WrapMode.WORD_CHAR)
    title_label.set_lines(2)
    title_label.set_justify(Gtk.Justification.LEFT)
    title_label.set_tooltip_text(title)
    title_clamp = Adw.Clamp(maximum_size=160)
    title_clamp.set_child(title_label)
    box.append(title_clamp)

    if subtitle:
        subtitle_label = Gtk.Label(label=subtitle)
        subtitle_label.set_halign(Gtk.Align.START)
        subtitle_label.set_ellipsize(Pango.EllipsizeMode.END)
        subtitle_label.add_css_class("dim-label")
        subtitle_label.add_css_class("caption")
        subtitle_clamp = Adw.Clamp(maximum_size=160)
        subtitle_clamp.set_child(subtitle_label)
        box.append(subtitle_clamp)

    child.set_child(box)
    child._cover_img = img
    child._search_title = title
    return child


class LibraryPage(Adw.Bin):
    def __init__(self, player, open_playlist_callback, *args, **kwargs):
        super().__init__(*args, **kwargs)
        self.player = player
        self.client = MusicClient()
        self.open_playlist_callback = open_playlist_callback
        self._is_loading = False

        self.main_box = Gtk.Box(orientation=Gtk.Orientation.VERTICAL)

        # Tab switcher: Library / Uploads
        self.lib_stack = Gtk.Stack()
        self.lib_stack.set_transition_type(Gtk.StackTransitionType.SLIDE_LEFT_RIGHT)

        # Single scrolled window for the whole page
        scrolled = ScrolledWindow()
        scrolled.set_vexpand(True)
        scrolled.set_policy(Gtk.PolicyType.NEVER, Gtk.PolicyType.AUTOMATIC)

        clamp = Adw.Clamp()
        # Match PlaylistPage's width so the Library/Explore/Playlist views
        # line up visually as the user tab-switches.
        clamp.set_maximum_size(1024)
        clamp.set_tightening_threshold(600)

        self.content_box = Gtk.Box(orientation=Gtk.Orientation.VERTICAL, spacing=24)
        self.content_box.set_margin_top(12)
        self.content_box.set_margin_bottom(24)
        self.content_box.set_margin_start(12)
        self.content_box.set_margin_end(12)

        # Tab row inside the content (same constraints as albums/artists)
        tab_row = Gtk.Box(orientation=Gtk.Orientation.HORIZONTAL, spacing=8)
        tab_row.set_margin_bottom(8)

        # Compact toggle buttons instead of StackSwitcher
        self._lib_tab_btn = Gtk.ToggleButton(label="Library")
        self._lib_tab_btn.set_active(True)
        self._upl_tab_btn = Gtk.ToggleButton(label="Uploads")
        self._upl_tab_btn.set_group(self._lib_tab_btn)

        self._lib_tab_btn.connect("toggled", lambda b: (
            self.lib_stack.set_visible_child_name("library") if b.get_active() else None
        ))
        self._upl_tab_btn.connect("toggled", lambda b: (
            self.lib_stack.set_visible_child_name("uploads") if b.get_active() else None
        ))

        tab_row.append(self._lib_tab_btn)
        tab_row.append(self._upl_tab_btn)

        spacer = Gtk.Box()
        spacer.set_hexpand(True)
        tab_row.append(spacer)

        # Library action buttons (only visible on library tab)
        self.lib_actions_box = Gtk.Box(orientation=Gtk.Orientation.HORIZONTAL, spacing=4)

        # Single toggle between list and grid view. Icon swaps to show the
        # mode we'd switch *to* on the next click (like Nautilus).
        self.view_toggle_btn = Gtk.Button()
        self.view_toggle_btn.add_css_class("flat")
        self.view_toggle_btn.add_css_class("circular")
        self.view_toggle_btn.set_valign(Gtk.Align.CENTER)
        self.view_toggle_btn.connect("clicked", self._on_view_toggle_clicked)
        self.lib_actions_box.append(self.view_toggle_btn)
        self._sync_view_toggle_button()

        # Downloads / History / Upload have moved to the avatar menu in
        # the header bar — see MainWindow._build_avatar_menu_button.
        tab_row.append(self.lib_actions_box)

        # Upload action buttons (only visible on uploads tab). The
        # "Upload songs" action itself lives in the avatar menu; what
        # stays here is the upload-tab-only "All songs" view shortcut.
        self.uploads_actions_box = Gtk.Box(orientation=Gtk.Orientation.HORIZONTAL, spacing=4)
        self.uploads_actions_box.set_visible(False)

        self.all_songs_btn = Gtk.Button(icon_name="audio-x-generic-symbolic")
        self.all_songs_btn.add_css_class("flat")
        self.all_songs_btn.add_css_class("circular")
        self.all_songs_btn.set_valign(Gtk.Align.CENTER)
        self.all_songs_btn.set_tooltip_text("All Uploaded Songs")
        weak_self = weakref.ref(self)
        self.all_songs_btn.connect("clicked", lambda b: weak_self().uploads_page._open_all_songs() if weak_self() else None)
        self.uploads_actions_box.append(self.all_songs_btn)

        tab_row.append(self.uploads_actions_box)
        self.content_box.append(tab_row)

        # The lib_stack goes below the tab row
        self.lib_stack.set_vexpand(True)
        self.content_box.append(self.lib_stack)

        # Library tab content (playlists, albums, artists)
        self.lib_content_box = Gtk.Box(orientation=Gtk.Orientation.VERTICAL, spacing=24)

        # Filter text is driven by the MainWindow global search bar via
        # `filter_content(text)`, matching PlaylistPage's pattern.
        self.current_filter_text = ""

        self.lib_stack.add_titled(self.lib_content_box, "library", "Library")

        # Loading spinner is placed as a Gtk.Overlay child at main_box
        # level (wired up below after the scrolled window is built).
        # Anchoring here — inside the sizing-constrained scrolled/clamp
        # hierarchy — never produced a usable center because Adw.Clamp's
        # allocation collapses to the natural height of its child when
        # the library is empty.
        self._loading_wrap = self._build_overlay_loader("Refreshing Library...")

        # Second overlay for the Uploads tab. UploadsPage's own nested
        # structure (scrolled → clamp → content_box → lib_stack → …)
        # swallows vexpand and doesn't centre reliably, so we hoist its
        # loading indicator up to the same main-box-level Overlay that
        # already works for the library tab.
        self._uploads_loading_wrap = self._build_overlay_loader(
            "Loading uploads..."
        )

        # 1. Playlists Section (inside lib_content_box, not content_box)
        self.playlists_section = Gtk.Box(orientation=Gtk.Orientation.VERTICAL, spacing=8)
        playlists_section = self.playlists_section

        playlists_header_box = Gtk.Box(
            orientation=Gtk.Orientation.HORIZONTAL, spacing=12
        )
        playlists_header_box.set_size_request(
            -1, 34
        )

        playlists_label = Gtk.Label(label="Playlists")
        playlists_label.add_css_class("heading")
        playlists_label.set_halign(Gtk.Align.START)
        playlists_label.set_valign(Gtk.Align.CENTER)
        playlists_label.set_hexpand(True)
        playlists_header_box.append(playlists_label)

        self.new_playlist_btn = Gtk.Button(icon_name="list-add-symbolic")
        self.new_playlist_btn.add_css_class("flat")
        self.new_playlist_btn.add_css_class("circular")
        self.new_playlist_btn.set_valign(Gtk.Align.CENTER)
        self.new_playlist_btn.set_tooltip_text("New Playlist")
        self.new_playlist_btn.connect("clicked", self.on_new_playlist_clicked)
        playlists_header_box.append(self.new_playlist_btn)

        playlists_section.append(playlists_header_box)

        self.playlists_list = Gtk.ListBox()
        self.playlists_list.set_selection_mode(Gtk.SelectionMode.NONE)
        self.playlists_list.add_css_class("boxed-list")
        self.playlists_list.connect("row-activated", self.on_playlist_activated)
        playlists_section.append(self.playlists_list)

        self.playlists_grid = _make_flow_grid()
        self.playlists_grid.connect("child-activated", self._on_playlist_grid_activated)
        playlists_section.append(self.playlists_grid)

        self.lib_content_box.append(playlists_section)

        # 2. Albums Section
        self.albums_section = Gtk.Box(orientation=Gtk.Orientation.VERTICAL, spacing=8)
        albums_section = self.albums_section

        albums_header_box = Gtk.Box(orientation=Gtk.Orientation.HORIZONTAL, spacing=12)
        albums_header_box.set_size_request(-1, 34)

        albums_label = Gtk.Label(label="Albums")
        albums_label.add_css_class("heading")
        albums_label.set_halign(Gtk.Align.START)
        albums_label.set_valign(Gtk.Align.CENTER)
        albums_header_box.append(albums_label)

        albums_section.append(albums_header_box)

        self.albums_list = Gtk.ListBox()
        self.albums_list.set_selection_mode(Gtk.SelectionMode.NONE)
        self.albums_list.add_css_class("boxed-list")
        self.albums_list.connect("row-activated", self.on_album_activated)
        albums_section.append(self.albums_list)

        self.albums_grid = _make_flow_grid()
        self.albums_grid.connect("child-activated", self._on_album_grid_activated)
        albums_section.append(self.albums_grid)

        self.lib_content_box.append(albums_section)

        # 3. Artists Section
        self.artists_section = Gtk.Box(orientation=Gtk.Orientation.VERTICAL, spacing=8)
        artists_section = self.artists_section

        artists_header_box = Gtk.Box(orientation=Gtk.Orientation.HORIZONTAL, spacing=12)
        artists_header_box.set_size_request(-1, 34)

        artists_label = Gtk.Label(label="Artists")
        artists_label.add_css_class("heading")
        artists_label.set_halign(Gtk.Align.START)
        artists_label.set_valign(Gtk.Align.CENTER)
        artists_header_box.append(artists_label)

        artists_section.append(artists_header_box)

        self.artists_list = Gtk.ListBox()
        self.artists_list.set_selection_mode(Gtk.SelectionMode.NONE)
        self.artists_list.add_css_class("boxed-list")
        self.artists_list.connect("row-activated", self.on_artist_activated)
        artists_section.append(self.artists_list)

        self.artists_grid = _make_flow_grid()
        self.artists_grid.connect("child-activated", self._on_artist_grid_activated)
        artists_section.append(self.artists_grid)

        self.lib_content_box.append(artists_section)

        # ── Tab 2: Uploads ──
        self.uploads_page = UploadsPage(self.player, self.client, self.open_playlist_callback)
        self.uploads_page._library_page = self  # Reference for upload queue UI
        # Hoist the uploads loading indicator to the main-box-level
        # Overlay so `valign=CENTER` actually centers — the Uploads
        # tab's internal sizing chain can't do it on its own. Gated on
        # the uploads tab being visible so it doesn't flash over the
        # library tab during the parallel initial load.
        def _set_uploads_loading(visible):
            on_uploads = self.lib_stack.get_visible_child_name() == "uploads"
            self._uploads_loading_wrap.set_visible(bool(visible) and on_uploads)
            self._uploads_loading_pending = bool(visible)
        self._uploads_loading_pending = False
        self.uploads_page.set_loading_cb(_set_uploads_loading)
        self.lib_stack.add_titled(self.uploads_page, "uploads", "Uploads")
        self._uploads_loaded = True  # Will be loaded on startup
        self.lib_stack.connect("notify::visible-child-name", self._on_tab_changed)

        clamp.set_child(self.content_box)
        scrolled.set_child(clamp)

        # Gtk.Overlay at the Bin level: the scrolled window is the base
        # (drives layout, takes the full Bin allocation via vexpand),
        # and the loading wraps float on top with their own alignment.
        # This is how DiscographyPage's spinner centers — the difference
        # for Library is the extra scrolled/clamp nesting, which an
        # overlay sidesteps entirely.
        loading_overlay = Gtk.Overlay()
        loading_overlay.set_vexpand(True)
        loading_overlay.set_child(scrolled)
        loading_overlay.add_overlay(self._loading_wrap)
        loading_overlay.add_overlay(self._uploads_loading_wrap)

        self.main_box.append(loading_overlay)
        self.set_child(self.main_box)

        # Load Library + Uploads
        self.load_library()
        self.uploads_page.load()

        # Connect Player
        self.loading_row_spinner = None
        self.player.connect("state-changed", self.on_player_state_changed)

        # Apply the correct list/grid layout once the widget is about to be
        # shown — by then self.get_root()._is_compact is trustworthy.
        # Without this, the first paint always shows the ListBox because
        # the breakpoint handlers haven't run yet.
        self.connect("map", self._on_mapped_for_layout)

    def _on_mapped_for_layout(self, *args):
        root = self.get_root()
        compact = bool(getattr(root, "_is_compact", False)) if root else False
        self._apply_library_layout(compact)

    def _sync_view_toggle_button(self):
        """Set the toggle button's icon + tooltip to show the mode we'd
        switch to on the next click (Nautilus convention)."""
        current = _get_library_view_mode_pref()
        if current == "grid":
            self.view_toggle_btn.set_icon_name("view-list-symbolic")
            self.view_toggle_btn.set_tooltip_text("Switch to list view")
        else:
            self.view_toggle_btn.set_icon_name("view-grid-symbolic")
            self.view_toggle_btn.set_tooltip_text("Switch to grid view")

    def _on_view_toggle_clicked(self, btn):
        new_mode = "list" if _get_library_view_mode_pref() == "grid" else "grid"
        _set_library_view_mode_pref(new_mode)
        self._sync_view_toggle_button()
        root = self.get_root()
        compact = bool(getattr(root, "_is_compact", False)) if root else False
        self._apply_library_layout(compact)

    def trigger_refresh(self):
        """Public entry point used by MainWindow's header-bar refresh button.
        Reloads both the Library data and the Uploads data."""
        if self._is_loading:
            return
        self.load_library(silent=True)
        if hasattr(self, "uploads_page") and hasattr(self.uploads_page, "load"):
            self.uploads_page.load()

    def set_compact_mode(self, compact):
        self._compact = compact
        if compact:
            self.add_css_class("compact")
            self.content_box.set_spacing(16)
        else:
            self.remove_css_class("compact")
            self.content_box.set_spacing(24)

        # Propagate compact to all song row images (library + uploads)
        self._propagate_compact(self.content_box, compact)
        if hasattr(self, 'uploads_page'):
            self.uploads_page.set_compact_mode(compact)

        # Swap between desktop grid and mobile list for each library section.
        self._apply_library_layout(compact)

    def filter_content(self, text):
        """Called by MainWindow global search bar — filters library rows/cards."""
        self.current_filter_text = (text or "").strip().lower()
        # If user is typing a search on the library tab, route into uploads
        # too so it filters the currently-visible sub-tab.
        if hasattr(self, "uploads_page") and hasattr(self.uploads_page, "filter_content"):
            self.uploads_page.filter_content(text)
        self._apply_library_filter()
        self._update_section_visibility()

    def _apply_library_filter(self):
        """Hide library rows whose title doesn't contain the query
        (case-insensitive). Albums also match by artist name."""
        query = self.current_filter_text

        def matches(*haystacks):
            if not query:
                return True
            for h in haystacks:
                if h and query in h.lower():
                    return True
            return False

        def album_artist_str(album):
            if not isinstance(album, dict):
                return ""
            artists = album.get("artists") or []
            if isinstance(artists, list):
                return ", ".join(
                    a.get("name", "") for a in artists if isinstance(a, dict)
                )
            return str(artists or "")

        row = self.playlists_list.get_row_at_index(0)
        while row:
            row.set_visible(matches(getattr(row, "playlist_title", "")))
            row = row.get_next_sibling()

        row = self.albums_list.get_row_at_index(0)
        while row:
            album = getattr(row, "album_data", None)
            title = album.get("title", "") if isinstance(album, dict) else ""
            row.set_visible(matches(title, album_artist_str(album)))
            row = row.get_next_sibling()

        row = self.artists_list.get_row_at_index(0)
        while row:
            row.set_visible(matches(getattr(row, "artist_name", "")))
            row = row.get_next_sibling()

        # Mirror the filter into the desktop FlowBox grids if they exist.
        # Album cards store their `album_data` so artists can be matched too.
        for grid_attr in ("playlists_grid", "albums_grid", "artists_grid"):
            grid = getattr(self, grid_attr, None)
            if grid is None:
                continue
            child = grid.get_first_child()
            while child:
                title = getattr(child, "_search_title", "")
                album = getattr(child, "_album_data", None)
                child.set_visible(matches(title, album_artist_str(album)))
                child = child.get_next_sibling()

    def _update_section_visibility(self):
        """Hide whole sections (header + list) whose data is empty — both
        because the user has nothing in that category and because the active
        filter matches nothing in it."""

        def any_visible(container):
            child = container.get_first_child() if container is not None else None
            while child:
                if child.get_visible():
                    return True
                child = child.get_next_sibling()
            return False

        sections = [
            (self.playlists_section, self.playlists_list, self.playlists_grid),
            (self.albums_section, self.albums_list, self.albums_grid),
            (self.artists_section, self.artists_list, self.artists_grid),
        ]
        for section, list_w, grid_w in sections:
            has_any = any_visible(list_w) or any_visible(grid_w)
            section.set_visible(has_any)

    def _apply_library_layout(self, compact):
        """Show the mobile ListBox or the desktop FlowBox grid for each section.

        The chosen mode comes from an explicit user pref ("list"/"grid") if
        set, otherwise auto-selects based on compact state."""
        user_mode = _get_library_view_mode_pref()
        if user_mode == "list":
            show_grid = False
        elif user_mode == "grid":
            show_grid = True
        else:
            show_grid = not compact

        pairs = [
            (getattr(self, "playlists_list", None), getattr(self, "playlists_grid", None)),
            (getattr(self, "albums_list", None), getattr(self, "albums_grid", None)),
            (getattr(self, "artists_list", None), getattr(self, "artists_grid", None)),
        ]
        for list_w, grid_w in pairs:
            if list_w is not None:
                list_w.set_visible(not show_grid)
            if grid_w is not None:
                grid_w.set_visible(show_grid)
        # Mirror into Uploads so the pref covers both sub-tabs.
        if hasattr(self, "uploads_page") and hasattr(self.uploads_page, "_apply_layout_pref"):
            self.uploads_page._apply_layout_pref()
        self._update_section_visibility()

    def _after_data_update(self):
        """Run after any update_* populates widgets — re-apply filter and
        recompute section visibility. Idle-scheduled so list/grid mutations
        settle first."""
        self._apply_library_filter()
        self._update_section_visibility()
        return False  # one-shot idle

    # ── Desktop grid ──────────────────────────────────────────────────────

    def _clear_flowbox(self, flowbox):
        child = flowbox.get_first_child()
        while child:
            nxt = child.get_next_sibling()
            flowbox.remove(child)
            child = nxt

    def _make_card_base(self, title, subtitle, thumb_url, fallback_icon):
        return _make_library_card(self.player, title, subtitle, thumb_url, fallback_icon)

    @staticmethod
    def _build_overlay_loader(text):
        """Centered spinner + caption label, hidden by default. Used as
        Gtk.Overlay children that float above the scrolled content so
        they always land mid-viewport regardless of content height."""
        wrap = Gtk.Box(orientation=Gtk.Orientation.VERTICAL, spacing=12)
        wrap.set_vexpand(True)
        wrap.set_valign(Gtk.Align.CENTER)
        wrap.set_halign(Gtk.Align.CENTER)
        spinner = Adw.Spinner()
        spinner.set_size_request(48, 48)
        wrap.append(spinner)
        lbl = Gtk.Label(label=text)
        lbl.add_css_class("caption")
        wrap.append(lbl)
        wrap.set_visible(False)
        return wrap

    def _attach_right_click(self, widget, handler):
        """Wire right-click + long-press on a FlowBoxChild so its
        context menu matches what list-view rows get."""
        click = Gtk.GestureClick()
        click.set_button(3)
        click.connect("released", handler, widget)
        widget.add_controller(click)

        lp = Gtk.GestureLongPress()
        lp.connect(
            "pressed",
            lambda g, x, y, w=widget: handler(g, 1, x, y, w),
        )
        widget.add_controller(lp)

    @staticmethod
    def _index_flowbox(flowbox, id_attr):
        """Map stored id → existing FlowBoxChild so we can update rather than
        recreate on subsequent refreshes. Avoids the placeholder-icon flash
        when returning to the Library after visiting a playlist."""
        index = {}
        child = flowbox.get_first_child()
        while child:
            key = getattr(child, id_attr, None)
            if key:
                index[key] = child
            child = child.get_next_sibling()
        return index

    @staticmethod
    def _update_card_text(card, title, subtitle):
        """Update an existing card's title + subtitle labels in place."""
        outer = card.get_child()
        if not outer:
            return
        # children: wrapper (card-wrapped image), title_clamp, [subtitle_clamp]
        clamp = outer.get_first_child()
        if clamp is None:
            return
        clamp = clamp.get_next_sibling()  # skip image wrapper
        if clamp is not None:
            lbl = clamp.get_child() if hasattr(clamp, "get_child") else None
            if isinstance(lbl, Gtk.Label) and lbl.get_label() != title:
                lbl.set_label(title)
                lbl.set_tooltip_text(title)
            card._search_title = title
        sub_clamp = clamp.get_next_sibling() if clamp is not None else None
        if sub_clamp is not None:
            sub_lbl = sub_clamp.get_child() if hasattr(sub_clamp, "get_child") else None
            if isinstance(sub_lbl, Gtk.Label) and sub_lbl.get_label() != (subtitle or ""):
                sub_lbl.set_label(subtitle or "")

    def _reconcile_grid(self, flowbox, items, id_attr, build_card):
        """Reuse existing cards for known ids, build cards for new items,
        remove cards whose ids are gone. `build_card(item) -> FlowBoxChild`
        is called only for genuinely-new items."""
        existing = self._index_flowbox(flowbox, id_attr)
        processed = set()
        desired_order = []

        for item in items:
            card = build_card(item, existing)
            if card is None:
                continue
            desired_order.append(card)
            key = getattr(card, id_attr, None)
            if key:
                processed.add(key)

        # Remove stale cards.
        for key, card in existing.items():
            if key not in processed:
                flowbox.remove(card)

        # Re-order: walk the flowbox and move children that are out of order.
        # FlowBox doesn't have a native reorder, so we remove + re-append for
        # mismatched positions. That's acceptable — only changed slots move.
        for expected_idx, card in enumerate(desired_order):
            actual = flowbox.get_child_at_index(expected_idx)
            if actual is not card:
                if card.get_parent() is flowbox:
                    flowbox.remove(card)
                flowbox.insert(card, expected_idx)

    def _rebuild_playlists_grid(self, playlists):
        from ui.utils import is_online

        offline = not is_online()

        def build(p, existing):
            p_id = p.get("playlistId")
            if not p_id:
                return None
            title = p.get("title", "Unknown")
            count = p.get("count") or p.get("itemCount", "")
            if len(p_id) == 2:
                subtitle = "Automatic Playlist"
            else:
                subtitle = f"{count} songs" if count and "songs" not in str(count) else str(count or "")

            thumbnails = p.get("thumbnails", [])
            thumb_url = thumbnails[-1]["url"] if thumbnails else None
            # Prefer the locally-cached playlist cover for instant render.
            # When a remote URL is available, fire a background refresh
            # of the on-disk copy so edits on YT propagate — the shared
            # helper dedupes so the grid rebuild doesn't spawn duplicate
            # downloads for the same playlist. The local URL carries the
            # file's mtime so a re-downloaded/edited cover changes the URL
            # and repaints this tile instead of reusing the stale pixbuf.
            from ui.utils import (
                save_playlist_cover_async,
                local_playlist_cover_url,
            )

            if not offline and thumb_url:
                save_playlist_cover_async(self.player, title, thumb_url)
            local_url = local_playlist_cover_url(title)
            if local_url:
                thumb_url = local_url

            is_owned = self.client.is_own_playlist(p, playlist_id=p_id)

            card = existing.get(p_id)
            if card is not None:
                self._update_card_text(card, title, subtitle)
                card._playlist_data = p
                card.playlist_title = title
                card.playlist_count = count
                card.is_owned = is_owned
                # Only reload the thumbnail if the URL actually changed,
                # otherwise the current cover stays visible (no placeholder).
                img = getattr(card, "_cover_img", None)
                if img is not None and thumb_url and img.url != thumb_url:
                    img.load_url(thumb_url)
                return card
            card = self._make_card_base(
                title, subtitle, thumb_url, "media-playlist-audio-symbolic"
            )
            card._playlist_id = p_id
            card._playlist_data = p
            # Attributes consumed by on_row_right_click / _confirm_delete.
            card.playlist_id = p_id
            card.playlist_title = title
            card.playlist_count = count
            card.is_owned = is_owned
            self._attach_right_click(card, self.on_row_right_click)
            return card

        self._reconcile_grid(self.playlists_grid, playlists, "_playlist_id", build)

    def _rebuild_albums_grid(self, albums):
        def build(album, existing):
            browse_id = album.get("browseId", "")
            if not browse_id:
                return None
            title = album.get("title", "Unknown")
            artists = album.get("artists", [])
            artist_str = ", ".join(
                a.get("name", "") for a in artists if isinstance(a, dict)
            )
            year = album.get("year", "")
            subtitle_parts = [p for p in [artist_str, str(year) if year else ""] if p]
            subtitle = " • ".join(subtitle_parts)
            thumbnails = album.get("thumbnails", [])
            thumb_url = thumbnails[-1]["url"] if thumbnails else None

            card = existing.get(browse_id)
            if card is not None:
                self._update_card_text(card, title, subtitle)
                card._album_data = album
                card.album_data = album
                img = getattr(card, "_cover_img", None)
                if img is not None and thumb_url and img.url != thumb_url:
                    img.load_url(thumb_url)
                return card
            card = self._make_card_base(
                title, subtitle, thumb_url, "media-optical-symbolic"
            )
            card._album_id = browse_id
            card._album_data = album
            # Attributes consumed by on_album_right_click.
            card.album_id = browse_id
            card.album_data = album
            self._attach_right_click(card, self.on_album_right_click)
            return card

        self._reconcile_grid(self.albums_grid, albums, "_album_id", build)

    def _rebuild_artists_grid(self, artists):
        def build(a, existing):
            a_id = a.get("browseId")
            if not a_id:
                return None
            name = a.get("artist", "Unknown")
            subscribers = a.get("subscribers", "")
            if subscribers and "subscribers" not in subscribers.lower():
                subscribers = f"{subscribers} subscribers"
            thumbnails = a.get("thumbnails", [])
            thumb_url = thumbnails[-1]["url"] if thumbnails else None

            card = existing.get(a_id)
            if card is not None:
                self._update_card_text(card, name, subscribers)
                card._artist_name = name
                img = getattr(card, "_cover_img", None)
                if img is not None and thumb_url and img.url != thumb_url:
                    img.load_url(thumb_url)
                return card
            card = self._make_card_base(
                name, subscribers, thumb_url, "avatar-default-symbolic"
            )
            card._artist_id = a_id
            card._artist_name = name
            return card

        self._reconcile_grid(self.artists_grid, artists, "_artist_id", build)

    def _on_playlist_grid_activated(self, flowbox, child):
        p_id = getattr(child, "_playlist_id", None)
        if not p_id:
            return
        initial_data = {
            "title": getattr(child, "_search_title", None),
            "thumb": child._cover_img.url if hasattr(child, "_cover_img") else None,
        }
        self.open_playlist_callback(p_id, initial_data)

    def _on_album_grid_activated(self, flowbox, child):
        if not hasattr(child, "_album_id"):
            return
        album = getattr(child, "_album_data", {})
        initial_data = {
            "title": album.get("title", ""),
            "thumb": album.get("thumbnails", [{}])[-1].get("url")
            if album.get("thumbnails")
            else None,
        }
        self.open_playlist_callback(child._album_id, initial_data)

    def _on_artist_grid_activated(self, flowbox, child):
        if not hasattr(child, "_artist_id"):
            return
        root = self.get_root()
        if hasattr(root, "open_artist"):
            root.open_artist(child._artist_id, getattr(child, "_artist_name", None))

    def _propagate_compact(self, widget, compact):
        if hasattr(widget, 'set_compact') and hasattr(widget, 'target_size'):
            widget.set_compact(compact)
        child = widget.get_first_child() if hasattr(widget, 'get_first_child') else None
        while child:
            self._propagate_compact(child, compact)
            child = child.get_next_sibling()

    def clear(self):
        """Clears all playlists from the UI."""
        print("Clearing LibraryPage playlists...")
        while row := self.playlists_list.get_row_at_index(0):
            self.playlists_list.remove(row)

    def load_library(self, silent=False):
        """Fetch library data in the background.

        `silent=True` skips the full-screen "loading" overlay (used by the
        manual refresh button — we show a small inline spinner instead)."""
        if self._is_loading:
            return
        self._is_loading = True
        # Show the spinner synchronously, BEFORE the worker thread starts.
        # The library fetch is several seconds; the previous code only
        # revealed the spinner from inside the worker via idle_add, which
        # left a noticeable gap between the click and any visible
        # feedback. We still only flash it on a cold load (the list is
        # empty) so subsequent silent refreshes don't blink.
        if not silent and self.playlists_list.get_row_at_index(0) is None:
            self._loading_wrap.set_visible(True)
        thread = threading.Thread(target=self._fetch_library, args=(silent,))
        thread.daemon = True
        thread.start()

    def _fetch_library(self, silent=False):
        try:
            # Fan the three independent library calls out across threads.
            # Each is an HTTP round-trip (500 ms – 2 s); running them
            # sequentially used to take 1.5–6 s before any UI updated,
            # while there's nothing connecting them — they can finish in
            # parallel and each render as soon as its result lands.
            results = {"playlists": [], "albums": [], "artists": []}

            def _fetch(key, fn, updater):
                try:
                    data = fn() or []
                except Exception as e:
                    print(f"[LIBRARY] {key} fetch failed: {e}")
                    data = []
                results[key] = data
                # Render this section as soon as its own call returns —
                # the user doesn't have to wait for the slowest sibling.
                GObject.idle_add(updater, data)

            threads = [
                threading.Thread(
                    target=_fetch,
                    args=("playlists", self.client.get_library_playlists,
                          self.update_playlists),
                    daemon=True,
                ),
                threading.Thread(
                    target=_fetch,
                    args=("albums", self.client.get_library_albums,
                          self.update_albums),
                    daemon=True,
                ),
                threading.Thread(
                    target=_fetch,
                    args=("artists", self.client.get_library_subscriptions,
                          self.update_artists),
                    daemon=True,
                ),
            ]
            for t in threads:
                t.start()
            for t in threads:
                t.join()

            GLib.idle_add(self._loading_wrap.set_visible, False)
            GLib.idle_add(self._apply_offline_state)
        finally:
            self._is_loading = False
            GLib.idle_add(self._on_refresh_finished)

    def _on_refresh_finished(self):
        # Notify the MainWindow-owned refresh button (in the header bar)
        # so it can hide its inline spinner and re-enable itself.
        root = self.get_root()
        if root and hasattr(root, "_on_library_refresh_finished"):
            root._on_library_refresh_finished()
        return False

    def update_playlists(self, playlists):
        # Sort: 2-letter IDs first (Automatic Playlists like LM, SE, etc.)
        def sort_key(p):
            pid = p.get("playlistId", "")
            return 0 if len(pid) == 2 else 1

        playlists.sort(key=sort_key)
        self._rebuild_playlists_grid(playlists)
        # Re-run filter + section visibility in case we came from a search state.
        GLib.idle_add(self._after_data_update)

        # 1. Map existing rows by playlist_id
        existing_rows = {}
        row = self.playlists_list.get_row_at_index(0)
        # ... (mapping logic remains same, but we can't easily skip lines in replacement without copying)
        # Let's just copy the mapping part briefly or assume it exists if I don't change it?
        # No, I must provide contiguous block.

        while row:
            if hasattr(row, "playlist_id"):
                existing_rows[row.playlist_id] = row
            row = row.get_next_sibling()

        processed_ids = set()

        for i, p in enumerate(playlists):
            p_id = p.get("playlistId")
            title = p.get("title", "Unknown")
            count = p.get("count")
            if not count:
                count = p.get("itemCount", "")

            thumbnails = p.get("thumbnails", [])
            thumb_url = thumbnails[-1]["url"] if thumbnails else None

            # Use locally saved playlist cover when offline
            from ui.utils import is_online, local_playlist_cover_url
            if not is_online():
                local_url = local_playlist_cover_url(title)
                if local_url:
                    thumb_url = local_url

            processed_ids.add(p_id)

            # Subtitle Logic
            subtitle = ""
            if len(p_id) == 2:
                subtitle = "Automatic Playlist"
                if count:
                    c_str = str(count)
                    if "songs" not in c_str:
                        c_str += " songs"
                    subtitle += f" • {c_str}"
            elif count:
                subtitle = f"{count} songs" if "songs" not in str(count) else str(count)

            row = existing_rows.get(p_id)

            if row:
                # Update existing
                box = row.get_child()
                if row.playlist_title != title:
                    row.playlist_title = title
                    box._title_label.set_label(title)

                box._subtitle_label.set_label(subtitle)
                row.playlist_count = count  # store raw count
                row.is_owned = self.client.is_own_playlist(p, playlist_id=p_id)

                # Image
                if hasattr(row, "cover_img"):
                    if row.cover_img.url != thumb_url:
                        row.cover_img.load_url(thumb_url)

                # Reordering
                current_idx = row.get_index()
                if current_idx != i:
                    self.playlists_list.remove(row)
                    self.playlists_list.insert(row, i)

            else:
                # Create New
                row = Gtk.ListBoxRow()
                box = Gtk.Box(orientation=Gtk.Orientation.HORIZONTAL, spacing=12)
                box.add_css_class("song-row")
                row.set_child(box)

                from ui.utils import AsyncPicture

                img = AsyncPicture(
                    url=thumb_url,
                    target_size=56,
                    crop_to_square=True,
                    player=self.player,
                )
                img.add_css_class("song-img")
                root = self.get_root()
                img.set_compact(getattr(root, '_is_compact', False) if root else False)
                if not thumb_url:
                    img.set_from_icon_name("media-playlist-audio-symbolic")

                box.append(img)
                row.cover_img = img

                vbox = Gtk.Box(orientation=Gtk.Orientation.VERTICAL, spacing=2)
                vbox.set_valign(Gtk.Align.CENTER)
                vbox.set_hexpand(True)

                title_label = Gtk.Label(label=title)
                title_label.set_halign(Gtk.Align.START)
                title_label.set_ellipsize(Pango.EllipsizeMode.END)
                title_label.set_lines(1)
                box._title_label = title_label

                subtitle_label = Gtk.Label(label=subtitle)
                subtitle_label.set_halign(Gtk.Align.START)
                subtitle_label.set_ellipsize(Pango.EllipsizeMode.END)
                subtitle_label.set_lines(1)
                subtitle_label.add_css_class("dim-label")
                subtitle_label.add_css_class("caption")
                box._subtitle_label = subtitle_label

                vbox.append(title_label)
                vbox.append(subtitle_label)
                box.append(vbox)

                row.playlist_id = p_id
                row.playlist_title = title
                row.playlist_count = count
                row.is_owned = self.client.is_own_playlist(p, playlist_id=p_id)
                row.set_activatable(True)

                # Context Menu
                gesture = Gtk.GestureClick()
                gesture.set_button(3)
                gesture.connect("released", self.on_row_right_click, row)
                row.add_controller(gesture)

                # Long Press for touch
                lp = Gtk.GestureLongPress()
                lp.connect(
                    "pressed",
                    lambda g, x, y, ws=weakref.ref(self): ws().on_row_right_click(g, 1, x, y, g.get_widget()) if ws() else None,
                )
                row.add_controller(lp)

                self.playlists_list.insert(row, i)

        # Identify and remove stale rows (those in existing_rows but not in processed_ids).
        # Moved widgets are kept safe by processed_ids check.
        for p_id, row in existing_rows.items():
            if p_id not in processed_ids:
                self.playlists_list.remove(row)

    def update_albums(self, albums):
        self._rebuild_albums_grid(albums)
        GLib.idle_add(self._after_data_update)
        # Map existing rows
        existing_rows = {}
        row = self.albums_list.get_row_at_index(0)
        while row:
            if hasattr(row, "album_id"):
                existing_rows[row.album_id] = row
            row = row.get_next_sibling()

        processed_ids = set()

        for i, album in enumerate(albums):
            browse_id = album.get("browseId", "")
            title = album.get("title", "Unknown")
            artists = album.get("artists", [])
            artist_str = ", ".join(a.get("name", "") for a in artists if isinstance(a, dict))
            year = album.get("year", "")
            album_type = album.get("type", "Album")

            subtitle_parts = []
            if artist_str:
                subtitle_parts.append(artist_str)
            if album_type:
                subtitle_parts.append(album_type)
            if year:
                subtitle_parts.append(str(year))
            subtitle = " • ".join(subtitle_parts)

            thumbnails = album.get("thumbnails", [])
            thumb_url = thumbnails[-1]["url"] if thumbnails else None

            processed_ids.add(browse_id)

            row = existing_rows.get(browse_id)

            if row:
                box = row.get_child()
                box._title_label.set_label(title)
                box._subtitle_label.set_label(subtitle)
                if hasattr(row, "cover_img") and row.cover_img.url != thumb_url:
                    row.cover_img.load_url(thumb_url)
                current_idx = row.get_index()
                if current_idx != i:
                    self.albums_list.remove(row)
                    self.albums_list.insert(row, i)
            else:
                row = Gtk.ListBoxRow()
                box = Gtk.Box(orientation=Gtk.Orientation.HORIZONTAL, spacing=12)
                box.add_css_class("song-row")
                row.set_child(box)

                from ui.utils import AsyncPicture

                img = AsyncPicture(
                    url=thumb_url,
                    target_size=56,
                    crop_to_square=True,
                    player=self.player,
                )
                img.add_css_class("song-img")
                root = self.get_root()
                img.set_compact(getattr(root, '_is_compact', False) if root else False)
                if not thumb_url:
                    img.set_from_icon_name("media-optical-symbolic")

                box.append(img)
                row.cover_img = img

                vbox = Gtk.Box(orientation=Gtk.Orientation.VERTICAL, spacing=2)
                vbox.set_valign(Gtk.Align.CENTER)
                vbox.set_hexpand(True)

                title_label = Gtk.Label(label=title)
                title_label.set_halign(Gtk.Align.START)
                title_label.set_ellipsize(Pango.EllipsizeMode.END)
                title_label.set_lines(1)
                box._title_label = title_label

                subtitle_label = Gtk.Label(label=subtitle)
                subtitle_label.set_halign(Gtk.Align.START)
                subtitle_label.set_ellipsize(Pango.EllipsizeMode.END)
                subtitle_label.set_lines(1)
                subtitle_label.add_css_class("dim-label")
                subtitle_label.add_css_class("caption")
                subtitle_label.set_visible(bool(subtitle))
                box._subtitle_label = subtitle_label

                vbox.append(title_label)
                vbox.append(subtitle_label)
                box.append(vbox)

                row.album_id = browse_id
                row.album_data = album
                row.set_activatable(True)

                # Context Menu
                gesture = Gtk.GestureClick()
                gesture.set_button(3)
                gesture.connect("released", self.on_album_right_click, row)
                row.add_controller(gesture)

                lp = Gtk.GestureLongPress()
                lp.connect(
                    "pressed",
                    lambda g, x, y, ws=weakref.ref(self): ws().on_album_right_click(g, 1, x, y, g.get_widget()) if ws() else None,
                )
                row.add_controller(lp)

                self.albums_list.insert(row, i)

        for aid, row in existing_rows.items():
            if aid not in processed_ids:
                self.albums_list.remove(row)

    def on_album_activated(self, listbox, row):
        if hasattr(row, "album_id"):
            album = getattr(row, "album_data", {})
            initial_data = {
                "title": album.get("title", ""),
                "thumb": album.get("thumbnails", [{}])[-1].get("url") if album.get("thumbnails") else None,
            }
            self.open_playlist_callback(row.album_id, initial_data)

    def on_album_right_click(self, gesture, n_press, x, y, row):
        if not hasattr(row, "album_id"):
            return

        browse_id = row.album_id
        album = getattr(row, "album_data", {})
        audio_pid = album.get("audioPlaylistId", "")

        group = Gio.SimpleActionGroup()
        row.insert_action_group("row", group)

        menu = Gio.Menu()

        # Copy Link
        if audio_pid:
            link = f"https://music.youtube.com/playlist?list={audio_pid}"
        else:
            link = f"https://music.youtube.com/browse/{browse_id}"

        action_copy = Gio.SimpleAction.new("copy_link", None)
        action_copy.connect("activate", lambda a, p, u=link: (
            Gdk.Display.get_default().get_clipboard().set(u)
        ))
        group.add_action(action_copy)
        menu.append("Copy Link", "row.copy_link")

        # Remove from Library
        action_unsave = Gio.SimpleAction.new("unsave", None)
        def _unsave(a, p, pid=audio_pid or browse_id):
            def _thread():
                success = self.client.rate_playlist(pid, "INDIFFERENT")
                if success:
                    GLib.idle_add(self.load_library)
            threading.Thread(target=_thread, daemon=True).start()
        action_unsave.connect("activate", _unsave)
        group.add_action(action_unsave)
        menu.append("Remove from Library", "row.unsave")

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

    def _open_downloads_page(self):
        """Open a pseudo-playlist with all downloaded songs."""
        nav = None
        parent = self.get_parent()
        while parent:
            if isinstance(parent, Adw.NavigationView):
                nav = parent
                break
            parent = parent.get_parent()
        if not nav:
            return

        from ui.pages.playlist import PlaylistPage
        page = PlaylistPage(self.player)
        page.playlist_id = "DOWNLOADS"
        page.is_fully_loaded = True
        page.is_fully_fetched = True

        root = self.get_root()
        if root and getattr(root, '_is_compact', False):
            page.set_compact_mode(True)

        nav_page = Adw.NavigationPage(child=page, title="Downloaded Songs")
        nav.push(nav_page)
        page.stack.set_visible_child_name("loading")

        def _fetch():
            from player.downloads import get_download_db
            db = get_download_db()
            downloads = db.get_all_downloads()
            tracks = []
            for d in downloads:
                t = {
                    "videoId": d.get("video_id"),
                    "title": d.get("title", "Unknown"),
                    "artists": [{"name": d.get("artist", ""), "id": None}] if d.get("artist") else [],
                    "album": {"name": d.get("album", "")},
                    "duration_seconds": d.get("duration_seconds", 0),
                    "thumbnails": [{"url": d.get("thumbnail_url")}] if d.get("thumbnail_url") else [],
                }
                dur = d.get("duration_seconds", 0)
                if dur:
                    t["duration"] = f"{dur // 60}:{dur % 60:02d}"
                tracks.append(t)

            GLib.idle_add(self._show_downloads_page, page, tracks)

        threading.Thread(target=_fetch, daemon=True).start()

    def _show_downloads_page(self, page, tracks):
        page.original_tracks = tracks
        page.current_tracks = tracks

        total_seconds = sum(t.get("duration_seconds", 0) for t in tracks)
        hours = total_seconds // 3600
        minutes = (total_seconds % 3600) // 60
        dur = f"{hours} hr {minutes} min" if hours > 0 else f"{minutes} min"

        page.update_ui(
            title="Downloaded Songs",
            description="",
            meta1=f"{len(tracks)} songs available offline",
            meta2=dur,
            thumbnails=tracks[0].get("thumbnails", []) if tracks else [],
            tracks=tracks,
        )

    def _open_history_page(self):
        """Push the dedicated HistoryPage (grouped by when each track
        was played, mirroring YT Music's Verlauf view)."""
        from ui.utils import is_online

        def _toast(msg):
            root = self.get_root()
            if root and hasattr(root, "add_toast"):
                root.add_toast(msg)

        if not is_online():
            _toast("History requires an internet connection")
            return
        if not self.player.client.is_authenticated():
            _toast("Sign in to view listening history")
            return

        nav = None
        parent = self.get_parent()
        while parent:
            if isinstance(parent, Adw.NavigationView):
                nav = parent
                break
            parent = parent.get_parent()
        if not nav:
            return

        from ui.pages.history import HistoryPage
        page = HistoryPage(self.player)

        root = self.get_root()
        if root and getattr(root, "_is_compact", False):
            page.set_compact_mode(True)

        nav_page = Adw.NavigationPage(child=page, title="Listening History")
        nav.push(nav_page)
        page.load()

    def _on_upload_clicked(self, btn):
        # Call uploads page but pass our root window since uploads tab might not be visible
        self.uploads_page._do_open_file_picker(self.get_root())

    def _on_tab_changed(self, stack, param):
        is_uploads = stack.get_visible_child_name() == "uploads"
        self.uploads_actions_box.set_visible(is_uploads)
        self.lib_actions_box.set_visible(not is_uploads)
        # Sync the uploads overlay visibility to the tab: if an upload
        # fetch is in flight and the user switches to this tab, reveal
        # the spinner; if they switch away, hide it so it doesn't show
        # on top of the library content.
        self._uploads_loading_wrap.set_visible(
            is_uploads and self._uploads_loading_pending
        )
        if is_uploads:
            # Refresh uploads every time the tab is revealed
            self.uploads_page.load()

    def on_row_right_click(self, gesture, n_press, x, y, row):
        if not hasattr(row, "playlist_id"):
            return

        pid = row.playlist_id
        # Determine URL
        url = f"https://music.youtube.com/playlist?list={pid}"

        group = Gio.SimpleActionGroup()
        row.insert_action_group("row", group)

        def copy_link_action(action, param):
            try:
                clipboard = Gdk.Display.get_default().get_clipboard()
                clipboard.set(url)
                root = self.get_root()
                if root and hasattr(root, "add_toast"):
                    root.add_toast("Link copied")
            except Exception:
                pass

        action_copy = Gio.SimpleAction.new("copy_link", None)
        action_copy.connect("activate", copy_link_action)
        group.add_action(action_copy)

        menu = Gio.Menu()
        menu.append("Copy Link", "row.copy_link")

        popover = Gtk.PopoverMenu.new_from_model(menu)
        popover.set_parent(row)
        popover.set_has_arrow(False)

        rect = Gdk.Rectangle()
        rect.x = int(x)
        rect.y = int(y)
        rect.width = 1
        rect.height = 1
        popover.set_pointing_to(rect)

        is_owned = getattr(row, "is_owned", False)
        if is_owned:
            menu.append("Delete Playlist", "row.delete_playlist")

            def delete_action(action, param):
                self._confirm_delete_playlist(row)

            action_delete = Gio.SimpleAction.new("delete_playlist", None)
            action_delete.connect("activate", delete_action)
            group.add_action(action_delete)
        else:
            # Non-owned playlists can be removed from library
            menu.append("Remove from Library", "row.unsave")

            def unsave_action(action, param):
                def _thread():
                    success = self.client.rate_playlist(pid, "INDIFFERENT")
                    if success:
                        GLib.idle_add(self.load_library)
                threading.Thread(target=_thread, daemon=True).start()

            action_unsave = Gio.SimpleAction.new("unsave", None)
            action_unsave.connect("activate", unsave_action)
            group.add_action(action_unsave)

        popover.popup()

    def _confirm_delete_playlist(self, row):
        dialog = Adw.MessageDialog(
            transient_for=self.get_root(),
            heading="Delete Playlist?",
            body=f'Are you sure you want to delete "{row.playlist_title}"?\nThis action cannot be undone.',
        )
        dialog.add_response("cancel", "Cancel")
        dialog.add_response("delete", "Delete")
        dialog.set_response_appearance("delete", Adw.ResponseAppearance.DESTRUCTIVE)
        dialog.set_default_response("cancel")
        dialog.set_close_response("cancel")

        def on_response(dg, response_id):
            if response_id == "delete":
                self._delete_playlist_confirmed(row)
            dg.destroy()

        dialog.connect("response", on_response)
        dialog.present()

    def _delete_playlist_confirmed(self, row):
        def thread_func():
            success = self.client.delete_playlist(row.playlist_id)
            if success:
                print(f"Playlist {row.playlist_id} deleted successfully.")
                GLib.idle_add(self.load_library)
            else:
                print(f"Failed to delete playlist {row.playlist_id}")

        threading.Thread(target=thread_func, daemon=True).start()

    def on_new_playlist_clicked(self, btn):
        dialog = Adw.Dialog()
        dialog.set_title("New Playlist")
        dialog.set_content_width(500)

        main_box = Gtk.Box(orientation=Gtk.Orientation.VERTICAL)
        dialog.set_child(main_box)

        header = Adw.HeaderBar()
        header.add_css_class("flat")
        main_box.append(header)

        create_btn = Gtk.Button(label="Create")
        create_btn.add_css_class("suggested-action")
        header.pack_start(create_btn)

        page = Adw.PreferencesPage()
        main_box.append(page)

        group = Adw.PreferencesGroup(title="Playlist Details")
        group.set_margin_start(12)
        group.set_margin_end(12)
        group.set_margin_top(12)
        group.set_margin_bottom(12)
        page.add(group)

        # Title
        title_row = Adw.EntryRow(title="Title")
        title_row.set_activates_default(True)
        group.add(title_row)

        # Description
        desc_row = Adw.EntryRow(title="Description")
        group.add(desc_row)

        # Privacy
        privacy_row = Adw.ComboRow(title="Visibility")
        privacy_options = ["Public", "Private", "Unlisted"]
        privacy_model = Gtk.StringList.new(privacy_options)
        privacy_row.set_model(privacy_model)
        privacy_row.set_selected(1)  # Private by default
        group.add(privacy_row)

        def on_create_clicked(button):
            title = title_row.get_text().strip()
            if not title:
                return

            description = desc_row.get_text().strip()
            privacy_idx = privacy_row.get_selected()
            privacy_status = ["PUBLIC", "PRIVATE", "UNLISTED"][privacy_idx]

            self._create_playlist_confirmed(title, description, privacy_status)
            dialog.close()

        create_btn.connect("clicked", on_create_clicked)
        dialog.present(self.get_root())
        title_row.grab_focus()

    def _create_playlist_confirmed(self, title, description, privacy_status):
        def thread_func():
            print(f"Creating playlist: {title}")
            playlist_id = self.client.create_playlist(
                title, description=description, privacy_status=privacy_status
            )

            if playlist_id:
                print(f"Playlist created successfully: {playlist_id}")
                # 1. Refresh library in background
                GLib.idle_add(self.load_library)

                # 2. Navigate to the new playlist immediately
                GLib.idle_add(
                    self.open_playlist_callback,
                    playlist_id,
                    {"title": title, "author": "You"},
                )
            else:
                print("Failed to create playlist.")

        threading.Thread(target=thread_func, daemon=True).start()

    def update_artists(self, artists):
        self._rebuild_artists_grid(artists)
        GLib.idle_add(self._after_data_update)
        # 1. Map existing rows by browse_id
        existing_rows = {}
        row = self.artists_list.get_row_at_index(0)
        while row:
            if hasattr(row, "artist_id"):
                existing_rows[row.artist_id] = row
            row = row.get_next_sibling()

        processed_ids = set()

        for i, a in enumerate(artists):
            a_id = a.get("browseId")
            name = a.get("artist", "Unknown")
            subscribers = a.get("subscribers", "")
            if subscribers and "subscribers" not in subscribers.lower():
                subscribers = f"{subscribers} subscribers"

            thumbnails = a.get("thumbnails", [])
            thumb_url = thumbnails[-1]["url"] if thumbnails else None

            processed_ids.add(a_id)

            row = existing_rows.get(a_id)

            if row:
                # Update existing
                box = row.get_child()
                if row.artist_name != name:
                    row.artist_name = name
                    box._title_label.set_label(name)

                box._subtitle_label.set_label(subscribers)

                # Image
                if hasattr(row, "cover_img"):
                    if row.cover_img.url != thumb_url:
                        row.cover_img.load_url(thumb_url)

                # Reordering
                current_idx = row.get_index()
                if current_idx != i:
                    self.artists_list.remove(row)
                    self.artists_list.insert(row, i)

            else:
                # Create New
                row = Gtk.ListBoxRow()
                box = Gtk.Box(orientation=Gtk.Orientation.HORIZONTAL, spacing=12)
                box.add_css_class("song-row")
                row.set_child(box)

                from ui.utils import AsyncPicture

                img = AsyncPicture(
                    url=thumb_url,
                    target_size=56,
                    crop_to_square=True,
                    player=self.player,
                )
                img.add_css_class("song-img")
                root = self.get_root()
                img.set_compact(getattr(root, '_is_compact', False) if root else False)
                if not thumb_url:
                    img.set_from_icon_name("avatar-default-symbolic")

                box.append(img)
                row.cover_img = img

                vbox = Gtk.Box(orientation=Gtk.Orientation.VERTICAL, spacing=2)
                vbox.set_valign(Gtk.Align.CENTER)
                vbox.set_hexpand(True)

                title_label = Gtk.Label(label=name)
                title_label.set_halign(Gtk.Align.START)
                title_label.set_ellipsize(Pango.EllipsizeMode.END)
                title_label.set_lines(1)
                box._title_label = title_label

                subtitle_label = Gtk.Label(label=subscribers)
                subtitle_label.set_halign(Gtk.Align.START)
                subtitle_label.set_ellipsize(Pango.EllipsizeMode.END)
                subtitle_label.set_lines(1)
                subtitle_label.add_css_class("dim-label")
                subtitle_label.add_css_class("caption")
                box._subtitle_label = subtitle_label

                vbox.append(title_label)
                vbox.append(subtitle_label)
                box.append(vbox)

                row.artist_id = a_id
                row.artist_name = name
                row.set_activatable(True)

                self.artists_list.insert(row, i)

        # Remove stale
        for a_id, row in existing_rows.items():
            if a_id not in processed_ids:
                self.artists_list.remove(row)

    def on_artist_activated(self, box, row):
        if hasattr(row, "artist_id"):
            # The MainWindow has open_artist, but here we only have open_playlist_callback.
            # However, open_playlist_callback in MainWindow.init_pages is bound to self.open_playlist.
            # We might need a separate callback for artists or use the root window.
            root = self.get_root()
            if hasattr(root, "open_artist"):
                root.open_artist(row.artist_id, row.artist_name)

    def _apply_offline_state(self):
        """Grey out items that are unavailable offline."""
        from ui.utils import is_online
        if is_online():
            # Re-enable everything
            for listbox in [self.playlists_list, self.albums_list, self.artists_list]:
                row = listbox.get_row_at_index(0)
                while row:
                    row.set_sensitive(True)
                    row.set_opacity(1.0)
                    row = row.get_next_sibling()
            return

        from player.downloads import get_download_db
        db = get_download_db()

        # Grey out playlists without cached data
        row = self.playlists_list.get_row_at_index(0)
        while row:
            pid = getattr(row, "playlist_id", None)
            if pid:
                cached = db.get_cached_playlist(pid)
                has_data = cached and cached.get("tracks")
                row.set_sensitive(bool(has_data))
                row.set_opacity(1.0 if has_data else 0.4)
            row = row.get_next_sibling()

        # Grey out albums without cached data
        row = self.albums_list.get_row_at_index(0)
        while row:
            bid = getattr(row, "album_id", None)
            if bid:
                cached = db.get_cached_playlist(bid)
                has_data = cached and cached.get("tracks")
                row.set_sensitive(bool(has_data))
                row.set_opacity(1.0 if has_data else 0.4)
            row = row.get_next_sibling()

        # Grey out all artists when offline (can't load artist pages)
        row = self.artists_list.get_row_at_index(0)
        while row:
            row.set_sensitive(False)
            row.set_opacity(0.4)
            row = row.get_next_sibling()

    def on_playlist_activated(self, box, row):
        if hasattr(row, "playlist_id"):
            initial_data = {
                "title": getattr(row, "playlist_title", None),
                "thumb": row.cover_img.url if hasattr(row, "cover_img") else None,
            }
            self.open_playlist_callback(row.playlist_id, initial_data)

    def on_player_state_changed(self, player, state):
        pass  # Not used currently for playlist list


class UploadsPage(Gtk.Box):
    """Sub-page showing uploaded albums from the user's YouTube Music library."""

    def __init__(self, player, client, open_playlist_callback):
        super().__init__(orientation=Gtk.Orientation.VERTICAL, spacing=0)
        self.player = player
        self.client = client
        self.open_playlist_callback = open_playlist_callback
        # LibraryPage owns the loading spinner for this tab — it floats
        # on a main-box-level Gtk.Overlay that reliably centres in the
        # viewport, which the deeper Uploads-internal positioning
        # couldn't achieve.
        self._set_loading_cb = None

        # Content page
        self.content_box = Gtk.Box(orientation=Gtk.Orientation.VERTICAL, spacing=24)
        self.content_box.set_vexpand(True)
        self.append(self.content_box)

        # Albums section
        self.albums_section = Gtk.Box(orientation=Gtk.Orientation.VERTICAL, spacing=8)
        albums_label = Gtk.Label(label="Albums")
        albums_label.add_css_class("heading")
        albums_label.set_halign(Gtk.Align.START)
        self.albums_section.append(albums_label)

        self.albums_list = Gtk.ListBox()
        self.albums_list.set_selection_mode(Gtk.SelectionMode.NONE)
        self.albums_list.add_css_class("boxed-list")
        self.albums_list.connect("row-activated", self._on_album_activated)
        self.albums_section.append(self.albums_list)

        self.albums_grid = _make_flow_grid()
        self.albums_grid.connect("child-activated", self._on_album_grid_activated)
        self.albums_section.append(self.albums_grid)
        self.content_box.append(self.albums_section)

        # Artists section
        self.artists_section = Gtk.Box(orientation=Gtk.Orientation.VERTICAL, spacing=8)
        artists_label = Gtk.Label(label="Artists")
        artists_label.add_css_class("heading")
        artists_label.set_halign(Gtk.Align.START)
        self.artists_section.append(artists_label)

        self.artists_list = Gtk.ListBox()
        self.artists_list.set_selection_mode(Gtk.SelectionMode.NONE)
        self.artists_list.add_css_class("boxed-list")
        self.artists_list.connect("row-activated", self._on_artist_activated)
        self.artists_section.append(self.artists_list)

        self.artists_grid = _make_flow_grid()
        self.artists_grid.connect("child-activated", self._on_artist_grid_activated)
        self.artists_section.append(self.artists_grid)
        self.content_box.append(self.artists_section)

        # Filter state driven by parent LibraryPage.filter_content().
        self.current_filter_text = ""

        self.empty_label = Gtk.Label(label="No uploaded music")
        self.empty_label.add_css_class("dim-label")
        self.empty_label.set_visible(False)
        self.content_box.append(self.empty_label)

        # Initial loading state is owned by the parent LibraryPage; it
        # flips the overlay's visibility via `set_loading_cb(...)`.


    def set_loading_cb(self, cb):
        """Wire a callback that flips the parent's overlay spinner."""
        self._set_loading_cb = cb

    def _set_loading(self, loading):
        if self._set_loading_cb:
            self._set_loading_cb(loading)

    def set_compact_mode(self, compact):
        if compact:
            self.add_css_class("compact")
            self.content_box.set_spacing(16)
        else:
            self.remove_css_class("compact")
            self.content_box.set_spacing(24)
        self._propagate_compact(self.content_box, compact)

    def _propagate_compact(self, widget, compact):
        if hasattr(widget, 'set_compact') and hasattr(widget, 'target_size'):
            widget.set_compact(compact)
        child = widget.get_first_child() if hasattr(widget, 'get_first_child') else None
        while child:
            self._propagate_compact(child, compact)
            child = child.get_next_sibling()

    def filter_content(self, text):
        """Called by LibraryPage.filter_content when the global search bar
        types on the Library tab. Filters uploaded albums/artists by title
        and hides sections that end up empty."""
        self.current_filter_text = (text or "").strip().lower()
        query = self.current_filter_text

        def matches(title):
            return (not query) or (query in (title or "").lower())

        row = self.albums_list.get_row_at_index(0)
        while row:
            title = getattr(row, "album_title", None)
            if title is None and hasattr(row, "album_data"):
                title = row.album_data.get("title", "")
            row.set_visible(matches(title))
            row = row.get_next_sibling()

        row = self.artists_list.get_row_at_index(0)
        while row:
            name = ""
            data = getattr(row, "artist_data", None)
            if data:
                name = data.get("artist") or data.get("name") or ""
            if not name:
                name = getattr(row, "artist_name", "")
            row.set_visible(matches(name))
            row = row.get_next_sibling()

        self._update_section_visibility()

    def _update_section_visibility(self):
        def any_visible(container):
            child = container.get_row_at_index(0)
            while child:
                if child.get_visible():
                    return True
                child = child.get_next_sibling()
            return False

        self.albums_section.set_visible(any_visible(self.albums_list))
        self.artists_section.set_visible(any_visible(self.artists_list))

    def load(self):
        from ui.utils import is_online
        from player.downloads import get_download_db

        # Optimistic render from the disk cache so Uploads opens instantly
        # just like the main Library sections do. The live fetch happens
        # in the background and replaces the rows when done.
        has_content = (
            self.albums_list.get_row_at_index(0) is not None
            or self.artists_list.get_row_at_index(0) is not None
        )
        if not has_content:
            try:
                cached_albums, cached_artists = get_download_db().get_cached_uploads()
            except Exception:
                cached_albums, cached_artists = None, None
            if cached_albums or cached_artists:
                self._display(cached_albums or [], cached_artists or [])
                has_content = True

        if not is_online():
            if not has_content:
                self._set_loading(False)
                self.empty_label.set_label("Uploads require an internet connection")
                self.empty_label.set_visible(True)
            return
        if not has_content:
            self._set_loading(True)
        self.empty_label.set_visible(False)
        threading.Thread(target=self._fetch, daemon=True).start()

    def _fetch(self):
        albums = self.client.get_library_upload_albums(limit=100)
        artists = self.client.get_library_upload_artists(limit=100)
        # Persist for the next launch's optimistic render.
        try:
            from player.downloads import get_download_db

            get_download_db().cache_uploads(albums or [], artists or [])
        except Exception as e:
            print(f"[UPLOADS] cache write failed: {e}")
        GLib.idle_add(self._display, albums or [], artists or [])

    def _display(self, albums, artists):
        self._set_loading(False)

        # Clear existing
        while row := self.albums_list.get_row_at_index(0):
            self.albums_list.remove(row)
        while row := self.artists_list.get_row_at_index(0):
            self.artists_list.remove(row)

        if not albums and not artists:
            self.empty_label.set_visible(True)
            self._update_section_visibility()
            return

        self.empty_label.set_visible(False)
        self._display_artists(artists)
        self._display_albums(albums)
        # Rebuild the desktop grid counterparts from the same data.
        self._rebuild_uploads_grids(albums, artists)
        # Honour the current library layout pref + active filter.
        self._apply_layout_pref()
        if self.current_filter_text:
            self.filter_content(self.current_filter_text)
        else:
            self._update_section_visibility()

    def _apply_layout_pref(self):
        """Show ListBox or FlowBox based on the shared library pref."""
        mode = _get_library_view_mode_pref()
        root = self.get_root()
        compact = bool(getattr(root, "_is_compact", False)) if root else False
        if mode == "list":
            show_grid = False
        elif mode == "grid":
            show_grid = True
        else:
            show_grid = not compact
        self.albums_list.set_visible(not show_grid)
        self.artists_list.set_visible(not show_grid)
        self.albums_grid.set_visible(show_grid)
        self.artists_grid.set_visible(show_grid)

    def _rebuild_uploads_grids(self, albums, artists):
        for grid in (self.albums_grid, self.artists_grid):
            child = grid.get_first_child()
            while child:
                nxt = child.get_next_sibling()
                grid.remove(child)
                child = nxt

        for album in albums:
            title = album.get("title", "Unknown")
            raw_artists = album.get("artists") or []
            artist_str = ", ".join(
                a.get("name", "") for a in raw_artists if isinstance(a, dict)
            )
            if not artist_str:
                artist_str = album.get("artist", "") or ""
            thumbs = album.get("thumbnails") or []
            thumb_url = thumbs[-1]["url"] if thumbs else None
            card = _make_library_card(
                self.player, title, artist_str, thumb_url, "media-optical-symbolic"
            )
            card._album_data = album
            self.albums_grid.append(card)

        for artist in artists:
            name = artist.get("artist", artist.get("name", "Unknown"))
            song_count = artist.get("songs")
            subtitle = f"{song_count} songs" if song_count else ""
            thumbs = artist.get("thumbnails") or []
            thumb_url = thumbs[-1]["url"] if thumbs else None
            card = _make_library_card(
                self.player, name, subtitle, thumb_url, "avatar-default-symbolic"
            )
            card._artist_data = artist
            self.artists_grid.append(card)

    def _on_album_grid_activated(self, flowbox, child):
        album = getattr(child, "_album_data", None)
        if not album:
            return
        # Defer to the list-row handler's logic.
        browse_id = album.get("browseId")
        if not browse_id:
            return
        self.open_playlist_callback(
            browse_id,
            {
                "title": album.get("title", ""),
                "thumb": (album.get("thumbnails") or [{}])[-1].get("url"),
            },
        )

    def _on_artist_grid_activated(self, flowbox, child):
        artist = getattr(child, "_artist_data", None)
        if not artist:
            return
        browse_id = artist.get("browseId")
        name = artist.get("artist") or artist.get("name")
        if not browse_id:
            return
        root = self.get_root()
        if root and hasattr(root, "open_artist"):
            root.open_artist(browse_id, name)

    def _display_artists(self, artists):
        from ui.utils import AsyncPicture

        for artist in artists:
            name = artist.get("artist", artist.get("name", "Unknown"))
            song_count = artist.get("songs")
            subtitle = f"{song_count} songs" if song_count else ""

            thumbnails = artist.get("thumbnails", [])
            thumb_url = thumbnails[-1]["url"] if thumbnails else None

            row = Gtk.ListBoxRow()
            box = Gtk.Box(orientation=Gtk.Orientation.HORIZONTAL, spacing=12)
            box.add_css_class("song-row")
            row.set_child(box)

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

            title_label = Gtk.Label(label=name)
            title_label.set_halign(Gtk.Align.START)
            title_label.set_ellipsize(Pango.EllipsizeMode.END)
            title_label.set_lines(1)
            vbox.append(title_label)

            if subtitle:
                sub_label = Gtk.Label(label=subtitle)
                sub_label.set_halign(Gtk.Align.START)
                sub_label.add_css_class("dim-label")
                sub_label.add_css_class("caption")
                vbox.append(sub_label)

            box.append(vbox)

            row.artist_data = artist
            row.set_activatable(True)
            self.artists_list.append(row)

    def _display_albums(self, albums):
        from ui.utils import AsyncPicture

        for album in albums:
            title = album.get("title", "Unknown")
            artists = album.get("artists", [])
            artist_str = ", ".join(a.get("name", "") for a in artists if isinstance(a, dict))
            if not artist_str:
                artist_str = album.get("artist", "")
            year = album.get("year", "")

            subtitle_parts = []
            if artist_str:
                subtitle_parts.append(artist_str)
            if year:
                subtitle_parts.append(str(year))
            subtitle = " • ".join(subtitle_parts)

            thumbnails = album.get("thumbnails", [])
            thumb_url = thumbnails[-1]["url"] if thumbnails else None

            row = Gtk.ListBoxRow()
            box = Gtk.Box(orientation=Gtk.Orientation.HORIZONTAL, spacing=12)
            box.add_css_class("song-row")
            row.set_child(box)

            img = AsyncPicture(
                url=thumb_url,
                target_size=56,
                crop_to_square=True,
                player=self.player,
            )
            img.add_css_class("song-img")
            root = self.get_root()
            img.set_compact(getattr(root, '_is_compact', False) if root else False)
            if not thumb_url:
                img.set_from_icon_name("media-optical-symbolic")

            box.append(img)

            vbox = Gtk.Box(orientation=Gtk.Orientation.VERTICAL, spacing=2)
            vbox.set_valign(Gtk.Align.CENTER)
            vbox.set_hexpand(True)

            title_label = Gtk.Label(label=title)
            title_label.set_halign(Gtk.Align.START)
            title_label.set_ellipsize(Pango.EllipsizeMode.END)
            title_label.set_lines(1)

            subtitle_label = Gtk.Label(label=subtitle)
            subtitle_label.set_halign(Gtk.Align.START)
            subtitle_label.set_ellipsize(Pango.EllipsizeMode.END)
            subtitle_label.set_lines(1)
            subtitle_label.add_css_class("dim-label")
            subtitle_label.add_css_class("caption")
            subtitle_label.set_visible(bool(subtitle))

            vbox.append(title_label)
            vbox.append(subtitle_label)
            box.append(vbox)

            row.album_data = album
            row.set_activatable(True)

            # Context menu
            gesture = Gtk.GestureClick()
            gesture.set_button(3)
            gesture.connect("released", self._on_album_right_click, row)
            row.add_controller(gesture)

            lp = Gtk.GestureLongPress()
            lp.connect("pressed", lambda g, x, y, ws=weakref.ref(self): ws()._on_album_right_click(g, 1, x, y, g.get_widget()) if ws() else None)
            row.add_controller(lp)

            self.albums_list.append(row)

    def _on_artist_activated(self, listbox, row):
        if not hasattr(row, "artist_data"):
            return
        artist = row.artist_data
        browse_id = artist.get("browseId")
        name = artist.get("artist", artist.get("name", "Unknown"))
        if not browse_id:
            return

        nav = self._find_nav()
        if not nav:
            return

        from ui.pages.playlist import PlaylistPage
        page = PlaylistPage(self.player)
        page.playlist_id = f"UPLOAD_ARTIST_{browse_id}"
        page.is_fully_loaded = True
        page.is_fully_fetched = True

        root = self.get_root()
        if root and getattr(root, '_is_compact', False):
            page.set_compact_mode(True)

        nav_page = Adw.NavigationPage(child=page, title=name)
        nav.push(nav_page)
        page.stack.set_visible_child_name("loading")

        def _fetch():
            songs = self.client.get_library_upload_artist(browse_id)
            GLib.idle_add(self._populate_songs_page, page, songs or [], name, "Uploaded Artist")

        threading.Thread(target=_fetch, daemon=True).start()

    def _populate_songs_page(self, page, songs, title="Uploaded Songs", meta1="Uploads"):
        tracks = []
        for s in songs:
            t = dict(s)
            if "thumbnail" in t and "thumbnails" not in t:
                t["thumbnails"] = t["thumbnail"]
            tracks.append(t)

        page.original_tracks = tracks
        page.current_tracks = tracks

        total_seconds = sum(t.get("duration_seconds", 0) for t in tracks)
        hours = total_seconds // 3600
        minutes = (total_seconds % 3600) // 60
        duration_str = f"{hours} hr {minutes} min" if hours > 0 else f"{minutes} min"

        page.update_ui(
            title=title,
            description="",
            meta1=meta1,
            meta2=f"{len(tracks)} songs • {duration_str}",
            thumbnails=tracks[0].get("thumbnails", []) if tracks else [],
            tracks=tracks,
        )

        # Apply current compact mode
        root = page.get_root()
        if root and getattr(root, '_is_compact', False):
            page.set_compact_mode(True)

    def _find_nav(self):
        """Walk up the widget tree to find the NavigationView."""
        widget = self
        while widget:
            parent = widget.get_parent()
            if isinstance(parent, Adw.NavigationView):
                return parent
            widget = parent
        return None

    def _open_all_songs(self):
        """Open a pseudo-playlist page immediately, load data in background."""
        nav = self._find_nav()
        if not nav:
            return

        from ui.pages.playlist import PlaylistPage
        page = PlaylistPage(self.player)
        page.playlist_id = "UPLOADS"
        page.is_fully_loaded = True
        page.is_fully_fetched = True

        # Apply compact mode before pushing
        root = self.get_root()
        if root and getattr(root, '_is_compact', False):
            page.set_compact_mode(True)

        nav_page = Adw.NavigationPage(child=page, title="Uploaded Songs")
        nav.push(nav_page)
        page.stack.set_visible_child_name("loading")

        def _fetch():
            songs = self.client.get_library_upload_songs(limit=None)
            GLib.idle_add(self._populate_songs_page, page, songs or [])

        threading.Thread(target=_fetch, daemon=True).start()

    def _on_album_activated(self, listbox, row):
        if not hasattr(row, "album_data"):
            return
        album = row.album_data
        browse_id = album.get("browseId")
        if browse_id:
            initial_data = {
                "title": album.get("title", ""),
                "thumb": album.get("thumbnails", [{}])[-1].get("url") if album.get("thumbnails") else None,
            }
            self.open_playlist_callback(browse_id, initial_data)

    def _on_album_right_click(self, gesture, n_press, x, y, row):
        if not hasattr(row, "album_data"):
            return

        album = row.album_data
        entity_id = album.get("entityId") or album.get("browseId")

        group = Gio.SimpleActionGroup()
        row.insert_action_group("upl", group)

        menu = Gio.Menu()
        action_section = Gio.Menu()

        # Delete upload
        if entity_id:
            title = album.get("title", "this album")
            action_section.append("Delete Album", "upl.delete")
            a_del = Gio.SimpleAction.new("delete", None)
            a_del.connect("activate", lambda a, p, eid=entity_id, t=title, ws=weakref.ref(self): ws()._confirm_delete_upload(eid, t) if ws() else None)
            group.add_action(a_del)

        if action_section.get_n_items() > 0:
            menu.append_section(None, action_section)

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

    def _confirm_delete_upload(self, entity_id, title):
        dialog = Adw.MessageDialog(
            transient_for=self.get_root(),
            heading="Delete Upload?",
            body=f'Are you sure you want to delete "{title}"?\nThis cannot be undone.',
        )
        dialog.add_response("cancel", "Cancel")
        dialog.add_response("delete", "Delete")
        dialog.set_response_appearance("delete", Adw.ResponseAppearance.DESTRUCTIVE)
        dialog.set_default_response("cancel")
        dialog.set_close_response("cancel")

        def on_response(dg, response_id):
            if response_id == "delete":
                def _thread():
                    success = self.client.delete_upload_entity(entity_id)
                    if success:
                        GLib.idle_add(self._show_toast, f"Deleted {title}")
                        GLib.idle_add(self.load)
                    else:
                        GLib.idle_add(self._show_toast, "Failed to delete")
                threading.Thread(target=_thread, daemon=True).start()
            dg.destroy()

        dialog.connect("response", on_response)
        dialog.present()

    def _show_toast(self, message):
        show_toast(self, message)

    def _on_upload_clicked(self, btn):
        self._do_open_file_picker(self.get_root())

    def _do_open_file_picker(self, parent_window=None):
        dialog = Gtk.FileDialog()
        dialog.set_title("Upload Songs")

        filter_audio = Gtk.FileFilter()
        filter_audio.set_name("Audio Files")
        for ext in ["mp3", "m4a", "wma", "flac", "ogg"]:
            filter_audio.add_pattern(f"*.{ext}")

        filters = Gio.ListStore.new(Gtk.FileFilter)
        filters.append(filter_audio)
        dialog.set_filters(filters)
        dialog.set_default_filter(filter_audio)

        win = parent_window or self.get_root()
        dialog.open_multiple(win, None, self._on_files_chosen)

    def _on_files_chosen(self, dialog, result):
        try:
            files = dialog.open_multiple_finish(result)
            if files:
                paths = [files.get_item(i).get_path() for i in range(files.get_n_items())]
                if paths:
                    self._start_upload_queue(paths)
        except GLib.Error:
            pass

    def _get_window(self):
        root = self.get_root()
        if not root:
            lp = getattr(self, '_library_page', None)
            root = lp.get_root() if lp else None
        return root

    def _start_upload_queue(self, filepaths):
        import os

        win = self._get_window()
        if not win or not hasattr(win, '_upload_queue_box'):
            return
        queue_box = win._upload_queue_box

        # Show the progress button
        GLib.idle_add(win._upload_progress_btn.set_visible, True)

        self._upload_total = getattr(self, '_upload_total', 0) + len(filepaths)
        self._upload_done_count = getattr(self, '_upload_done_count', 0)

        for filepath in filepaths:
            filename = os.path.basename(filepath)
            row = Gtk.Box(orientation=Gtk.Orientation.HORIZONTAL, spacing=8)

            info_box = Gtk.Box(orientation=Gtk.Orientation.VERTICAL, spacing=2)
            info_box.set_hexpand(True)
            info_box.set_margin_top(4)
            info_box.set_margin_bottom(4)

            name_label = Gtk.Label(label=filename)
            name_label.set_halign(Gtk.Align.START)
            name_label.set_ellipsize(Pango.EllipsizeMode.END)
            name_label.add_css_class("caption")
            info_box.append(name_label)

            status_label = Gtk.Label(label="Queued")
            status_label.set_halign(Gtk.Align.START)
            status_label.add_css_class("caption")
            status_label.add_css_class("dim-label")
            info_box.append(status_label)

            row.append(info_box)
            row._filepath = filepath
            row._filename = filename
            row._status_label = status_label
            row._done = False

            queue_box.append(row)

        if not getattr(self, '_uploading', False):
            threading.Thread(target=self._process_upload_queue, daemon=True).start()

    def _process_upload_queue(self):
        self._uploading = True
        has_success = False

        import time
        while True:
            child_holder = [None]
            def _find():
                w = self._get_window()
                if not w or not hasattr(w, '_upload_queue_box'):
                    return
                c = w._upload_queue_box.get_first_child()
                while c:
                    if not getattr(c, '_done', False):
                        child_holder[0] = c
                        return
                    c = c.get_next_sibling()
            GLib.idle_add(_find)
            time.sleep(0.2)

            child = child_holder[0]
            if child is None:
                break

            GLib.idle_add(child._status_label.set_label, "Uploading...")

            result = self.client.upload_song(child._filepath)
            result_str = str(result) if result else ""

            success = result and ("SUCCEEDED" in result_str.upper() or "200" in result_str)
            if success:
                has_success = True
                GLib.idle_add(child._status_label.set_label, "Done")
            else:
                GLib.idle_add(child._status_label.set_label, "Failed")

            child._done = True
            self._upload_done_count = getattr(self, '_upload_done_count', 0) + 1

            # Update pie chart on the window
            def _update_pie():
                w = self._get_window()
                if w and hasattr(w, '_upload_progress_fraction'):
                    total = getattr(self, '_upload_total', 1)
                    done = getattr(self, '_upload_done_count', 0)
                    w._upload_progress_fraction = done / max(total, 1)
                    w._pie_area.queue_draw()
            GLib.idle_add(_update_pie)

        self._uploading = False

        if has_success:
            GLib.idle_add(self._show_toast, "Uploads complete")
            GLib.timeout_add(5000, self._delayed_refresh)

        GLib.timeout_add(8000, self._clear_upload_queue)

    def _clear_upload_queue(self):
        win = self._get_window()
        if not win or not hasattr(win, '_upload_queue_box'):
            return False
        child = win._upload_queue_box.get_first_child()
        while child:
            next_c = child.get_next_sibling()
            if getattr(child, '_done', False):
                win._upload_queue_box.remove(child)
            child = next_c
        if not win._upload_queue_box.get_first_child():
            win._upload_progress_btn.set_visible(False)
            win._upload_progress_fraction = 0.0
            win._pie_area.queue_draw()
            self._upload_total = 0
            self._upload_done_count = 0
        return False

    def _delayed_refresh(self):
        self.load()
        root = self.get_root()
        if root and hasattr(root, "library_page"):
            root.library_page.load_library()
        return False

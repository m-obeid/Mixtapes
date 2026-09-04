import os
import sys
import threading
import time
import gi

gi.require_version("Gtk", "4.0")
gi.require_version("Adw", "1")

from gi.repository import Gtk, Gdk, Adw, GObject, Gio, GLib, Pango
from player.player import Player
from ui import color_utils
from ui.util_classes import ScrolledWindow


HAS_TRAY = False
if sys.platform == "win32":
    try:
        from ui.tray_win import TrayIcon
        HAS_TRAY = True
    except ImportError:
        pass


# The luminance band cover_effects normalizes the backdrop into:
# typical luminance, and the end worst for text.
BLUR_BACKDROP = {           # is_dark -> (typical, worst-for-text)
    True: (0.025, 0.14),
    False: (0.55, 0.35),
}
# The playing row lifts off the backdrop by a fixed contrast ratio, not
# a fixed color, so it looks equally strong in both schemes. A pinned
# color gave a white band in light mode and nothing in dark.
BLUR_ROW_SEPARATION = 1.30
BLUR_ROW_OPACITY = 0.5
# Band edges are percentiles. One point of headroom covers the tail.
BLUR_LABEL_HEADROOM = 1.0
# Damp the highlight's chroma. Equal luminance contrast is not equal
# apparent strength, and dark mode keeps far more chroma.
BLUR_ROW_TINT = 0.4
# Panels (player bar, queue, sidebar) recede instead of lifting.
# alpha(@window_bg_color, 0.35) sat above the backdrop in light mode and
# below it in dark: a 1.41 lift versus 1.04.
BLUR_PANEL_SEPARATION = 1.18
BLUR_PANEL_OPACITY = 0.35
BLUR_PANEL_TINT = 0.25

# CSS that makes the chrome translucent when blurred-cover-bg is active.
# Loaded via a Gtk.CssProvider at PRIORITY_USER + 1 so it actually wins
# the cascade against a user's ~/.config/gtk-4.0/gtk.css. Putting these
# rules in style.css (PRIORITY_APPLICATION = 600) made user CSS at USER
# (800) overwrite them, which is why the player bar / sidebar / mobile
# view switcher kept rendering opaque despite the rules being there.
_BLUR_OVERRIDE_CSS = """
/* Every container painting a flat fill goes transparent. Watch
   Adw.ToolbarView's .top-bar and .bottom-bar wrappers, named "toolbars"
   internally, and Adw.OverlaySplitView's pane wrappers. Those sit behind
   the headerbar, player bar and queue, and keep painting when only the
   inner widgets are cleared. */
window.cover-bg-active > windowhandle,
window.cover-bg-active toolbarview,
window.cover-bg-active toolbarview > .top-bar,
window.cover-bg-active toolbarview > .bottom-bar,
window.cover-bg-active toolbars.top-bar,
window.cover-bg-active toolbars.bottom-bar,
window.cover-bg-active toolbarview > box,
window.cover-bg-active overlaysplitview,
window.cover-bg-active overlaysplitview > box,
window.cover-bg-active overlaysplitview > .background:not(.sidebar-pane),
window.cover-bg-active overlaysplitview > .content-pane,
window.cover-bg-active navigation-view,
window.cover-bg-active navigation-view > .background,
window.cover-bg-active navigation-view-page,
window.cover-bg-active clamp,
window.cover-bg-active scrolledwindow,
window.cover-bg-active scrolledwindow > viewport,
window.cover-bg-active stack,
window.cover-bg-active toastoverlay,
window.cover-bg-active listview,
window.cover-bg-active listview > row,
window.cover-bg-active listbox,
window.cover-bg-active listbox > row,
window.cover-bg-active flowbox,
window.cover-bg-active view,
window.cover-bg-active flap,
window.cover-bg-active leaflet,
window.cover-bg-active clamp {
  background-color: transparent;
  background: none;
}

/* Headerbar: fully transparent. */
window.cover-bg-active headerbar,
window.cover-bg-active headerbar > windowhandle,
window.cover-bg-active headerbar > windowhandle > box {
  background: none;
  background-color: transparent;
  box-shadow: none;
  border: none;
}

/* Mobile view switcher bar. The widget tree is `viewswitcherbar` →
   `revealer` → internal `actionbar` → `box`. Adwaita styles the
   actionbar with a flat fill. Wildcard inside the bar to hit it
   whatever the internal structure. */
window.cover-bg-active viewswitcherbar,
window.cover-bg-active viewswitcherbar *,
window.cover-bg-active viewswitcherbar > actionbar,
window.cover-bg-active viewswitcherbar actionbar,
window.cover-bg-active viewswitcherbar actionbar > revealer,
window.cover-bg-active viewswitcherbar actionbar > revealer > box {
  background: none;
  background-color: transparent;
  border: none;
  box-shadow: none;
}

/* Mobile bottom sheet, the full expanded player on mobile. The sheet
   covers the page underneath, so it stays opaque; clearing it lets the
   playlist and the nav bar show through the player. Paint the panel
   wash as a gradient layer over an opaque background-color instead, so
   the whole sheet including the drag-handle strip is one surface.

   The node is `bottom-sheet`, not `bottomsheet`. The old selectors read
   `bottomsheet` and matched nothing. */
window.cover-bg-active bottom-sheet > sheet {
  background-color: @window_bg_color;
  background-image: linear-gradient(@blur_panel_bg, @blur_panel_bg);
}

/* Player bar, queue panel, expanded player are Gtk.Box widgets carrying
   both libadwaita .background AND their own class. Match both for
   specificity, and use lower alpha so the blur reads through. The
   sidebar itself goes fully transparent. The surrounding .sidebar-pane
   wrapper carries its tint, so the two read as one continuous panel
   matching the player bar. */
window.cover-bg-active .background.player-bar,
window.cover-bg-active .background.queue-panel,
window.cover-bg-active .background.player-drawer,
window.cover-bg-active .player-bar,
window.cover-bg-active .queue-panel,
window.cover-bg-active .player-drawer,
window.cover-bg-active .sidebar-pane {
  background: none;
  background-color: @blur_panel_bg;
}

window.cover-bg-active .background.sidebar,
window.cover-bg-active .sidebar {
  background: none;
  background-color: transparent;
}

/* Inside the sheet the surface above already carries the wash. Anything
   painting its own on top would stack: the queue lives inside the
   expanded player, so the Queue tab came out a different shade from the
   Player tab. QueuePanel carries `.background` as well as
   `.queue-panel`, so the `.background.queue-panel` form needs listing
   too or it out-specifies this reset. Desktop keeps its washes, where
   the expanded player sits in the main stack over the blur rather than
   over the page. */
window.cover-bg-active bottom-sheet .background.player-drawer,
window.cover-bg-active bottom-sheet .background.queue-panel,
window.cover-bg-active bottom-sheet .background.player-bar,
window.cover-bg-active bottom-sheet .player-drawer,
window.cover-bg-active bottom-sheet .queue-panel,
window.cover-bg-active bottom-sheet .player-bar,
window.cover-bg-active bottom-sheet .queue-header {
  background: none;
  background-color: transparent;
}

/* The desktop cover view's lyrics column is an Adw.OverlaySplitView with
   the `.lyrics-split` class. Override the generic .sidebar-pane tint
   above so the lyrics column reads as part of the cover background
   rather than a darker panel floating in front of it. */
window.cover-bg-active .lyrics-split > .sidebar-pane,
window.cover-bg-active .lyrics-split > .sidebar-pane > .background {
  background: none;
  background-color: transparent;
}

window.cover-bg-active .queue-header {
  background-color: @blur_panel_bg_weak;
}

window.cover-bg-active searchbar > revealer > box {
  background-color: @blur_panel_bg;
}

/* Cards and boxed-lists. A currentColor tint instead of @card_bg_color,
   so they read bright on a dark blur and subtle on a light one. Matches
   the .home-speed-tile quick-picks look instead of a muddy gray wash. */
window.cover-bg-active .boxed-list,
window.cover-bg-active .card {
  background-color: alpha(currentColor, 0.1);
}

/* Cards inside floating dialogs (Adw.PreferencesDialog etc.) and
   popovers do NOT sit on the blurred cover bg. They sit on the dialog's
   own surface, where the translucent treatment looks washed out and
   inconsistent. Restore full opacity inside dialogs and popovers. Higher
   specificity than the rule above, so this one wins. */
window.cover-bg-active dialog .boxed-list,
window.cover-bg-active dialog .card,
window.cover-bg-active popover .boxed-list,
window.cover-bg-active popover .card {
  background-color: @card_bg_color;
}

/* Artist banner scrim in blur mode. Darken behind the artist name and
   play button, 60 to 75% down. Fade back to transparent at the bottom,
   where FadeBottomBin masks the image to alpha 0; a scrim still
   translucent there brings back the colored band the mask removes. */
window.cover-bg-active .banner-scrim {
  background: linear-gradient(
    to bottom,
    transparent 0%,
    alpha(@window_bg_color, 0.25) 55%,
    alpha(@window_bg_color, 0.45) 75%,
    transparent 100%
  );
}

/* Playing row over the blurred cover, keeping its accent color. The old
   13% wash could not support one: across 80 covers an accent-tinted
   label measured 1.0 to 1.2:1. A translucent surface of the row's own
   buys it back. _refresh_derived_colors computes both tokens against the
   normalized backdrop band.

   `list row.playing` is the ListBox shape (Home, Explore, search). The
   selector read `listboxrow.song-row-wrapper.playing` before, which
   matches nothing: the CSS node is `row`, and no widget carries that
   class. */
window.cover-bg-active box.song-row.playing,
window.cover-bg-active list row.playing,
window.cover-bg-active .queue-row.playing {
  background-color: @playing_surface_over_blur;
  color: @playing_fg_over_blur;
}
window.cover-bg-active box.song-row.playing label,
window.cover-bg-active list row.playing label,
window.cover-bg-active .queue-row.playing label {
  color: @playing_fg_over_blur;
}

/* Context menus keep regular text. A popover is a CSS child of the row
   it is parented to, so it inherits the color above. */
window.cover-bg-active box.song-row.playing popover label,
window.cover-bg-active box.song-row.playing popover modelbutton,
window.cover-bg-active list row.playing popover label,
window.cover-bg-active list row.playing popover modelbutton,
window.cover-bg-active .queue-row.playing popover label,
window.cover-bg-active .queue-row.playing popover modelbutton {
  color: @popover_fg_color;
}

/* Queue rows lose libadwaita's default :hover tint to the
   "all listview rows transparent" rule above. Restore a subtle hover so
   the row responds to the pointer in blur mode. @view_fg_color gives
   theme-neutral contrast, the same approach as the .playing rule. Lower
   opacity keeps it weaker than the playing highlight. */
window.cover-bg-active .queue-row:hover {
  background-color: alpha(@view_fg_color, 0.08);
}
window.cover-bg-active .queue-row.playing:hover {
  background-color: alpha(@view_fg_color, 0.18);
}

/* Same fix for lyric lines. They are tap-to-seek and need the pointer
   affordance, but the catch-all transparency above kills
   the base hover defined in style.css. Slightly lighter than queue
   rows since lyrics are content, not a list of actions. */
window.cover-bg-active .lyrics-line:hover {
  background-color: alpha(@view_fg_color, 0.06);
}
"""


class MainWindow(Adw.ApplicationWindow):
    def __init__(self, *args, **kwargs):
        super().__init__(*args, **kwargs)

        self.set_default_size(1000, 700)
        self.set_title("Mixtapes")
        self._is_compact = False


        # Add custom icons path relative to current file or project root

        project_root = os.path.dirname(os.path.dirname(os.path.dirname(__file__)))
        assets_path = os.path.join(project_root, "assets", "icons")

        icon_theme = Gtk.IconTheme.get_for_display(Gdk.Display.get_default())
        # Add GResource path
        # Add GResource path
        # The resource prefix is /com/pocoguy/muse/icons
        # The content inside is hicolor/scalable/actions/compass2-symbolic.svg
        icon_theme.add_resource_path("/com/pocoguy/muse/icons")

        # Keep file path as backup/dev
        icon_theme.add_search_path(assets_path)

        # Setup Actions
        self.setup_actions()

        # Key Controller (Global Type to Search)
        # Use CAPTURE phase to ensure we see events before children (like SearchEntry) swallow them
        ctrl = Gtk.EventControllerKey()
        ctrl.set_propagation_phase(Gtk.PropagationPhase.CAPTURE)
        ctrl.connect("key-pressed", self.on_window_key_pressed)
        self.add_controller(ctrl)

        # Header-bar menus (Bazaar layout): a hamburger primary menu
        # at the far right for app-scoped entries (Downloaded Songs /
        # theme swatches / Shortcuts / Preferences / About / Quit),
        # and an avatar button just to its left for account-scoped
        # entries (Your Channel / Upload / History / Log Out).
        menu_btn = self._build_avatar_menu_button()
        primary_btn = self._build_primary_menu_button()

        # Content setup: ViewStack
        self.view_stack = Adw.ViewStack()
        self.view_stack.connect("notify::visible-child-name", self.on_view_changed)

        # Toolbar View (Root) - Wraps EVERYTHING
        self.root_content_view = Adw.ToolbarView()

        # Global Header Setup
        self.header_bar = Adw.HeaderBar()

        # Back Button
        self.back_btn = Gtk.Button(icon_name="go-previous-symbolic")
        self.back_btn.set_visible(False)  # Hidden by default
        self.back_btn.connect("clicked", self.on_back_clicked)
        self.header_bar.pack_start(self.back_btn)

        # Center Widget (Switcher / Title)
        self.title_bin = Adw.Bin()

        self.switcher = Adw.ViewSwitcher()
        self.switcher.set_stack(self.view_stack)
        self.switcher.set_policy(Adw.ViewSwitcherPolicy.WIDE)

        self.title_widget = Adw.WindowTitle(title="Mixtapes")

        # Default to Desktop
        self.title_bin.set_child(self.switcher)
        self.header_bar.set_title_widget(self.title_bin)

        # Upload progress button (pie chart, hidden by default)
        self._upload_progress_btn = Gtk.Button()
        self._upload_progress_btn.add_css_class("flat")
        self._upload_progress_btn.set_tooltip_text("Upload Progress")
        self._upload_progress_btn.set_visible(False)

        self._upload_progress_fraction = 0.0
        self._pie_area = Gtk.DrawingArea()
        self._pie_area.set_size_request(16, 16)
        self._pie_area.set_halign(Gtk.Align.CENTER)
        self._pie_area.set_valign(Gtk.Align.CENTER)
        self._pie_area.set_can_target(False)
        self._pie_area.set_draw_func(self._draw_upload_pie)
        self._upload_progress_btn.set_child(self._pie_area)

        self._ul_popover = Gtk.Popover()
        self._ul_popover.set_size_request(300, -1)
        self._ul_popover.set_parent(self._upload_progress_btn)
        popover_box = Gtk.Box(orientation=Gtk.Orientation.VERTICAL, spacing=4)
        popover_box.set_margin_top(8)
        popover_box.set_margin_bottom(8)
        popover_box.set_margin_start(8)
        popover_box.set_margin_end(8)
        self._upload_queue_box = Gtk.Box(
            orientation=Gtk.Orientation.VERTICAL, spacing=4
        )
        popover_box.append(self._upload_queue_box)
        self._ul_popover.set_child(popover_box)
        self._upload_progress_btn.connect("clicked", lambda b: self._ul_popover.popup())

        # Download progress button (pie chart, hidden by default)
        self._download_progress_btn = Gtk.Button()
        self._download_progress_btn.add_css_class("flat")
        self._download_progress_btn.set_tooltip_text("Download Progress")
        self._download_progress_btn.set_visible(False)

        self._download_progress_fraction = 0.0
        self._dl_pie_area = Gtk.DrawingArea()
        self._dl_pie_area.set_size_request(16, 16)
        self._dl_pie_area.set_halign(Gtk.Align.CENTER)
        self._dl_pie_area.set_valign(Gtk.Align.CENTER)
        self._dl_pie_area.set_can_target(False)
        self._dl_pie_area.set_draw_func(self._draw_download_pie)
        self._download_progress_btn.set_child(self._dl_pie_area)

        self._dl_popover = Gtk.Popover()
        self._dl_popover.set_size_request(300, -1)
        self._dl_popover.set_parent(self._download_progress_btn)
        dl_scroll = ScrolledWindow()
        dl_scroll.set_policy(Gtk.PolicyType.NEVER, Gtk.PolicyType.AUTOMATIC)
        dl_scroll.set_max_content_height(400)
        dl_scroll.set_propagate_natural_height(True)
        dl_popover_box = Gtk.Box(orientation=Gtk.Orientation.VERTICAL, spacing=4)
        dl_popover_box.set_margin_top(8)
        dl_popover_box.set_margin_bottom(8)
        dl_popover_box.set_margin_start(8)
        dl_popover_box.set_margin_end(8)
        self._download_queue_box = Gtk.Box(
            orientation=Gtk.Orientation.VERTICAL, spacing=4
        )
        dl_popover_box.append(self._download_queue_box)
        dl_scroll.set_child(dl_popover_box)
        self._dl_popover.set_child(dl_scroll)
        self._download_progress_btn.connect(
            "clicked", lambda b: self._dl_popover.popup()
        )

        # pack_end stacks from the right, so primary_btn (packed
        # first) ends up rightmost, avatar sits just left of it.
        self.header_bar.pack_end(primary_btn)
        self.header_bar.pack_end(menu_btn)
        self.header_bar.pack_end(self._upload_progress_btn)
        self.header_bar.pack_end(self._download_progress_btn)

        # Refresh Library + Uploads. Visible only when the Library tab is
        # active; has a small inline spinner that shows during the refresh.
        self._lib_refresh_box = Gtk.Box(orientation=Gtk.Orientation.HORIZONTAL, spacing=0)
        self._lib_refresh_box.set_visible(False)
        self._lib_refresh_btn = Gtk.Button(icon_name="view-refresh-symbolic")
        self._lib_refresh_btn.add_css_class("flat")
        self._lib_refresh_btn.set_valign(Gtk.Align.CENTER)
        self._lib_refresh_btn.set_tooltip_text("Refresh library")
        self._lib_refresh_btn.connect("clicked", self._on_library_refresh_clicked)
        self._lib_refresh_spinner = Adw.Spinner()
        self._lib_refresh_spinner.set_valign(Gtk.Align.CENTER)
        self._lib_refresh_spinner.set_margin_start(4)
        self._lib_refresh_spinner.set_margin_end(4)
        self._lib_refresh_spinner.set_visible(False)
        self._lib_refresh_box.append(self._lib_refresh_btn)
        self._lib_refresh_box.append(self._lib_refresh_spinner)
        self.header_bar.pack_end(self._lib_refresh_box)

        # Search Button (Mobile/Contextual) - Toggle
        self.search_btn = Gtk.ToggleButton(icon_name="system-search-symbolic")
        self.header_bar.pack_start(self.search_btn)

        self.root_content_view.add_top_bar(self.header_bar)

        self.search_bar = Gtk.SearchBar()
        self.search_bar.connect(
            "notify::search-mode-enabled", self.on_search_mode_changed
        )

        # Ensure it stays in sync (Binding)
        # We need to bind self.search_btn.active <-> self.search_bar.search_mode_enabled
        # But Gtk.SearchBar property is 'search-mode-enabled'
        self.search_bar.bind_property(
            "search-mode-enabled",
            self.search_btn,
            "active",
            GObject.BindingFlags.BIDIRECTIONAL | GObject.BindingFlags.SYNC_CREATE,
        )

        # Configure Search Entry
        search_clamp = Adw.Clamp()
        search_clamp.set_maximum_size(600)

        self.search_entry = Gtk.SearchEntry()
        self.search_entry.set_placeholder_text("Search...")
        self.search_entry.set_hexpand(True)
        self.search_entry.connect("search-changed", self.on_global_search_changed)
        self.search_entry.connect("stop-search", self.on_search_stop)

        search_clamp.set_child(self.search_entry)
        self.search_bar.set_child(search_clamp)
        self.search_bar.connect_entry(self.search_entry)  # NOW it exists

        self.root_content_view.add_top_bar(self.search_bar)

        # Wrap content in OverlaySplitView for Sidebar (Nautilus-style)
        self.split_view = Adw.OverlaySplitView()
        self.split_view.set_sidebar_position(self._read_sidebar_position())
        self.split_view.set_min_sidebar_width(250)
        self.split_view.set_max_sidebar_width(450)

        # Main Stack for switching between Browser and Player on desktop
        self.main_stack = Gtk.Stack()
        self.main_stack.set_transition_type(Gtk.StackTransitionType.SLIDE_LEFT_RIGHT)
        self.main_stack.set_transition_duration(300)

        # Main Content Area (Scrolled Browser)
        self.content_bin = ScrolledWindow()
        self.content_bin.set_policy(Gtk.PolicyType.AUTOMATIC, Gtk.PolicyType.NEVER)
        self.content_bin.set_child(self.view_stack)

        self.main_stack.add_named(self.content_bin, "browser")

        # Queue Sidebar (Right Side)
        from ui.queue_panel import QueuePanel

        # Global Player (Init before queue panel)

        self.player = Player()

        # Connect download manager progress to UI
        self.player.download_manager.connect("progress", self._on_download_progress)
        self.player.download_manager.connect("complete", self._on_download_complete)
        self.player.download_manager.connect("item-done", self._on_download_item_done)
        self.player.download_manager.connect(
            "item-progress", self._on_download_item_progress
        )

        self.queue_panel = QueuePanel(self.player)

        # Sidebar Content
        self.queue_panel.add_css_class("sidebar")
        self.split_view.set_sidebar(self.queue_panel)

        # Set main_stack as content of root_content_view (ToolbarView)
        self.root_content_view.set_content(self.main_stack)
        self.split_view.set_content(self.root_content_view)

        self._sidebar_explicitly_opened = False
        self.split_view.set_show_sidebar(False)  # Hidden by default
        self.split_view.set_enable_show_gesture(False)
        self.split_view.set_enable_hide_gesture(False)

        # Signal for Sidebar visibility sync
        self.split_view.connect(
            "notify::show-sidebar", self._on_sidebar_visibility_changed
        )
        self.split_view.connect("notify::collapsed", self._on_split_view_collapsed)
        self._apply_window_controls_position()

        # 5. Initialize BottomSheet
        self.bottom_sheet = Adw.BottomSheet()
        self.bottom_sheet.set_show_drag_handle(True)
        self.bottom_sheet.set_open(False)  # Ensure it's closed by default
        self.bottom_sheet.set_content(self.split_view)
        # Mobile-only swipe? No, expanded player handles it.

        # Global Player Bar (Always Visible)
        from ui.player_bar import PlayerBar

        # Player already inited above
        self.player_bar = PlayerBar(
            self.player,
            on_artist_click=self.on_player_bar_artist_click,
            on_queue_click=self.toggle_queue,
            on_album_click=self.on_player_bar_album_click,
        )
        self.player_bar.connect("expand-requested", self.on_expand_requested)

        # Wrap in Revealer for autohide when queue is empty
        self.player_bar_revealer = Gtk.Revealer()
        self.player_bar_revealer.set_transition_type(
            Gtk.RevealerTransitionType.SLIDE_UP
        )
        self.player_bar_revealer.set_transition_duration(200)
        self.player_bar_revealer.set_reveal_child(len(self.player.queue) > 0)
        self.player_bar_revealer.set_overflow(Gtk.Overflow.VISIBLE)
        self.player_bar_revealer.set_child(self.player_bar)
        self.root_content_view.add_bottom_bar(self.player_bar_revealer)

        # Connect signals to auto-show/hide player bar
        self.player.connect("state-changed", self._on_player_bar_visibility)
        self.player.connect("metadata-changed", self._on_player_bar_visibility)
        self.player.connect("track-error", self._on_track_error)

        # View Switcher Bar (Mobile) - Stacked above Player Bar?
        self.view_switcher_bar = Adw.ViewSwitcherBar()
        self.view_switcher_bar.set_stack(self.view_stack)
        self.view_switcher_bar.set_reveal(False)
        self.view_switcher_bar.set_visible(False)
        self.root_content_view.add_bottom_bar(self.view_switcher_bar)

        # Tab Re-click Gesture Setup
        self.switcher_click = Gtk.GestureClick()
        self.switcher_click.connect("pressed", self.on_switcher_reclick)
        self.switcher.add_controller(self.switcher_click)

        self.mobile_switcher_click = Gtk.GestureClick()
        self.mobile_switcher_click.connect("pressed", self.on_switcher_reclick)
        self.view_switcher_bar.add_controller(self.mobile_switcher_click)

        from ui.expanded_player import ExpandedPlayer
        from ui.desktop_cover_view import DesktopCoverView

        # Initialize your ExpandedPlayer (now as a standalone Box/Widget)
        self.expanded_player = ExpandedPlayer(
            self.player,
            on_artist_click=self.on_player_bar_artist_click,
            on_album_click=self.on_player_bar_album_click,
        )
        self.expanded_player.add_css_class("player-drawer")
        self.expanded_player.set_vexpand(True)
        # Connect the dismiss signal to close the sheet
        self.expanded_player.connect("dismiss", self._on_player_dismissed)

        # Desktop equivalent: just the cover art as a separate
        # main_stack page. Animated via SLIDE_UP (both pages translate
        # together instead of overlapping), which avoids the OVER_UP
        # bleed-through without needing any opaque-background tricks.
        self.desktop_cover_view = DesktopCoverView(self.player)
        self.main_stack.add_named(self.desktop_cover_view, "cover")

        # Do NOT set sheet or add to stack yet, managed by breakpoint or expand request

        # Register with OverlaySplitView or ToastOverlay
        self.toast_overlay = Adw.ToastOverlay()
        self.toast_overlay.set_child(self.bottom_sheet)
        self.set_content(self.toast_overlay)

        # Two CSS providers for the cover-derived appearance, kept
        # separate so toggling one leaves the other alone. Pushed at
        # PRIORITY_USER+1 to win over the user's gtk.css.
        self._dynamic_bg_css = Gtk.CssProvider()
        self._dynamic_accent_css = Gtk.CssProvider()
        # Third provider for tokens we compute from whichever accent is in
        # force (see _refresh_derived_colors). Added last so it wins the
        # tie against _dynamic_accent_css at the same priority.
        self._derived_css = Gtk.CssProvider()
        priority = Gtk.STYLE_PROVIDER_PRIORITY_USER + 1
        Gtk.StyleContext.add_provider_for_display(
            Gdk.Display.get_default(), self._dynamic_bg_css, priority,
        )
        Gtk.StyleContext.add_provider_for_display(
            Gdk.Display.get_default(), self._dynamic_accent_css, priority,
        )
        Gtk.StyleContext.add_provider_for_display(
            Gdk.Display.get_default(), self._derived_css, priority,
        )
        # Set by _set_dynamic_accent while the cover-derived accent is
        # active; None means libadwaita's own accent is in force.
        self._accent_override = None
        # (typical, worst-for-text) luminance of the blurred backdrop
        # currently painted, measured by cover_effects; None when there
        # isn't one.
        self._blur_backdrop = None
        self._refresh_derived_colors()
        self._last_cover_url = None
        # Hook metadata for the appearance pipeline (blur + dynamic accent).
        # Applied immediately if either pref is already on at startup.
        self.player.connect("metadata-changed", self._on_metadata_for_appearance)
        # Re-blur on dark/light flips so the tint tracks the active theme,
        # and re-derive the contrast-checked tokens whenever the scheme,
        # the system accent, or the high-contrast preference changes.
        # Each moves the background or the color measured against.
        try:
            style_manager = Adw.StyleManager.get_default()
            style_manager.connect(
                "notify::dark", self._on_color_scheme_changed
            )
            for prop in ("accent-color", "high-contrast"):
                style_manager.connect(
                    f"notify::{prop}",
                    lambda *_: self._refresh_derived_colors(),
                )
        except Exception:
            pass
        self._apply_appearance_prefs_initial()

        # Initialize Pages (Must be before breakpoint)
        self.init_pages()

        # 6. Responsive Breakpoints

        # COLLAPSE SIDERBAR (< 750px)
        collapse_breakpoint = Adw.Breakpoint.new(
            Adw.BreakpointCondition.parse("max-width: 750px")
        )
        collapse_breakpoint.add_setter(self.split_view, "collapsed", True)
        self.add_breakpoint(collapse_breakpoint)

        # MOBILE UI (< 500px)
        mobile_breakpoint = Adw.Breakpoint.new(
            Adw.BreakpointCondition.parse("max-width: 500px")
        )
        mobile_breakpoint.add_setter(self.view_switcher_bar, "reveal", True)
        mobile_breakpoint.add_setter(self.view_switcher_bar, "visible", True)
        mobile_breakpoint.connect("apply", self._on_mobile_breakpoint_apply)
        mobile_breakpoint.connect("unapply", self._on_mobile_breakpoint_unapply)
        self.add_breakpoint(mobile_breakpoint)

        # 7. Initial Checks
        self.check_auth()

        # Monitor network connectivity.
        #
        # Two sources feed this. Gio.NetworkMonitor is fast but noisy: it
        # emits in bursts while routes and DNS settle, and it calls a link
        # "available" that can't resolve a name. The TCP probe in ui.utils
        # is slower but authoritative, and it keeps running when the
        # monitor stays silent. So the monitor only *triggers* a probe,
        # and the probe decides. Toasts and page reloads fire once per
        # real transition instead of once per emission.
        from ui.utils import add_online_listener, is_online

        self._net_online = is_online()
        self._net_debounce_id = 0
        self._last_net_poll = 0.0
        monitor = Gio.NetworkMonitor.get_default()
        monitor.connect("network-changed", self._on_network_changed)
        add_online_listener(self._apply_network_state)
        GLib.timeout_add_seconds(5, self._network_poll_tick)

    def _on_network_changed(self, monitor, available):
        # Coalesce the burst: act on the state that's still there 1.5 s
        # after the last emission.
        if self._net_debounce_id:
            GLib.source_remove(self._net_debounce_id)
        self._net_debounce_id = GLib.timeout_add(1500, self._settle_network_change)

    def _settle_network_change(self):
        from ui.utils import invalidate_is_online_cache, probe_online_now

        self._net_debounce_id = 0
        invalidate_is_online_cache()
        probe_online_now()
        return False

    def _network_poll_tick(self):
        """Backstop for transitions NetworkMonitor never reports (the
        Flatpak portal, VPNs, suspend/resume). Probes every tick while
        offline so recovery is picked up within ~5 s, and every ~30 s
        while online. Each probe is one 2 s-timeout connect off-thread."""
        from ui.utils import probe_online_now, remove_online_listener

        if self.in_destruction():
            remove_online_listener(self._apply_network_state)
            return False
        now = time.monotonic()
        if self._net_online and now - self._last_net_poll < 30:
            return True
        self._last_net_poll = now
        probe_online_now()
        return True

    def _adopt_network_state(self, online):
        """Take a probe result as the current state without announcing it.
        Used where the user caused the change and the UI is already being
        redrawn, so a toast would be noise."""
        self._net_online = bool(online)
        return False

    def _apply_network_state(self, online):
        online = bool(online)
        if online == self._net_online:
            return False
        self._net_online = online
        if online:
            print("[NETWORK] Back online - refreshing library")
            self.add_toast("Back online")
            if hasattr(self, "library_page"):
                self.library_page.load_library()
            if hasattr(self, "search_page"):
                self.search_page.load_explore_data(force=True)
            if hasattr(self, "home_page"):
                self.home_page.refresh()
            # Re-validate auth if needed
            from api.client import MusicClient

            client = MusicClient()
            if not client.is_authenticated():
                threading.Thread(target=self._revalidate_auth, daemon=True).start()
        else:
            print("[NETWORK] Went offline")
            self.add_toast("Offline - downloaded songs still available")
            # Grey out unavailable items
            if hasattr(self, "library_page"):
                self.library_page._apply_offline_state()
            # Show offline message on explore
            if hasattr(self, "search_page"):
                self.search_page.load_explore_data(force=True)
            # Show offline message on home
            if hasattr(self, "home_page"):
                self.home_page.refresh()
        return False

    def _revalidate_auth(self):
        from api.client import MusicClient

        client = MusicClient()
        client.try_login()
        if client.is_authenticated():
            GLib.idle_add(self.add_toast, "Signed in")
            if hasattr(self, "library_page"):
                GLib.idle_add(self.library_page.load_library)

    def add_toast(self, message):
        toast = Adw.Toast.new(message)
        self.toast_overlay.add_toast(toast)

    # ─── Cover-derived appearance (blurred bg + dynamic accent) ───────────

    def _read_appearance_prefs(self):
        """Return a small dict of just the appearance prefs we care about."""
        import json as _json
        path = os.path.join(GLib.get_user_data_dir(), "muse", "prefs.json")
        prefs = {}
        try:
            if os.path.exists(path):
                with open(path) as f:
                    prefs = _json.load(f)
        except Exception:
            pass
        return {
            "blurred_background": bool(prefs.get("blurred_background", False)),
            "dynamic_accent": bool(prefs.get("dynamic_accent", False)),
        }

    def _on_metadata_for_appearance(self, player, title, artist,
                                    thumb_url, video_id, like_status):
        # If the queue is empty or there's no cover (stopped, cleared),
        # tear cover-bg down completely so the app falls back to the
        # normal theme bg / accent.
        queue_empty = not getattr(player, "queue", None)
        if not thumb_url or queue_empty:
            self._last_cover_url = None
            self._deactivate_cover_bg()
            self._clear_dynamic_accent()
            return
        self._last_cover_url = thumb_url
        prefs = self._read_appearance_prefs()
        if prefs["blurred_background"]:
            self._activate_cover_bg(thumb_url)
        if prefs["dynamic_accent"]:
            self._update_dynamic_accent(thumb_url)

    def _activate_cover_bg(self, thumb_url):
        """Mark the window as cover-bg-active and load the override CSS
        right away, before the blur is even computed. That way the chrome
        becomes translucent immediately instead of waiting for the PIL
        blur thread to finish. The bg image is added on top when ready."""
        self.add_css_class("cover-bg-active")
        print("[BLUR] activated cover-bg-active class")
        # Load the override stylesheet by itself if the provider is empty.
        # _update_blurred_background's callback will re-load with the
        # background-image rule appended once the PNG is ready.
        try:
            current = self._dynamic_bg_css.to_string() if hasattr(
                self._dynamic_bg_css, "to_string"
            ) else ""
        except Exception:
            current = ""
        if not current:
            try:
                self._dynamic_bg_css.load_from_string(_BLUR_OVERRIDE_CSS)
            except Exception:
                pass
        self._update_blurred_background(thumb_url)

    def _deactivate_cover_bg(self):
        """Remove the cover-bg-active class and clear the CSS provider so
        the chrome returns to its opaque default."""
        self.remove_css_class("cover-bg-active")
        self._clear_blurred_background()

    def _apply_appearance_prefs_initial(self):
        """At startup, paint anything that's already enabled if we happen to
        already have a track playing (e.g. on auto-resume)."""
        prefs = self._read_appearance_prefs()
        thumb = getattr(self.player, "mpris_art_url", None)
        if not thumb:
            return
        self._last_cover_url = thumb
        if prefs["blurred_background"]:
            self._activate_cover_bg(thumb)
        if prefs["dynamic_accent"]:
            self._update_dynamic_accent(thumb)

    def _on_color_scheme_changed(self, *_):
        # libadwaita already animates @window_bg_color / @view_fg_color
        # crossfades on color-scheme change, but our dynamic accent +
        # blur normalization run in Python against the active scheme.
        # Re-run them, or the chrome stays on the old scheme's values
        # until the next track change.
        prefs = self._read_appearance_prefs()
        if prefs["blurred_background"] and self._last_cover_url:
            self._update_blurred_background(self._last_cover_url)
        if prefs["dynamic_accent"] and self._last_cover_url:
            # Recomputed against the new scheme; drop the old scheme's
            # result first so nothing derives from it in the meantime
            # (the cover color arrives on a worker thread).
            self._accent_override = None
            self._update_dynamic_accent(self._last_cover_url)
        self._refresh_derived_colors()

    def _update_blurred_background(self, thumb_url):
        from ui.cover_effects import get_blurred_cover

        def _apply(path, backdrop):
            if not path or not os.path.exists(path):
                # Fetch failed, or the cover has no color to show.
                # Drop back to opaque chrome: translucent chrome over a
                # flat gray field looks like mismatched patches.
                self._blur_backdrop = None
                self._deactivate_cover_bg()
                self._refresh_derived_colors()
                return False
            self._blur_backdrop = backdrop
            self._set_blurred_background_css(path)
            self._refresh_derived_colors()
            return False

        get_blurred_cover(thumb_url, dark=self._is_dark(), callback=_apply)

    def _set_blurred_background_css(self, path):
        """Compose the dynamic CSS for blurred-bg mode:
          1. The override stylesheet (_BLUR_OVERRIDE_CSS) that makes the
             chrome translucent. We bundle it into the same provider as
             the bg image so it loads at PRIORITY_USER + 1, high enough
             to override the user's ~/.config/gtk-4.0/gtk.css.
          2. The window's background-image rule pointing at the cached
             blurred PNG, and the same image on the mobile sheet.

        The sheet gets its own copy rather than going transparent. It
        covers the page underneath, so clearing it shows the playlist
        through the player. Its own crop of the blur keeps it opaque and
        still shows the cover. The panel wash rides on top as a second
        background layer, so the sheet keeps the tint the contrast
        numbers were derived against.
        """
        # pathlib handles Windows drive letters + backslashes correctly
        # (file:///C:/...); urllib.quote would percent-escape the colon
        # and slashes and produce an unparseable URI for GTK's CSS loader.
        from pathlib import Path
        url = Path(path).as_uri()
        bg_rule = (
            "window.cover-bg-active {\n"
            f'    background-image: url("{url}");\n'
            "    background-size: cover;\n"
            "    background-position: center;\n"
            "}\n"
            "window.cover-bg-active bottom-sheet > sheet {\n"
            "    background-image: linear-gradient(@blur_panel_bg, @blur_panel_bg),\n"
            f'                      url("{url}");\n'
            "    background-size: cover;\n"
            "    background-position: center;\n"
            "}\n"
        )
        try:
            self._dynamic_bg_css.load_from_string(_BLUR_OVERRIDE_CSS + bg_rule)
        except Exception as e:
            print(f"[appearance] bg CSS load failed: {e}")

    def _clear_blurred_background(self):
        had_backdrop = self._blur_backdrop is not None
        self._blur_backdrop = None
        try:
            self._dynamic_bg_css.load_from_string("")
        except Exception:
            pass
        if had_backdrop:
            # The tokens from that backdrop describe nothing on screen
            # now.
            self._refresh_derived_colors()

    def _update_dynamic_accent(self, thumb_url):
        from ui.cover_effects import get_dominant_color

        def _apply(rgb):
            if not rgb:
                # Featureless cover. Fall back to the theme accent
                # rather than keeping the previous track's color.
                self._clear_dynamic_accent()
                return False
            self._set_dynamic_accent(rgb)
            return False

        get_dominant_color(thumb_url, callback=_apply)

    def _is_dark(self):
        try:
            return Adw.StyleManager.get_default().get_dark()
        except Exception:
            return True

    def _contrast_target(self):
        """Contrast ratio every derived text color has to clear. AA
        normally, AAA under high contrast."""
        try:
            if Adw.StyleManager.get_default().get_high_contrast():
                return color_utils.WCAG_AAA
        except Exception:
            pass
        return color_utils.WCAG_AA

    def _set_dynamic_accent(self, rgb):
        """Push an accent override into the dynamic accent CSS provider.

        libadwaita splits the accent: `accent_bg_color` fills buttons,
        `accent_color` is the standalone text variant. The old code
        overrode both with the raw cover color and lost the split, so a
        pale-yellow cover painted near-invisible text.

        Rebuilds both in OkLCh. The fill needs 3:1 as a shape, the
        standalone variant needs to be readable as text.
        """
        is_dark = self._is_dark()
        target = self._contrast_target()
        # Bases libadwaita paints under the accent wash below.
        bg_base = "#242424" if is_dark else "#fafafa"
        view_base = "#1e1e1e" if is_dark else "#ffffff"
        card_base = "#363636" if is_dark else "#ffffff"
        sidebar_base = "#2e2e2e" if is_dark else "#ebebeb"

        # 1. The fill. Keep inside a real accent's lightness band, then
        #    separate the shape from the window behind it.
        solid = color_utils.clamp_lightness(rgb, 0.45, 0.85)
        solid = color_utils.ensure_contrast(
            solid, color_utils.from_hex(bg_base), 3.0
        )
        # 2. The standalone text variant. The accent tints the view
        #    background below, so mix it the same way first.
        view_bg = color_utils.mix(
            color_utils.from_hex(view_base), solid, 0.08
        )
        standalone = color_utils.ensure_contrast(solid, view_bg, target)
        # 3. Label color for anything drawn on the fill.
        fg = color_utils.best_foreground(solid)

        solid_css = color_utils.to_css(solid)
        # Wash a little accent into the bg tokens so plain GTK surfaces
        # (dialogs, popovers) pick up the cover hue. Around 10% stays
        # cohesive without competing with the accent.
        css = (
            f"@define-color accent_color {color_utils.to_css(standalone)};\n"
            f"@define-color accent_bg_color {solid_css};\n"
            f"@define-color accent_fg_color {color_utils.to_css(fg)};\n"
            f"@define-color window_bg_color mix({bg_base}, {solid_css}, 0.10);\n"
            f"@define-color view_bg_color mix({view_base}, {solid_css}, 0.08);\n"
            f"@define-color card_bg_color mix({card_base}, {solid_css}, 0.08);\n"
            f"@define-color popover_bg_color mix({card_base}, {solid_css}, 0.10);\n"
            f"@define-color dialog_bg_color mix({bg_base}, {solid_css}, 0.10);\n"
            f"@define-color headerbar_bg_color mix({bg_base}, {solid_css}, 0.10);\n"
            f"@define-color sidebar_bg_color mix({sidebar_base}, {solid_css}, 0.10);\n"
            f"@define-color sidebar_backdrop_color mix({sidebar_base}, {solid_css}, 0.10);\n"
            f"@define-color secondary_sidebar_bg_color mix({sidebar_base}, {solid_css}, 0.10);\n"
            f"@define-color secondary_sidebar_backdrop_color mix({sidebar_base}, {solid_css}, 0.10);\n"
        )
        try:
            self._dynamic_accent_css.load_from_string(css)
        except Exception as e:
            print(f"[appearance] dynamic accent CSS load failed: {e}")
            return
        self._accent_override = (solid, standalone, view_bg)
        self._refresh_derived_colors()

    def _clear_dynamic_accent(self):
        try:
            self._dynamic_accent_css.load_from_string("")
        except Exception:
            pass
        self._accent_override = None
        self._refresh_derived_colors()

    # ─── Colors derived from whichever accent is in force ──────────────

    def _accent_in_force(self):
        """`(solid, standalone, view_bg)` for the accent in force.

        The cover-derived override, or libadwaita's own accent when
        dynamic accent is off.
        """
        if self._accent_override is not None:
            return self._accent_override
        is_dark = self._is_dark()
        view_bg = color_utils.from_hex("#1e1e1e" if is_dark else "#ffffff")
        try:
            accent = Adw.StyleManager.get_default().get_accent_color()
            rgba = accent.to_rgba()
            standalone_rgba = accent.to_standalone_rgba(is_dark)
            solid = (rgba.red, rgba.green, rgba.blue)
            standalone = (
                standalone_rgba.red, standalone_rgba.green, standalone_rgba.blue
            )
        except Exception:
            # No accent API on old libadwaita. Fall back to GNOME blue.
            solid = color_utils.from_hex("#3584e4")
            standalone = color_utils.from_hex(
                "#81b7f5" if is_dark else "#1c71d8"
            )
        return solid, standalone, view_bg

    def _refresh_derived_colors(self):
        """Recompute the app color tokens needing to stay legible.

        `@playing_fg` matters most. The old `hsl(from @accent_color h
        100% 80%)` pinned lightness at 80%, fine on dark themes and
        around 1.1:1 on light ones. Derive against the background the
        label lands on instead.
        """
        solid, standalone, view_bg = self._accent_in_force()
        # Boxed lists tint the .playing row hardest, at 0.18. The
        # lighter 0.10 tint follows.
        row_bg = color_utils.mix(view_bg, solid, 0.18)
        target = self._contrast_target()
        fg = color_utils.ensure_contrast(standalone, row_bg, target)

        # Blurred-background mode. The row lifts off the normalized
        # backdrop by BLUR_ROW_SEPARATION, and the label is checked
        # against the composite.
        is_dark = self._is_dark()
        # Measured off the blur on screen. The constants cover the
        # moment before it lands.
        typical, worst = self._blur_backdrop or BLUR_BACKDROP[is_dark]
        lightness, chroma, hue = color_utils.rgb_to_oklch(solid)
        overlay = color_utils.overlay_for_contrast(
            color_utils.gray(typical),
            color_utils.oklch_to_rgb(lightness, chroma * BLUR_ROW_TINT, hue),
            BLUR_ROW_OPACITY, BLUR_ROW_SEPARATION,
        )
        # Worst case: the band end moving the row toward the label's
        # own lightness.
        over_blur = color_utils.ensure_contrast(
            standalone,
            color_utils.mix(color_utils.gray(worst), overlay, BLUR_ROW_OPACITY),
            target + BLUR_LABEL_HEADROOM,
        )
        panel = color_utils.overlay_for_contrast(
            color_utils.gray(typical),
            color_utils.oklch_to_rgb(lightness, chroma * BLUR_PANEL_TINT, hue),
            BLUR_PANEL_OPACITY, BLUR_PANEL_SEPARATION, lighter=False,
        )

        def rgba(color, alpha):
            r, g, b = (
                int(round(min(1.0, max(0.0, c)) * 255)) for c in color
            )
            return f"rgba({r}, {g}, {b}, {alpha})"

        try:
            self._derived_css.load_from_string(
                f"@define-color playing_fg {color_utils.to_css(fg)};\n"
                f"@define-color playing_fg_over_blur "
                f"{color_utils.to_css(over_blur)};\n"
                f"@define-color playing_surface_over_blur "
                f"{rgba(overlay, BLUR_ROW_OPACITY)};\n"
                f"@define-color blur_panel_bg "
                f"{rgba(panel, BLUR_PANEL_OPACITY)};\n"
                f"@define-color blur_panel_bg_weak "
                f"{rgba(panel, BLUR_PANEL_OPACITY * 0.7)};\n"
            )
        except Exception as e:
            print(f"[appearance] derived color CSS load failed: {e}")

    def _on_track_error(self, player, video_id, title, reason):
        """Surface yt-dlp failures (video unavailable, region-locked, removed)
        instead of letting the player sit in 'loading' forever. The player
        itself already auto-advances to the next track."""
        if title:
            self.add_toast(f"Couldn't play '{title}': {reason}")
        else:
            self.add_toast(f"Couldn't play track: {reason}")

    def _get_active_responsive_child(self):
        # Helper to find if visible view has responsive features (compact mode)
        nav = self.view_stack.get_visible_child()
        if isinstance(nav, Adw.NavigationView):
            page = nav.get_visible_page()
            if page:
                child = page.get_child()
                if isinstance(child, Adw.ToolbarView):
                    content = child.get_content()
                    if hasattr(content, "set_compact_mode"):
                        return content
                elif hasattr(child, "set_compact_mode"):
                    return child
        return None

    def _get_active_filterable_child(self):
        # Helper to find if currently visible child supports search filtering (Playlist, Album)
        active_nav = self.view_stack.get_visible_child()
        if isinstance(active_nav, Adw.NavigationView):
            nav_page = active_nav.get_visible_page()
            if nav_page:
                child = nav_page.get_child()
                if isinstance(child, Adw.ToolbarView):
                    content = child.get_content()
                    if hasattr(content, "filter_content"):
                        return content
                elif hasattr(child, "filter_content"):
                    return child
        return None

    def on_switcher_reclick(self, gesture, n_press, x, y):
        # We want to detect if the user clicked the ALREADY active tab.
        # Adw.ViewSwitcher doesn't tell us which button was clicked easily.
        # But we can check if the visible child remains the same after a short delay.
        old_name = self.view_stack.get_visible_child_name()

        def check_reclick():
            new_name = self.view_stack.get_visible_child_name()
            if old_name == new_name:
                # Same tab clicked! Reset it to root.
                nav = self._get_active_nav_view()
                if nav:
                    nav.pop_to_tag("root")
            return False

        GLib.timeout_add(100, check_reclick)

    def _dismiss_cover_if_open(self):
        """Collapse the desktop cover view if it's currently showing.
        Called from any code path that navigates to a new page so the
        cover view can't linger behind a push that the user wouldn't
        otherwise see."""
        if (
            not self._is_compact
            and self.main_stack.get_visible_child_name() == "cover"
        ):
            self._on_player_dismissed(None)

    def _on_player_dismissed(self, player):
        """Called when the player is dismissed (tapped back on desktop or swiped down on mobile)."""
        if self._is_compact:
            self.bottom_sheet.set_open(False)
        else:
            was_cover = self.main_stack.get_visible_child_name() == "cover"
            if was_cover:
                # SLIDE_DOWN is the inverse of SLIDE_UP — browser comes
                # back in from the top, cover exits downward.
                self.main_stack.set_transition_type(
                    Gtk.StackTransitionType.SLIDE_DOWN
                )
            self.main_stack.set_visible_child_name("browser")
            if was_cover and hasattr(self, "_prev_main_transition"):
                self.main_stack.set_transition_type(self._prev_main_transition)
            if was_cover and hasattr(self, "_prev_main_duration"):
                self.main_stack.set_transition_duration(
                    self._prev_main_duration
                )
            self.back_btn.set_visible(False)
            self.update_back_button_visibility()
        if hasattr(self, "player_bar"):
            self.player_bar.set_expanded(False)

    def on_view_changed(self, stack, param):
        visible_name = self.view_stack.get_visible_child_name()

        # Any top-level navigation (Home/Library/Explore) should
        # collapse the cover view — it's a full-window takeover and
        # staying on it through a tab switch makes no sense.
        self._dismiss_cover_if_open()

        # Update Back Button for the new active tab
        self.update_back_button_visibility()

        # Auto-refresh library if selected
        if visible_name == "library" and hasattr(self, "library_page"):
            # Delay slightly to allow UI transition and background state settlement
            GLib.timeout_add(100, self.library_page.load_library)

        # Refresh button visibility — recomputed also on navigation-stack
        # changes inside each tab (see update_back_button_visibility).
        self._update_refresh_button_visibility()

        # Close Search Bar when switching tabs
        if self.search_bar.get_search_mode():
            if visible_name != "search":
                self.search_bar.set_search_mode(False)

    def _get_refresh_target(self):
        """Pick which page (if any) the header-bar refresh button should act
        on based on what's currently visible. Returns a callable that, when
        invoked, reloads that page, or None to hide the button.

        Rules:
          - Library tab root: refresh the whole library (+ uploads).
          - PlaylistPage showing a user playlist: refresh its tracks.
          - Anything else (album, artist, uploads-album, home/explore,
            pages opened via navigation into derived YTM content): hide.
        """
        visible_name = self.view_stack.get_visible_child_name()
        if visible_name == "library" and hasattr(self, "library_page"):
            nav = self.view_stack.get_child_by_name("library")
            if isinstance(nav, Adw.NavigationView):
                page = nav.get_visible_page()
                # Library root page has no previous → we're on the list view.
                if page and not nav.get_previous_page(page):
                    return self.library_page.trigger_refresh
                # A sub-page is showing — check if it's a refreshable playlist.
                child = page.get_child() if page else None
                if isinstance(child, Adw.ToolbarView):
                    child = child.get_content()
                return self._playlist_page_refresh(child)
        # Playlist pages can live under Home/Explore too.
        nav = self._get_active_nav_view()
        if nav:
            page = nav.get_visible_page()
            child = page.get_child() if page else None
            if isinstance(child, Adw.ToolbarView):
                child = child.get_content()
            return self._playlist_page_refresh(child)
        return None

    def _playlist_page_refresh(self, child):
        """Return a no-arg callable that re-fetches this PlaylistPage or
        HistoryPage, or None if it's an album / artist page (derived
        content we don't own the refresh semantics for)."""
        try:
            from ui.pages.playlist import PlaylistPage
            from ui.pages.album import AlbumPage
            from ui.pages.history import HistoryPage
        except Exception:
            return None

        # HistoryPage owns its own load/refresh path.
        if isinstance(child, HistoryPage):
            def _do_history():
                child.load()

                def poll():
                    if not child._loading_wrap.get_visible():
                        self._on_library_refresh_finished()
                        return False
                    return True
                GLib.timeout_add(250, poll)
            return _do_history

        if not isinstance(child, PlaylistPage):
            return None
        if isinstance(child, AlbumPage):
            return None
        pid = getattr(child, "playlist_id", None) or ""
        # Derived YTM content (albums, uploads) don't get a refresh button.
        if pid.startswith("MPRE") or pid.startswith("OLAK"):
            return None
        if pid.startswith("FEmusic_library_privately_owned"):
            return None
        if not pid:
            return None

        def _do():
            # refresh_in_place invalidates cache, resets state, and repopulates
            # the SAME page — no new NavigationPage is pushed.
            child.refresh_in_place()

            # The PlaylistPage hides its inline `content_spinner` once the
            # main fetch completes. Poll for that so the header-bar spinner
            # matches, instead of hard-coding a timer.
            def poll():
                spinner = getattr(child, "content_spinner", None)
                if spinner is None or not spinner.get_visible():
                    self._on_library_refresh_finished()
                    return False
                return True
            GLib.timeout_add(250, poll)

        return _do

    def _update_refresh_button_visibility(self):
        if not hasattr(self, "_lib_refresh_box"):
            return
        target = self._get_refresh_target()
        self._lib_refresh_box.set_visible(target is not None)
        self._refresh_target = target

    def _on_library_refresh_clicked(self, btn):
        target = getattr(self, "_refresh_target", None) or self._get_refresh_target()
        if target is None:
            return
        self._lib_refresh_btn.set_visible(False)
        self._lib_refresh_spinner.set_visible(True)
        try:
            target()
        except Exception as e:
            print(f"[REFRESH] failed: {e}")
            self._on_library_refresh_finished()

    def _on_library_refresh_finished(self):
        if hasattr(self, "_lib_refresh_spinner"):
            self._lib_refresh_spinner.set_visible(False)
        if hasattr(self, "_lib_refresh_btn"):
            self._lib_refresh_btn.set_visible(True)
            self._lib_refresh_btn.set_sensitive(True)

    def on_playlist_header_title_changed(self, page, title):
        if hasattr(self, "title_widget"):
            self.title_widget.set_title(title if title else "Mixtapes")

    def update_back_button_visibility(self, *args):
        # Refresh-button visibility follows the currently-visible page.
        self._update_refresh_button_visibility()
        # On desktop, show back button whenever a full-window player
        # view is active (legacy expanded player or the cover view).
        if (
            not self._is_compact
            and self.main_stack.get_visible_child_name() in ("player", "cover")
        ):
            self.back_btn.set_visible(True)
            return

        nav = self._get_active_nav_view()
        if nav:
            visible_page = nav.get_visible_page()
            if visible_page and nav.get_previous_page(visible_page):
                self.back_btn.set_visible(True)
            else:
                self.back_btn.set_visible(False)
                # Reset title when back at root
                if hasattr(self, "title_widget"):
                    self.title_widget.set_title("Mixtapes")

                # Refresh library if we just returned to root of library tab
                if self.view_stack.get_visible_child_name() == "library" and hasattr(
                    self, "library_page"
                ):
                    self.library_page.load_library()
        else:
            self.back_btn.set_visible(False)

    def on_back_clicked(self, btn):
        if (
            not self._is_compact
            and self.main_stack.get_visible_child_name() in ("player", "cover")
        ):
            self._on_player_dismissed(None)
            return

        nav = self._get_active_nav_view()
        if nav:
            nav.pop()

    def _build_avatar_menu_button(self):
        """Account button in the header bar — Bazaar-style.

        Holds only account-scoped actions (channel / upload / history
        / log out) plus a profile header. The app-scoped entries live
        in a separate hamburger next to it — see
        `_build_primary_menu_button`.

        Menu items backed by win.* actions use `hidden-when=
        "action-disabled"` so the popover shows the Sign In entry
        when signed out and the account rows + Log Out when signed
        in, driven entirely by action state."""

        menu_btn = Gtk.MenuButton()
        menu_btn.add_css_class("flat")
        menu_btn.add_css_class("circular")
        menu_btn.set_tooltip_text("Account")

        # Adw.Avatar handles the circular mask natively; a hand-rolled
        # Gtk.Image in a Box with overflow:hidden was getting squeezed
        # into a non-square allocation inside the MenuButton's layout.
        self._avatar_small = Adw.Avatar.new(28, "", False)
        menu_btn.set_child(self._avatar_small)

        # ── Menu model ───────────────────────────────────────────────
        menu = Gio.Menu()

        # 1. Profile header — a custom child slot ("profile-header").
        header_section = Gio.Menu()
        header_item = Gio.MenuItem.new(None, None)
        header_item.set_attribute_value(
            "custom", GLib.Variant.new_string("profile-header")
        )
        header_section.append_item(header_item)
        menu.append_section(None, header_section)

        # 2. Signed-in-only account actions.
        authed_section = Gio.Menu()
        for label, action in (
            ("Your Channel",       "win.open-channel"),
            ("Upload Songs",       "win.open-upload"),
            ("Listening History",  "win.open-history"),
        ):
            item = Gio.MenuItem.new(label, action)
            item.set_attribute_value(
                "hidden-when", GLib.Variant.new_string("action-disabled")
            )
            authed_section.append_item(item)
        menu.append_section(None, authed_section)

        # 3. Sign in / Log out — one is enabled at a time, the other
        # hidden via `hidden-when="action-disabled"`.
        auth_section = Gio.Menu()
        signin_item = Gio.MenuItem.new("Sign In", "win.sign-in")
        signin_item.set_attribute_value(
            "hidden-when", GLib.Variant.new_string("action-disabled")
        )
        auth_section.append_item(signin_item)
        logout_item = Gio.MenuItem.new("Log Out", "win.logout")
        logout_item.set_attribute_value(
            "hidden-when", GLib.Variant.new_string("action-disabled")
        )
        auth_section.append_item(logout_item)
        menu.append_section(None, auth_section)

        popover = Gtk.PopoverMenu.new_from_model(menu)
        popover.add_css_class("menu")
        menu_btn.set_popover(popover)
        self._avatar_popover = popover

        # ── Custom profile-header child ──────────────────────────────
        header = Gtk.Box(orientation=Gtk.Orientation.HORIZONTAL, spacing=12)
        header.add_css_class("avatar-menu-header")
        header.set_margin_top(2)
        header.set_margin_bottom(6)
        header.set_margin_start(6)
        header.set_margin_end(6)

        self._avatar_large = Adw.Avatar.new(48, "", False)
        header.append(self._avatar_large)

        name_col = Gtk.Box(orientation=Gtk.Orientation.VERTICAL, spacing=2)
        name_col.set_valign(Gtk.Align.CENTER)
        name_col.set_hexpand(True)
        self._avatar_name_label = Gtk.Label(label="Not signed in")
        self._avatar_name_label.add_css_class("heading")
        self._avatar_name_label.set_halign(Gtk.Align.START)
        self._avatar_name_label.set_ellipsize(Pango.EllipsizeMode.END)
        self._avatar_handle_label = Gtk.Label(label="")
        self._avatar_handle_label.add_css_class("caption")
        self._avatar_handle_label.add_css_class("dim-label")
        self._avatar_handle_label.set_halign(Gtk.Align.START)
        self._avatar_handle_label.set_ellipsize(Pango.EllipsizeMode.END)
        self._avatar_handle_label.set_visible(False)
        name_col.append(self._avatar_name_label)
        name_col.append(self._avatar_handle_label)
        header.append(name_col)

        popover.add_child(header, "profile-header")

        # Backwards-compat alias — the old code toggled this single
        # action; the auth-state flip now walks a group of them.
        self._channel_action = self.lookup_action("open-channel")

        # Kick off an async fetch to populate the profile so the first
        # paint shows the user's real photo/name.
        GLib.idle_add(self._refresh_avatar_profile)
        return menu_btn

    def _build_primary_menu_button(self):
        """Hamburger primary menu — app-scoped entries only. Sits at
        the far-right of the header bar, immediately to the right of
        the profile avatar (Bazaar layout).

        The top of the popover carries a Whisp-style row of three
        circular theme swatches (System / Light / Dark) instead of a
        submenu."""

        btn = Gtk.MenuButton()
        btn.add_css_class("flat")
        btn.set_icon_name("open-menu-symbolic")
        btn.set_tooltip_text("Main Menu")

        menu = Gio.Menu()

        # 1. Theme swatches — custom child.
        theme_section = Gio.Menu()
        theme_item = Gio.MenuItem.new(None, None)
        theme_item.set_attribute_value(
            "custom", GLib.Variant.new_string("theme-swatches")
        )
        theme_section.append_item(theme_item)
        menu.append_section(None, theme_section)

        # 2. Downloaded songs — works offline, not account-scoped, so
        # this lives in the app menu rather than the profile one.
        lib_section = Gio.Menu()
        lib_section.append("Downloaded Songs", "win.open-downloads")
        menu.append_section(None, lib_section)

        # 3. App entries.
        app_section = Gio.Menu()
        app_section.append("Keyboard Shortcuts", "win.shortcuts")
        app_section.append("Preferences", "win.preferences")
        app_section.append("About Mixtapes", "win.about")
        app_section.append("Quit", "win.quit")
        menu.append_section(None, app_section)

        popover = Gtk.PopoverMenu.new_from_model(menu)
        popover.add_css_class("menu")
        btn.set_popover(popover)

        popover.add_child(self._build_theme_swatches(), "theme-swatches")
        return btn

    def _build_theme_swatches(self):
        """Three theme swatches (System / Light / Dark) at the top of
        the primary menu — the shared GNOME pattern used by Text
        Editor, Papers, Loupe et al. Three GtkCheckButton radios with
        CSS classes `theme-selector` + `follow` / `light` / `dark`;
        the diagonal split for System and the check overlay are drawn
        by the CSS in style.css."""

        row = Gtk.Box(orientation=Gtk.Orientation.HORIZONTAL, spacing=12)
        row.add_css_class("themeselector")
        row.set_hexpand(True)

        specs = [
            ("default", "follow", "Follow System Style"),
            ("light",   "light",  "Light Style"),
            ("dark",    "dark",   "Dark Style"),
        ]
        self._theme_swatch_buttons = {}
        self._theme_swatch_syncing = False
        group_head = None

        for value, variant, tooltip in specs:
            cb = Gtk.CheckButton()
            cb.add_css_class("theme-selector")
            cb.add_css_class(variant)
            cb.set_tooltip_text(tooltip)
            cb.set_hexpand(True)
            cb.set_halign(Gtk.Align.CENTER)
            cb.set_focus_on_click(False)
            if group_head is None:
                group_head = cb
            else:
                cb.set_group(group_head)

            def _on_toggled(button, v=value):
                if self._theme_swatch_syncing or not button.get_active():
                    return
                self.activate_action(
                    "color-scheme", GLib.Variant.new_string(v)
                )

            cb.connect("toggled", _on_toggled)
            row.append(cb)
            self._theme_swatch_buttons[value] = cb

        # Reflect the current action state now and on every change.
        action = self.lookup_action("color-scheme")
        if action is not None:
            self._sync_theme_swatch_selection(action.get_state())
            action.connect(
                "notify::state",
                lambda a, _p: self._sync_theme_swatch_selection(a.get_state()),
            )
        return row

    def _sync_theme_swatch_selection(self, state):
        current = state.get_string() if state is not None else "default"
        # Guard the toggled handler so the action-driven update does
        # not fire a redundant activate_action back into ourselves.
        self._theme_swatch_syncing = True
        try:
            target = self._theme_swatch_buttons.get(current)
            if target is not None and not target.get_active():
                target.set_active(True)
        finally:
            self._theme_swatch_syncing = False

    def _set_account_actions_authed(self, is_authed):
        """Flip the auth-gated action enabled states so the profile
        menu shows Sign In vs {Your Channel / Upload / History / Log
        Out}. Called from _apply_avatar_profile (signed in) and
        _reset_avatar_profile (signed out)."""
        for name in ("open-channel", "open-upload", "open-history", "logout"):
            act = self.lookup_action(name)
            if act is not None:
                act.set_enabled(is_authed)
        act = self.lookup_action("sign-in")
        if act is not None:
            act.set_enabled(not is_authed)

    def _refresh_avatar_profile(self):
        """Fetch account info in a background thread and paint the
        avatar + name when it returns."""
        if not self.player.client.is_authenticated():
            return False

        def _work():
            info = self.player.client.get_account_info()
            GLib.idle_add(self._apply_avatar_profile, info or {})

        threading.Thread(target=_work, daemon=True).start()
        return False

    def _apply_avatar_profile(self, info):
        name = info.get("accountName") or "Not signed in"
        handle = info.get("channelHandle") or ""
        photo = info.get("accountPhotoUrl") or ""
        self._avatar_name_label.set_label(name)
        self._avatar_small.set_text(name)
        self._avatar_large.set_text(name)
        if handle:
            self._avatar_handle_label.set_label(handle)
            self._avatar_handle_label.set_visible(True)
        else:
            self._avatar_handle_label.set_visible(False)
        if photo:
            self._load_avatar_photo(photo)
        # Enable the account-scoped actions now that we know we're
        # signed in (channel needs the handle specifically).
        self._set_account_actions_authed(True)
        channel_action = self.lookup_action("open-channel")
        if channel_action is not None:
            channel_action.set_enabled(bool(handle))

    def _load_avatar_photo(self, url):
        """Fetch the account photo and feed it into both Adw.Avatar
        widgets as a GdkTexture. Adw.Avatar needs a paintable — it
        doesn't take a URL directly.

        ytmusicapi returns the smallest thumbnail (~48px), which looks
        blurry on HiDPI displays. `get_high_res_url` swaps the `s48`
        path segment for `s800`, giving us a sharp source that Adw.Avatar
        can downscale cleanly."""
        from ui.utils import read_thumb_cache, write_thumb_cache, get_high_res_url

        hi_url = get_high_res_url(url) or url

        def _work():
            data = read_thumb_cache(hi_url)
            if not data:
                try:
                    import requests
                    resp = requests.get(
                        hi_url,
                        headers={"User-Agent": "Mozilla/5.0"},
                        timeout=10,
                    )
                    resp.raise_for_status()
                    data = resp.content
                    write_thumb_cache(hi_url, data)
                except Exception as e:
                    print(f"[AVATAR] fetch failed: {e}")
                    return

            def _apply():
                try:
                    from gi.repository import GdkPixbuf
                    loader = GdkPixbuf.PixbufLoader()
                    loader.write(data)
                    loader.close()
                    pixbuf = loader.get_pixbuf()
                    if pixbuf is None:
                        return False
                    texture = Gdk.Texture.new_for_pixbuf(pixbuf)
                    self._avatar_small.set_custom_image(texture)
                    self._avatar_large.set_custom_image(texture)
                except Exception as e:
                    print(f"[AVATAR] texture build failed: {e}")
                return False

            GLib.idle_add(_apply)

        threading.Thread(target=_work, daemon=True).start()

    def _open_own_channel(self):
        """Resolve the user's @handle to a channel browseId and push an
        ArtistPage inside the app (same as tapping any other artist
        link). The resolution runs in a background thread so the
        popover doesn't hang while YT's endpoint responds."""
        info = self.player.client.get_account_info() or {}
        handle = info.get("channelHandle") or ""
        name = info.get("accountName") or ""
        if not handle:
            return

        def _work():
            browse_id = self.player.client.resolve_channel_handle(handle)
            if browse_id:
                GLib.idle_add(self.open_artist, browse_id, name)
            else:
                GLib.idle_add(
                    self.add_toast, "Couldn't open your channel"
                )

        threading.Thread(target=_work, daemon=True).start()

    def _open_upload_picker(self):
        lib = getattr(self, "library_page", None)
        if lib and hasattr(lib, "uploads_page"):
            lib.uploads_page._do_open_file_picker(self)

    def _open_history_from_menu(self):
        """Push HistoryPage onto the currently-visible tab's nav view.
        The heavy row-building happens after a short delay so the
        forward-nav slide animation runs on an empty page — rendering
        a few hundred rows synchronously inside `page.load()` was
        stalling the transition."""
        from ui.utils import is_online
        if not is_online():
            self.add_toast("History requires an internet connection")
            return
        if not self.player.client.is_authenticated():
            self.add_toast("Sign in to view listening history")
            return

        nav = self._get_active_nav_view()
        if not nav:
            return
        from ui.pages.history import HistoryPage
        page = HistoryPage(self.player)
        if getattr(self, "_is_compact", False):
            page.set_compact_mode(True)
        nav_page = Adw.NavigationPage(child=page, title="Listening History")

        def _on_shown(p):
            page.load()

        nav_page.connect("shown", _on_shown)
        nav.push(nav_page)

    def _open_downloads_from_menu(self):
        """Push the Downloads PlaylistPage onto the visible tab's nav
        view. Same rationale as _open_history_from_menu — keep the
        forward-nav animation."""
        nav = self._get_active_nav_view()
        if not nav:
            return
        from ui.pages.playlist import PlaylistPage
        page = PlaylistPage(self.player)
        page.playlist_id = "DOWNLOADS"
        page.is_fully_loaded = True
        page.is_fully_fetched = True
        if getattr(self, "_is_compact", False):
            page.set_compact_mode(True)
        nav_page = Adw.NavigationPage(child=page, title="Downloaded Songs")

        def _on_shown(page):
            threading.Thread(target=_fetch, daemon=True).start()

        nav_page.connect("shown", _on_shown)
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
                    "artists": (
                        [{"name": d.get("artist", ""), "id": None}]
                        if d.get("artist") else []
                    ),
                    "album": {"name": d.get("album", "")},
                    "duration_seconds": d.get("duration_seconds", 0),
                    "thumbnails": (
                        [{"url": d.get("thumbnail_url")}]
                        if d.get("thumbnail_url") else []
                    ),
                }
                dur = d.get("duration_seconds", 0)
                if dur:
                    t["duration"] = f"{dur // 60}:{dur % 60:02d}"
                tracks.append(t)
            GLib.idle_add(self._fill_downloads_page, page, tracks)

    def _fill_downloads_page(self, page, tracks):
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

    def _get_active_nav_view(self):
        nav = self.view_stack.get_visible_child()
        if isinstance(nav, Adw.NavigationView):
            return nav
        return None

    def _get_visualizer(self):
        """Return the cover-view's visualizer widget, or None if it hasn't
        been constructed (e.g. mobile breakpoint before desktop cover view
        is created)."""
        cover = getattr(self, "desktop_cover_view", None)
        if cover is None:
            return None
        return getattr(cover, "visualizer", None)

    def _draw_upload_pie(self, area, cr, width, height):
        import math

        cx, cy = width / 2, height / 2
        radius = min(cx, cy) - 1
        frac = self._upload_progress_fraction

        # Background circle
        style = area.get_style_context()
        color = style.lookup_color("theme_fg_color")
        if color[0]:
            cr.set_source_rgba(color[1].red, color[1].green, color[1].blue, 0.3)
        else:
            cr.set_source_rgba(1, 1, 1, 0.3)
        cr.arc(cx, cy, radius, 0, 2 * math.pi)
        cr.fill()

        # Progress pie
        if color[0]:
            cr.set_source_rgba(color[1].red, color[1].green, color[1].blue, 1.0)
        else:
            cr.set_source_rgba(1, 1, 1, 1.0)
        cr.move_to(cx, cy)
        cr.arc(cx, cy, radius, -math.pi / 2, -math.pi / 2 + frac * 2 * math.pi)
        cr.close_path()
        cr.fill()

    def download_tracks(self, tracks, album_title=None, album_id=None, thumb_url=None):
        """Public API to queue tracks for download from anywhere in the app."""
        dm = self.player.download_manager
        dm.queue_tracks(tracks, album_title, album_id)

        # Register playlist for incremental m3u8 generation. We deliberately
        # DON'T fall back to tracks[0]'s thumbnail — that would paint the
        # first song's cover onto the playlist when a user downloads a
        # single track. The playlist cover is owned by PlaylistPage and
        # cached on open; register_playlist no longer writes it.
        if album_title and tracks:
            dm.register_playlist(album_id, album_title, tracks, thumb_url)

        # Add items to the popover queue
        for t in tracks:
            vid = t.get("videoId")
            if not vid or dm.db.is_downloaded(vid):
                continue
            title = t.get("title", "Unknown")
            row = Gtk.Box(orientation=Gtk.Orientation.HORIZONTAL, spacing=8)
            info = Gtk.Box(orientation=Gtk.Orientation.VERTICAL, spacing=2)
            info.set_hexpand(True)
            info.set_margin_top(4)
            info.set_margin_bottom(4)
            lbl = Gtk.Label(label=title)
            lbl.set_halign(Gtk.Align.START)
            lbl.set_ellipsize(Pango.EllipsizeMode.END)
            lbl.add_css_class("caption")
            info.append(lbl)
            status = Gtk.Label(label="Queued")
            status.set_halign(Gtk.Align.START)
            status.add_css_class("caption")
            status.add_css_class("dim-label")
            info.append(status)
            progress = Gtk.ProgressBar()
            progress.set_visible(False)
            info.append(progress)
            row.append(info)
            cancel_btn = Gtk.Button.new_from_icon_name("window-close-symbolic")
            cancel_btn.set_valign(Gtk.Align.CENTER)
            cancel_btn.add_css_class("flat")
            cancel_btn.add_css_class("circular")
            cancel_btn.set_tooltip_text("Cancel")
            cancel_btn.connect("clicked", self._on_cancel_download_clicked, vid)
            row.append(cancel_btn)
            row._video_id = vid
            row._status_label = status
            row._progress_bar = progress
            row._cancel_btn = cancel_btn
            self._download_queue_box.append(row)

        self._download_progress_btn.set_visible(True)
        dm.start()

    def download_track(self, track, album_title=None, album_id=None):
        """Download a single track."""
        self.download_tracks([track], album_title, album_id)

    def _on_download_progress(self, dm, done, total, current_title):
        self._download_progress_fraction = done / max(total, 1)
        self._dl_pie_area.queue_draw()

        # Mark the current item as downloading
        child = self._download_queue_box.get_first_child()
        while child:
            status = getattr(child, "_status_label", None)
            bar = getattr(child, "_progress_bar", None)
            if status and status.get_label() == "Queued":
                status.set_label("Downloading...")
                if bar:
                    bar.set_visible(True)
                    bar.set_fraction(0)
                break
            child = child.get_next_sibling()

    def _on_download_item_progress(self, dm, video_id, fraction):
        """Update per-item progress bar with actual download percentage."""
        child = self._download_queue_box.get_first_child()
        while child:
            if getattr(child, "_video_id", None) == video_id:
                bar = getattr(child, "_progress_bar", None)
                status = getattr(child, "_status_label", None)
                if bar:
                    bar.set_visible(True)
                    bar.set_fraction(fraction)
                if status:
                    status.set_label(f"{int(fraction * 100)}%")
                # yt_dlp has already started writing bytes — too late to cancel.
                cancel_btn = getattr(child, "_cancel_btn", None)
                if cancel_btn:
                    cancel_btn.set_visible(False)
                break
            child = child.get_next_sibling()

    def _on_download_item_done(self, dm, video_id, success, message):
        if success:
            self._download_success_count = (
                getattr(self, "_download_success_count", 0) + 1
            )
        child = self._download_queue_box.get_first_child()
        while child:
            if getattr(child, "_video_id", None) == video_id:
                if success:
                    child._status_label.set_label("Done")
                elif message == "Cancelled":
                    child._status_label.set_label("Cancelled")
                else:
                    child._status_label.set_label("Failed")
                bar = getattr(child, "_progress_bar", None)
                if bar:
                    if success:
                        bar.set_fraction(1.0)
                    bar.set_visible(False)
                cancel_btn = getattr(child, "_cancel_btn", None)
                if cancel_btn:
                    cancel_btn.set_visible(False)
                break
            child = child.get_next_sibling()

    def _on_cancel_download_clicked(self, btn, video_id):
        dm = self.player.download_manager
        dm.cancel_queued(video_id)

    def _on_download_complete(self, dm):
        if getattr(self, "_download_success_count", 0) > 0:
            self.add_toast("Downloads complete")
        self._download_success_count = 0
        # Clear done items after delay
        GLib.timeout_add(5000, self._clear_download_queue)

    def _clear_download_queue(self):
        child = self._download_queue_box.get_first_child()
        while child:
            next_c = child.get_next_sibling()
            self._download_queue_box.remove(child)
            child = next_c
        self._download_progress_btn.set_visible(False)
        self._download_progress_fraction = 0.0
        self._dl_pie_area.queue_draw()
        return False

    def _draw_download_pie(self, area, cr, width, height):
        import math

        cx, cy = width / 2, height / 2
        radius = min(cx, cy) - 1
        frac = self._download_progress_fraction

        style = area.get_style_context()
        color = style.lookup_color("theme_fg_color")
        if color[0]:
            cr.set_source_rgba(color[1].red, color[1].green, color[1].blue, 0.3)
        else:
            cr.set_source_rgba(1, 1, 1, 0.3)
        cr.arc(cx, cy, radius, 0, 2 * math.pi)
        cr.fill()

        if color[0]:
            cr.set_source_rgba(color[1].red, color[1].green, color[1].blue, 1.0)
        else:
            cr.set_source_rgba(1, 1, 1, 1.0)
        cr.move_to(cx, cy)
        cr.arc(cx, cy, radius, -math.pi / 2, -math.pi / 2 + frac * 2 * math.pi)
        cr.close_path()
        cr.fill()

    def setup_actions(self):
        # About Action
        action = Gio.SimpleAction.new("about", None)
        action.connect("activate", self.show_about)
        self.add_action(action)

        # Preferences Action
        pref_action = Gio.SimpleAction.new("preferences", None)
        pref_action.connect("activate", self.show_preferences)
        self.add_action(pref_action)

        # Quit Action (force quit even with songs in queue)
        quit_action = Gio.SimpleAction.new("quit", None)
        quit_action.connect("activate", self._on_force_quit)
        self.add_action(quit_action)

        # Primary-menu targets — library shortcuts. Kept as window
        # actions so the popover items map straight onto them. The
        # auth-gated ones start disabled; _set_account_actions_authed
        # flips them once we've resolved the sign-in state.
        for name, cb, gated in (
            ("open-channel",   self._open_own_channel,       True),
            ("open-upload",    self._open_upload_picker,     True),
            ("open-history",   self._open_history_from_menu, True),
            ("open-downloads", self._open_downloads_from_menu, False),
        ):
            act = Gio.SimpleAction.new(name, None)
            act.connect("activate", lambda a, p, _cb=cb: _cb())
            if gated:
                act.set_enabled(False)
            self.add_action(act)

        # Sign-in / Log-out actions — the primary-menu items that
        # target these use `hidden-when="action-disabled"` so exactly
        # one appears at a time.
        signin_action = Gio.SimpleAction.new("sign-in", None)
        signin_action.connect("activate", lambda *_: self.check_auth())
        self.add_action(signin_action)

        logout_action = Gio.SimpleAction.new("logout", None)
        logout_action.connect("activate", lambda *_: self.on_logout_clicked(None, None))
        logout_action.set_enabled(False)
        self.add_action(logout_action)

        # Keyboard Shortcuts dialog (Adw.ShortcutsDialog).
        shortcuts_action = Gio.SimpleAction.new("shortcuts", None)
        shortcuts_action.connect("activate", self._show_shortcuts_dialog)
        self.add_action(shortcuts_action)

        # Stateful color-scheme action. State is one of
        # "default" / "light" / "dark" — matches the string suffix on
        # the menu items ("win.color-scheme::light" etc.).
        current_scheme = self._load_color_scheme_pref()
        color_scheme_action = Gio.SimpleAction.new_stateful(
            "color-scheme",
            GLib.VariantType.new("s"),
            GLib.Variant.new_string(current_scheme),
        )
        color_scheme_action.connect(
            "change-state", self._on_color_scheme_action
        )
        self.add_action(color_scheme_action)
        # Push the loaded preference into Adw.StyleManager now that the
        # window (and thus the display) exists.
        self._apply_color_scheme(current_scheme)

        # Register app-level accelerators for the menu entries.
        app = self.get_application()
        if app is not None:
            app.set_accels_for_action("win.preferences", ["<Primary>comma"])
            app.set_accels_for_action("win.quit", ["<Primary>q"])
            app.set_accels_for_action(
                "win.shortcuts", ["<Primary>question", "<Primary>slash"]
            )

        # Intercept window close to hide instead of quit when playing
        self.connect("close-request", self._on_close_request)

        # On Windows, manage tray icon when window visibility changes
        if HAS_TRAY:
            self.connect("notify::visible", self._on_visibility_changed)

    # ── Color scheme (Follow System / Light / Dark) ─────────────────
    _COLOR_SCHEME_MAP = {
        "default": Adw.ColorScheme.DEFAULT,
        "light": Adw.ColorScheme.FORCE_LIGHT,
        "dark": Adw.ColorScheme.FORCE_DARK,
    }

    def _prefs_path(self):
        return os.path.join(GLib.get_user_data_dir(), "muse", "prefs.json")

    def _load_color_scheme_pref(self):
        import json as _json
        try:
            path = self._prefs_path()
            if os.path.exists(path):
                with open(path) as f:
                    val = _json.load(f).get("color_scheme", "default")
                if val in self._COLOR_SCHEME_MAP:
                    return val
        except Exception:
            pass
        return "default"

    def _save_color_scheme_pref(self, value):
        import json as _json
        path = self._prefs_path()
        data = {}
        try:
            if os.path.exists(path):
                with open(path) as f:
                    data = _json.load(f) or {}
        except Exception:
            data = {}
        data["color_scheme"] = value
        try:
            os.makedirs(os.path.dirname(path), exist_ok=True)
            with open(path, "w") as f:
                _json.dump(data, f)
        except Exception as e:
            print(f"[PREFS] failed to save color scheme: {e}")

    def _apply_color_scheme(self, value):
        scheme = self._COLOR_SCHEME_MAP.get(value, Adw.ColorScheme.DEFAULT)
        Adw.StyleManager.get_default().set_color_scheme(scheme)

    def _on_color_scheme_action(self, action, value):
        s = value.get_string() if value is not None else "default"
        if s not in self._COLOR_SCHEME_MAP:
            s = "default"
        action.set_state(GLib.Variant.new_string(s))
        self._apply_color_scheme(s)
        self._save_color_scheme_pref(s)

    def _show_shortcuts_dialog(self, action, param):
        """Present an Adw.ShortcutsDialog with the app's key bindings."""
        # Adw.ShortcutsDialog landed in libadwaita 1.9. Fall back to a
        # simple info toast on older runtimes so we don't crash.
        if not hasattr(Adw, "ShortcutsDialog"):
            self.add_toast("Shortcuts dialog not available on this system")
            return

        dialog = Adw.ShortcutsDialog()

        general = Adw.ShortcutsSection()
        general.set_title("General")
        dialog.add(general)
        for accel, title in (
            ("<Primary>comma", "Preferences"),
            ("<Primary>question", "Keyboard Shortcuts"),
            ("<Primary>q", "Quit"),
            ("Escape", "Go Back / Close Search"),
        ):
            item = Adw.ShortcutsItem()
            item.set_title(title)
            item.set_accelerator(accel)
            general.add(item)

        playback = Adw.ShortcutsSection()
        playback.set_title("Playback")
        dialog.add(playback)
        pp = Adw.ShortcutsItem()
        pp.set_title("Play / Pause")
        pp.set_accelerator("space")
        playback.add(pp)

        search = Adw.ShortcutsSection()
        search.set_title("Search")
        dialog.add(search)
        gs = Adw.ShortcutsItem()
        gs.set_title("Start Typing to Search")
        gs.set_accelerator("a")
        search.add(gs)

        dialog.present(self)

    def _get_background_play_enabled(self):
        import json as _json
        path = os.path.join(GLib.get_user_data_dir(), "muse", "prefs.json")
        try:
            if os.path.exists(path):
                with open(path) as f:
                    return _json.load(f).get("background_play", True)
        except Exception:
            pass

        return True

    def _on_close_request(self, window):
        """Hide window instead of quitting if there are songs in the queue."""
        if self._get_background_play_enabled() and self.player.queue and self.player.current_queue_index >= 0:
            self.set_visible(False)
            return True  # Prevent default close
        if HAS_TRAY and hasattr(self, "_tray_icon"):
            self._tray_icon.hide()
        return False  # Allow normal close

    def _on_visibility_changed(self, window, pspec):
        if self.get_visible():
            # Window shown — hide tray icon
            if hasattr(self, "_tray_icon"):
                self._tray_icon.hide()
                del self._tray_icon
        else:
            # Window hidden — show tray icon
            if not hasattr(self, "_tray_icon"):
                self._tray_icon = TrayIcon(self, self.player)
                self._tray_icon.show()

    def _on_force_quit(self, action, param):
        """Force quit the application."""
        self.player.stop()
        app = self.get_application()
        if app:
            app.quit()

    def show_about(self, action, param):
        about = Adw.AboutDialog()
        about.set_application_icon("com.pocoguy.Muse")
        about.set_application_name("Mixtapes")
        about.set_developer_name("POCOGuy")
        about.set_version("2026.04.09-0")
        about.set_website("https://www.pocoguy.com/#!/mixtapes")
        about.set_copyright("© 2026 POCOGuy")
        about.set_license_type(Gtk.License.GPL_3_0)
        about.present(self)

    def _read_sidebar_position(self):
        import json as _json
        path = os.path.join(GLib.get_user_data_dir(), "muse", "prefs.json")
        side = "left"
        try:
            if os.path.exists(path):
                with open(path) as f:
                    side = _json.load(f).get("sidebar_position", "left")
        except Exception:
            pass
        return Gtk.PackType.END if side == "right" else Gtk.PackType.START

    def _apply_window_controls_position(self):
        """Route window controls (close/min/max) to the correct outer edge.

        Each pane in an OverlaySplitView has its own HeaderBar, and the
        sidebar can be hidden, collapsed to an overlay, or shown beside the
        content. The sidebar only "owns" the outer trailing edge when it
        is on the right AND visible AND not collapsed; in every other case
        the content header owns it.
        """
        if not hasattr(self, "queue_panel") or not hasattr(self, "header_bar"):
            return
        is_right = self.split_view.get_sidebar_position() == Gtk.PackType.END
        collapsed = self.split_view.get_collapsed()
        sidebar_visible = self.split_view.get_show_sidebar()
        sidebar_owns_trailing = is_right and sidebar_visible and not collapsed
        sidebar_hdr = self.queue_panel.header_bar
        content_hdr = self.header_bar

        if sidebar_owns_trailing:
            content_hdr.set_show_start_title_buttons(True)
            content_hdr.set_show_end_title_buttons(False)
            sidebar_hdr.set_show_start_title_buttons(False)
            sidebar_hdr.set_show_end_title_buttons(True)
        else:
            content_hdr.set_show_start_title_buttons(False)
            content_hdr.set_show_end_title_buttons(True)
            # Only show start-side buttons on the sidebar when it's visibly
            # hugging the outer left edge (rare close-on-left layouts).
            sidebar_hdr.set_show_start_title_buttons(
                not is_right and sidebar_visible and not collapsed
            )
            sidebar_hdr.set_show_end_title_buttons(False)

    def _on_show_stream_info(self, row):
        try:
            info = self.player.get_stream_debug()
        except Exception as e:
            info = f"Failed to read stream info: {e}"

        label = Gtk.Label(label=info)
        label.set_selectable(True)
        label.set_wrap(True)
        label.set_xalign(0.0)
        label.add_css_class("monospace")
        label.set_margin_top(4)

        dialog = Adw.MessageDialog(
            transient_for=row.get_root() or self,
            heading="Stream Info",
        )
        dialog.set_extra_child(label)
        dialog.add_response("close", "Close")
        dialog.add_response("copy", "Copy")
        dialog.set_default_response("close")
        dialog.set_close_response("close")

        def on_response(dg, response_id):
            if response_id == "copy":
                try:
                    full = self.player.get_stream_debug(full=True)
                except Exception:
                    full = info
                Gdk.Display.get_default().get_clipboard().set(full)
            dg.destroy()

        dialog.connect("response", on_response)
        dialog.present()

    def show_preferences(self, action, param):
        prefs = Adw.PreferencesDialog()

        page = Adw.PreferencesPage()
        page.set_title("General")
        # "settings-symbolic" isn't in Adwaita — the page icon silently fell
        # back to a missing-image box. It only became visible once a second
        # page gave the dialog a view switcher to draw.
        page.set_icon_name("preferences-system-symbolic")
        prefs.add(page)

        # Account group first — profile header + Sign In/Sign Out.
        from api.client import MusicClient
        is_authed = MusicClient().is_authenticated()
        account_group = self._build_account_group(prefs, is_authed)
        page.add(account_group)

        app_group = Adw.PreferencesGroup()
        app_group.set_title("Application")
        page.add(app_group)

        import logger

        debug_row = Adw.SwitchRow()
        debug_row.set_title("Enable Debug Logs")
        debug_row.set_subtitle("Print diagnostic information to the terminal")
        debug_row.set_active(logger.get_debug_logs())
        debug_row.connect(
            "notify::active",
            lambda switch, param: logger.set_debug_logs(switch.get_active()),
        )
        app_group.add(debug_row)

        # Live stream diagnostics — format/protocol/seek-range of whatever's
        # currently playing. Handy for "why won't this song seek?".
        stream_info_row = Adw.ActionRow()
        stream_info_row.set_title("Stream Info (Debug)")
        stream_info_row.set_subtitle(
            "Show format, protocol and seek range of the current stream"
        )
        stream_info_row.set_activatable(True)
        stream_info_row.add_suffix(
            Gtk.Image.new_from_icon_name("go-next-symbolic")
        )
        stream_info_row.connect("activated", self._on_show_stream_info)
        app_group.add(stream_info_row)

        # Force offline mode
        import json as _json

        _prefs_path = os.path.join(GLib.get_user_data_dir(), "muse", "prefs.json")
        _prefs = {}
        try:
            if os.path.exists(_prefs_path):
                with open(_prefs_path) as f:
                    _prefs = _json.load(f)
        except Exception:
            pass

        offline_row = Adw.SwitchRow()
        offline_row.set_title("Force Offline Mode")
        offline_row.set_subtitle(
            "Disable all network requests and use only downloaded content"
        )
        offline_row.set_active(_prefs.get("force_offline", False))

        def on_offline_toggled(switch, pspec):
            _prefs["force_offline"] = switch.get_active()
            os.makedirs(os.path.dirname(_prefs_path), exist_ok=True)
            with open(_prefs_path, "w") as f:
                _json.dump(_prefs, f)
            # is_online() caches the pref for 10 s. Without this the pages
            # reloaded below would read the value we just replaced.
            from ui.utils import invalidate_is_online_cache, probe_online_now

            invalidate_is_online_cache()
            if switch.get_active():
                self._net_online = False
            else:
                # Resync against the real link. The reloads below already
                # redraw every page, so adopt the result without a toast.
                probe_online_now(self._adopt_network_state)
            if hasattr(self, "library_page"):
                self.library_page._apply_offline_state()
                self.library_page.load_library()
            if hasattr(self, "search_page"):
                self.search_page.load_explore_data()

        offline_row.connect("notify::active", on_offline_toggled)
        app_group.add(offline_row)

        background_play_row = Adw.SwitchRow()
        background_play_row.set_title("Background Playback")
        background_play_row.set_subtitle("Allow music to keep playing when the window is closed")
        background_play_row.set_active(_prefs.get("background_play", True))

        def on_background_play_toggled(switch, pspec):
            _prefs["background_play"] = switch.get_active() 
            os.makedirs(os.path.dirname(_prefs_path), exist_ok=True)
            with open(_prefs_path, "w") as f:
                _json.dump(_prefs, f)
        
        background_play_row.connect("notify::active", on_background_play_toggled)
        app_group.add(background_play_row)

        sidebar_right_row = Adw.SwitchRow()
        sidebar_right_row.set_title("Sidebar on the Right")
        sidebar_right_row.set_subtitle("Place the queue sidebar on the right edge")
        sidebar_right_row.set_active(
            _prefs.get("sidebar_position", "left") == "right"
        )

        def on_sidebar_position_toggled(switch, pspec):
            on_right = switch.get_active()
            _prefs["sidebar_position"] = "right" if on_right else "left"
            os.makedirs(os.path.dirname(_prefs_path), exist_ok=True)
            with open(_prefs_path, "w") as f:
                _json.dump(_prefs, f)
            if hasattr(self, "split_view"):
                self.split_view.set_sidebar_position(
                    Gtk.PackType.END if on_right else Gtk.PackType.START
                )
                self._apply_window_controls_position()

        sidebar_right_row.connect("notify::active", on_sidebar_position_toggled)
        app_group.add(sidebar_right_row)

        # GSK renderer override. Some GPU/driver combos (notably certain NVIDIA
        # versions) crash inside the default renderer; switching to "gl" or
        # "cairo" is a known workaround. Takes effect on next launch.
        renderer_row = Adw.ComboRow()
        renderer_row.set_title("Renderer")
        renderer_row.set_subtitle(
            "Switch if you hit GPU-related crashes. Applies on next launch."
        )
        renderer_keys = ["default", "ngl", "gl", "vulkan", "cairo"]
        renderer_labels = [
            "Default (recommended)",
            "NGL",
            "Legacy GL",
            "Vulkan",
            "Cairo (Software)",
        ]
        renderer_row.set_model(Gtk.StringList.new(renderer_labels))
        current_renderer = _prefs.get("gsk_renderer", "default")
        for i, key in enumerate(renderer_keys):
            if key == current_renderer:
                renderer_row.set_selected(i)
                break

        def on_renderer_changed(row, pspec):
            idx = row.get_selected()
            if not (0 <= idx < len(renderer_keys)):
                return
            _prefs["gsk_renderer"] = renderer_keys[idx]
            os.makedirs(os.path.dirname(_prefs_path), exist_ok=True)
            with open(_prefs_path, "w") as f:
                _json.dump(_prefs, f)

        renderer_row.connect("notify::selected", on_renderer_changed)
        app_group.add(renderer_row)

        # Listening-history recording timing. YT Music counts a play the
        # moment you open a track; we default to matching that, but offer
        # "After 30s" (stricter) and "Never" (opt-out).
        history_keys = ["immediate", "after_30s", "never"]
        history_labels = [
            "Immediately",
            "After 30 seconds",
            "Never",
        ]
        history_row = Adw.ComboRow()
        history_row.set_title("Record Plays to History")
        history_row.set_subtitle(
            "When Mixtapes should tell YouTube Music a song was played"
        )
        history_row.set_model(Gtk.StringList.new(history_labels))
        current_history_mode = _prefs.get("history_mode", "immediate")
        for i, key in enumerate(history_keys):
            if key == current_history_mode:
                history_row.set_selected(i)
                break

        def on_history_mode_changed(row, pspec):
            idx = row.get_selected()
            if idx < 0 or idx >= len(history_keys):
                return
            _prefs["history_mode"] = history_keys[idx]
            os.makedirs(os.path.dirname(_prefs_path), exist_ok=True)
            with open(_prefs_path, "w") as f:
                _json.dump(_prefs, f)
            # Reflect the change live on the player so the next track
            # respects the new mode without a restart.
            if hasattr(self.player, "set_history_mode"):
                self.player.set_history_mode(history_keys[idx])

        history_row.connect("notify::selected", on_history_mode_changed)
        app_group.add(history_row)

        # ── Appearance group (blurred bg + dynamic accent) ──────────────
        appearance_group = Adw.PreferencesGroup()
        appearance_group.set_title("Appearance")
        page.add(appearance_group)

        blur_row = Adw.SwitchRow()
        blur_row.set_title("Blurred Cover Background")
        blur_row.set_subtitle(
            "Use the current track's cover as a blurred window background"
        )
        blur_row.set_active(bool(_prefs.get("blurred_background", False)))

        def on_blur_toggled(switch, pspec):
            on = switch.get_active()
            _prefs["blurred_background"] = on
            os.makedirs(os.path.dirname(_prefs_path), exist_ok=True)
            with open(_prefs_path, "w") as f:
                _json.dump(_prefs, f)
            if on:
                target = self._last_cover_url or getattr(self.player, "mpris_art_url", None)
                if target and getattr(self.player, "queue", None):
                    self._activate_cover_bg(target)
            else:
                self._deactivate_cover_bg()

        blur_row.connect("notify::active", on_blur_toggled)
        appearance_group.add(blur_row)

        accent_row = Adw.SwitchRow()
        accent_row.set_title("Dynamic Cover Color")
        accent_row.set_subtitle(
            "Match the app accent color to the current track's cover"
        )
        accent_row.set_active(bool(_prefs.get("dynamic_accent", False)))

        def on_accent_toggled(switch, pspec):
            on = switch.get_active()
            _prefs["dynamic_accent"] = on
            os.makedirs(os.path.dirname(_prefs_path), exist_ok=True)
            with open(_prefs_path, "w") as f:
                _json.dump(_prefs, f)
            if on:
                target = self._last_cover_url or getattr(self.player, "mpris_art_url", None)
                if target:
                    self._update_dynamic_accent(target)
            else:
                self._clear_dynamic_accent()

        accent_row.connect("notify::active", on_accent_toggled)
        appearance_group.add(accent_row)

        # ── Visualizer group ────────────────────────────────────────────
        viz_group = Adw.PreferencesGroup()
        viz_group.set_title("Visualizer")
        viz_group.set_description(
            "Bar visualizer beneath the cover art in the expanded player"
        )
        page.add(viz_group)

        viz_enabled_row = Adw.SwitchRow()
        viz_enabled_row.set_title("Enable Visualizer")
        viz_enabled_row.set_subtitle("Show audio bars beneath the cover art")
        viz_enabled_row.set_active(bool(_prefs.get("visualizer_enabled", True)))

        def on_viz_enabled(switch, pspec):
            on = switch.get_active()
            _prefs["visualizer_enabled"] = on
            os.makedirs(os.path.dirname(_prefs_path), exist_ok=True)
            with open(_prefs_path, "w") as f:
                _json.dump(_prefs, f)
            viz = self._get_visualizer()
            if viz is not None:
                viz.set_visible(on)
            bars_row.set_sensitive(on)
            smooth_row.set_sensitive(on)

        viz_enabled_row.connect("notify::active", on_viz_enabled)
        viz_group.add(viz_enabled_row)

        # Bar count
        bars_row = Adw.ActionRow()
        bars_row.set_title("Bar Count")
        bars_row.set_subtitle("Number of bars in the visualizer (more = finer)")

        bars_initial = int(_prefs.get("visualizer_bars", 56))
        bars_initial = max(8, min(100, bars_initial))

        bars_scale = Gtk.Scale.new_with_range(
            Gtk.Orientation.HORIZONTAL, 16, 100, 4
        )
        bars_scale.set_value(bars_initial)
        bars_scale.set_draw_value(True)
        bars_scale.set_value_pos(Gtk.PositionType.RIGHT)
        bars_scale.set_digits(0)
        bars_scale.set_size_request(220, -1)
        bars_scale.set_valign(Gtk.Align.CENTER)
        bars_scale.set_hexpand(False)
        bars_row.add_suffix(bars_scale)

        def on_bars_changed(scale):
            n = int(scale.get_value())
            _prefs["visualizer_bars"] = n
            os.makedirs(os.path.dirname(_prefs_path), exist_ok=True)
            with open(_prefs_path, "w") as f:
                _json.dump(_prefs, f)
            viz = self._get_visualizer()
            if viz is not None:
                viz.set_bar_count(n)

        bars_scale.connect("value-changed", on_bars_changed)
        viz_group.add(bars_row)

        # Smoothing (peak-spread between bars)
        smooth_row = Adw.ActionRow()
        smooth_row.set_title("Smoothing")
        smooth_row.set_subtitle(
            "Higher = tighter spikes, lower = peaks bleed into neighbors"
        )

        smooth_initial = float(_prefs.get("visualizer_smoothing", 1.5))
        smooth_initial = max(1.05, min(3.0, smooth_initial))

        smooth_scale = Gtk.Scale.new_with_range(
            Gtk.Orientation.HORIZONTAL, 1.1, 3.0, 0.05
        )
        smooth_scale.set_value(smooth_initial)
        smooth_scale.set_draw_value(True)
        smooth_scale.set_value_pos(Gtk.PositionType.RIGHT)
        smooth_scale.set_digits(2)
        smooth_scale.set_size_request(220, -1)
        smooth_scale.set_valign(Gtk.Align.CENTER)
        smooth_scale.set_hexpand(False)
        smooth_row.add_suffix(smooth_scale)

        def on_smooth_changed(scale):
            v = float(scale.get_value())
            _prefs["visualizer_smoothing"] = v
            os.makedirs(os.path.dirname(_prefs_path), exist_ok=True)
            with open(_prefs_path, "w") as f:
                _json.dump(_prefs, f)
            viz = self._get_visualizer()
            if viz is not None:
                viz.set_smoothing(v)

        smooth_scale.connect("value-changed", on_smooth_changed)
        viz_group.add(smooth_row)

        # Reflect the current enable state on first open.
        _viz_initial = bool(_prefs.get("visualizer_enabled", True))
        bars_row.set_sensitive(_viz_initial)
        smooth_row.set_sensitive(_viz_initial)

        # Discord RPC group
        from player.discord_rpc import (
            STATUS_DISPLAY_TYPES,
            STATUS_DISPLAY_DEFAULT,
        )

        rpc_group = Adw.PreferencesGroup()
        rpc_group.set_title("Discord Rich Presence")
        page.add(rpc_group)

        rpc_adapter = getattr(self.player, "discord_rpc", None)

        # Connection status
        status_text = rpc_adapter.status if rpc_adapter else "Unavailable"
        status_row = Adw.ActionRow()
        status_row.set_title("Connection Status")
        status_label = Gtk.Label(label=status_text)
        status_label.set_valign(Gtk.Align.CENTER)
        status_label.add_css_class("dim-label")
        status_row.add_suffix(status_label)
        rpc_group.add(status_row)

        # Enable/disable toggle
        rpc_enabled_row = Adw.SwitchRow()
        rpc_enabled_row.set_title("Enable Discord RPC")
        rpc_enabled_row.set_subtitle(
            "Show what you're listening to on Discord"
        )
        rpc_enabled_row.set_active(_prefs.get("discord_rpc_enabled", True))

        # Status display type
        display_row = Adw.ComboRow()
        display_row.set_title("Status Display")
        display_row.set_subtitle("What appears in the status line under your name")
        display_keys = list(STATUS_DISPLAY_TYPES.keys())
        display_labels = ["App Name (Mixtapes)", "Artist", "Song Title"]
        display_row.set_model(Gtk.StringList.new(display_labels))
        display_row.set_sensitive(rpc_enabled_row.get_active())

        current_display = _prefs.get("discord_rpc_status_display", STATUS_DISPLAY_DEFAULT)
        for i, key in enumerate(display_keys):
            if key == current_display:
                display_row.set_selected(i)
                break

        def on_rpc_toggled(switch, pspec):
            enabled = switch.get_active()
            _prefs["discord_rpc_enabled"] = enabled
            os.makedirs(os.path.dirname(_prefs_path), exist_ok=True)
            with open(_prefs_path, "w") as f:
                _json.dump(_prefs, f)
            display_row.set_sensitive(enabled)
            small_icon_row.set_sensitive(enabled)
            if rpc_adapter:
                rpc_adapter.set_enabled(enabled)
                status_label.set_label(rpc_adapter.status)

        rpc_enabled_row.connect("notify::active", on_rpc_toggled)
        rpc_group.add(rpc_enabled_row)

        def on_display_changed(row, pspec):
            idx = row.get_selected()
            if 0 <= idx < len(display_keys):
                _prefs["discord_rpc_status_display"] = display_keys[idx]
                os.makedirs(os.path.dirname(_prefs_path), exist_ok=True)
                with open(_prefs_path, "w") as f:
                    _json.dump(_prefs, f)
                if rpc_adapter and rpc_adapter._enabled:
                    rpc_adapter.update()

        display_row.connect("notify::selected", on_display_changed)
        rpc_group.add(display_row)

        hide_pause_row = Adw.SwitchRow()
        hide_pause_row.set_title("Hide on Pause")
        hide_pause_row.set_subtitle("Hide Discord RPC when music is paused")
        hide_pause_row.set_active(_prefs.get("discord_rpc_hide_pause_enabled", False))
        hide_pause_row.set_sensitive(rpc_enabled_row.get_active())

        def on_hide_pause_toggled(switch, pspec):
            _prefs["discord_rpc_hide_pause_enabled"] = switch.get_active()
            os.makedirs(os.path.dirname(_prefs_path), exist_ok=True)
            with open(_prefs_path, "w") as f:
                _json.dump(_prefs, f)
            if rpc_adapter and rpc_adapter._enabled:
                rpc_adapter.update()
        
        hide_pause_row.connect("notify::active", on_hide_pause_toggled)
        rpc_group.add(hide_pause_row)

        # Small icon toggle
        small_icon_row = Adw.SwitchRow()
        small_icon_row.set_title("Show Play/Pause Icon")
        small_icon_row.set_subtitle(
            "Display a small play or pause indicator on the album art"
        )
        small_icon_row.set_active(_prefs.get("discord_rpc_small_icon_enabled", True))
        small_icon_row.set_sensitive(rpc_enabled_row.get_active())

        def on_small_icon_toggled(switch, pspec):
            _prefs["discord_rpc_small_icon_enabled"] = switch.get_active()
            os.makedirs(os.path.dirname(_prefs_path), exist_ok=True)
            with open(_prefs_path, "w") as f:
                _json.dump(_prefs, f)
            if rpc_adapter and rpc_adapter._enabled:
                rpc_adapter.update()

        small_icon_row.connect("notify::active", on_small_icon_toggled)
        rpc_group.add(small_icon_row)

        page.add(self._build_scrobbler_group(prefs))

        # Account group was moved to the top of this page; see the
        # _build_account_group call right after `page.add(page)`.

        # Downloads group
        dl_group = Adw.PreferencesGroup()
        dl_group.set_title("Downloads")
        page.add(dl_group)

        from player.downloads import (
            get_preferred_format,
            set_preferred_format,
            get_folder_structure,
            set_folder_structure,
            FORMATS,
            FOLDER_STRUCTURES,
            get_music_dir,
            use_songs_subdir,
            set_use_songs_subdir,
        )

        format_row = Adw.ComboRow()
        format_row.set_title("Audio Format")
        format_row.set_subtitle(f"Songs are saved to {get_music_dir()}")
        format_names = list(FORMATS.keys())
        format_labels = [
            "Opus (smallest)",
            "MP3 (universal)",
            "M4A (Apple)",
            "FLAC (lossless)",
            "OGG (Vorbis)",
        ]
        format_row.set_model(Gtk.StringList.new(format_labels))

        current_fmt = get_preferred_format()
        for i, name in enumerate(format_names):
            if name == current_fmt:
                format_row.set_selected(i)
                break

        def on_format_changed(row, pspec):
            idx = row.get_selected()
            if 0 <= idx < len(format_names):
                set_preferred_format(format_names[idx])

        format_row.connect("notify::selected", on_format_changed)
        dl_group.add(format_row)

        structure_row = Adw.ComboRow()
        structure_row.set_title("Folder Structure")
        structure_row.set_subtitle("How new downloads are organized on disk")
        structure_labels = [
            "Artist / Album / Song",
            "Artist / Song",
            "No folders",
        ]
        structure_row.set_model(Gtk.StringList.new(structure_labels))

        current_structure = get_folder_structure()
        for i, name in enumerate(FOLDER_STRUCTURES):
            if name == current_structure:
                structure_row.set_selected(i)
                break

        def on_structure_changed(row, pspec):
            idx = row.get_selected()
            if not (0 <= idx < len(FOLDER_STRUCTURES)):
                return
            if not set_folder_structure(FOLDER_STRUCTURES[idx]):
                return
            dm = self.player.download_manager
            if getattr(dm, "_downloading", False):
                self.add_toast(
                    "Structure saved. Existing files will be reorganized after downloads finish."
                )
                return
            self.add_toast("Reorganizing downloads...")

            def _run_migration():
                moved, errors = dm.migrate_folder_structure()
                if moved == 0 and errors == 0:
                    msg = "Downloads already organized"
                elif errors:
                    msg = f"Reorganized {moved} file(s); {errors} skipped"
                else:
                    msg = f"Reorganized {moved} file(s)"
                GLib.idle_add(self.add_toast, msg)

            threading.Thread(target=_run_migration, daemon=True).start()

        structure_row.connect("notify::selected", on_structure_changed)
        dl_group.add(structure_row)

        songs_subdir_row = Adw.SwitchRow()
        songs_subdir_row.set_title("Use Songs Subfolder")
        songs_subdir_row.set_subtitle(
            "Place downloads inside a Songs/ subfolder within the music directory"
        )
        songs_subdir_row.set_active(use_songs_subdir())

        def on_songs_subdir_toggled(switch, pspec):
            set_use_songs_subdir(switch.get_active())
            dm = self.player.download_manager
            if getattr(dm, "_downloading", False):
                self.add_toast(
                    "Subfolder setting saved. Existing files will be reorganized after downloads finish."
                )
                return
            self.add_toast("Reorganizing downloads...")

            def _run_migration():
                moved, errors = dm.migrate_folder_structure()
                if moved == 0 and errors == 0:
                    msg = "Downloads already organized"
                elif errors:
                    msg = f"Reorganized {moved} file(s); {errors} skipped"
                else:
                    msg = f"Reorganized {moved} file(s)"
                GLib.idle_add(self.add_toast, msg)

            threading.Thread(target=_run_migration, daemon=True).start()

        songs_subdir_row.connect("notify::active", on_songs_subdir_toggled)
        dl_group.add(songs_subdir_row)

        prefs.add(self._build_lyrics_page())

        prefs.present(self)

    # ── Lyrics preferences ────────────────────────────────────────────
    # One line each on what a provider is actually good at, so ordering
    # the queue is an informed choice rather than trial and error.
    _LYRICS_PROVIDER_BLURBS = {
        "Apple Music": "Word-level timing. Best coverage for Western pop",
        "BetterLyrics": "Word-level timing. Mirrors Apple's database",
        "BiniLyrics": "Word-level timing. Strong on Japanese tracks",
        "NetEase": "Line-synced. Romanization and translation for CJK",
        "LRCLIB": "Line-synced. Large community LRC database",
        "YouTube Music": "Plain text, no timing. Always available when signed in",
    }

    def _build_lyrics_page(self):
        from player import lyrics_prefs

        page = Adw.PreferencesPage()
        page.set_title("Lyrics")
        page.set_icon_name("format-justify-fill-symbolic")

        # ── Search queue ──────────────────────────────────────────────
        queue_group = Adw.PreferencesGroup()
        queue_group.set_title("Search Queue")
        queue_group.set_description(
            "Tried from the top down. Switch one off to skip it."
        )
        page.add(queue_group)

        queue_rows = []

        def rebuild_queue():
            for row in queue_rows:
                queue_group.remove(row)
            queue_rows.clear()
            order = lyrics_prefs.full_provider_order()
            disabled = lyrics_prefs.disabled_providers()
            for i, name in enumerate(order):
                row = Adw.ActionRow()
                row.set_title(f"{i + 1}. {name}")
                row.set_subtitle(self._LYRICS_PROVIDER_BLURBS.get(name, ""))

                up = Gtk.Button(icon_name="go-up-symbolic")
                up.set_valign(Gtk.Align.CENTER)
                up.add_css_class("flat")
                up.set_tooltip_text("Move up")
                up.set_sensitive(i > 0)
                up.connect("clicked", lambda _b, n=name: move(n, -1))
                row.add_suffix(up)

                down = Gtk.Button(icon_name="go-down-symbolic")
                down.set_valign(Gtk.Align.CENTER)
                down.add_css_class("flat")
                down.set_tooltip_text("Move down")
                down.set_sensitive(i < len(order) - 1)
                down.connect("clicked", lambda _b, n=name: move(n, 1))
                row.add_suffix(down)

                switch = Gtk.Switch()
                switch.set_valign(Gtk.Align.CENTER)
                switch.set_active(name not in disabled)
                switch.connect(
                    "notify::active",
                    lambda sw, _p, n=name: toggle(n, sw.get_active()),
                )
                row.add_suffix(switch)
                row.set_activatable_widget(switch)

                queue_group.add(row)
                queue_rows.append(row)

        def move(name, delta):
            order = lyrics_prefs.full_provider_order()
            try:
                i = order.index(name)
            except ValueError:
                return
            j = i + delta
            if not (0 <= j < len(order)):
                return
            order[i], order[j] = order[j], order[i]
            lyrics_prefs.set_provider_order(order)
            rebuild_queue()

        def toggle(name, enabled):
            # Refuse to switch off the last enabled provider — an empty
            # queue silently means "no lyrics, ever", with nothing on
            # screen to explain why.
            if not enabled and len(lyrics_prefs.provider_order()) <= 1:
                self.add_toast("Keep at least one lyrics provider enabled")
                rebuild_queue()
                return
            lyrics_prefs.set_provider_enabled(name, enabled)
            rebuild_queue()

        rebuild_queue()

        # ── Matching ──────────────────────────────────────────────────
        match_group = Adw.PreferencesGroup()
        match_group.set_title("Matching")
        # The explanations live on the group rather than as row subtitles:
        # a wrapped subtitle claims the row's whole natural width and
        # squeezes the combo's value down to an ellipsis.
        match_group.set_description(
            "Quality-aware keeps looking for synced lyrics before settling "
            "for plain text. Strict takes the first hit of any kind."
        )
        page.add(match_group)

        match_keys = [lyrics_prefs.MATCH_QUALITY, lyrics_prefs.MATCH_STRICT]
        match_row = Adw.ComboRow()
        match_row.set_title("When to Stop Searching")
        match_row.set_model(Gtk.StringList.new(["Quality-aware", "Strict"]))
        match_row.set_selected(match_keys.index(lyrics_prefs.match_mode()))

        def on_match_changed(row, _pspec):
            idx = row.get_selected()
            if 0 <= idx < len(match_keys):
                lyrics_prefs.set_match_mode(match_keys[idx])

        match_row.connect("notify::selected", on_match_changed)
        match_group.add(match_row)

        cache_row = Adw.ActionRow()
        cache_row.set_title("Clear Cached Lyrics")
        cache_row.set_subtitle(
            "Queue changes only apply to tracks that aren't cached yet"
        )
        clear_btn = Gtk.Button(label="Clear")
        clear_btn.set_valign(Gtk.Align.CENTER)
        clear_btn.add_css_class("destructive-action")
        clear_btn.connect("clicked", self._on_clear_lyrics_cache)
        cache_row.add_suffix(clear_btn)
        cache_row.set_activatable_widget(clear_btn)
        match_group.add(cache_row)

        # ── Display ───────────────────────────────────────────────────
        display_group = Adw.PreferencesGroup()
        display_group.set_title("Second Line")
        display_group.set_description(
            "An extra line under each lyric. Auto picks a romanization for "
            "non-Latin scripts and background vocals otherwise. What's "
            "available depends on the provider."
        )
        page.add(display_group)

        second_keys = ["off", "auto", "romanization", "translation", "background"]
        second_row = Adw.ComboRow()
        second_row.set_title("Show")
        second_row.set_model(Gtk.StringList.new([
            "Off", "Auto", "Romanization", "Translation", "Background",
        ]))
        second_row.set_selected(
            second_keys.index(lyrics_prefs.second_line_mode())
        )

        def on_second_changed(row, _pspec):
            idx = row.get_selected()
            if 0 <= idx < len(second_keys):
                lyrics_prefs.set_second_line_mode(second_keys[idx])
                self._apply_lyrics_display_prefs()

        second_row.connect("notify::selected", on_second_changed)
        display_group.add(second_row)

        effects_group = Adw.PreferencesGroup()
        effects_group.set_title("Effects")
        effects_group.set_description(
            "Subtle fades each word in over the time it's actually held "
            "and grows the active line. Full adds a glow on the active "
            "line and blurs the lines furthest from it."
        )
        page.add(effects_group)

        effect_keys = ["off", "subtle", "full"]
        effect_row = Adw.ComboRow()
        effect_row.set_title("Level")
        effect_row.set_model(Gtk.StringList.new(["Off", "Subtle", "Full"]))
        effect_row.set_selected(effect_keys.index(lyrics_prefs.effects_level()))

        def on_effect_changed(row, _pspec):
            idx = row.get_selected()
            if 0 <= idx < len(effect_keys):
                lyrics_prefs.set_effects_level(effect_keys[idx])
                grown = getattr(self, "_lyrics_grown_row", None)
                if grown is not None:
                    grown.set_sensitive(effect_keys[idx] != "off")
                self._apply_lyrics_display_prefs()

        effect_row.connect("notify::selected", on_effect_changed)
        effects_group.add(effect_row)

        sweep_row = Adw.SwitchRow()
        sweep_row.set_title("Emulate Word Timing")
        sweep_row.set_subtitle(
            "On sources with no word timing, move the highlight across the "
            "line instead of lighting the whole line at once"
        )
        sweep_row.set_active(lyrics_prefs.line_sweep())

        def on_sweep_toggled(switch, _pspec):
            lyrics_prefs.set_line_sweep(switch.get_active())
            self._apply_lyrics_display_prefs()

        sweep_row.connect("notify::active", on_sweep_toggled)
        effects_group.add(sweep_row)

        # ── Text size ─────────────────────────────────────────────────
        size_group = Adw.PreferencesGroup()
        size_group.set_title("Text Size")
        size_group.set_description(
            "Resting size of the lyric column, and how much bigger the "
            "line being sung is drawn. The active line is scaled when it "
            "is painted, so growing it never changes the row's height or "
            "disturbs the scrolling."
        )
        page.add(size_group)

        base_row = Adw.ActionRow()
        base_row.set_title("Lyrics Size")
        base_scale = Gtk.Scale.new_with_range(
            Gtk.Orientation.HORIZONTAL,
            lyrics_prefs.FONT_SCALE_MIN, lyrics_prefs.FONT_SCALE_MAX, 0.05,
        )
        base_scale.set_value(lyrics_prefs.font_scale())
        base_scale.set_draw_value(True)
        base_scale.set_value_pos(Gtk.PositionType.RIGHT)
        base_scale.set_digits(2)
        base_scale.set_size_request(220, -1)
        base_scale.set_valign(Gtk.Align.CENTER)
        base_scale.add_mark(
            lyrics_prefs.FONT_SCALE_DEFAULT, Gtk.PositionType.BOTTOM, None
        )
        base_row.add_suffix(base_scale)

        def on_base_size(scale):
            lyrics_prefs.set_font_scale(scale.get_value())
            self._apply_lyrics_display_prefs()

        base_scale.connect("value-changed", on_base_size)
        size_group.add(base_row)

        grown_row = Adw.ActionRow()
        grown_row.set_title("Active Line Size")
        grown_scale = Gtk.Scale.new_with_range(
            Gtk.Orientation.HORIZONTAL,
            lyrics_prefs.ACTIVE_SCALE_MIN, lyrics_prefs.ACTIVE_SCALE_MAX, 0.01,
        )
        grown_scale.set_value(lyrics_prefs.active_scale())
        grown_scale.set_draw_value(True)
        grown_scale.set_value_pos(Gtk.PositionType.RIGHT)
        grown_scale.set_digits(2)
        grown_scale.set_size_request(220, -1)
        grown_scale.set_valign(Gtk.Align.CENTER)
        grown_scale.add_mark(
            lyrics_prefs.ACTIVE_SCALE_DEFAULT, Gtk.PositionType.BOTTOM, None
        )
        grown_row.add_suffix(grown_scale)

        def on_grown_size(scale):
            lyrics_prefs.set_active_scale(scale.get_value())
            self._apply_lyrics_display_prefs()

        grown_scale.connect("value-changed", on_grown_size)
        # Growing only happens at Subtle and above.
        grown_row.set_sensitive(lyrics_prefs.effects_level() != "off")
        size_group.add(grown_row)

        self._lyrics_grown_row = grown_row

        return page

    def _lyrics_views(self):
        """Both live LyricsView instances — the mobile expanded player's
        and the desktop cover view's. Either may not exist yet."""
        views = []
        for holder in ("expanded_player", "desktop_cover_view"):
            view = getattr(getattr(self, holder, None), "lyrics_view", None)
            if view is not None:
                views.append(view)
        return views

    def _apply_lyrics_display_prefs(self):
        for view in self._lyrics_views():
            view.apply_display_prefs()

    def _on_clear_lyrics_cache(self, _button):
        from player.lyrics_cache import LyricsCache

        try:
            removed = self.player.client._lyrics_cache.clear_all()
        except Exception:
            removed = LyricsCache().clear_all()
        self.add_toast(
            f"Cleared {removed} cached track(s)" if removed
            else "No cached lyrics to clear"
        )
        for view in self._lyrics_views():
            view.refresh()

    def _build_account_group(self, prefs, is_authed):
        """Build the 'Account' Adw.PreferencesGroup that leads the
        Preferences page — a big avatar row with the signed-in account
        name/handle, and a Sign In/Sign Out button."""
        group = Adw.PreferencesGroup()
        group.set_title("Account")

        row = Adw.ActionRow()

        # Avatar prefix
        avatar = Adw.Avatar.new(40, "", False)
        row.add_prefix(avatar)

        # Suffix button — Sign Out (destructive) or Sign In (suggested).
        btn = Gtk.Button(label="Sign Out" if is_authed else "Sign In")
        btn.set_valign(Gtk.Align.CENTER)
        if is_authed:
            btn.add_css_class("destructive-action")
            btn.connect("clicked", self.on_logout_clicked, prefs)
        else:
            btn.add_css_class("suggested-action")
            btn.connect(
                "clicked", lambda b, p: (p.close(), self.check_auth()), prefs
            )
        row.add_suffix(btn)

        if not is_authed:
            row.set_title("Not signed in")
            row.set_subtitle(
                "Sign in to YouTube Music to access your library"
            )
            group.add(row)
            return group

        # Signed in — fill from the cached account info if we already
        # have it, otherwise show a placeholder and repaint when the
        # background fetch lands.
        info = None
        try:
            info = self.player.client.get_account_info()
        except Exception:
            info = None

        def _apply(_info):
            name = (_info or {}).get("accountName") or "Signed in"
            handle = (_info or {}).get("channelHandle") or ""
            photo = (_info or {}).get("accountPhotoUrl") or ""
            row.set_title(name)
            row.set_subtitle(handle or "YouTube Music account")
            avatar.set_text(name)
            if photo:
                from ui.utils import (
                    read_thumb_cache, write_thumb_cache, get_high_res_url,
                )
                hi_url = get_high_res_url(photo) or photo

                def _work():
                    data = read_thumb_cache(hi_url)
                    if not data:
                        try:
                            import requests
                            resp = requests.get(
                                hi_url,
                                headers={"User-Agent": "Mozilla/5.0"},
                                timeout=10,
                            )
                            resp.raise_for_status()
                            data = resp.content
                            write_thumb_cache(hi_url, data)
                        except Exception:
                            return

                    def _paint():
                        try:
                            from gi.repository import GdkPixbuf
                            loader = GdkPixbuf.PixbufLoader()
                            loader.write(data)
                            loader.close()
                            pb = loader.get_pixbuf()
                            if pb is not None:
                                avatar.set_custom_image(
                                    Gdk.Texture.new_for_pixbuf(pb)
                                )
                        except Exception:
                            pass
                        return False

                    GLib.idle_add(_paint)

                threading.Thread(target=_work, daemon=True).start()

        if info:
            _apply(info)
        else:
            row.set_title("Loading account…")
            row.set_subtitle("")

            def _fetch():
                data = self.player.client.get_account_info()
                GLib.idle_add(_apply, data or {})

            threading.Thread(target=_fetch, daemon=True).start()

        group.add(row)
        return group

    def _build_scrobbler_group(self, prefs):
        """Build the 'Scrobbling' Adw.PreferencesGroup: a master switch plus
        a connect/disconnect row per service."""
        import json as _json
        from player.scrobbler import SERVICE_LABELS

        group = Adw.PreferencesGroup()
        group.set_title("Scrobbling")
        group.set_description(
            "Submit the tracks you play to Last.fm and ListenBrainz"
        )

        adapter = getattr(self.player, "scrobbler", None)
        if adapter is None:
            unavailable = Adw.ActionRow()
            unavailable.set_title("Unavailable")
            unavailable.set_subtitle("The scrobbler failed to start")
            group.add(unavailable)
            return group

        prefs_path = self._prefs_path()

        def _read_pref(key, default):
            try:
                if os.path.exists(prefs_path):
                    with open(prefs_path) as f:
                        return _json.load(f).get(key, default)
            except Exception:
                pass
            return default

        def _save_pref(key, value):
            data = {}
            try:
                if os.path.exists(prefs_path):
                    with open(prefs_path) as f:
                        data = _json.load(f)
            except Exception:
                data = {}
            data[key] = value
            os.makedirs(os.path.dirname(prefs_path), exist_ok=True)
            with open(prefs_path, "w") as f:
                _json.dump(data, f)

        def _toast(message):
            prefs.add_toast(Adw.Toast.new(message))

        enabled_row = Adw.SwitchRow()
        enabled_row.set_title("Enable Scrobbling")
        enabled_row.set_subtitle(
            "Submit a play once you've heard half a track, or four minutes"
        )
        enabled_row.set_active(adapter.get_enabled())
        enabled_row.connect(
            "notify::active",
            lambda switch, _p: (
                _save_pref("scrobble_enabled", switch.get_active()),
                adapter.set_enabled(switch.get_active()),
            ),
        )
        group.add(enabled_row)

        now_playing_row = Adw.SwitchRow()
        now_playing_row.set_title('Send "Now Playing"')
        now_playing_row.set_subtitle(
            "Show the current track on your profile while it plays"
        )
        now_playing_row.set_active(
            _read_pref("scrobble_now_playing", True)
        )
        now_playing_row.connect(
            "notify::active",
            lambda switch, _p: (
                _save_pref("scrobble_now_playing", switch.get_active()),
                adapter.set_now_playing_enabled(switch.get_active()),
            ),
        )
        group.add(now_playing_row)

        rows = {}
        buttons = {}
        for service in ("lastfm", "listenbrainz"):
            row = Adw.ActionRow()
            row.set_title(SERVICE_LABELS[service])
            button = Gtk.Button()
            button.set_valign(Gtk.Align.CENTER)
            row.add_suffix(button)
            group.add(row)
            rows[service] = row
            buttons[service] = button

        pending_row = Adw.ActionRow()
        pending_row.set_title("Queued Listens")
        group.add(pending_row)

        def _refresh():
            for service, row in rows.items():
                button = buttons[service]
                connected = adapter.is_connected(service)
                if service == "lastfm" and not adapter.lastfm_configured():
                    row.set_subtitle(
                        "This build ships without Last.fm API credentials"
                    )
                    button.set_label("Connect")
                    button.set_sensitive(False)
                    continue
                button.set_sensitive(True)
                if connected:
                    name = adapter.username(service)
                    row.set_subtitle(
                        f"Connected as {name}" if name else "Connected"
                    )
                    button.set_label("Disconnect")
                    button.remove_css_class("suggested-action")
                    button.add_css_class("destructive-action")
                else:
                    # A revoked credential disconnects the service from the
                    # worker thread, so say why rather than just "not connected".
                    error = adapter.last_error or ""
                    label = SERVICE_LABELS[service]
                    row.set_subtitle(
                        error if error.startswith(label) else "Not connected"
                    )
                    button.set_label("Connect")
                    button.remove_css_class("destructive-action")
                    button.add_css_class("suggested-action")
            waiting = adapter.pending_count()
            pending_row.set_visible(bool(waiting))
            pending_row.set_subtitle(
                f"{waiting} play saved while offline, retried automatically"
                if waiting == 1
                else f"{waiting} plays saved while offline, retried automatically"
            )
            return False

        def _open_uri(uri):
            def _done(launcher, result):
                try:
                    launcher.launch_finish(result)
                except Exception as e:
                    print(f"[SCROBBLE] could not open {uri}: {e}")
                    _toast("Could not open your browser")

            # Held on the window: the launcher has to outlive this call for
            # the async portal request to complete.
            self._scrobbler_uri_launcher = Gtk.UriLauncher(uri=uri)
            self._scrobbler_uri_launcher.launch(self, None, _done)

        # Last.fm's desktop flow: ask for a token, let the user approve it in
        # a browser, then poll until Last.fm hands back a session key.
        def _lastfm_connect():
            # The token request is a round trip. Lock the button so a second
            # click cannot start a competing authorization.
            buttons["lastfm"].set_sensitive(False)

            def _work():
                try:
                    token, url = adapter.lastfm_request_token()
                except Exception as e:
                    GLib.idle_add(_toast, f"Last.fm: {e}")
                    GLib.idle_add(_refresh)
                    return
                GLib.idle_add(_refresh)
                GLib.idle_add(_await_approval, token, url)

            threading.Thread(target=_work, daemon=True).start()

        def _await_approval(token, url):
            dialog = Adw.AlertDialog(
                heading="Authorize Mixtapes",
                body=(
                    "Approve access in the browser tab that just opened. "
                    "This closes on its own once Last.fm confirms."
                ),
            )
            dialog.add_response("cancel", "Cancel")
            dialog.set_close_response("cancel")
            spinner = Adw.Spinner() if hasattr(Adw, "Spinner") else Gtk.Spinner()
            if isinstance(spinner, Gtk.Spinner):
                spinner.start()
            spinner.set_size_request(32, 32)
            spinner.set_margin_top(6)
            dialog.set_extra_child(spinner)

            state = {"done": False}

            def _on_response(_dialog, _response):
                state["done"] = True

            dialog.connect("response", _on_response)
            dialog.present(self)
            _open_uri(url)

            def _poll():
                try:
                    _poll_until_authorized()
                finally:
                    adapter.close_thread_session()

            def _poll_until_authorized():
                # Last.fm keeps the token valid for an hour. Allow enough
                # room to log in and clear 2FA before giving up.
                deadline = time.time() + 300
                while not state["done"] and time.time() < deadline:
                    time.sleep(2.0)
                    if state["done"]:
                        return
                    try:
                        name = adapter.lastfm_finish_auth(token)
                    except Exception:
                        # Last.fm reports an unauthorized token until the
                        # user presses Allow, so keep waiting.
                        continue
                    state["done"] = True
                    GLib.idle_add(dialog.close)
                    GLib.idle_add(_refresh)
                    GLib.idle_add(
                        _toast,
                        f"Scrobbling to Last.fm as {name}" if name
                        else "Connected to Last.fm",
                    )
                    return
                if not state["done"]:
                    state["done"] = True
                    GLib.idle_add(dialog.close)
                    GLib.idle_add(_toast, "Last.fm authorization timed out")

            threading.Thread(target=_poll, daemon=True).start()
            return False

        def _listenbrainz_connect():
            dialog = Adw.AlertDialog(
                heading="Connect ListenBrainz",
                body="Paste the user token from your ListenBrainz settings.",
            )
            entry = Adw.PasswordEntryRow(title="User Token")
            listbox = Gtk.ListBox()
            listbox.add_css_class("boxed-list")
            listbox.set_selection_mode(Gtk.SelectionMode.NONE)
            listbox.append(entry)

            link = Gtk.Button(label="Get Your Token")
            link.set_halign(Gtk.Align.CENTER)
            link.add_css_class("flat")
            link.connect(
                "clicked",
                lambda _b: _open_uri("https://listenbrainz.org/settings/"),
            )

            box = Gtk.Box(orientation=Gtk.Orientation.VERTICAL, spacing=8)
            box.set_margin_top(6)
            box.append(listbox)
            box.append(link)
            dialog.set_extra_child(box)

            dialog.add_response("cancel", "Cancel")
            dialog.add_response("connect", "Connect")
            dialog.set_response_appearance(
                "connect", Adw.ResponseAppearance.SUGGESTED
            )
            dialog.set_default_response("connect")
            dialog.set_close_response("cancel")

            def _on_response(_dialog, response):
                if response != "connect":
                    return
                token = entry.get_text().strip()

                def _work():
                    try:
                        name = adapter.listenbrainz_connect(token)
                    except Exception as e:
                        GLib.idle_add(_toast, f"ListenBrainz: {e}")
                        return
                    GLib.idle_add(_refresh)
                    GLib.idle_add(
                        _toast,
                        f"Scrobbling to ListenBrainz as {name}" if name
                        else "Connected to ListenBrainz",
                    )

                threading.Thread(target=_work, daemon=True).start()

            dialog.connect("response", _on_response)
            dialog.present(self)

        def _on_service_clicked(button, service):
            if adapter.is_connected(service):
                adapter.disconnect(service)
                _refresh()
                _toast(f"Disconnected from {SERVICE_LABELS[service]}")
            elif service == "lastfm":
                _lastfm_connect()
            else:
                _listenbrainz_connect()

        for service, button in buttons.items():
            button.connect("clicked", _on_service_clicked, service)

        _refresh()
        return group

    def on_logout_clicked(self, btn, prefs_window=None):
        from api.client import MusicClient

        client = MusicClient()
        if client.logout():
            if prefs_window is not None:
                prefs_window.close()
            # Clear library UI immediately
            if hasattr(self, "library_page"):
                self.library_page.clear()
            # Reset the avatar button back to "Not signed in" so it
            # doesn't keep showing the previous user's photo/name.
            self._reset_avatar_profile()
            # Trigger auth check to show login dialog
            self.check_auth()

    def _reset_avatar_profile(self):
        """Clear the avatar-menu widgets back to their signed-out state.
        Called on logout and after a successful login (before the fresh
        account info lands)."""
        self._avatar_small.set_custom_image(None)
        self._avatar_small.set_text("")
        self._avatar_large.set_custom_image(None)
        self._avatar_large.set_text("")
        self._avatar_name_label.set_label("Not signed in")
        self._avatar_handle_label.set_label("")
        self._avatar_handle_label.set_visible(False)
        self._set_account_actions_authed(False)

    def init_pages(self):

        # patching Adw.NavigationView's push() to not push into the same page as the current visible page
        # takes the original push() function, and replaces it with additional checks before calling the original push() function
        if not getattr(Adw.NavigationView, '_push_patched', False):
            original_push = Adw.NavigationView.push

            # if both pages have tags, the patch_push checks if the tags between the visible page and the page to push
            # if the tags are the same, then no push is called
            #
            # if tag checks is invalid, then the patch_push checkes the title
            # if titles are the same, then no push is called
            def patch_push(self, page):
                def tag_diff(page_a, page_b) -> bool:
                    tag_a = page_a.get_tag()
                    tag_b = page_b.get_tag()
                    return tag_a != None and tag_b != None and tag_a != tag_b
                
                def title_match(page_a, page_b) -> bool:
                    title_a = page_a.get_title()
                    title_b = page_b.get_title()
                    return title_a == title_b

                current = self.get_visible_page()

                if current is None:
                    original_push(self, page)
                    return
                
                if tag_diff(current, page) or not title_match(current, page):
                    original_push(self, page)
                    return

            Adw.NavigationView.push = patch_push
            Adw.NavigationView._push_patched = True

        # PlaylistPage imported at top level now

        # Create Pages
        # Refactored to Single Global Header architecture
        # Each tab is just a NavigationView wrapping the content

        def create_tab_nav(page_content, title, icon, name):
            # Nav Page & View
            # We wrap content in NavigationPage because NavigationView requires it
            nav_page = Adw.NavigationPage(child=page_content, title=title)
            nav_page.set_tag("root")  # Tag for resetting
            nav_view = Adw.NavigationView()
            nav_view.add(nav_page)

            # Connect to page changes to update Back Button
            nav_view.connect("notify::visible-page", self.update_back_button_visibility)

            def on_push(nav_view):
                def tag_match(page_a, page_b) -> bool:
                    tag_a = page_a.get_tag()
                    tag_b = page_b.get_tag()
                    return tag_a != None and tag_b != None and tag_a == tag_b
                
                def title_match(page_a, page_b) -> bool:
                    title_a = page_a.get_title()
                    title_b = page_b.get_title()
                    return title_a == title_b

                stack = list(nav_view.get_navigation_stack())
                current_page = nav_view.get_visible_page()

                # just removing the first matching page should be enough 
                # because there shouldnt be more than one that exists in the current stack
                for i, p in enumerate(stack[:max(len(stack)-1, 0)]):
                    if tag_match(p, current_page) or title_match(p, current_page):
                        nav_view.replace(stack[:i] + stack[i+1:])
                        return

            nav_view.connect("pushed", on_push)

            return nav_view

        from ui.pages.home import HomePage
        from ui.pages.library import LibraryPage
        from ui.pages.search import SearchPage

        # Instantiate Pages
        self.home_page = HomePage(self.player)
        self.library_page = LibraryPage(self.player, self.open_playlist)
        search_page = SearchPage(self.player, self.open_playlist)
        self.search_page = search_page  # Store for global key controller

        self.tab_header_widgets = []  # Init list

        # Add to Stack and Configure Pages
        page_home = self.view_stack.add_named(
            create_tab_nav(self.home_page, "Home", "user-home-symbolic", "home"), "home"
        )
        page_home.set_title("Home")
        page_home.set_icon_name("user-home-symbolic")

        page_lib = self.view_stack.add_named(
            create_tab_nav(
                self.library_page, "Library", "media-optical-symbolic", "library"
            ),
            "library",
        )
        page_lib.set_title("Library")
        page_lib.set_icon_name("media-optical-symbolic")

        page_lib.set_icon_name("media-optical-symbolic")

        page_search = self.view_stack.add_named(
            create_tab_nav(search_page, "Explore", "compass2-symbolic", "search"),
            "search",
        )
        page_search.set_title("Explore")
        page_search.set_icon_name("compass2-symbolic")

        self.previous_view_stack_item = "home"

    def set_header_title(self, title):
        pass

    def _get_page_content(self, tab_name):
        # Helper to traverse: NavView -> NavPage -> ToolbarView -> Content
        nav_view = self.view_stack.get_child_by_name(tab_name)
        if isinstance(nav_view, Adw.NavigationView):
            # We assume the root page of the nav view is our tab page
            # We stored page instances in init_pages, so direct traversal is not needed for Search/Library.
            pass
        return None

    def on_window_key_pressed(self, controller, keyval, keycode, state):
        # Handle Escape key for Back / Close Search
        if keyval == Gdk.KEY_Escape:
            if self.search_bar.get_search_mode():
                # Manually close it and stop propagation
                self.search_bar.set_search_mode(False)
                # Clear focus from entry to ensure next keys are handled by the window
                self.grab_focus()
                return True

            if self.back_btn.get_visible():
                self.on_back_clicked(None)
                return True
            return False

        # Redirection logic for Global Search (Alphanumeric characters)
        # 1. Ignore if focus is in an entry
        focus = self.get_focus()
        if isinstance(focus, (Gtk.Entry, Gtk.SearchEntry, Gtk.TextView, Gtk.Editable)):
            return False

        if keyval == Gdk.KEY_space:
            self.player_bar.on_play_clicked(None)
            return True

        # 2. DECIDE if it's a searchable character
        uni = Gdk.keyval_to_unicode(keyval)
        if uni == 0:
            return False
        char = chr(uni)
        if not char.isprintable():
            return False

        # 3. Ignore control/alt/meta keys
        mask = state & (
            Gdk.ModifierType.CONTROL_MASK
            | Gdk.ModifierType.ALT_MASK
            | Gdk.ModifierType.META_MASK
        )
        if mask:
            return False

        # 4. Context-Aware Redirection: If NOT in a filterable playlist, switch tab first
        if not self._get_active_filterable_child():
            if self.view_stack.get_visible_child_name() != "search":
                # Ensure we switch tab before SearchBar captures the character
                self.view_stack.set_visible_child_name("search")

            # Ensure search tab is at root (results view)
            nav = self.view_stack.get_child_by_name("search")
            if isinstance(nav, Adw.NavigationView):
                root_page = nav.get_visible_page()
                if root_page and nav.get_previous_page(root_page):
                    nav.pop_to_tag("root")

        # Manually trigger search mode and insert the character
        # This avoids the "ignored first character" bug during tab switches
        # A flag is used to ignore the intermediate search-changed emitted by set_text()
        # since replacing non-empty text emits search-changed twice
        self.search_bar.set_search_mode(True)
        self.search_entry.grab_focus()
        self._replacing_search_text = True
        self.search_entry.set_text(char)
        self._replacing_search_text = False
        self.search_entry.set_position(-1)  # Move cursor to end
        return True

    def on_global_search_changed(self, entry):
        text = entry.get_text()
        if not text and getattr(self, "_replacing_search_text", False):
            return

        # Context-Aware Search Logic (Double check redirection here too)
        filterable_child = self._get_active_filterable_child()
        if filterable_child:
            filterable_child.filter_content(text)
        else:
            # Global Search Redirection (Safety fallback)
            if self.view_stack.get_visible_child_name() != "search":
                GLib.idle_add(self.view_stack.set_visible_child_name, "search")

            nav = self.view_stack.get_child_by_name("search")
            if isinstance(nav, Adw.NavigationView):
                root_page = nav.get_visible_page()
                if root_page and nav.get_previous_page(root_page):
                    nav.pop_to_tag("root")

            if hasattr(self, "search_page"):
                self.search_page.on_external_search(text)

    def on_search_stop(self, entry):
        self.search_bar.set_search_mode(False)
        # Crucial: Clear focus so the next Esc goes to the Window Controller
        self.grab_focus()

        filterable_child = self._get_active_filterable_child()
        if filterable_child:
            filterable_child.filter_content("")

    def on_search_mode_changed(self, search_bar, param):
        mode = search_bar.get_search_mode()

        if mode:
            # Enabling search
            self.search_entry.grab_focus()

            # If we are NOT in a playlist, switch to Explore tab
            filterable = self._get_active_filterable_child()
            if not filterable:
                if self.view_stack.get_visible_child_name() != "search":
                    # Use idle_add to avoid issues with current signal processing
                    GLib.idle_add(self.view_stack.set_visible_child_name, "search")

                # Reset search view to root
                nav = self.view_stack.get_child_by_name("search")
                if isinstance(nav, Adw.NavigationView):
                    root_page = nav.get_visible_page()
                    if root_page and nav.get_previous_page(root_page):
                        nav.pop_to_tag("root")

    # on_search_btn_clicked removed (replaced by binding)

    def open_playlist(self, playlist_id, initial_data=None):
        # Collapse the cover view so the pushed page is visible.
        self._dismiss_cover_if_open()
        # Close search bar when navigating to a detail page
        if self.search_bar.get_search_mode():
            self.search_bar.set_search_mode(False)

        # Find active navigation view
        active_nav = self.view_stack.get_visible_child()
        if not isinstance(active_nav, Adw.NavigationView):
            print("Error: Active view is not a NavigationView")
            return

        # Create fresh playlist page (to ensure clean state and avoid parent issues)
        # We need to pass self.network_client? No, PlaylistPage creates its own.
        # We need self.player.
        # We need self.player.
        from ui.pages.playlist import PlaylistPage

        playlist_page = PlaylistPage(self.player)
        # Set playlist_id BEFORE push so the header-bar refresh button's
        # visibility check (fires on notify::visible-page) sees a real id
        # instead of None. Without this, the button stays hidden until the
        # next navigation event.
        playlist_page.playlist_id = playlist_id

        # Wrap in NavigationPage
        # PlaylistPage already has a ToolbarView/Header internally.
        # Adw.NavigationView expects Adw.NavigationPage.
        # Adw.NavigationPage expects a child widget.
        nav_page = Adw.NavigationPage(child=playlist_page, title=f"Playlist_{playlist_id}")

        # Load data
        def _on_shown(page):
            playlist_page.load_playlist(playlist_id, initial_data)

        nav_page.connect("shown", _on_shown)

        # Push to stack
        active_nav.push(nav_page)

        # Connect title change signal
        playlist_page.connect(
            "header-title-changed", self.on_playlist_header_title_changed
        )

        # Check if we are in mobile mode (compact) - Force true if width < 500
        # self.view_switcher_bar.get_reveal() might be delayed?
        width = self.get_width()
        if width < 500:
            playlist_page.set_compact_mode(True)
        elif hasattr(self, "view_switcher_bar") and self.view_switcher_bar.get_reveal():
            playlist_page.set_compact_mode(True)

        # Connect tab re-click logic if not already done?
        # (This is handled globally in init_pages now)

        # Note: We don't need to manually update window title or back button.
        # Adw.NavigationView handles the transition.
        # PlaylistPage's internal header will show a back button IF it's an Adw.HeaderBar
        # AND we are using Adw.NavigationView.
        # BUT: PlaylistPage has `self.header_bar = Adw.HeaderBar()`.
        # When inside NavigationView, this header should automatically get a back button.
        pass

    def on_playlist_back(self):
        # Called when playlist internal back is triggered (if any)
        # We rely on NavView pop.
        pass

    def open_artist(self, channel_id, initial_name=None):
        # Collapse the cover view so the pushed page is visible.
        self._dismiss_cover_if_open()
        # Uploaded artists can't be opened as regular artists
        if channel_id and channel_id.startswith("FEmusic_library_privately_owned"):
            self._open_upload_artist(channel_id, initial_name or "Artist")
            return

        # Close search bar when navigating to a detail page
        if self.search_bar.get_search_mode():
            self.search_bar.set_search_mode(False)

        # Find active navigation view
        active_nav = self.view_stack.get_visible_child()
        if not isinstance(active_nav, Adw.NavigationView):
            print("Error: Active view is not a NavigationView")
            return
        from ui.pages.artist import ArtistPage

        # Create fresh artist page
        artist_page = ArtistPage(self.player, self.open_playlist)

        nav_page = Adw.NavigationPage(
            child=artist_page, title=initial_name if initial_name else f"Artist_{channel_id}"
        )

        active_nav.push(nav_page)

        artist_page.load_artist(channel_id, initial_name)

        # Connect title change
        artist_page.connect(
            "header-title-changed", self.on_playlist_header_title_changed
        )  # Reuse same handler

    def open_discography(
        self, channel_id, title, browse_id=None, params=None, initial_items=None
    ):
        if self.search_bar.get_search_mode():
            self.search_bar.set_search_mode(False)

        active_nav = self.view_stack.get_visible_child()
        if not isinstance(active_nav, Adw.NavigationView):
            print("Error: Active view is not a NavigationView")
            return

        from ui.pages.discography import DiscographyPage

        disco_page = DiscographyPage(self.player, self.open_playlist)
        disco_page.connect(
            "header-title-changed", self.on_playlist_header_title_changed
        )

        nav_page = Adw.NavigationPage(child=disco_page, title=title)

        active_nav.push(nav_page)

        disco_page.load_discography(channel_id, title, browse_id, params, initial_items)

    def open_mood(self, params, title):
        if self.search_bar.get_search_mode():
            self.search_bar.set_search_mode(False)

        active_nav = self.view_stack.get_visible_child()
        if not isinstance(active_nav, Adw.NavigationView):
            print("Error: Active view is not a NavigationView")
            return

        from ui.pages.mood import MoodPage

        mood_page = MoodPage(self.player, self.open_playlist)
        mood_page.connect("header-title-changed", self.on_playlist_header_title_changed)

        nav_page = Adw.NavigationPage(child=mood_page, title=title)

        active_nav.push(nav_page)

        mood_page.load_mood(params, title)

    def open_all_moods(self, items, title):
        if self.search_bar.get_search_mode():
            self.search_bar.set_search_mode(False)

        active_nav = self.view_stack.get_visible_child()
        if not isinstance(active_nav, Adw.NavigationView):
            print("Error: Active view is not a NavigationView")
            return

        from ui.pages.all_moods import AllMoodsPage

        all_moods_page = AllMoodsPage(items, title)
        all_moods_page.connect(
            "header-title-changed", self.on_playlist_header_title_changed
        )

        display_title = f"All {title}"
        if title == "Moods & Moments":
            display_title = "All Moods & Moments"

        nav_page = Adw.NavigationPage(child=all_moods_page, title=display_title)
        active_nav.push(nav_page)

    def open_category(self, params, title):
        if self.search_bar.get_search_mode():
            self.search_bar.set_search_mode(False)

        active_nav = self.view_stack.get_visible_child()
        if not isinstance(active_nav, Adw.NavigationView):
            return

        from ui.pages.category import CategoryPage

        cat_page = CategoryPage(self.player, self.open_playlist)
        cat_page.connect("header-title-changed", self.on_playlist_header_title_changed)

        nav_page = Adw.NavigationPage(child=cat_page, title=title)
        active_nav.push(nav_page)

        cat_page.load_category(params, title)

    def on_player_bar_artist_click(self):
        # Try to get artist ID from the current queue track's data first
        idx = self.player.current_queue_index
        if 0 <= idx < len(self.player.queue):
            track = self.player.queue[idx]
            artists = track.get("artists", [])
            if artists and isinstance(artists, list):
                artist = artists[0]
                if isinstance(artist, dict) and artist.get("id"):
                    aid = artist["id"]
                    name = artist.get("name", "Artist")
                    # Upload artists can't be opened as regular artists
                    if aid.startswith("FEmusic_library_privately_owned"):
                        self._open_upload_artist(aid, name)
                    else:
                        self.open_artist(aid, name)
                    return

        # Fallback: resolve via get_song API (won't work for uploaded songs)
        vid = self.player.current_video_id
        if vid:
            threading.Thread(
                target=self._resolve_artist_from_player, daemon=True
            ).start()

    def _open_upload_artist(self, browse_id, name):
        """Open an uploaded artist as a pseudo-playlist."""
        if hasattr(self, "uploads_page"):
            # Use the UploadsPage's artist handler
            self.uploads_page._on_artist_activated(
                None,
                type(
                    "Row", (), {"artist_data": {"browseId": browse_id, "artist": name}}
                )(),
            )
        elif hasattr(self, "library_page") and hasattr(
            self.library_page, "uploads_page"
        ):
            self.library_page.uploads_page._on_artist_activated(
                None,
                type(
                    "Row", (), {"artist_data": {"browseId": browse_id, "artist": name}}
                )(),
            )

    def _resolve_artist_from_player(self):
        vid = self.player.current_video_id
        if not vid:
            return

        from api.client import MusicClient

        client = MusicClient()
        song_data = client.get_song(vid)
        if song_data and "videoDetails" in song_data:
            channel_id = song_data["videoDetails"].get("channelId")
            if channel_id:
                artist_name = song_data["videoDetails"].get("author", "Artist")
                GObject.idle_add(self.open_artist, channel_id, artist_name)

    def on_player_bar_album_click(self):
        print("Player Bar Album Clicked")
        threading.Thread(target=self._resolve_album_from_player).start()

    def _resolve_album_from_player(self):
        vid = self.player.current_video_id
        if not vid:
            return

        # First check if the current track object in queue has the album ID natively
        track = None
        if 0 <= self.player.current_queue_index < len(self.player.queue):
            track = self.player.queue[self.player.current_queue_index]

        album_id = None
        album_name = "Album"

        if track and "album" in track and track["album"]:
            album = track["album"]
            if isinstance(album, dict):
                album_id = album.get("id")
                album_name = album.get("name", album_name)
            elif isinstance(album, str):
                album_name = album

        if not album_id:
            # Fall back to fetching watch playlist to see if it belongs to an album
            from api.client import MusicClient

            client = MusicClient()
            if client.api:
                try:
                    res = client.api.get_watch_playlist(videoId=vid)
                    tracks = res.get("tracks", [])
                    if tracks and "album" in tracks[0] and tracks[0]["album"]:
                        album = tracks[0]["album"]
                        if isinstance(album, dict):
                            album_id = album.get("id")
                            album_name = album.get("name", "Album")
                        elif isinstance(album, str):
                            album_name = album
                except Exception as e:
                    print(f"Failed to resolve album: {e}")

        if album_id:
            # Check if it starts with 'MPREb'
            if album_id.startswith("MPREb_"):
                # Get album, then take the audioPlaylistId
                from api.client import MusicClient

                client = MusicClient()
                playlist_id = client.api.get_album(album_id).get("audioPlaylistId")
                GObject.idle_add(self.open_playlist, playlist_id, {"title": album_name})
            else:
                # It's an implied playlist ID or similar
                GObject.idle_add(self.open_playlist, album_id, {"title": album_name})
        else:
            print("No album found for the current track.")

    def on_sidebar_row_selected(self, box, row):
        if row:
            # Ensure we are not in playlist view (pop if needed)
            # Basic logic: If we are deep in nav stack, pop to root.
            # self.nav_view.pop_to_tag("root")? No, "root" isn't a tag in that sense.
            # pop_to_page(self.root_nav_page)
            self.nav_view.pop_to_page(self.root_nav_page)

            self.view_stack.set_visible_child_name(row.name_id)
            self.set_header_title("Mixtapes")

            if row.name_id == "library":
                self.library_page.load_library()

    def _is_online(self):
        """Quick check if we have network connectivity."""
        import socket

        try:
            socket.create_connection(("music.youtube.com", 443), timeout=3)
            return True
        except OSError:
            return False

    def check_auth(self):
        from api.client import MusicClient
        from ui.login import LoginDialog

        client = MusicClient()

        # If no auth file at all and we're online, show login
        if not client.is_authenticated():
            if self._is_online():
                print("Authentication missing. Showing login dialog.")
                GObject.timeout_add(500, lambda: self.show_login(LoginDialog))
            else:
                print("Offline and no auth. Running in offline mode.")
                self.add_toast("No internet - running in offline mode")
            return

        # Validate session in background, but only if online
        def _validate():
            if not self._is_online():
                print("Offline - skipping auth validation, using cached session.")
                GLib.idle_add(self.add_toast, "Offline mode - using cached library")
                return
            valid = client.validate_session()
            if not valid:
                client._is_authed = False
                GLib.idle_add(self._on_auth_invalid)

        threading.Thread(target=_validate, daemon=True).start()

    def _on_auth_invalid(self):
        from ui.login import LoginDialog

        print("Authentication invalid. Showing login dialog.")
        self.show_login(LoginDialog)

    def show_login(self, dialog_cls):
        dialog = dialog_cls(self)
        dialog.connect("close-request", self.on_login_close)  # Handle close if needed
        dialog.present()
        return False

    def on_login_close(self, dialog):
        # Wipe the signed-out avatar state, then kick off a fresh fetch
        # so the new account's photo + name land in the menu.
        self._reset_avatar_profile()
        self._refresh_avatar_profile()
        # Refresh data
        if hasattr(self, "library_page"):
            self.library_page.load_library()
        if hasattr(self, "home_page"):
            self.home_page.refresh()

    def _on_mobile_breakpoint_apply(self, *args):
        # Adw.Breakpoint can fire 'apply' repeatedly while the user drags the
        # window across the threshold. Every re-entry reparents the expanded
        # player and re-syncs every page's compact mode, which is expensive
        # enough to look like a freeze. Short-circuit if we're already compact.
        if self._is_compact:
            return
        self._is_compact = True
        self.add_css_class("compact")

        # The desktop cover view is a desktop-only affordance — mobile
        # has its own full expanded player. Snap back to browser
        # silently (no animation) on resize into compact so the mobile
        # layout can take over immediately.
        if self.main_stack.get_visible_child_name() == "cover":
            prev = self.main_stack.get_transition_type()
            self.main_stack.set_transition_type(Gtk.StackTransitionType.NONE)
            self.main_stack.set_visible_child_name("browser")
            self.main_stack.set_transition_type(prev)
            if hasattr(self, "player_bar"):
                self.player_bar.set_expanded(False)

        # Hide tabs, show title
        if hasattr(self, "title_bin") and hasattr(self, "title_widget"):
            self.title_bin.set_child(self.title_widget)

        if hasattr(self, "player_bar"):
            self.player_bar.set_compact(True)

        # On mobile, the sidebar starts closed; don't touch
        # _sidebar_explicitly_opened so desktop remembers the last state.
        if hasattr(self, "split_view"):
            self.split_view.set_show_sidebar(False)

        # Dynamic Reparenting for ExpandedPlayer
        if hasattr(self, "expanded_player"):
            parent = self.expanded_player.get_parent()
            if parent == self.main_stack:
                self.main_stack.remove(self.expanded_player)
            self.bottom_sheet.set_sheet(self.expanded_player)

        # Defer the per-page compact sync — each page does its own layout
        # work and piling them into the breakpoint-apply frame is the
        # single biggest source of the resize jank.
        GLib.idle_add(self._sync_page_compact)

    def _on_mobile_breakpoint_unapply(self, *args):
        if not self._is_compact:
            return
        self._is_compact = False
        self.remove_css_class("compact")

        # Show tabs, hide title
        if hasattr(self, "title_bin") and hasattr(self, "switcher"):
            self.title_bin.set_child(self.switcher)

        if hasattr(self, "player_bar"):
            self.player_bar.set_compact(False)

        # Close BottomSheet when moving back to desktop
        if hasattr(self, "bottom_sheet"):
            self.bottom_sheet.set_open(False)

        # Restore desktop state
        if hasattr(self, "split_view"):
            GLib.idle_add(self._restore_sidebar_state)

        # Dynamic Reparenting back to Stack for Desktop
        if hasattr(self, "expanded_player"):
            self.bottom_sheet.set_sheet(None)
            parent = self.expanded_player.get_parent()
            if parent != self.main_stack:
                self.main_stack.add_named(self.expanded_player, "player")

        # Same deferral trick as the apply handler.
        GLib.idle_add(self._sync_page_compact)

    def _restore_sidebar_state(self):
        if hasattr(self, "split_view"):
            has_queue = len(self.player.queue) > 0
            # Sidebar is desktop-only. Don't let a pending restore open it
            # on mobile — the queue belongs in the expanded-player's Queue
            # tab there.
            show = (
                self._sidebar_explicitly_opened
                and has_queue
                and not self._is_compact
            )
            self.split_view.set_show_sidebar(show)
        return False  # Run once

    def _sync_page_compact(self):
        # Notify current pages
        for page_name in ["home", "library", "search"]:
            if hasattr(self, f"{page_name}_page"):
                page = getattr(self, f"{page_name}_page")
                if hasattr(page, "set_compact_mode"):
                    page.set_compact_mode(self._is_compact)

        # Also notify any dynamic pages in navigation stacks?
        # For simplicity, we can look at the visible page of the navigation stack
        nav = self.view_stack.get_visible_child()
        if isinstance(nav, Adw.NavigationView):
            page = nav.get_visible_page()
            if page:
                child = page.get_child()
                # If it's a ToolbarView, look at content
                if isinstance(child, Adw.ToolbarView):
                    child = child.get_content()
                if hasattr(child, "set_compact_mode"):
                    child.set_compact_mode(self._is_compact)

    def _on_sidebar_visibility_changed(self, split_view, param):
        is_visible = split_view.get_show_sidebar()
        if hasattr(self, "player_bar"):
            self.player_bar.set_queue_active(is_visible)
        # Window controls may need to move — if the sidebar is on the right
        # and just became hidden, the content pane now owns the trailing edge.
        self._apply_window_controls_position()

    def _on_player_bar_visibility(self, player, *args):
        has_queue = len(self.player.queue) > 0
        self.player_bar_revealer.set_reveal_child(has_queue)

        if not has_queue:
            # Close sidebar if queue becomes empty
            if hasattr(self, "split_view") and self.split_view.get_show_sidebar():
                self.split_view.set_show_sidebar(False)
                # The "context" is gone, forget the explicit-open state too.
                self._sidebar_explicitly_opened = False
            # Close the expanded-player sheet on mobile — otherwise it stays
            # open over an empty queue with no player bar behind it.
            if (
                self._is_compact
                and hasattr(self, "bottom_sheet")
                and self.bottom_sheet.get_open()
            ):
                self.bottom_sheet.set_open(False)
            # Collapse the desktop cover revealer for the same reason:
            # no track is playing, so there's nothing for it to show.
            self._dismiss_cover_if_open()

    def _on_split_view_collapsed(self, split_view, param):
        collapsed = split_view.get_collapsed()
        self._apply_window_controls_position()
        if not collapsed:
            # When uncollapsing (going back to desktop), force the state
            GLib.idle_add(self._restore_sidebar_state)

    def toggle_queue(self):
        """Toggles the visibility of the Queue Sidebar."""
        # Sidebar is desktop-only. The queue is reached via the expanded
        # player's Queue tab on mobile, so bail out of any accidental toggle.
        if self._is_compact:
            return False
        if hasattr(self, "split_view"):
            current = self.split_view.get_show_sidebar()
            new_state = not current

            if new_state and not self.player.queue:
                return False

            self.split_view.set_show_sidebar(new_state)

            # Persist state only when not collapsed (desktop view)
            # or if explicitly toggled in mobile overlay
            self._sidebar_explicitly_opened = new_state

        # Refresh explore/search
        if hasattr(self, "search_page"):
            self.search_page.refresh_explore()

        return False

    def on_expand_requested(self, player_bar):
        # Desktop: page-switch to the cover view with SLIDE_UP. Both the
        # browser and the cover translate together (no overlap), so
        # neither page's background can bleed through mid-animation.
        # Restored in _on_player_dismissed.
        if not self._is_compact:
            if self.main_stack.get_visible_child_name() == "cover":
                self._on_player_dismissed(None)
                return
            self._prev_main_transition = self.main_stack.get_transition_type()
            self._prev_main_duration = self.main_stack.get_transition_duration()
            self.main_stack.set_transition_duration(200)
            self.main_stack.set_transition_type(
                Gtk.StackTransitionType.SLIDE_UP
            )
            self.main_stack.set_visible_child_name("cover")
            # Prime the cover image so the view starts with the right
            # artwork even if no metadata-changed signal has fired yet.
            v_id = self.player.current_video_id
            if v_id:
                thumb = self.player_bar.cover_img.url
                self.desktop_cover_view._on_metadata_changed(
                    self.player, "", "", thumb, v_id, "INDIFFERENT"
                )
            self.back_btn.set_visible(True)
            self.player_bar.set_expanded(True)
            return

        # Compact / mobile: full ExpandedPlayer in the bottom sheet.
        v_id = self.player.current_video_id
        if v_id:
            t = (
                self.player_bar.current_title
                if hasattr(self.player_bar, "current_title")
                else "Loading..."
            )
            a = (
                self.player_bar.current_artist
                if hasattr(self.player_bar, "current_artist")
                else "Unknown"
            )
            self.expanded_player.on_metadata_changed(
                self.player, t, a, self.player_bar.cover_img.url, v_id, "INDIFFERENT"
            )
        if self.expanded_player.get_parent() != self.bottom_sheet:
            self.bottom_sheet.set_sheet(self.expanded_player)
        self.bottom_sheet.set_open(True)

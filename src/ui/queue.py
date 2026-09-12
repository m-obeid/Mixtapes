import gi

gi.require_version("Gtk", "4.0")
gi.require_version("Adw", "1")
from gi.repository import Gtk, Adw, GObject, Pango, Gdk, Gio
from ui.util_classes import ScrolledWindow


class QueueItem(GObject.Object):
    __gtype_name__ = "QueueItem"

    def __init__(self, track, index, is_playing):
        super().__init__()
        self.track = track
        self.index = index
        self.is_playing = is_playing


class QueueRowWidget(Gtk.Box):
    def __init__(self):
        super().__init__(orientation=Gtk.Orientation.HORIZONTAL, spacing=12)
        self.add_css_class("queue-row")

        self.model_item = None  # QueueItem

        # Drag Handle
        self.handle = Gtk.Image.new_from_icon_name("list-drag-handle-symbolic")
        self.handle.add_css_class("dim-label")
        self.handle.add_css_class("drag-handle")
        self.handle.set_margin_start(6)
        self.handle.set_margin_end(4)
        self.append(self.handle)

        # Helper to find parent QueuePopover for callbacks
        self.popover = None

        # Setup Drag Source
        drag_source = Gtk.DragSource()
        drag_source.set_actions(Gdk.DragAction.MOVE)
        drag_source.connect("prepare", self.on_drag_prepare)
        drag_source.connect("drag-begin", self.on_drag_begin)
        self.handle.add_controller(drag_source)

        # Indicator / Index
        self.indicator_stack = Gtk.Stack()
        self.indicator_lbl = Gtk.Label()
        self.indicator_lbl.add_css_class("dim-label")
        self.indicator_lbl.set_width_chars(3)
        self.indicator_icon = Gtk.Image.new_from_icon_name(
            "media-playback-start-symbolic"
        )
        self.indicator_icon.add_css_class("accent")

        self.indicator_stack.add_named(self.indicator_lbl, "index")
        self.indicator_stack.add_named(self.indicator_icon, "playing")
        self.append(self.indicator_stack)

        # Info Box
        info_box = Gtk.Box(orientation=Gtk.Orientation.VERTICAL, spacing=2)
        info_box.set_hexpand(True)

        self.title_lbl = Gtk.Label()
        self.title_lbl.set_halign(Gtk.Align.START)
        self.title_lbl.set_ellipsize(Pango.EllipsizeMode.END)
        self.title_lbl.add_css_class("body")

        self.artist_lbl = Gtk.Label()
        self.artist_lbl.set_halign(Gtk.Align.START)
        self.artist_lbl.set_ellipsize(Pango.EllipsizeMode.END)
        self.artist_lbl.add_css_class("caption")
        self.artist_lbl.add_css_class("dim-label")

        info_box.append(self.title_lbl)
        info_box.append(self.artist_lbl)
        self.append(info_box)

        # Drop Target
        drop_target = Gtk.DropTarget.new(GObject.TYPE_STRING, Gdk.DragAction.MOVE)
        drop_target.connect("drop", self.on_drop)
        self.add_controller(drop_target)

    def bind(self, item, popover):
        self.model_item = item
        self.popover = popover

        track = item.track

        # Update Text
        self.title_lbl.set_label(track.get("title", "Unknown"))

        artist_txt = track.get("artist")
        if isinstance(artist_txt, list):
            artist_txt = ", ".join([a.get("name", "") for a in artist_txt])
        elif not artist_txt and "artists" in track:
            artist_txt = ", ".join(
                [a.get("name", "") for a in track.get("artists", [])]
            )
        if not artist_txt:
            artist_txt = "Unknown"
        self.artist_lbl.set_label(artist_txt)

        # Update Indicator
        if item.is_playing:
            self.add_css_class("playing")
            self.remove_css_class("flat")
            self.indicator_stack.set_visible_child_name("playing")
        else:
            self.remove_css_class("playing")
            self.add_css_class("flat")
            self.indicator_lbl.set_label(str(item.index + 1))
            self.indicator_stack.set_visible_child_name("index")

    def on_drag_prepare(self, source, x, y):
        if self.model_item:
            value = GObject.Value(str, str(self.model_item.index))
            return Gdk.ContentProvider.new_for_value(value)
        return None

    def on_drag_begin(self, source, drag):
        paintable = Gtk.WidgetPaintable.new(self)
        source.set_icon(paintable, 0, 0)

    def on_drop(self, target, value, x, y):
        try:
            source_index = int(value)
            if self.model_item and source_index != self.model_item.index:
                # Callback to Popover
                if self.popover:
                    self.popover._on_row_move(source_index, self.model_item.index)
            return True
        except ValueError:
            return False


class QueuePopover(Gtk.Popover):
    def __init__(self, player):
        super().__init__()
        self.player = player
        self.set_autohide(True)
        self.set_size_request(350, 500)

        box = Gtk.Box(orientation=Gtk.Orientation.VERTICAL, spacing=0)
        self.set_child(box)

        # Header
        header = Gtk.Box(orientation=Gtk.Orientation.HORIZONTAL, spacing=12)
        header.set_margin_top(12)
        header.set_margin_bottom(12)
        header.set_margin_start(12)
        header.set_margin_end(12)

        title = Gtk.Label(label="Queue")
        title.add_css_class("title-4")
        title.set_hexpand(True)
        title.set_halign(Gtk.Align.START)
        header.append(title)

        # Shuffle Toggle
        self.shuffle_btn = Gtk.ToggleButton(icon_name="media-playlist-shuffle-symbolic")
        self.shuffle_btn.add_css_class("flat")
        self.shuffle_btn.set_tooltip_text("Shuffle Queue")
        # Handle toggle manually to match player state or just connect clicked?
        # If we connect "toggled", it fires when we set_active too.
        # Better connect "clicked" and handle state changes from player events.
        self.shuffle_btn.connect("clicked", self._on_shuffle_clicked)
        header.append(self.shuffle_btn)

        clear_btn = Gtk.Button(label="Clear")
        clear_btn.add_css_class("flat")
        clear_btn.connect("clicked", lambda x: self.player.clear_queue())
        header.append(clear_btn)

        box.append(header)

        # ListView Setup
        self.store = Gio.ListStore(item_type=QueueItem)
        self.selection_model = Gtk.SingleSelection(model=self.store)
        self.selection_model.set_autoselect(False)
        self.selection_model.connect("selection-changed", self._on_selection_changed)

        factory = Gtk.SignalListItemFactory()
        factory.connect("setup", self._on_factory_setup)
        factory.connect("bind", self._on_factory_bind)

        self.list_view = Gtk.ListView(model=self.selection_model, factory=factory)

        scrolled = ScrolledWindow()
        scrolled.set_vexpand(True)
        scrolled.set_child(self.list_view)
        box.append(scrolled)

        # Signals
        self.player.connect("state-changed", self._on_player_update)
        self.player.connect("metadata-changed", self._on_player_update)

        # Initial Populate
        self._populate()
        self._update_shuffle_state()

    def _on_shuffle_clicked(self, btn):
        self.player.shuffle_queue()
        # State will be updated by _on_player_update -> _update_shuffle_state

    def _update_shuffle_state(self):
        # Block signal if needed, or just set_active
        if self.player.shuffle_mode != self.shuffle_btn.get_active():
            self.shuffle_btn.set_active(self.player.shuffle_mode)

        if self.player.shuffle_mode:
            self.shuffle_btn.add_css_class("accent")
        else:
            self.shuffle_btn.remove_css_class("accent")

    def _populate(self):
        queue = self.player.queue
        current_idx = self.player.current_queue_index

        items = []
        for i, track in enumerate(queue):
            items.append(QueueItem(track, i, i == current_idx))

        # Efficient update using splice
        self.store.splice(0, self.store.get_n_items(), items)

    def _on_factory_setup(self, factory, list_item):
        widget = QueueRowWidget()
        list_item.set_child(widget)

    def _on_factory_bind(self, factory, list_item):
        widget = list_item.get_child()
        item = list_item.get_item()
        widget.bind(item, self)

    def _on_selection_changed(self, model, position, n_items):
        item = model.get_selected_item()
        if item:
            # Activate
            self.player.current_queue_index = item.index
            self.player._play_current_index()
            pass

    # We need row activation. ListView has "activate" signal.
    # SingleSelection implies selection.
    # Let's clean up _on_selection_changed and usage.

    def _on_row_move(self, old_index, new_index):
        if self.player.move_queue_item(old_index, new_index):
            pass

    def _on_player_update(self, *args):
        self._update_shuffle_state()

        # Only full rebuild if queue changed structurally
        if self.store.get_n_items() != len(self.player.queue):
            self._populate()
        else:
            self._update_item_states()

    def _update_item_states(self):
        current_idx = self.player.current_queue_index
        n = self.store.get_n_items()
        for i in range(n):
            item = self.store.get_item(i)
            item.is_playing = i == current_idx

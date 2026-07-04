import weakref
from gi.repository import Gtk, Adw, GObject
from ui.util_classes import ScrolledWindow

class AllMoodsPage(Adw.Bin):
    __gsignals__ = {
        "header-title-changed": (GObject.SignalFlags.RUN_FIRST, None, (str,))
    }

    def __init__(self, items, title, *args, **kwargs):
        super().__init__(*args, **kwargs)
        weak_self = weakref.ref(self)
        self.items = items
        self.category_title = title

        self.main_box = Gtk.Box(orientation=Gtk.Orientation.VERTICAL)

        self.scrolled = ScrolledWindow()
        self.scrolled.set_policy(Gtk.PolicyType.NEVER, Gtk.PolicyType.AUTOMATIC)
        self.scrolled.set_vexpand(True)

        self.content_box = Gtk.Box(orientation=Gtk.Orientation.VERTICAL, spacing=16)
        self.content_box.set_margin_top(24)
        self.content_box.set_margin_bottom(24)
        self.content_box.set_margin_start(24)
        self.content_box.set_margin_end(24)

        # Title Label
        self.page_title_label = Gtk.Label(label="")
        self.page_title_label.add_css_class("title-1")
        self.page_title_label.set_halign(Gtk.Align.START)
        self.page_title_label.set_margin_bottom(16)
        
        display_title = f"All {self.category_title}"
        if self.category_title == "Moods & Moments":
            display_title = "All Moods & Moments"
            
        self.page_title_label.set_label(display_title)
        self.content_box.append(self.page_title_label)

        self.list_box = Gtk.ListBox()
        self.list_box.add_css_class("boxed-list")
        self.list_box.set_selection_mode(Gtk.SelectionMode.NONE)

        for item in self.items:
            row = Gtk.ListBoxRow()
            
            box = Gtk.Box(orientation=Gtk.Orientation.HORIZONTAL, spacing=12)
            box.set_margin_top(12)
            box.set_margin_bottom(12)
            box.set_margin_start(16)
            box.set_margin_end(16)
            
            lbl = Gtk.Label(label=item.get("title", ""))
            lbl.set_halign(Gtk.Align.START)
            lbl.set_hexpand(True)
            box.append(lbl)
            
            icon = Gtk.Image.new_from_icon_name("go-next-symbolic")
            icon.set_valign(Gtk.Align.CENTER)
            box.append(icon)
            
            row.set_child(box)
            row.item_data = item
            
            click_gesture.connect(
                "released", lambda g, n, x, y, it=item: weak_self()._on_row_activated(g, n, x, y, it) if weak_self() else None
            )
            row.add_controller(click_gesture)
            
            self.list_box.append(row)

        self.content_box.append(self.list_box)

        self.clamp = Adw.Clamp()
        self.clamp.set_maximum_size(1024)
        self.clamp.set_tightening_threshold(600)
        self.clamp.set_child(self.content_box)

        self.scrolled.set_child(self.clamp)
        self.main_box.append(self.scrolled)

        self.set_child(self.main_box)
        
        self.connect("map", lambda w: weak_self()._on_map(w) if weak_self() else None)

    def filter_content(self, text):
        query = text.lower().strip()
        child = self.list_box.get_first_child()
        while child:
            if hasattr(child, "item_data"):
                title = child.item_data.get("title", "").lower()
                child.set_visible(not query or query in title)
            child = child.get_next_sibling()

    def _on_map(self, widget):
        if self.category_title == "Moods & Moments":
            self.emit("header-title-changed", "All Moods & Moments")
        else:
            self.emit("header-title-changed", f"All {self.category_title}")

    def _on_row_activated(self, gesture, n_press, x, y, item):
        if "params" in item:
            root = self.get_root()
            if hasattr(root, "open_category"):
                nav_title = item.get("title", self.category_title)
                root.open_category(item["params"], nav_title)

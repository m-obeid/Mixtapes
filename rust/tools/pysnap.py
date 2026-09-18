"""Run the Python app, type a search, and save PNGs of the window from inside GTK."""
import os
import sys

ROOT = "/home/omori/Projects/Mixtapes"
os.environ["MALLOC_ARENA_MAX"] = "2"
sys.path.insert(0, os.path.join(ROOT, "src"))
os.chdir(os.path.join(ROOT, "src"))

import gi
gi.require_version("Gtk", "4.0")
gi.require_version("Adw", "1")
from gi.repository import Gtk, Adw, GLib, Graphene

import main as muse_main

WIDTH = int(os.environ.get("PY_WIDTH", "1000"))
QUERY = os.environ.get("PY_QUERY", "")
TAB = os.environ.get("PY_TAB", "")
PLAYLIST = os.environ.get("PY_PLAYLIST", "")
QUEUE = os.environ.get("PY_QUEUE", "")
ARTIST = os.environ.get("PY_ARTIST", "")
SCROLL = float(os.environ.get("PY_SCROLL", "0"))
CATEGORY = os.environ.get("PY_CATEGORY", "")
HISTORY = os.environ.get("PY_HISTORY", "")
# "px" or "px,lyrics": open Preferences, scroll that far, optionally on the Lyrics page.
PREFS = os.environ.get("PY_PREFS", "")
OUT = os.environ["PY_OUT"]

def log(msg):
    sys.stderr.write(f"[pysnap] {msg}\n")
    sys.stderr.flush()

def snapshot(win, path):
    """Render after the next paint, when the widget's render node is fresh."""
    clock = win.get_frame_clock()
    handler = []

    def on_after_paint(clock):
        clock.disconnect(handler[0])
        paintable = Gtk.WidgetPaintable.new(win)
        w, h = paintable.get_intrinsic_width(), paintable.get_intrinsic_height()
        snap = Gtk.Snapshot()
        paintable.snapshot(snap, w, h)
        node = snap.to_node()
        if node is None:
            log(f"snapshot empty after paint: {path}")
            return
        renderer = win.get_native().get_renderer()
        tex = renderer.render_texture(node, Graphene.Rect().init(0, 0, w, h))
        tex.save_to_png(path)
        log(f"snapshot {w}x{h} -> {path}")

    handler.append(clock.connect("after-paint", on_after_paint))
    win.queue_draw()
    return False

class SnapApp(muse_main.MusicApp):
    def do_activate(self):
        muse_main.MusicApp.do_activate(self)
        win = self.props.active_window
        win.set_default_size(WIDTH, 700)
        log(f"window presented, default size {WIDTH}x700")

        def start_search():
            if TAB:
                win.view_stack.set_visible_child_name(TAB)
                log(f"tab: {TAB}")
            if QUEUE:
                win.split_view.set_show_sidebar(True)
                log("queue sidebar opened")
            if ARTIST:
                win.open_artist(ARTIST, None)
                log(f"artist opened: {ARTIST}")
            if PLAYLIST:
                win.open_playlist(PLAYLIST)
                log(f"playlist opened: {PLAYLIST}")
            if CATEGORY:
                params, _, name = CATEGORY.partition(",")
                win.open_category(params, name or "Category")
                log(f"category opened: {params}")
            if HISTORY:
                win._open_history_from_menu()
                log("history opened")
            if QUERY:
                win.search_bar.set_search_mode(True)
                win.search_entry.set_text(QUERY)
                log(f"search typed: {QUERY}")
            return False

        def scroll_page():
            """Scroll the first scroller under the visible page that can move."""
            def find(widget):
                if isinstance(widget, Gtk.ScrolledWindow):
                    adj = widget.get_vadjustment()
                    if adj.get_upper() > adj.get_page_size():
                        return widget
                child = widget.get_first_child()
                while child:
                    found = find(child)
                    if found:
                        return found
                    child = child.get_next_sibling()
                return None

            nav = win.view_stack.get_visible_child()
            page = nav.get_visible_page() if isinstance(nav, Adw.NavigationView) else nav
            scroller = find(page) if page else None
            if scroller:
                adj = scroller.get_vadjustment()
                adj.set_value(min(SCROLL, adj.get_upper() - adj.get_page_size()))
                log(f"scrolled to {adj.get_value()}")
            else:
                log("nothing to scroll")
            return False

        def open_prefs():
            win.show_preferences(None, None)
            parts = PREFS.split(",")
            dialog = win.get_visible_dialog()
            if len(parts) > 1 and parts[1] == "lyrics":
                for page in _pages(dialog):
                    if page.get_title() == "Lyrics":
                        dialog.set_visible_page(page)

            def scroll():
                def walk(widget):
                    if isinstance(widget, Gtk.ScrolledWindow) and widget.get_mapped():
                        adj = widget.get_vadjustment()
                        adj.set_value(min(float(parts[0] or 0), adj.get_upper() - adj.get_page_size()))
                    child = widget.get_first_child()
                    while child:
                        walk(child)
                        child = child.get_next_sibling()
                walk(dialog)
                return False

            GLib.timeout_add(1200, scroll)
            return False

        def _pages(widget):
            found = []
            def walk(w):
                if isinstance(w, Adw.PreferencesPage):
                    found.append(w)
                child = w.get_first_child()
                while child:
                    walk(child)
                    child = child.get_next_sibling()
            walk(widget)
            return found

        def press_play():
            nav = win.view_stack.get_visible_child()
            page = nav.get_visible_page()
            target = page.get_child() if page else None
            if hasattr(target, "on_play_clicked"):
                target.on_play_clicked(None)
                log("play pressed")
            return False

        if os.environ.get("PY_PLAYLIST_PLAY"):
            GLib.timeout_add(7500, press_play)
        if PREFS:
            GLib.timeout_add(4000, open_prefs)
        GLib.timeout_add(3000, start_search)
        if SCROLL:
            GLib.timeout_add(9000, scroll_page)
        snap_at = int(os.environ.get("PY_SNAPSHOT_AT", "10000"))
        GLib.timeout_add(snap_at, lambda: snapshot(win, OUT + "-1.png"))
        GLib.timeout_add(snap_at + 1500, lambda: (win.player.stop(), self.quit(), False)[-1])

if __name__ == "__main__":
    sys.exit(SnapApp().run([]))

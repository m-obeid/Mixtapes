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
from gi.repository import Gtk, GLib, Graphene

import main as muse_main

WIDTH = int(os.environ.get("PY_WIDTH", "1000"))
QUERY = os.environ.get("PY_QUERY", "")
TAB = os.environ.get("PY_TAB", "")
PLAYLIST = os.environ.get("PY_PLAYLIST", "")
QUEUE = os.environ.get("PY_QUEUE", "")
ARTIST = os.environ.get("PY_ARTIST", "")
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
            if QUERY:
                win.search_bar.set_search_mode(True)
                win.search_entry.set_text(QUERY)
                log(f"search typed: {QUERY}")
            return False

        GLib.timeout_add(3000, start_search)
        GLib.timeout_add(10000, lambda: snapshot(win, OUT + "-1.png"))
        GLib.timeout_add(11500, lambda: (win.player.stop(), self.quit(), False)[-1])

if __name__ == "__main__":
    sys.exit(SnapApp().run([]))

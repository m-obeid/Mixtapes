from gi.repository import Gtk, Adw, GObject, GLib, Pango, Gdk, Gio
import threading
from ui.pages.base_playlist import BasePlaylistPage
from ui.models.song import SongItem


class MixPage(BasePlaylistPage):
    def __init__(self, player, *args, **kwargs):
        super().__init__(player, *args, **kwargs)
        # self.sort_row.set_visible(False)

    def load_mix(self, mix_id, initial_data=None):
        if self.playlist_id != mix_id:
            self.playlist_id = mix_id
            self.playlist_title_text = ""
            self.emit("header-title-changed", "")

            # Clear list
            self.store.remove_all()
            self.current_tracks = []
            self.original_tracks = []

        if initial_data:
            self.playlist_title_text = initial_data.get("title", "")
            self.playlist_name_label.set_label(self.playlist_title_text)
            self.meta_label.set_label("Loading mix...")
            thumb = initial_data.get("thumb")
            if thumb:
                self.cover_img.load_url(thumb)
            self.stack.set_visible_child_name("content")
            self.content_spinner.set_visible(True)
        else:
            self.stack.set_visible_child_name("loading")
            self.content_spinner.set_visible(False)

        thread = threading.Thread(target=self._fetch_details)
        thread.daemon = True
        thread.start()

    def _fetch_details(self, is_incremental=False):
        client = self.client
        pid = self.playlist_id
        if not client or not pid:
            return
        try:
            # ytmusicapi get_playlist with limit=25 initially
            data = client.get_playlist(pid, limit=100)

            title = data.get("title", "Unknown Mix")
            description = data.get("description", "")
            tracks = data.get("tracks", [])
            thumbnails = data.get("thumbnails", [])

            meta1 = "Auto-generated Mix"
            meta2 = "Infinite Playlist"

            GObject.idle_add(
                self.update_ui, title, description, meta1, meta2, thumbnails, tracks
            )
            self.is_fully_loaded = False  # Mixes are infinite

        except Exception as e:
            print(f"Error fetching mix: {e}")

    def _is_infinite(self):
        return True

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
        super().update_ui(
            title, description, meta1, meta2, thumbnails, tracks, append, total_tracks
        )
        # self.sort_row.set_visible(False)
        self.load_more_spinner.set_visible(False)
        self.is_loading_more = False


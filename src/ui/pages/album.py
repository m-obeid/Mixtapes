from gi.repository import Gtk, Adw, GObject, GLib, Pango, Gdk, Gio
import threading
import re
from ui.pages.base_playlist import BasePlaylistPage
from ui.models.song import SongItem


class AlbumPage(BasePlaylistPage):
    def __init__(self, player, *args, **kwargs):
        super().__init__(player, *args, **kwargs)
        # self.sort_row.set_visible(False)

    def load_album(self, album_id, initial_data=None):
        if self.playlist_id != album_id:
            self.playlist_id = album_id
            self.playlist_title_text = ""
            self.emit("header-title-changed", "")

            # Clear list
            self.store.remove_all()
            self.current_tracks = []
            self.original_tracks = []

        if initial_data:
            self.playlist_title_text = initial_data.get("title", "")
            self.playlist_name_label.set_label(self.playlist_title_text)
            self.meta_label.set_label("Loading album...")
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

    def _fetch_details(self):
        client = self.client
        pid = self.playlist_id
        if not client or not pid:
            return
        try:
            data = client.get_album(pid)
            title = data.get("title", "Unknown Album")
            description = data.get("description", "")
            tracks = data.get("tracks", [])
            thumbnails = data.get("thumbnails", [])
            track_count = data.get("trackCount", len(tracks))
            year = data.get("year", "")

            # Construct Meta
            artist_data = data.get("artists", [])
            parts = []
            for a in artist_data:
                name = GLib.markup_escape_text(a.get("name", "Unknown"))
                aid = a.get("id")
                if aid:
                    parts.append(f"<a href='artist:{aid}'>{name}</a>")
                else:
                    parts.append(name)
            author = ", ".join(parts)

            # Infer Album Type
            if track_count == 1:
                album_type = "Single"
            elif 2 <= track_count <= 6:
                album_type = "EP"
            else:
                album_type = "Album"

            meta1_parts = [album_type]
            if year:
                meta1_parts.append(str(year))
            if author:
                meta1_parts.append(author)
            meta1 = " • ".join(meta1_parts)

            song_text = "song" if track_count == 1 else "songs"
            meta2 = f"{track_count} {song_text}"

            # High-Res Cover art hack
            if thumbnails:
                for t in thumbnails:
                    if "url" in t:
                        t["url"] = re.sub(r"w\d+-h\d+", "w544-h544", t["url"])
                # Propagate cover to tracks
                for t in tracks:
                    if not t.get("thumbnails"):
                        t["thumbnails"] = thumbnails

            GObject.idle_add(
                self.update_ui, title, description, meta1, meta2, thumbnails, tracks
            )
            self.is_fully_loaded = True

        except Exception as e:
            print(f"Error fetching album: {e}")

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


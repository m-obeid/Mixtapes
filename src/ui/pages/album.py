import re
import threading
from gi.repository import GLib, GObject
from ui.pages.playlist import PlaylistPage


class AlbumPage(PlaylistPage):
    def __init__(self, player, *args, **kwargs):
        super().__init__(player, *args, **kwargs)
        self._is_album_view = True

    def load_album(self, album_id, initial_data=None):
        self._is_album_view = True
        self.load_playlist(album_id, initial_data=initial_data)

    def _fetch_playlist_details(self, playlist_id, is_incremental=False):
        try:
            data = self.client.get_album(playlist_id)
            self._audio_playlist_id = data.get("audioPlaylistId")
            title = data.get("title", "Unknown Album")
            description = data.get("description", "")
            tracks = data.get("tracks", [])
            thumbnails = data.get("thumbnails", [])
            track_count = data.get("trackCount", len(tracks))
            year = data.get("year", "")

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

            if thumbnails:
                for t in thumbnails:
                    if "url" in t:
                        t["url"] = re.sub(r"w\d+-h\d+", "w544-h544", t["url"])

            album_thumb_url = thumbnails[-1]["url"] if thumbnails else None
            for t in tracks:
                if not t.get("thumbnails") and album_thumb_url:
                    t["thumbnails"] = [{"url": album_thumb_url}]
                if not t.get("thumb") and album_thumb_url:
                    t["thumb"] = album_thumb_url

            GObject.idle_add(
                self.update_ui,
                title,
                description,
                meta1,
                meta2,
                thumbnails,
                tracks,
                is_incremental,
                track_count,
                False,
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
        is_owned=False,
    ):
        super().update_ui(
            title,
            description,
            meta1,
            meta2,
            thumbnails,
            tracks,
            append,
            total_tracks,
            is_owned,
        )
        self._is_album_view = True
        self.sort_row.set_visible(False)

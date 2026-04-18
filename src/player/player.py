import os
import random
import sys
import threading

import gi

gi.require_version("Gst", "1.0")
gi.require_version("GstAudio", "1.0")
import glob

from gi.repository import GdkPixbuf, GLib, GObject, Gst, GstAudio
from yt_dlp import YoutubeDL

from api.client import MusicClient
from player.cache import StreamCache
from player.downloads import DownloadManager
from ui.utils import get_high_res_url, get_ytimg_fallbacks

HAS_MPRIS = False
HAS_SMTC = False
if sys.platform == "win32":
    try:
        from player.smtc import SMTCAdapter

        HAS_SMTC = True
    except ImportError:
        pass
elif sys.platform == "linux":
    try:
        from mprisify.server import Server

        from player.mpris import MuseEventAdapter, MuseMprisAdapter

        HAS_MPRIS = True
    except ImportError:
        pass

from player.discord_rpc import DiscordRPCAdapter


class Player(GObject.Object):
    __gsignals__ = {
        "state-changed": (
            GObject.SignalFlags.RUN_FIRST,
            None,
            (str,),
        ),  # playing, paused, stopped
        "progression": (
            GObject.SignalFlags.RUN_FIRST,
            None,
            (float, float),
        ),  # position, duration (seconds) -> Changed to float
        "metadata-changed": (
            GObject.SignalFlags.RUN_FIRST,
            None,
            (str, str, str, str, str),
        ),  # title, artist, thumbnail_url, video_id, like_status
        "volume-changed": (
            GObject.SignalFlags.RUN_FIRST,
            None,
            (float, bool),
        ),  # volume, muted
    }

    def __init__(self):
        super().__init__()
        Gst.init(None)
        self.client = MusicClient()
        self.player = Gst.ElementFactory.make("playbin", "player")

        # Disable video output using playbin flags (unsetting GST_PLAY_FLAG_VIDEO)
        # GST_PLAY_FLAG_VIDEO is 1 << 0
        flags = self.player.get_property("flags")
        self.player.set_property("flags", flags & ~(1 << 0))

        self.ydl_opts = {
            "js_runtimes": {"node": {}},
            "format": "bestaudio/best",
            "quiet": True,
            "noplaylist": True,
            "extractor_args": {
                "youtube": {
                    "player_client": [
                        "web_music",
                        "mweb",
                        "tv",
                        "web_safari",
                        "android_vr",
                        "android",
                        "ios",
                    ],
                }
            },
        }

        self.bus = self.player.get_bus()
        self.bus.add_signal_watch()
        self.bus.connect("message", self.on_message)

        # Listen for external volume changes (system mixer)
        self.player.connect("notify::volume", self._on_external_volume_change)
        self.player.connect("notify::mute", self._on_external_mute_change)
        self._internal_volume_change = False
        self._user_volume = self.get_volume()
        self._track_started_at = 0.0

        self.current_video_id = None

        # Queue State
        self.queue = []  # List of dicts: {id, title, artist, thumb, ...}
        self.current_queue_index = -1
        self.shuffle_mode = False
        self.original_queue = []  # Backup for un-shuffle
        self.load_generation = 0  # To handle race conditions in loading
        self.mpris_art_url = None
        self.current_url = None
        self.last_seek_time = 0.0
        self.duration = -1
        self._is_loading = False
        self._current_logical_state = "stopped"

        # New modes
        self.repeat_mode = "none"  # none, track, all
        self.queue_source_id = None
        self.queue_is_infinite = False
        self._is_fetching_infinite = False

        # Audio snippet cache
        self.stream_cache = StreamCache()

        # Download manager for offline playback
        self.download_manager = DownloadManager(self.client)
        self._playing_from_cache = False
        self._pending_stream_url = None

        # Timer for progress
        GObject.timeout_add(100, self.update_position)

        # MPRIS Setup (Linux-only, requires D-Bus)
        if HAS_MPRIS:
            self.mpris_adapter = MuseMprisAdapter(self)
            self.mpris_server = Server("Mixtapes", adapter=self.mpris_adapter)
            self.mpris_events = MuseEventAdapter(
                self.mpris_server.root, self.mpris_server.player
            )
            self.mpris_server.set_event_adapter(self.mpris_events)
            self.mpris_server.loop(background=True)

            # Connect signals for MPRIS updates
            self.connect("state-changed", self._on_mpris_state_changed)
            self.connect("metadata-changed", self._on_mpris_metadata_changed)
            self.connect("progression", self._on_mpris_progression)
            self.connect("volume-changed", self._on_mpris_volume_changed)

        # SMTC Setup (Windows-only)
        if HAS_SMTC:
            try:
                self.smtc = SMTCAdapter(self)
                self.connect("state-changed", self._on_smtc_state_changed)
                self.connect("metadata-changed", self._on_smtc_metadata_changed)
                self.connect("progression", self._on_smtc_progression)
            except Exception as e:
                print(f"SMTC init failed: {e}")
                self.smtc = None

        # Discord Rich Presence (cross-platform; no-op if pypresence missing
        # or Discord not running).
        try:
            self.discord_rpc = DiscordRPCAdapter(self)
            self.connect("state-changed", self._on_discord_state_changed)
            self.connect("metadata-changed", self._on_discord_metadata_changed)
        except Exception as e:
            print(f"Discord RPC init failed: {e}")
            self.discord_rpc = None

    def _on_discord_state_changed(self, obj, state):
        if getattr(self, "discord_rpc", None):
            self.discord_rpc.update()

    def _on_discord_metadata_changed(
        self, obj, title, artist, thumb, video_id, like_status
    ):
        if getattr(self, "discord_rpc", None):
            self.discord_rpc.update()

    def _on_mpris_state_changed(self, obj, state):
        print(f"DEBUG-MPRIS-STATE-START: state={state}")
        if hasattr(self, "mpris_events"):
            # Explicitly tell the server the PlaybackStatus changed
            self.mpris_events.on_playpause()
            # Update metadata because length or 'CanGoNext' might have changed
            self.mpris_events.on_player_all()
        print("DEBUG-MPRIS-STATE-END")

    def _on_mpris_metadata_changed(
        self, obj, title, artist, thumb, video_id, like_status
    ):
        print(f"DEBUG-MPRIS-META-START: video_id={video_id}")
        if hasattr(self, "mpris_events"):
            # Trigger the 'Metadata' property update
            self.mpris_events.on_title()
            # Update UI-related flags like CanGoNext/Previous
            self.mpris_events.on_player_all()
        print("DEBUG-MPRIS-META-END")

    def _on_mpris_progression(self, obj, pos, dur):
        # We don't usually emit D-Bus signals for every progression tick
        # as it's too frequent, but mpris-server handles position queries.
        pass

    def _on_mpris_volume_changed(self, obj, volume, muted):
        self.mpris_events.on_volume()

    def _on_smtc_state_changed(self, obj, state):
        if hasattr(self, "smtc") and self.smtc:
            self.smtc.update_playback_status(state)
            can_next = self.current_queue_index + 1 < len(self.queue)
            can_prev = self.current_queue_index > 0
            self.smtc.update_controls(can_next=can_next, can_previous=can_prev)

    def _on_smtc_metadata_changed(
        self, obj, title, artist, thumb, video_id, like_status
    ):
        if hasattr(self, "smtc") and self.smtc:
            self.smtc.update_metadata(title, artist, thumb)

    def _on_smtc_progression(self, obj, pos, dur):
        if hasattr(self, "smtc") and self.smtc:
            self.smtc.update_timeline(pos, dur)

    def load_video(
        self, video_id, title="Loading...", artist="Unknown", thumbnail_url=None
    ):
        """Legacy/Single-track load. Clears queue and plays this one."""
        track = {
            "videoId": video_id,
            "title": title,
            "artist": artist,  # String or list, normalized later
            "thumb": thumbnail_url,
        }
        self.set_queue([track])

    def play_tracks(self, tracks):
        """Sets the queue to the given tracks and starts playback of the first one."""
        self.set_queue(tracks, 0)

    def start_radio(self, video_id=None, playlist_id=None):
        """Start a radio (mix) from a song or playlist. Runs in background."""

        def _fetch():
            try:
                data = self.client.get_watch_playlist(
                    video_id=video_id, playlist_id=playlist_id, limit=50, radio=True
                )
                tracks = data.get("tracks", [])
                if tracks:
                    # Normalize thumbnail field: watch_playlist uses 'thumbnail' not 'thumbnails'/'thumb'
                    for t in tracks:
                        if "thumbnail" in t and "thumbnails" not in t:
                            t["thumbnails"] = t["thumbnail"]
                        if t.get("thumbnails") and not t.get("thumb"):
                            thumbs = t["thumbnails"]
                            if isinstance(thumbs, list) and thumbs:
                                t["thumb"] = thumbs[-1].get("url", "")

                    pid = data.get("playlistId")
                    GObject.idle_add(self.set_queue, tracks, 0, False, pid, True)
                else:
                    print("[RADIO] No tracks returned")
            except Exception as e:
                print(f"[RADIO] Error: {e}")

        threading.Thread(target=_fetch, daemon=True).start()

    def set_queue(
        self, tracks, start_index=0, shuffle=False, source_id=None, is_infinite=False
    ):
        """
        Sets the global queue and plays the track at start_index.
        tracks: list of dicts with videoId, title, artist, thumb
        """
        self.stop()
        self.queue = list(tracks)  # Copy for playing
        self.original_queue = list(tracks)  # Backup for un-shuffle
        self.shuffle_mode = shuffle  # Set mode based on request
        self.queue_source_id = source_id
        self.queue_is_infinite = is_infinite
        self._is_fetching_infinite = False

        target_track = (
            self.queue[start_index] if 0 <= start_index < len(self.queue) else None
        )

        if shuffle:
            import random

            # If start_index is valid, we want to play that track FIRST, then shuffle the rest.
            if target_track:
                # Remove target
                self.queue.remove(target_track)
                # Shuffle rest
                random.shuffle(self.queue)
                # Insert target at 0
                self.queue.insert(0, target_track)
                self.current_queue_index = 0
            else:
                random.shuffle(self.queue)
                self.current_queue_index = 0
            # Note: original_queue remains ordered as passed
        else:
            self.current_queue_index = start_index

        if self.current_queue_index >= 0 and self.current_queue_index < len(self.queue):
            self._play_current_index()
        else:
            self.stop()
        self.emit("state-changed", "queue-updated")

    def add_to_queue(self, track, next=False):
        """Adds a track to the queue. if next=True, inserts after current."""
        if next and self.current_queue_index >= 0:
            self.queue.insert(self.current_queue_index + 1, track)
            self.original_queue.insert(
                self.current_queue_index + 1, track
            )  # Keep sync roughly
        else:
            self.queue.append(track)
            self.original_queue.append(track)

        # If nothing is playing, play this
        if self.current_queue_index == -1:
            self.current_queue_index = 0
            self._play_current_index()

        self.emit("state-changed", "queue-updated")

    def add_tracks_to_queue(self, tracks, next=False):
        """Adds multiple tracks to the queue. If next=True, inserts after current."""
        if not tracks:
            return
        if next and self.current_queue_index >= 0:
            pos = self.current_queue_index + 1
            for i, t in enumerate(tracks):
                self.queue.insert(pos + i, t)
                self.original_queue.insert(pos + i, t)
        else:
            self.queue.extend(tracks)
            self.original_queue.extend(tracks)

        if self.current_queue_index == -1:
            self.current_queue_index = 0
            self._play_current_index()

        self.emit("state-changed", "queue-updated")

    def remove_from_queue(self, index):
        if 0 <= index < len(self.queue):
            pop = self.queue.pop(index)
            # Adjust current index
            if index < self.current_queue_index:
                self.current_queue_index -= 1
            elif index == self.current_queue_index:
                # We removed the playing track. Play next?
                if self.current_queue_index < len(self.queue):
                    self._play_current_index()
                else:
                    self.stop()
                    self.current_queue_index = -1

            # Remove from original if present (simplified)
            if pop in self.original_queue:
                self.original_queue.remove(pop)

            self.emit("state-changed", "queue-updated")

    def move_queue_item(self, old_index, new_index):
        if 0 <= old_index < len(self.queue) and 0 <= new_index < len(self.queue):
            # Adjust index when moving down to insert before target, accounting for the list shift from popping.

            insert_index = new_index
            if old_index < new_index:
                insert_index -= 1

            item = self.queue.pop(old_index)
            self.queue.insert(insert_index, item)

            # Update current_queue_index
            # This is tricky. Let's just re-find the playing track if possible, or simple math.
            # The Simple math in question:
            if self.current_queue_index == old_index:
                self.current_queue_index = insert_index
            elif old_index < self.current_queue_index <= insert_index:
                self.current_queue_index -= 1
            elif insert_index <= self.current_queue_index < old_index:
                self.current_queue_index += 1

            # Notify UI
            self.emit("state-changed", "queue-updated")
            return True
        return False

    def clear_queue(self):
        self.stop()
        self.queue = []
        self.original_queue = []
        self.current_queue_index = -1
        self.current_video_id = None
        self.emit("state-changed", "stopped")
        self.emit("metadata-changed", "", "", "", "", "INDIFFERENT")

    def play_queue_index(self, index):
        if 0 <= index < len(self.queue):
            self.stop()
            self.current_queue_index = index
            self._play_current_index()

            # Check for infinite auto-append on manual skip
            if self.queue_is_infinite and self.queue_source_id and self.client:
                if (
                    not self._is_fetching_infinite
                    and self.current_queue_index >= len(self.queue) // 2
                ):
                    self._start_infinite_fetch()
                else:
                    print(
                        f"\033[91m[DEBUG-INFINITE] Conditions NOT met (is_fetching={self._is_fetching_infinite}, index={self.current_queue_index}, halfway={len(self.queue) // 2})\033[0m"
                    )

            self.emit("state-changed", "queue-updated")

    def next(self):
        if self.current_queue_index + 1 < len(self.queue):
            self.current_queue_index += 1
            self._play_current_index()

            # Check for infinite auto-append
            if self.queue_is_infinite and self.queue_source_id and self.client:
                if (
                    not self._is_fetching_infinite
                    and self.current_queue_index >= len(self.queue) // 2
                ):
                    self._start_infinite_fetch()
                else:
                    print(
                        f"\033[91m[DEBUG-INFINITE] Conditions NOT met (is_fetching={self._is_fetching_infinite}, index={self.current_queue_index}, halfway={len(self.queue) // 2})\033[0m"
                    )
        else:
            if self.repeat_mode == "all" and self.queue:
                self.current_queue_index = 0
                self._play_current_index()
            else:
                self.stop()  # End of queue
                self.current_queue_index = -1

        self.emit("state-changed", "queue-updated")

    def previous(self):
        # If > 5 seconds in, restart song
        try:
            pos = self.player.query_position(Gst.Format.TIME)[1]
            if pos > 5 * Gst.SECOND:
                self.player.seek_simple(
                    Gst.Format.TIME, Gst.SeekFlags.FLUSH | Gst.SeekFlags.KEY_UNIT, 0
                )
                return
        except:
            pass

        if self.current_queue_index > 0:
            self.current_queue_index -= 1
            self._play_current_index()
        else:
            # Restart current if at 0
            self.player.seek_simple(
                Gst.Format.TIME, Gst.SeekFlags.FLUSH | Gst.SeekFlags.KEY_UNIT, 0
            )

    def shuffle_queue(self):
        if not self.shuffle_mode:
            # Enable Shuffle
            self.shuffle_mode = True
            if self.queue:
                current = (
                    self.queue[self.current_queue_index]
                    if self.current_queue_index >= 0
                    else None
                )

                # Shuffle the list
                remaining = [
                    t for i, t in enumerate(self.queue) if i != self.current_queue_index
                ]
                random.shuffle(remaining)

                if current:
                    self.queue = [current] + remaining
                    self.current_queue_index = 0
                else:
                    self.queue = remaining
                    self.current_queue_index = -1
        else:
            # Disable Shuffle (Restore original order)
            self.shuffle_mode = False
            # Try to find current track in original queue
            if self.current_queue_index >= 0 and self.current_queue_index < len(
                self.queue
            ):
                current = self.queue[self.current_queue_index]
                self.queue = list(self.original_queue)
                # Restore index
                try:
                    self.current_queue_index = self.queue.index(current)
                except ValueError:
                    self.current_queue_index = 0  # Fallback
            else:
                self.queue = list(self.original_queue)

        # Emit signal to update UI
        self.emit("state-changed", "queue-updated")

    def set_repeat_mode(self, mode):
        if mode in ["none", "track", "all"]:
            self.repeat_mode = mode
            self.emit("state-changed", "repeat-updated")
            if hasattr(self, "mpris_events"):
                self.mpris_events.on_options()

    def _play_current_index(self):
        if 0 <= self.current_queue_index < len(self.queue):
            track = self.queue[self.current_queue_index]
            video_id = str(track.get("videoId") or "")
            title = str(track.get("title") or "Unknown")
            artist = track.get("artist", "")
            thumb = track.get("thumb")
            like_status = str(track.get("likeStatus") or "INDIFFERENT")

            import traceback as _tb

            caller_stack = "".join(_tb.format_stack(limit=6)[:-1])
            print(
                f"DEBUG-PLAY: index={self.current_queue_index} video_id={video_id} queue_len={len(self.queue)}\nCALLER:\n{caller_stack}",
                flush=True,
            )

            # Metadata Normalization & Persistence
            # Handle raw ytmusicapi data and ensure persistent strings in the queue
            if not artist and track.get("artists"):
                artist = ", ".join(
                    [str(a.get("name", "")) for a in track.get("artists") if a]
                )

            if isinstance(artist, list):
                artist = ", ".join([str(a.get("name", "")) for a in artist])

            artist = str(artist or "")

            if not thumb and track.get("thumbnails"):
                thumbs = track.get("thumbnails")
                if thumbs:
                    thumb = thumbs[-1]["url"]

            thumb = str(thumb or "")
            if "ytimg.com" in thumb:
                thumb = get_high_res_url(thumb)

            # SAVE BACK TO QUEUE to ensure UI refreshes (like fallbacks) use normalized strings
            track["artist"] = artist
            track["title"] = title
            track["thumb"] = thumb

            self._load_internal(video_id, title, artist, thumb, like_status)

    def _load_internal(
        self, video_id, title, artist, thumbnail_url, like_status="INDIFFERENT"
    ):
        self.current_video_id = video_id

        self._is_loading = True
        try:
            self.player.set_state(Gst.State.NULL)
        except Exception as e:
            print(f"set_state ERROR: {e}")

        self.current_video_id = video_id
        self.duration = -1
        self.emit("progression", 0.0, 0.0)

        self.load_generation += 1
        current_gen = self.load_generation

        GLib.idle_add(
            self.emit,
            "metadata-changed",
            str(title),
            str(artist),
            str(thumbnail_url if thumbnail_url else ""),
            str(video_id),
            str(like_status),
        )

        # Trigger MPRIS art sync in background
        if thumbnail_url:
            self._sync_mpris_art(thumbnail_url, video_id)

        GLib.idle_add(self._update_logical_state)

        if hasattr(self, "mpris_events"):
            try:
                self.mpris_events.on_player_all()
            except Exception as e:
                print(f"mpris ERROR: {e}")

        # Check for local download - instant offline playback, skip yt-dlp entirely
        local_path = self.download_manager.get_local_path(video_id)
        if local_path:
            print(f"[OFFLINE] Playing local file: {local_path}")
            file_uri = GLib.filename_to_uri(os.path.abspath(local_path), None)
            self._used_cached_url = False
            GLib.idle_add(self._start_playback, file_uri)
            return

        # Check stream URL cache - skip yt-dlp if we have a valid cached URL
        self._playing_from_cache = False
        self._pending_stream_url = None
        self._waiting_for_stream = False
        self._swap_seek_target = None
        self._used_cached_url = False
        self._fallback_stream_url = None
        self._cache_failed_waiting = False
        cached_url = self.stream_cache.get(video_id)
        if cached_url:
            print(f"[CACHE] Using cached stream URL for {video_id}")
            self._used_cached_url = True
            GLib.idle_add(self._start_playback, cached_url)

        thread = threading.Thread(
            target=self._fetch_and_play,
            args=(video_id, title, artist, thumbnail_url, like_status, current_gen),
        )
        thread.daemon = True
        thread.start()

    def extend_queue(self, tracks):
        """Appends new tracks to the queue (and original_queue)."""
        if not tracks:
            return

        # Append to original queue always
        self.original_queue.extend(tracks)

        if self.shuffle_mode:
            # Smart Shuffle: Mix new tracks with UPCOMING tracks
            # We don't want to touch history or current song.

            current_idx = self.current_queue_index

            # Assume valid index; fallback handling can be added if needed.
            if 0 <= current_idx < len(self.queue):
                history_and_current = self.queue[: current_idx + 1]
                upcoming = self.queue[current_idx + 1 :]

                combined = upcoming + tracks
                import random

                random.shuffle(combined)

                self.queue = history_and_current + combined
                # current_queue_index stays same
            else:
                # Queue empty or invalid index, just shuffle all
                self.queue.extend(tracks)
                import random

                random.shuffle(self.queue)
                # If we were playing, index might be -1.
                # If we were stopped, index -1.

                if self.current_queue_index == -1 and self.queue:
                    self.current_queue_index = 0

        else:
            self.queue.extend(tracks)

        self.emit("state-changed", "queue-updated")

    def update_track_thumbnail(self, video_id, working_url):
        """
        Updates the thumbnail URL for a track if a better/working one is found.
        This is called by UI components (AsyncPicture/AsyncImage) when they
        successfully resolve a fallback URL.
        """
        if not video_id or not working_url:
            return

        changed = False
        # Update in current queue
        for track in self.queue:
            if track.get("videoId") == video_id:
                if track.get("thumb") != working_url:
                    track["thumb"] = working_url
                    changed = True

        # Update in original queue
        for track in self.original_queue:
            if track.get("videoId") == video_id:
                track["thumb"] = working_url

        if changed:
            # If this is the currently playing track, re-emit metadata to update MPRIS
            current_track = (
                self.queue[self.current_queue_index]
                if 0 <= self.current_queue_index < len(self.queue)
                else None
            )
            if current_track and current_track.get("videoId") == video_id:
                print(
                    f"[PLAYER] Updating working thumbnail for {video_id}: {working_url}"
                )
                # Re-emit metadata changed to trigger MPRIS update
                self.emit(
                    "metadata-changed",
                    current_track.get("title", ""),
                    current_track.get("artist", ""),
                    working_url,
                    video_id,
                    current_track.get("likeStatus", "INDIFFERENT"),
                )
                self._sync_mpris_art(working_url, video_id)

    def _start_infinite_fetch(self):
        self._is_fetching_infinite = True
        limit = 50

        last_video_id = None
        if self.queue:
            last_video_id = self.queue[-1].get("videoId")

        def fetch_job():
            try:
                data = self.client.get_watch_playlist(
                    video_id=last_video_id,
                    playlist_id=self.queue_source_id,
                    limit=limit,
                    radio=True,
                )
                tracks = data.get("tracks", [])

                # Filter out tracks already in our queue
                existing_ids = {
                    t.get("videoId") for t in self.queue if t.get("videoId")
                }
                new_tracks = [t for t in tracks if t.get("videoId") not in existing_ids]

                if new_tracks:
                    GObject.idle_add(self._on_infinite_fetch_complete, new_tracks)
                else:
                    self._is_fetching_infinite = False
            except Exception as e:
                print(f"Error fetching infinite queue: {e}")
                self._is_fetching_infinite = False

        thread = threading.Thread(target=fetch_job)
        thread.daemon = True
        thread.start()

    def _on_infinite_fetch_complete(self, new_tracks):
        self.extend_queue(new_tracks)
        self._is_fetching_infinite = False

    def _create_cookie_file(self, headers):
        """Creates a temporary Netscape format cookie file from headers."""
        import tempfile
        import time

        cookie_str = headers.get("Cookie", "")
        if not cookie_str:
            return None

        # Netscape format requires specific tab-separated columns
        fd, path = tempfile.mkstemp(suffix=".txt", text=True)
        with os.fdopen(fd, "w") as f:
            f.write("# Netscape HTTP Cookie File\n")

            now = int(time.time()) + 3600 * 24 * 365  # 1 year validity

            parts = cookie_str.split(";")
            for part in parts:
                if "=" in part:
                    # Handle potential whitespace around parts
                    pair = part.strip().split("=", 1)
                    if len(pair) != 2:
                        continue
                    key, value = pair

                    # Use .youtube.com for everything - proven effective for locking tracks in sweep
                    f.write(f".youtube.com\tTRUE\t/\tTRUE\t{now}\t{key}\t{value}\n")

        return path

    def _fetch_and_play(
        self,
        video_id,
        title_hint,
        artist_hint,
        thumb_hint,
        like_status_hint,
        generation,
    ):
        if generation != self.load_generation:
            print(
                f"Stale load generation {generation} (current {self.load_generation}). Aborting."
            )
            return
        import os

        url = f"https://music.youtube.com/watch?v={video_id}"

        # Use a local copy of options to prevent race conditions
        opts = self.ydl_opts.copy()
        # Enable verbose for one cycle to debug cookie usage
        opts["verbose"] = True

        cookie_file = None
        try:
            # Inject headers/cookies if authenticated
            if self.client.is_authenticated() and self.client.api:
                # Create Netscape cookie file
                cookie_file = self._create_cookie_file(self.client.api.headers)
                if cookie_file:
                    opts["cookiefile"] = cookie_file

                # CRITICAL: User-Agent MUST match the cookies for them to be accepted by YouTube
                ua = self.client.api.headers.get("User-Agent")
                if ua:
                    opts["user_agent"] = ua
                    # Also set it in http_headers for good measure
                    opts["http_headers"] = {"User-Agent": ua}

                # Still pass Authorization if available
                auth = self.client.api.headers.get("Authorization")
                if auth:
                    if "http_headers" not in opts:
                        opts["http_headers"] = {}
                    opts["http_headers"]["Authorization"] = auth
            else:
                pass

            with YoutubeDL(opts) as ydl:
                info = ydl.extract_info(url, download=False)
                stream_url = info["url"]

                # Extract only what we need, then drop the large info dict
                fetched_title = info.get("title", "Unknown")
                fetched_artist = info.get("uploader", "Unknown")
                fetched_thumb = info.get("thumbnail")
                del info  # Free 100KB+ of format/subtitle data

                # If hints are placeholders, try to get better metadata from ytmusicapi
                if (not title_hint or title_hint == "Loading...") or (
                    not artist_hint or artist_hint == "Unknown"
                ):
                    try:
                        song_details = self.client.get_song(video_id)
                        if song_details:
                            v_details = song_details.get("videoDetails", {})
                            if "title" in v_details:
                                fetched_title = v_details["title"]
                            if "author" in v_details:
                                fetched_artist = v_details["author"]

                            # Use high-res thumbnail from get_song if available
                            if (
                                not thumb_hint
                                and "thumbnail" in v_details
                                and "thumbnails" in v_details["thumbnail"]
                            ):
                                thumbs = v_details["thumbnail"]["thumbnails"]
                                if thumbs:
                                    fetched_thumb = thumbs[-1]["url"]

                    except Exception as e:
                        print(f"Error fetching metadata from ytmusicapi: {e}")

                final_title = (
                    title_hint
                    if title_hint and title_hint != "Loading..."
                    else fetched_title
                )
                final_artist = (
                    artist_hint
                    if artist_hint and artist_hint != "Unknown"
                    else fetched_artist
                )

                print(f"Playing: {final_title} by {final_artist}")

                final_thumb = thumb_hint or fetched_thumb or ""
                if "ytimg.com" in final_thumb:
                    final_thumb = get_high_res_url(final_thumb)

                # Update the queue track if possible so subsequent refreshes find it
                if 0 <= self.current_queue_index < len(self.queue):
                    track = self.queue[self.current_queue_index]
                    if track.get("videoId") == video_id:
                        track["title"] = final_title
                        track["artist"] = final_artist
                        track["thumb"] = final_thumb

                        # Fetch album if missing (needed for Discord RPC)
                        if not track.get("album"):
                            try:
                                wp = self.client.get_watch_playlist(
                                    video_id=video_id, limit=1
                                )
                                wp_tracks = wp.get("tracks", [])
                                if wp_tracks and wp_tracks[0].get("album"):
                                    track["album"] = wp_tracks[0]["album"]
                                    if getattr(self, "discord_rpc", None):
                                        self.discord_rpc.update()
                            except Exception:
                                pass

                # Check generation again before playing
                if generation != self.load_generation:
                    print(
                        f"Stale load generation {generation} before playbin set. Aborting."
                    )
                    if cookie_file and os.path.exists(cookie_file):
                        os.remove(cookie_file)
                    return

                # Cache the stream URL for future plays
                self.stream_cache.put(video_id, stream_url)

                if getattr(self, "_cache_failed_waiting", False):
                    # Cached URL failed earlier, yt-dlp just finished - play now
                    print("[CACHE] yt-dlp finished, playing after cache failure")
                    self._cache_failed_waiting = False
                    GObject.idle_add(self._start_playback, stream_url)
                elif self._used_cached_url:
                    # Store the fresh URL as fallback in case cached URL fails
                    self._fallback_stream_url = stream_url
                else:
                    GObject.idle_add(self._start_playback, stream_url)

                GObject.idle_add(
                    self.emit,
                    "metadata-changed",
                    final_title,
                    final_artist,
                    final_thumb,
                    video_id,
                    like_status_hint,
                )

                # Pre-cache next songs in queue
                self._precache_next(generation)
        except Exception as e:
            print(f"Error fetching URL: {e}")
        finally:
            if cookie_file and os.path.exists(cookie_file):
                try:
                    os.remove(cookie_file)
                except:
                    pass

    def _precache_next(self, generation):
        """Pre-cache stream URLs for 3 songs ahead and 3 behind."""
        if generation != self.load_generation:
            return

        current = self.current_queue_index
        queue_len = len(self.queue)
        indices = []
        for offset in range(1, 4):
            if current + offset < queue_len:
                indices.append(current + offset)
            if current - offset >= 0:
                indices.append(current - offset)

        from yt_dlp import YoutubeDL

        for idx in indices:
            if generation != self.load_generation:
                return
            track = self.queue[idx]
            vid = track.get("videoId")
            if not vid or self.stream_cache.get(vid):
                continue
            try:
                url = f"https://music.youtube.com/watch?v={vid}"
                opts = self.ydl_opts.copy()
                opts["quiet"] = True
                opts.pop("verbose", None)

                cookie_file = None
                if self.client.is_authenticated() and self.client.api:
                    cookie_file = self._create_cookie_file(self.client.api.headers)
                    if cookie_file:
                        opts["cookiefile"] = cookie_file
                    ua = self.client.api.headers.get("User-Agent")
                    if ua:
                        opts["user_agent"] = ua

                with YoutubeDL(opts) as ydl:
                    info = ydl.extract_info(url, download=False)
                    stream_url = info["url"]
                    del info
                self.stream_cache.put(vid, stream_url)
                print(f"[CACHE] Pre-cached stream URL for song {idx}: {vid}")
            except Exception as e:
                print(f"[CACHE] Pre-cache error for {vid}: {e}")
            finally:
                if cookie_file and os.path.exists(cookie_file):
                    try:
                        os.remove(cookie_file)
                    except OSError:
                        pass

    def _start_playback(self, uri, cookie_file=None):
        self.player.set_state(Gst.State.NULL)
        self.player.set_property("uri", uri)
        self.player.set_state(Gst.State.PLAYING)

        # Direct URLs typically work without explicit cookies. Stale URLs are handled in _load_internal.
        return False

    def play(self):
        self.player.set_state(Gst.State.PLAYING)
        self._update_logical_state()

    def pause(self):
        self.player.set_state(Gst.State.PAUSED)
        self._update_logical_state()

    def stop(self):
        self.player.set_state(Gst.State.NULL)
        self._is_loading = False
        # Force stopped state immediately
        if self._current_logical_state != "stopped":
            self._current_logical_state = "stopped"
            self.emit("state-changed", "stopped")

    def _update_logical_state(self):
        new_state = "stopped"
        if self.player:
            state = self.player.get_state(0)[1]
            if state == Gst.State.PLAYING:
                new_state = "playing"
            elif state == Gst.State.PAUSED:
                new_state = "paused"

        if new_state != self._current_logical_state:
            self._current_logical_state = new_state
            try:
                GLib.idle_add(self.emit, "state-changed", new_state)
            except Exception as e:
                pass

    def on_message(self, bus, message):
        t = message.type
        if t == Gst.MessageType.EOS:
            # Ignore EOS that arrives mid-load. When the user skips rapidly,
            # GStreamer can emit EOS for the *previous* stream as it tears
            # down — acting on it would queue an extra next() and over-advance
            # the queue, eventually wrapping to 0 under repeat=all.
            if self._is_loading:
                print("EOS during load — ignoring (stale stream).", flush=True)
                return
            # Bus messages are async — a stale EOS from the previous pipeline
            # can land *after* the new track has already reached PLAYING.
            # Reject EOS that arrives within the first second of a new track.
            import time as _time

            if self._track_started_at and _time.time() - self._track_started_at < 1.0:
                print(
                    "EOS within 1s of track start — ignoring (stale stream).",
                    flush=True,
                )
                return
            print("EOS Reached. Advancing to next track.", flush=True)
            self.stop()
            if self.repeat_mode == "track":
                GObject.idle_add(self._play_current_index)
            else:
                GObject.idle_add(self.next)
        elif t == Gst.MessageType.ASYNC_DONE:
            # The stream is actually loaded and ready
            if hasattr(self, "mpris_events"):
                self.mpris_events.on_player_all()  # Refresh duration and status
        elif t == Gst.MessageType.ERROR:
            err, debug = message.parse_error()
            print(f"Error: {err}, {debug}")

            # If cached URL failed, try the fresh yt-dlp resolved URL
            if self._used_cached_url:
                fallback = getattr(self, "_fallback_stream_url", None)
                self._used_cached_url = False
                if fallback:
                    print("[CACHE] Cached URL failed, using fresh URL")
                    self._fallback_stream_url = None
                    if self.current_video_id:
                        self.stream_cache.put(self.current_video_id, fallback)
                    self._start_playback(fallback)
                    return
                else:
                    # yt-dlp hasn't finished yet - flag so it plays when ready
                    print("[CACHE] Cached URL failed, waiting for yt-dlp...")
                    self._cache_failed_waiting = True
                    self.player.set_state(Gst.State.NULL)
                    return

            self.player.set_state(Gst.State.NULL)
            self._is_loading = False
            self._update_logical_state()
        elif t == Gst.MessageType.STATE_CHANGED:
            if message.src == self.player:
                old, new, pending = message.parse_state_changed()
                if new == Gst.State.PLAYING:
                    if abs(self.get_volume() - self._user_volume) > 0.001:
                        linear = GstAudio.StreamVolume.convert_volume(
                            GstAudio.StreamVolumeFormat.CUBIC,
                            GstAudio.StreamVolumeFormat.LINEAR,
                            self._user_volume,
                        )
                        self._internal_volume_change = True
                        self.player.set_property("volume", linear)
                        self._internal_volume_change = False
                    self._is_loading = False
                    import time as _time

                    self._track_started_at = _time.time()
                    if getattr(self, "discord_rpc", None):
                        self.discord_rpc.update()
                self._update_logical_state()
        # BUFFERING messages are intentionally ignored - playbin manages
        # stream buffering internally and briefly pauses the pipeline,
        # which would cause the spinner to flash unnecessarily.

    def get_state_string(self):
        """Returns the current logical player state."""
        return self._current_logical_state

    def _sync_mpris_art(self, url, video_id):
        """Downloads, crops, and saves artwork locally for MPRIS with fallback support."""
        # Try local cover first (works offline)
        if video_id:
            local_path = self.download_manager.get_local_path(video_id)
            if local_path:
                import os as _os

                cover = _os.path.join(_os.path.dirname(local_path), "cover.jpg")
                if _os.path.exists(cover):
                    self.mpris_art_url = f"file://{cover}"
                    if hasattr(self, "mpris_events"):
                        self.mpris_events.on_title()
                    return
        if not url:
            return

        def job(current_url, fallbacks=None):
            if not current_url or self.current_video_id != video_id:
                return

            try:
                # 1. Ensure we use clean high-res URL if not already provided
                if fallbacks is None:
                    clean_url = get_high_res_url(current_url)
                    fallbacks = get_ytimg_fallbacks(clean_url)
                    if current_url != clean_url and current_url not in fallbacks:
                        fallbacks.append(current_url)
                    fetch_url = clean_url
                else:
                    fetch_url = current_url

                headers = {
                    "User-Agent": "Mozilla/5.0 (Windows NT 10.0; Win64; x64) AppleWebKit/537.36 (KHTML, like Gecko) Chrome/91.0.4472.124 Safari/537.36"
                }
                if self.client and self.client.is_authenticated():
                    # Use cookies for YouTube related domains to support private covers
                    if any(
                        d in fetch_url
                        for d in [
                            "youtube.com",
                            "ytimg.com",
                            "googleusercontent.com",
                            "ggpht.com",
                        ]
                    ):
                        cookie = self.client.api.headers.get("Cookie")
                        if cookie:
                            headers["Cookie"] = cookie

                import requests

                resp = requests.get(fetch_url, headers=headers, timeout=10)
                resp.raise_for_status()
                data = resp.content

                # 2. Load and Crop
                loader = GdkPixbuf.PixbufLoader()
                loader.write(data)
                loader.close()
                pixbuf = loader.get_pixbuf()

                if pixbuf:
                    w = pixbuf.get_width()
                    h = pixbuf.get_height()
                    size = min(w, h)
                    pixbuf = pixbuf.new_subpixbuf(
                        (w - size) // 2, (h - size) // 2, size, size
                    )

                    # 3. Save to cache
                    cache_dir = os.path.join(GLib.get_user_cache_dir(), "mixtapes")
                    os.makedirs(cache_dir, exist_ok=True)

                    # Cleanup old art files to prevent bloat and cache issues
                    for old_art in glob.glob(
                        os.path.join(cache_dir, "mpris_art_*.jpg")
                    ):
                        try:
                            os.remove(old_art)
                        except:
                            pass

                    # Use unique filename per track to bypass MPRIS client caching
                    safe_video_id = video_id.replace("-", "_").replace(".", "_")
                    target_path = os.path.join(
                        cache_dir, f"mpris_art_{safe_video_id}.jpg"
                    )

                    pixbuf.savev(target_path, "jpeg", ["quality"], ["90"])
                    self.mpris_art_url = f"file://{target_path}"

                    # 4. Notify MPRIS to refresh metadata with the NEW local URL
                    if hasattr(self, "mpris_events"):
                        GLib.idle_add(self.mpris_events.on_player_all)

            except Exception as e:
                if fallbacks:
                    next_url = fallbacks.pop(0)
                    print(f"[PLAYER] MPRIS art fallback to: {next_url}")
                    job(next_url, fallbacks)
                else:
                    print(f"[PLAYER] MPRIS art sync failed: {e}")

        thread = threading.Thread(target=job, args=(url,), daemon=True)
        thread.start()

    def update_position(self):
        import time

        now = time.time()

        # 1. Protection during seek/load
        # If we are loading or just sought, don't trust GStreamer yet
        if self._is_loading or (now - self.last_seek_time < 0.8):
            return True

        ret, state, pending = self.player.get_state(0)
        if state in [Gst.State.PLAYING, Gst.State.PAUSED]:
            # 2. Update Duration if it changed (vital for MPRIS progress bar scale)
            success_dur, dur_nanos = self.player.query_duration(Gst.Format.TIME)
            if success_dur:
                new_dur = dur_nanos / Gst.SECOND
                if (
                    abs(new_dur - self.duration) > 0.1
                ):  # Threshold to avoid float jitter
                    self.duration = new_dur
                    if hasattr(self, "mpris_events"):
                        self.mpris_events.on_title()  # Syncs 'mpris:length'
                    if getattr(self, "discord_rpc", None):
                        self.discord_rpc.update()

            # 3. Update Position
            success_pos, pos_nanos = self.player.query_position(Gst.Format.TIME)
            if success_pos:
                current_time = pos_nanos / Gst.SECOND

                # Update the Adapter's cache immediately
                if hasattr(self, "mpris_adapter"):
                    self.mpris_adapter._last_pos = pos_nanos // 1000

                # 4. Emit progression for local UI
                # We use float(d) to ensure the UI progress bar has a max value
                d = self.duration if self.duration > 0 else 0
                self.emit("progression", float(current_time), float(d))

        return True

    def seek(self, position, flush=True):
        """Seek to position in seconds"""
        if self.player.get_state(0)[1] == Gst.State.NULL:
            return

        import time

        self.last_seek_time = time.time()

        flags = Gst.SeekFlags.ACCURATE
        if flush:
            flags |= Gst.SeekFlags.FLUSH

        self.player.seek_simple(
            Gst.Format.TIME,
            flags,
            int(position * Gst.SECOND),
        )

        if hasattr(self, "mpris_events"):
            self.mpris_events.on_seek(int(position * 1_000_000))

    def get_volume(self):
        """Get volume in cubic (perceptual) scale 0.0-1.0, matching system mixer."""
        linear = self.player.get_property("volume")
        return GstAudio.StreamVolume.convert_volume(
            GstAudio.StreamVolumeFormat.LINEAR,
            GstAudio.StreamVolumeFormat.CUBIC,
            linear,
        )

    def set_volume(self, value):
        """Set volume from cubic (perceptual) scale 0.0-1.0."""
        self._user_volume = float(value)
        linear = GstAudio.StreamVolume.convert_volume(
            GstAudio.StreamVolumeFormat.CUBIC,
            GstAudio.StreamVolumeFormat.LINEAR,
            float(value),
        )
        self._internal_volume_change = True
        self.player.set_property("volume", linear)
        self._internal_volume_change = False
        if value > 0 and self.get_mute():
            self.set_mute(False)
        else:
            GLib.idle_add(self.emit, "volume-changed", float(value), self.get_mute())

    def get_mute(self):
        return self.player.get_property("mute")

    def set_mute(self, is_muted):
        self._internal_volume_change = True
        self.player.set_property("mute", is_muted)
        self._internal_volume_change = False
        GLib.idle_add(self.emit, "volume-changed", self.get_volume(), is_muted)

    def _on_external_volume_change(self, element, param):
        """Called when volume changes externally (system mixer)."""
        if self._internal_volume_change:
            return
        # During track loads, playbin can rebuild its audio sink and briefly
        # report the new sink's default volume. Ignore those spurious notifies
        # so the UI doesn't snap to 100%; the real value is restored once the
        # pipeline reaches PLAYING (see on_message).
        if self._is_loading:
            return
        GLib.idle_add(self.emit, "volume-changed", self.get_volume(), self.get_mute())

    def _on_external_mute_change(self, element, param):
        """Called when mute changes externally."""
        if self._internal_volume_change:
            return
        GLib.idle_add(self.emit, "volume-changed", self.get_volume(), self.get_mute())

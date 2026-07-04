import gi
import sys
import threading
import random
import os

gi.require_version("Gst", "1.0")
gi.require_version("GstAudio", "1.0")
from gi.repository import Gst, GstAudio, GObject, GLib, GdkPixbuf
import glob
from yt_dlp import YoutubeDL
from ui.utils import get_high_res_url, get_ytimg_fallbacks
from player.cache import StreamCache
from player.downloads import DownloadManager
from api.client import MusicClient

HAS_MPRIS = False
HAS_SMTC = False
if sys.platform == "win32":
    try:
        from player.smtc import SMTCAdapter
        HAS_SMTC = True
    except ImportError:
        pass
else:
    try:
        from player.mpris import MuseMprisAdapter, MuseServer, MuseEventAdapter
        HAS_MPRIS = True
    except ImportError:
        pass

from player.discord_rpc import DiscordRPCAdapter


_libc = None
_last_malloc_trim = 0.0


def _malloc_trim(min_interval=0.0):
    """Hand glibc's freed-but-retained heap back to the OS. After a track is
    torn down, free() leaves the blocks sitting in the arena so RSS stays
    high — this is the "memory isn't released after I clear the queue" the
    user sees. With MALLOC_ARENA_MAX=2 (set in main.py) almost all allocation
    lives in the main arena, which malloc_trim(0) can actually return. No-op
    off glibc. `min_interval` throttles back-to-back calls (rapid skips)."""
    global _libc, _last_malloc_trim
    if not sys.platform.startswith("linux"):
        return
    import time as _t
    now = _t.monotonic()
    if min_interval and (now - _last_malloc_trim) < min_interval:
        return
    try:
        import ctypes
        if _libc is None:
            _libc = ctypes.CDLL("libc.so.6")
        _libc.malloc_trim(0)
        _last_malloc_trim = now
    except Exception:
        pass


def _extract_spectrum_bands(structure):
    """Pull a list[float] of magnitudes out of a GStreamer `spectrum`
    bus-message Structure. Returns None if no path yields data.

    PyGObject's surface for GstValueList varies by version:
      - `structure.get_list("magnitude")` returns (True, Gst.ValueArray)
        on modern builds. The ValueArray is NOT iterable but exposes
        `.n_values` + `.get_nth(i)`.
      - `structure.get_value("magnitude")` may return a plain Python
        list, a GValueArray-like with `.n_values`, or raise
        `TypeError: unknown type GstValueList` if PyGObject doesn't
        have a converter registered for the inner GType.
    Try both APIs and walk whatever shape comes back.
    """
    def _walk(obj):
        if obj is None:
            return None
        if isinstance(obj, (list, tuple)):
            try:
                return [float(v) for v in obj]
            except Exception:
                return None
        if hasattr(obj, "n_values") and hasattr(obj, "get_nth"):
            try:
                out = []
                for i in range(obj.n_values):
                    v = obj.get_nth(i)
                    out.append(
                        float(v.get_float()) if hasattr(v, "get_float") else float(v)
                    )
                return out
            except Exception:
                return None
        try:
            return [float(v) for v in obj]
        except Exception:
            return None

    try:
        ok, mags = structure.get_list("magnitude")
        if ok:
            bands = _walk(mags)
            if bands:
                return bands
    except Exception:
        pass

    try:
        raw = structure.get_value("magnitude")
    except Exception:
        raw = None
    return _walk(raw)


def _parse_track_duration(track):
    """Return a positive duration in seconds for `track`, or 0 if unknown.

    ytmusicapi populates `duration_seconds` for most surfaces, but uploaded
    songs from get_library_upload_songs() only carry `duration` as a "M:SS"
    (or "H:MM:SS") string. Check both so the seek-bar fallback works for
    uploads too.
    """
    secs = track.get("duration_seconds")
    if isinstance(secs, (int, float)) and secs > 0:
        return int(secs)
    if isinstance(secs, str) and secs.isdigit():
        n = int(secs)
        if n > 0:
            return n
    dur = track.get("duration")
    if isinstance(dur, str):
        parts = dur.strip().split(":")
        try:
            if len(parts) == 2:
                return int(parts[0]) * 60 + int(parts[1])
            if len(parts) == 3:
                return int(parts[0]) * 3600 + int(parts[1]) * 60 + int(parts[2])
        except ValueError:
            pass
    return 0


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
        "track-error": (
            GObject.SignalFlags.RUN_FIRST,
            None,
            (str, str, str),
        ),  # video_id, title, error_message
    }

    def __init__(self):
        super().__init__()
        GLib.set_application_name("Mixtapes")
        Gst.init(None)
        self.client = MusicClient()
        self.player = Gst.ElementFactory.make("playbin", "player")

        # Disable video output using playbin flags (unsetting GST_PLAY_FLAG_VIDEO)
        # GST_PLAY_FLAG_VIDEO is 1 << 0
        flags = self.player.get_property("flags")
        self.player.set_property("flags", flags & ~(1 << 0))

        # Insert a passthrough spectrum analyzer between the decoder and the
        # audio sink. The element emits ELEMENT bus messages with per-band
        # magnitudes that the cover view's visualizer subscribes to. We pull
        # plenty of raw bands (128) and let the UI reduce them to fewer
        # display bars on a log scale, which gives a much more "alive" feel
        # than a flat 32-band mapping (bass and treble each get their own
        # display real estate instead of treble dominating).
        self._visualizer_bands = 128
        self._visualizer_threshold_db = -80.0
        # Position-keyed queue of (running_time_ns, bands) entries fed by
        # the spectrum bus message and drained by pull_visualizer_bands.
        # Sized for ~3s of buffer at the spectrum element's 30Hz tick.
        from collections import deque
        self._viz_queue = deque(maxlen=120)
        spectrum = Gst.ElementFactory.make("spectrum", "visualizer-spectrum")
        if spectrum is not None:
            spectrum.set_property("post-messages", True)
            spectrum.set_property("message-magnitude", True)
            spectrum.set_property("message-phase", False)
            spectrum.set_property("interval", 33_000_000)  # 33 ms / ~30 Hz
            spectrum.set_property("bands", self._visualizer_bands)
            spectrum.set_property("threshold", int(self._visualizer_threshold_db))
            spectrum.set_property("multi-channel", False)
            self.player.set_property("audio-filter", spectrum)
            print("[VISUALIZER] spectrum element loaded — bars should animate")
        else:
            print(
                "[VISUALIZER] spectrum element NOT available "
                "(missing gst-plugins-good in this runtime) — bars will be inert"
            )

        # Inject auth cookies + User-Agent into the HTTP source on every
        # source-setup. Helps for non-upload streams that need cookies on
        # follow-up range requests; uploads still go via tmpfs (below).
        self.player.connect("source-setup", self._on_source_setup)

        # Tracks the current /dev/shm-backed local file for upload playback.
        # Deleted when a new track loads or the app shuts down so we don't
        # leak hundreds of MB into RAM across a long session.
        self._current_tmpfs_path = None
        import atexit
        atexit.register(self._cleanup_all_tmpfs)

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

        # Gapless playback: about-to-finish fires when the current uri is
        # close to ending. If we hand playbin a new uri synchronously in
        # the handler, it switches without re-creating the pipeline — no
        # NULL transition, no preroll, no audible gap. Critical for album
        # listening where tracks are mastered to flow together.
        self.player.connect("about-to-finish", self._on_about_to_finish)
        self._pending_gapless_index = None  # set by about-to-finish, cleared by stream-start

        # Listen for external volume changes (system mixer)
        self.player.connect("notify::volume", self._on_external_volume_change)
        self.player.connect("notify::mute", self._on_external_mute_change)
        self._internal_volume_change = False
        self._user_volume = None # initially set as none, and will depend on wireplumber's external volume change call to set its initial volume (from last session)
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
        # History sync: record a play to the user's YT account.
        # `_history_mode` is one of:
        #   - "immediate" (default, matches YT Music's behavior): record
        #     as soon as the track starts loading.
        #   - "after_30s": wait until the user has actually been playing
        #     it for 30 seconds.
        #   - "never": don't record.
        # Keyed by videoId so a single track is only reported once even
        # if the user seeks around inside it.
        self._history_recorded_for = None
        self._history_record_after_sec = 30.0
        self._history_mode = self._load_history_mode()

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

        # boolean checker if media api (MPRIS or SMTC) is loaded
        self.media_api_loaded = False

        # Discord Rich Presence (cross-platform; no-op if pypresence missing
        # or Discord not running).
        try:
            self.discord_rpc = DiscordRPCAdapter(self)
            self.connect("state-changed", self._on_discord_state_changed)
            self.connect("metadata-changed", self._on_discord_metadata_changed)
        except Exception as e:
            print(f"Discord RPC init failed: {e}")
            self.discord_rpc = None

    def _load_media_api(self):
        "Starts MPRIS or SMTC for Linux or Windows, loads once only when _start_playback is called"
        if self.media_api_loaded:
            return

        self.media_api_loaded = True

        # MPRIS Setup (Linux-only, requires D-Bus)
        if HAS_MPRIS:
            self.mpris_adapter = MuseMprisAdapter(self)
            self.mpris_server = MuseServer("Mixtapes", adapter=self.mpris_adapter)
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

    def _on_discord_state_changed(self, obj, state):
        if getattr(self, "discord_rpc", None):
            self.discord_rpc.update()

    def _on_discord_metadata_changed(
        self, obj, title, artist, thumb, video_id, like_status
    ):
        if getattr(self, "discord_rpc", None):
            self.discord_rpc.update()

    def _on_mpris_state_changed(self, obj, state):
        if hasattr(self, "mpris_events"):
            # Explicitly tell the server the PlaybackStatus changed
            self.mpris_events.on_playpause()
            # Update metadata because length or 'CanGoNext' might have changed
            self.mpris_events.on_player_all()

    def _on_mpris_metadata_changed(
        self, obj, title, artist, thumb, video_id, like_status
    ):
        if hasattr(self, "mpris_events"):
            # Trigger the 'Metadata' property update
            self.mpris_events.on_title()
            # Update UI-related flags like CanGoNext/Previous
            self.mpris_events.on_player_all()

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

    def _on_smtc_metadata_changed(self, obj, title, artist, thumb, video_id, like_status):
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

    @staticmethod
    def _normalize_watch_playlist_tracks(tracks):
        """watch_playlist results use `thumbnail` (singular); the rest of the
        app expects `thumbnails` and `thumb`. Without this, infinite-radio
        extensions past the first batch end up with no usable thumbnail url,
        which makes MPRIS reuse the previous track's cover."""
        for t in tracks:
            if "thumbnail" in t and "thumbnails" not in t:
                t["thumbnails"] = t["thumbnail"]
            if t.get("thumbnails") and not t.get("thumb"):
                thumbs = t["thumbnails"]
                if isinstance(thumbs, list) and thumbs:
                    t["thumb"] = thumbs[-1].get("url", "")

    def start_radio(self, video_id=None, playlist_id=None):
        """Start a radio (mix) from a song or playlist. Runs in background."""

        def _fetch():
            try:
                data = self.client.get_watch_playlist(
                    video_id=video_id, playlist_id=playlist_id, limit=50, radio=True
                )
                tracks = data.get("tracks", [])
                if tracks:
                    self._normalize_watch_playlist_tracks(tracks)
                    pid = data.get("playlistId")
                    GObject.idle_add(self.set_queue, tracks, 0, False, pid, True)
                else:
                    print("[RADIO] No tracks returned")
            except Exception as e:
                print(f"[RADIO] Error: {e}")

        threading.Thread(target=_fetch, daemon=True).start()

    def play_then_radio(self, tracks, start_index, seed_video_id):
        """Play `tracks` starting at `start_index`, then continue with a radio
        seeded from `seed_video_id` (typically the last track in the group).

        Used by the home feed so that activating a song from a section plays
        the rest of the section, then transitions into an infinite radio mix
        when the section runs out.
        """
        if not tracks or not seed_video_id:
            self.set_queue(tracks, start_index)
            return

        stamp = f"home-radio:{seed_video_id}:{id(tracks)}"
        self.set_queue(tracks, start_index, source_id=stamp)

        def _fetch():
            try:
                data = self.client.get_watch_playlist(
                    video_id=seed_video_id, limit=50, radio=True
                )
                radio_tracks = data.get("tracks", [])
                if not radio_tracks:
                    return
                self._normalize_watch_playlist_tracks(radio_tracks)
                pid = data.get("playlistId")

                def _apply():
                    # If the user already replaced the queue, drop the result.
                    if self.queue_source_id != stamp:
                        return False
                    existing = {
                        t.get("videoId") for t in self.queue if t.get("videoId")
                    }
                    new = [
                        t for t in radio_tracks
                        if t.get("videoId") and t.get("videoId") not in existing
                    ]
                    if new:
                        self.extend_queue(new)
                    # Switch the source over to the real radio playlist so the
                    # built-in infinite extender takes it from here.
                    if pid:
                        self.queue_source_id = pid
                        self.queue_is_infinite = True
                    return False

                GObject.idle_add(_apply)
            except Exception as e:
                print(f"[HOME-RADIO] failed: {e}")

        threading.Thread(target=_fetch, daemon=True).start()

    def set_queue(
        self, tracks, start_index=0, shuffle=False, source_id=None, is_infinite=False
    ):
        """
        Sets the global queue and plays the track at start_index.
        tracks: list of dicts with videoId, title, artist, thumb
        """
        import traceback as _tb
        _caller = "".join(_tb.format_stack(limit=4)[:-1])
        print(
            f"[JUMP-TRACE] set_queue n={len(tracks)} start_index={start_index}"
            f" shuffle={shuffle} source_id={source_id}\nCALLER:\n{_caller}",
            flush=True,
        )
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
        # Bump the load generation so any in-flight _fetch_and_play /
        # yt-dlp resolution that hasn't reached _start_playback yet
        # sees the new generation and aborts. Without this, clicking
        # Clear while a track was loading would let the resolution
        # finish and start playback into an empty queue.
        self.load_generation += 1
        self.queue = []
        self.original_queue = []
        self.current_queue_index = -1
        self.current_video_id = None
        self._current_source_video_id = None
        self.emit("state-changed", "stopped")
        self.emit("metadata-changed", "", "", "", "", "INDIFFERENT")
        # The user explicitly dropped the whole queue — a good moment to
        # return the freed decode/UI buffers to the OS rather than letting
        # glibc sit on them.
        _malloc_trim()

    def play_queue_index(self, index):
        import traceback as _tb
        _caller = "".join(_tb.format_stack(limit=4)[:-1])
        print(
            f"[JUMP-TRACE] play_queue_index({index}) queue_len={len(self.queue)}"
            f" current_idx={self.current_queue_index}\nCALLER:\n{_caller}",
            flush=True,
        )
        if 0 <= index < len(self.queue):
            self.stop()
            self.current_queue_index = index
            self._play_current_index()
            self._maybe_extend_infinite()
            self.emit("state-changed", "queue-updated")

    def next(self):
        if self.current_queue_index + 1 < len(self.queue):
            self.current_queue_index += 1
            self._play_current_index()
            self._maybe_extend_infinite()
        else:
            if self.repeat_mode == "all" and self.queue:
                self.current_queue_index = 0
                self._play_current_index()
            elif (
                self.queue_is_infinite
                and self.queue_source_id
                and self.client
                and self.queue
            ):
                # Queue actually ran out on an infinite source. The standard
                # halfway-trigger should normally hide this, but YT sometimes
                # returns the same radio batch for the same seed videoId and
                # our dedup filter ends up wiping the whole response. Kick
                # off a final extension keyed on the currently-playing track
                # before declaring the queue dead.
                self._force_radio_extend()
            else:
                self.stop()  # End of queue
                self.current_queue_index = -1

        self.emit("state-changed", "queue-updated")

    # ── Gapless handlers ──────────────────────────────────────────────────────

    def _compute_next_gapless_index(self):
        """Decide which queue index the current track should flow into,
        applying repeat / end-of-queue rules. Returns None if no gapless
        candidate exists (let the normal EOS path handle it)."""
        if self.repeat_mode == "track":
            return self.current_queue_index
        cur = self.current_queue_index
        if 0 <= cur and cur + 1 < len(self.queue):
            return cur + 1
        if self.repeat_mode == "all" and self.queue:
            return 0
        # Infinite radio queues *can* gapless-into the next track once
        # the extend lands, but if the queue genuinely ran out at this
        # moment we let EOS trigger the force-extend fallback.
        return None

    def _on_about_to_finish(self, _playbin):
        """Runs on the GStreamer streaming thread. Must complete fast —
        playbin uses whatever uri is set when this returns. yt-dlp
        resolution is too slow to fit here, so we only enable gapless
        when the next track's URL is already cached (file:// for
        downloads, the StreamCache for streams). Upload tracks (tmpfs
        path) are skipped — they need their own pre-buffering."""
        nxt = self._compute_next_gapless_index()
        if nxt is None or nxt >= len(self.queue):
            return

        track = self.queue[nxt]
        if not track:
            return
        # Upload tracks need to be staged onto tmpfs before they're
        # playable — defer to the normal _load_internal path on EOS.
        if track.get("entityId"):
            return

        vid = track.get("videoId")
        if not vid:
            return

        # Prefer a downloaded local copy (always works, offline-safe).
        local_path = self.download_manager.get_local_path(vid)
        if local_path:
            try:
                uri = GLib.filename_to_uri(os.path.abspath(local_path), None)
            except Exception:
                uri = None
        else:
            uri = self.stream_cache.get(vid)

        if not uri:
            # No cached URL → no gapless this time. EOS will fire and
            # _load_internal will resolve via yt-dlp the normal way.
            return

        try:
            self.player.set_property("uri", uri)
            self._pending_gapless_index = nxt
            print(
                f"[GAPLESS] queued next uri for index={nxt} vid={vid} "
                f"({'local' if local_path else 'cached'})"
            )
        except Exception as e:
            print(f"[GAPLESS] failed to set next uri: {e}")
            self._pending_gapless_index = None

    def _apply_gapless_transition(self):
        """Main-thread finisher for a gapless track swap. Mirrors the
        post-load housekeeping in _load_internal — queue index, history,
        metadata signal, MPRIS, precache — but skips everything pipeline-
        related since playbin already handed the new uri off."""
        nxt = self._pending_gapless_index
        self._pending_gapless_index = None
        if nxt is None or nxt < 0 or nxt >= len(self.queue):
            return False

        self.current_queue_index = nxt
        track = self.queue[nxt]
        # Use the same normalization path as _play_current_index — without
        # this, freshly-queued tracks (where the dict only has `artists`
        # and `thumbnails` lists, not yet the normalized `artist`/`thumb`
        # strings) ship empty values to the UI and the user sees the
        # artist disappear and covers stop loading on every gapless
        # transition.
        video_id, title, artist, thumb, like_status = (
            self._normalize_track_metadata(track)
        )

        self.current_video_id = video_id
        self._current_source_video_id = video_id
        self.duration = -1
        # Spectrum stream-times restart at 0 for the new uri, so the queue
        # would otherwise be polluted by the previous track's tail.
        self._viz_queue.clear()
        # New track → fresh history-record gate.
        self._history_recorded_for = None
        if (
            self._history_mode == "immediate"
            and video_id
            and self._history_recorded_for != video_id
        ):
            self._history_recorded_for = video_id
            try:
                self.client.add_history_item_async(video_id)
            except Exception as e:
                print(f"[HISTORY] gapless immediate record failed: {e}")

        # Drop the previous track's tmpfs buffer (only used by upload
        # tracks, which we skipped — but cheap to clear anyway).
        if self._current_tmpfs_path:
            self._cleanup_tmpfs_path(self._current_tmpfs_path)
            self._current_tmpfs_path = None

        self.load_generation += 1
        current_gen = self.load_generation

        self.emit(
            "metadata-changed",
            title, artist, thumb, video_id, like_status,
        )
        if thumb:
            self._sync_mpris_art(thumb, video_id)
        self._update_logical_state()
        if hasattr(self, "mpris_events"):
            try:
                self.mpris_events.on_player_all()
            except Exception as e:
                print(f"mpris ERROR: {e}")
        self.emit("state-changed", "queue-updated")

        # Top up the cache for whatever comes after this newly-current track.
        threading.Thread(
            target=self._precache_next,
            args=(current_gen,),
            kwargs={"max_count": 1},
            daemon=True,
        ).start()
        return False

    def _maybe_extend_infinite(self):
        """Trigger background fetch of more radio tracks when the queue is
        running low. Called from manual skip, next(), and seek paths.

        We fire earlier than the old "halfway" trigger — extending at
        halfway leaves no headroom if the fetch is slow OR if dedup kills
        the response. Firing when fewer than 15 tracks remain (or we're
        past halfway, whichever comes first) gives us multiple shots."""
        if not (self.queue_is_infinite and self.queue_source_id and self.client):
            return
        if self._is_fetching_infinite:
            return
        if self.current_queue_index < 0 or not self.queue:
            return

        remaining = len(self.queue) - 1 - self.current_queue_index
        past_halfway = self.current_queue_index >= len(self.queue) // 2
        if remaining <= 15 or past_halfway:
            self._start_infinite_fetch()

    def _force_radio_extend(self):
        """Last-ditch radio extension when the queue is empty and the
        normal infinite fetch hasn't bought us new tracks. Seeds the
        watch_playlist from the currently/last-played track instead of the
        queue's tail (which has typically already produced a deduped
        response for radios with a fixed first batch). If even this
        comes back with nothing new, we accept the response as-is rather
        than silently stopping playback."""
        if self._is_fetching_infinite:
            return
        self._is_fetching_infinite = True

        seed_vid = self.current_video_id
        if not seed_vid and self.queue:
            seed_vid = self.queue[-1].get("videoId")
        if not seed_vid:
            self._is_fetching_infinite = False
            self.stop()
            self.current_queue_index = -1
            return

        def fetch():
            try:
                data = self.client.get_watch_playlist(
                    video_id=seed_vid, limit=50, radio=True
                )
                tracks = data.get("tracks", []) or []
                self._normalize_watch_playlist_tracks(tracks)
                existing = {t.get("videoId") for t in self.queue if t.get("videoId")}
                new_tracks = [
                    t for t in tracks
                    if t.get("videoId") and t.get("videoId") not in existing
                ]
                # If every result was a dupe, fall back to appending the
                # response anyway (skipping just the seed itself). Replaying
                # a few songs is a better outcome than silent stop.
                if not new_tracks and tracks:
                    new_tracks = [
                        t for t in tracks
                        if t.get("videoId") and t.get("videoId") != seed_vid
                    ]

                def _apply():
                    self._is_fetching_infinite = False
                    if not new_tracks:
                        self.stop()
                        self.current_queue_index = -1
                        self.emit("state-changed", "queue-updated")
                        return False
                    start_idx = len(self.queue)
                    self.extend_queue(new_tracks)
                    self.current_queue_index = start_idx
                    self._play_current_index()
                    self.emit("state-changed", "queue-updated")
                    return False

                GObject.idle_add(_apply)
            except Exception as e:
                print(f"[RADIO-EXTEND-FORCED] failed: {e}")

                def _give_up():
                    self._is_fetching_infinite = False
                    self.stop()
                    self.current_queue_index = -1
                    self.emit("state-changed", "queue-updated")
                    return False

                GObject.idle_add(_give_up)

        threading.Thread(target=fetch, daemon=True).start()

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

    def _normalize_track_metadata(self, track):
        """Resolve a queue entry's ytmusicapi-shaped fields into the
        normalized strings the UI / MPRIS / Discord all consume. Persists
        the normalized values back onto the queue dict so callers that
        re-read it (like-button fallback, history page, the gapless
        finisher) see the same strings the player already emitted.

        Returns ``(video_id, title, artist, thumb, like_status)``.
        """
        video_id = str(track.get("videoId") or "")
        title = str(track.get("title") or "Unknown")
        artist = track.get("artist", "")
        thumb = track.get("thumb")
        like_status = str(track.get("likeStatus") or "INDIFFERENT")

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

        track["artist"] = artist
        track["title"] = title
        track["thumb"] = thumb
        return video_id, title, artist, thumb, like_status

    def _play_current_index(self):
        if 0 <= self.current_queue_index < len(self.queue):
            track = self.queue[self.current_queue_index]
            video_id, title, artist, thumb, like_status = (
                self._normalize_track_metadata(track)
            )
            self._load_internal(video_id, title, artist, thumb, like_status)

    def _load_internal(
        self, video_id, title, artist, thumbnail_url, like_status="INDIFFERENT"
    ):
        self.current_video_id = video_id
        # Remember the videoId we were asked to play *before* any
        # OMV→ATV swap kicks in. Pages that show the original videoId
        # (album/playlist track rows) compare against this so the
        # currently-playing highlight still matches their row even
        # after the player swaps to the audio version.
        self._current_source_video_id = video_id

        self._is_loading = True
        # The new stream restarts running-time, so anything still queued
        # from the previous track is now nonsense — drop it before the
        # next visualizer tick pulls.
        self._viz_queue.clear()
        # Any in-flight gapless plan is now superseded by this manual
        # load — the user (or our own next() / repeat path) is taking
        # control of the pipeline. Don't let a late stream-start apply
        # the obsolete index.
        self._pending_gapless_index = None
        # Each track change frees the previous stream's decode buffers; nudge
        # glibc to return them to the OS so RSS doesn't ratchet up over a long
        # session. Throttled so rapid skipping doesn't thrash the heap walk.
        _malloc_trim(min_interval=10.0)
        # set_state(NULL) blocks until the pipeline flushes buffers and
        # closes any open HTTP sockets — measured at ~800ms occasionally
        # when the previous track was streaming. Running it on the main
        # thread froze the UI on every skip. GStreamer state changes are
        # documented thread-safe, so push it to a worker; _start_playback
        # serializes against it via the same pipeline's internal lock and
        # _is_loading already gates anyone reading pipeline state.
        try:
            threading.Thread(
                target=self.player.set_state,
                args=(Gst.State.NULL,),
                daemon=True,
            ).start()
        except Exception as e:
            print(f"set_state ERROR: {e}")

        # Drop the previous track's tmpfs buffer (if any) — by this point
        # the pipeline has released the file. We do this before setting the
        # new video_id so a fast track-change can't leave the buffer behind.
        if self._current_tmpfs_path:
            self._cleanup_tmpfs_path(self._current_tmpfs_path)
            self._current_tmpfs_path = None

        self.current_video_id = video_id
        self.duration = -1
        self.emit("progression", 0.0, 0.0)
        # New track → fresh history-record gate.
        if self._history_recorded_for != video_id:
            self._history_recorded_for = None
        # Immediate mode: record the play now, same as YT Music itself.
        if (
            self._history_mode == "immediate"
            and video_id
            and self._history_recorded_for != video_id
        ):
            self._history_recorded_for = video_id
            print(f"[HISTORY] immediate record for {video_id}")
            try:
                self.client.add_history_item_async(video_id)
            except Exception as e:
                print(f"[HISTORY] immediate record failed: {e}")

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
        # Per-track retry counter. If a URL 503s mid-play we re-resolve
        # via yt-dlp (up to this many times) — googlevideo CDNs rotate
        # and a fresh extraction usually picks a healthier host.
        self._stream_retry_count = 0
        self._stream_retry_max = 2

        # Upload tracks (entityId set by ytmusicapi's library-upload surfaces)
        # always go through the tmpfs path in _fetch_and_play. Using the
        # cached stream URL here would start streaming first, then the tmpfs
        # download would restart playback when it finished — visible to the
        # user as "the song jumped back to 0 when I tried to seek".
        cur_track = (
            self.queue[self.current_queue_index]
            if 0 <= self.current_queue_index < len(self.queue)
            else {}
        )
        is_upload = bool(cur_track.get("entityId"))
        # If the queued track is a music video (OMV/UGC), `_fetch_and_play`
        # will swap to the audio (ATV) version before yt-dlp resolves —
        # skip the early cache check here because it would key on the
        # OMV videoId and start playing the video stream before the swap
        # can happen.
        vtype_upper = (cur_track.get("videoType") or "").upper()
        will_swap = (
            vtype_upper.startswith("MUSIC_VIDEO_TYPE_")
            and vtype_upper != "MUSIC_VIDEO_TYPE_ATV"
        )

        if not is_upload and not will_swap:
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

        # Eagerly precache *only* the immediate next track — that's the
        # one the user is most likely to skip to. yt-dlp does heavy
        # Python-side work (JSON parsing, JS interpretation) under the
        # GIL; precaching 6 neighbours up front demonstrably starves the
        # GTK main loop while the user opens a playlist or scrolls. The
        # remaining neighbours are still pre-cached, but only after the
        # current track's _fetch_and_play completes (see the trailing
        # _precache_next call inside that method).
        threading.Thread(
            target=self._precache_next,
            args=(current_gen,),
            kwargs={"max_count": 1},
            daemon=True,
        ).start()

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

        def _same_cover(a, b):
            # Treat ytimg thumbnails of the same video as the same cover even
            # when they differ in quality (maxresdefault vs sddefault, ...).
            # Otherwise the UI (which upgrades to maxresdefault) and the MPRIS
            # art job (which downgrades to whatever actually fetched) keep
            # overwriting each other's URL, and each flip re-emits
            # metadata-changed — an infinite loop that also re-triggers a
            # lyrics fetch every iteration (LRCLIB 429 storm).
            if a == b:
                return True
            import re

            ma = re.search(r"i\.ytimg\.com/vi/([^/]+)/", a or "")
            mb = re.search(r"i\.ytimg\.com/vi/([^/]+)/", b or "")
            return bool(ma and mb and ma.group(1) == mb.group(1))

        changed = False
        # Update in current queue
        for track in self.queue:
            if track.get("videoId") == video_id:
                if not _same_cover(track.get("thumb"), working_url):
                    track["thumb"] = working_url
                    changed = True

        # Update in original queue
        for track in self.original_queue:
            if track.get("videoId") == video_id:
                if not _same_cover(track.get("thumb"), working_url):
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
        playlist_id = self.queue_source_id
        # Sources stamped by play_then_radio aren't real playlist IDs (they
        # use a "home-radio:…" prefix as a queue-identity stamp). Don't pass
        # them as playlist_id — that confuses watch_playlist. The video seed
        # is enough to get a fresh radio.
        if playlist_id and ":" in playlist_id:
            playlist_id = None

        def fetch_job():
            try:
                data = self.client.get_watch_playlist(
                    video_id=last_video_id,
                    playlist_id=playlist_id,
                    limit=limit,
                    radio=True,
                )
                tracks = data.get("tracks", []) or []
                self._normalize_watch_playlist_tracks(tracks)
                existing_ids = {
                    t.get("videoId") for t in self.queue if t.get("videoId")
                }
                new_tracks = [
                    t for t in tracks
                    if t.get("videoId") and t.get("videoId") not in existing_ids
                ]

                # Dedup-rescue: radios with a deterministic first batch will
                # return the same tracks for the same seed, so the first
                # `existing_ids` filter wipes the entire response. Retry
                # once with the currently-playing track as the seed (which
                # is typically a different position in the radio than the
                # queue's tail).
                if (
                    not new_tracks
                    and self.current_video_id
                    and self.current_video_id != last_video_id
                ):
                    retry = self.client.get_watch_playlist(
                        video_id=self.current_video_id,
                        limit=limit,
                        radio=True,
                    )
                    retry_tracks = retry.get("tracks", []) or []
                    self._normalize_watch_playlist_tracks(retry_tracks)
                    new_tracks = [
                        t for t in retry_tracks
                        if t.get("videoId") and t.get("videoId") not in existing_ids
                    ]

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

    def _tmpfs_root(self):
        """Pick a RAM-backed directory for upload buffers. /dev/shm is a
        tmpfs on every modern Linux distro; /tmp is sometimes tmpfs and
        sometimes not. Falls back to the platform tempdir if neither
        works."""
        import tempfile
        for candidate in ("/dev/shm", "/run/user/{}".format(os.getuid())):
            if os.path.isdir(candidate) and os.access(candidate, os.W_OK):
                return candidate
        return tempfile.gettempdir()

    def _download_upload_to_tmpfs(self, video_id, generation):
        """Pull an upload track into a tmpfs file via yt-dlp. Returns the
        local file path on success, None otherwise. Blocks the calling
        worker thread until the download is complete — typical upload is
        a few MB so this finishes in <2s on a normal connection. The
        caller is responsible for tracking the path so we can delete it
        on track change.

        Bails early if the user skipped to a different track mid-download
        (load_generation changes invalidate the result)."""
        import tempfile

        tmp_dir = tempfile.mkdtemp(prefix="muse-stream-", dir=self._tmpfs_root())
        outtmpl = os.path.join(tmp_dir, f"{video_id}.%(ext)s")

        opts = self.ydl_opts.copy()
        opts["outtmpl"] = outtmpl
        opts["quiet"] = True
        opts["noprogress"] = True
        # Don't write thumbnails / json / etc into the tmpfs dir.
        opts["writethumbnail"] = False
        opts["writeinfojson"] = False

        cookie_file = None
        try:
            if self.client.is_authenticated() and self.client.api:
                cookie_file = self._create_cookie_file(self.client.api.headers)
                if cookie_file:
                    opts["cookiefile"] = cookie_file
                ua = self.client.api.headers.get("User-Agent")
                if ua:
                    opts["user_agent"] = ua
                    opts["http_headers"] = {"User-Agent": ua}

            url = f"https://music.youtube.com/watch?v={video_id}"
            with YoutubeDL(opts) as ydl:
                ydl.download([url])

            if generation != self.load_generation:
                # User skipped during the download — discard the half-baked
                # file rather than handing it back for playback.
                self._rm_tmpfs_dir(tmp_dir)
                return None

            for name in os.listdir(tmp_dir):
                if name.startswith(video_id + "."):
                    return os.path.join(tmp_dir, name)
            self._rm_tmpfs_dir(tmp_dir)
            return None
        except Exception as e:
            print(f"[PLAYER] tmpfs download error for {video_id}: {e}")
            self._rm_tmpfs_dir(tmp_dir)
            return None
        finally:
            if cookie_file and os.path.exists(cookie_file):
                try:
                    os.remove(cookie_file)
                except OSError:
                    pass

    @staticmethod
    def _rm_tmpfs_dir(path):
        if not path:
            return
        try:
            import shutil
            shutil.rmtree(path, ignore_errors=True)
        except Exception:
            pass

    def _cleanup_tmpfs_path(self, path):
        """Remove a single tmpfs file + its parent dir (we use a fresh
        mkdtemp per track so the dir contains only the one file)."""
        if not path:
            return
        try:
            parent = os.path.dirname(path)
            if parent and "muse-stream-" in parent:
                self._rm_tmpfs_dir(parent)
            elif os.path.exists(path):
                os.remove(path)
        except OSError as e:
            print(f"[PLAYER] tmpfs cleanup failed for {path}: {e}")

    def _cleanup_all_tmpfs(self):
        """atexit hook — sweep our prefix dirs in the tmpfs root in case a
        prior crash left orphans, plus our currently tracked path."""
        if self._current_tmpfs_path:
            self._cleanup_tmpfs_path(self._current_tmpfs_path)
            self._current_tmpfs_path = None
        try:
            root = self._tmpfs_root()
            for name in os.listdir(root):
                if name.startswith("muse-stream-"):
                    self._rm_tmpfs_dir(os.path.join(root, name))
        except OSError:
            pass

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

        # If the queued track is YT Music's music-video version of a song,
        # swap to the audio (ATV) version BEFORE yt-dlp resolves so we
        # stream the cleaner album audio and get clean metadata. We do
        # this in the worker thread (not in _load_internal) because the
        # lookup is a network round-trip and shouldn't block the UI.
        # find_audio_version() short-circuits cheaply when the source is
        # already ATV (returns None), so we can call it unconditionally
        # — needed because YT Music's album/single endpoint doesn't
        # always populate `videoType`, and gating on it here meant the
        # swap never fired for those cases. Caching the result on the
        # track ensures we only pay the API cost on first play.
        track = (
            self.queue[self.current_queue_index]
            if 0 <= self.current_queue_index < len(self.queue)
            else {}
        )
        already_checked = track.get("_swap_checked")
        if (
            not already_checked
            and track.get("videoId") == video_id
            and not track.get("entityId")  # uploads have no counterpart
        ):
            try:
                swap_info = self.client.find_audio_version(video_id)
            except Exception as e:
                print(f"[swap-version] lookup failed: {e}")
                swap_info = None
            if generation != self.load_generation:
                return
            swapped = (
                swap_info.get("videoId") if isinstance(swap_info, dict) else None
            )
            # Only memoize success — a transient API failure shouldn't
            # permanently pin this track to the music-video version.
            if swapped:
                track["_swap_checked"] = True
            if swapped and swapped != video_id:
                print(
                    f"[swap-version] {video_id} → {swapped} ({track.get('title')})"
                )
                track["videoId"] = swapped
                track["videoType"] = "MUSIC_VIDEO_TYPE_ATV"
                # Pull the album-cover thumbnail from the swap result so
                # the player bar / queue / MPRIS art stop showing the
                # music-video still. Upgrade ytimg URLs to the same
                # high-res form the rest of the player uses.
                new_thumb = swap_info.get("thumb") or ""
                if new_thumb and "ytimg.com" in new_thumb:
                    new_thumb = get_high_res_url(new_thumb) or new_thumb
                if new_thumb:
                    track["thumb"] = new_thumb
                    thumb_hint = new_thumb
                new_title = swap_info.get("title")
                if new_title:
                    track["title"] = new_title
                    title_hint = new_title
                new_artists = swap_info.get("artists")
                if isinstance(new_artists, list) and new_artists:
                    first = new_artists[0]
                    new_artist_name = (
                        first.get("name", "") if isinstance(first, dict) else str(first)
                    )
                    if new_artist_name:
                        track["artist"] = new_artist_name
                        artist_hint = new_artist_name
                video_id = swapped
                self.current_video_id = swapped
                GObject.idle_add(
                    self.emit,
                    "metadata-changed",
                    str(title_hint),
                    str(artist_hint),
                    str(thumb_hint or ""),
                    str(swapped),
                    str(like_status_hint),
                )

        # Upload-locker tracks: YT's range-request handling on those URLs
        # silently breaks seeking even with cookies attached. Sidestep the
        # whole streaming pipeline by downloading the file into tmpfs
        # (/dev/shm — RAM-backed on Linux) and playing from there. The
        # local file is deleted as soon as the user moves to a new track,
        # so nothing accumulates on disk.
        if track.get("entityId"):
            tmpfs_path = self._download_upload_to_tmpfs(video_id, generation)
            if generation != self.load_generation:
                # User skipped while we were downloading — _download cleans
                # up its own tmp dir on cancel via the generation check.
                return
            if tmpfs_path:
                self._current_tmpfs_path = tmpfs_path
                file_uri = GLib.filename_to_uri(os.path.abspath(tmpfs_path), None)
                self._used_cached_url = False
                final_title = title_hint or track.get("title") or "Unknown"
                final_artist = artist_hint or track.get("artist") or "Unknown"
                final_thumb = thumb_hint or track.get("thumb") or ""
                GObject.idle_add(
                    self.emit,
                    "metadata-changed",
                    final_title,
                    final_artist,
                    final_thumb,
                    video_id,
                    like_status_hint,
                )
                GLib.idle_add(self._start_playback, file_uri)
                return
            # tmpfs download failed — fall through to the normal streaming
            # path. Seek won't work, but at least playback won't be blocked.
            print(f"[PLAYER] tmpfs download failed for upload {video_id}, falling back to streaming")

        url = f"https://music.youtube.com/watch?v={video_id}"

        # Use a local copy of options to prevent race conditions
        opts = self.ydl_opts.copy()

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
            # yt-dlp throws DownloadError / ExtractorError when a video is
            # unavailable, region-locked, removed, etc. The previous code
            # silently swallowed this — the player just sat in "loading"
            # forever. Skip to the next track and surface the error to the
            # UI via the new track-error signal.
            print(f"Error fetching URL for {video_id}: {e}")
            if generation == self.load_generation:
                msg = self._summarize_yt_dlp_error(e)
                failed_title = title_hint or "Track"
                GObject.idle_add(
                    self.emit, "track-error", video_id, failed_title, msg
                )
                # Auto-advance if there's something to advance to. If we're
                # already at the end of the queue, just stop instead of
                # looping back.
                if self.current_queue_index + 1 < len(self.queue):
                    GObject.idle_add(self.next)
                else:
                    GObject.idle_add(self.stop)
        finally:
            if cookie_file and os.path.exists(cookie_file):
                try:
                    os.remove(cookie_file)
                except:
                    pass

    def _summarize_yt_dlp_error(self, exc):
        """Pull a short, user-readable reason out of a yt-dlp exception.
        yt-dlp's str(exc) is usually a long path + verbose traceback line;
        we only want the bit a human would put in a toast."""
        msg = str(exc) if exc else ""
        # Prefer the first 'ERROR: ...' chunk if yt-dlp emitted one.
        if "ERROR:" in msg:
            msg = msg.split("ERROR:", 1)[1].strip()
        # Strip the ': <verbose>' tail past the first sentence so the toast
        # doesn't wrap forever.
        for sep in (". ", "; "):
            if sep in msg:
                msg = msg.split(sep, 1)[0]
                break
        msg = msg.strip(" .")
        if not msg:
            msg = "Could not load this track"
        return msg[:140]

    def _precache_next(self, generation, max_count=None):
        """Pre-cache stream URLs for songs ahead and behind in the queue.

        ``max_count`` caps how many neighbours we resolve in this call.
        We use ``max_count=1`` when this fires *eagerly* (at the start of
        the current track's load) so the user sees no Next-press delay,
        but we don't pile yt-dlp work onto the GIL while the playlist
        page is being assembled. The full 6-neighbour sweep still fires
        from the end of ``_fetch_and_play``, by which time the bind
        storm has settled.
        """
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

        if max_count is not None:
            indices = indices[:max_count]

        from yt_dlp import YoutubeDL
        import time as _time

        for i, idx in enumerate(indices):
            if generation != self.load_generation:
                return
            # Throttle between extractions so the GIL stays free for the
            # UI thread between batches. yt-dlp holds the GIL during JSON
            # parse + JS interpretation; a brief release lets the main
            # loop draw a frame.
            if i > 0:
                _time.sleep(0.25)
            if generation != self.load_generation:
                return
            track = self.queue[idx]
            vid = track.get("videoId")
            if not vid or self.stream_cache.get(vid):
                continue
            # Skip upload tracks — their stream URL would be unusable anyway
            # (we always play them from a tmpfs buffer). Avoids wasted yt-dlp
            # extractions in the background.
            if track.get("entityId"):
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

    def _on_source_setup(self, playbin, source):
        """Configure the HTTP source element playbin just created. We push
        the signed-in client's Cookie + User-Agent onto every request so
        YT's upload-locker URL honors byte-range requests (i.e. seeking)
        the same way the web player does."""
        if not source:
            return
        # Only relevant for HTTP-based sources (souphttpsrc, curlhttpsrc).
        # file:// playback gets a filesrc which doesn't have these props.
        try:
            name = source.get_factory().get_name()
        except Exception:
            name = ""
        if name not in ("souphttpsrc", "curlhttpsrc"):
            return

        try:
            if self.client and self.client.is_authenticated() and self.client.api:
                headers = self.client.api.headers or {}
                ua = headers.get("User-Agent")
                cookie = headers.get("Cookie")
                if ua:
                    try:
                        source.set_property("user-agent", ua)
                    except Exception:
                        pass

                # extra-headers is a Gst.Structure of arbitrary HTTP
                # headers — this is what gets sent on EVERY request the
                # source makes (initial GET + each Range follow-up).
                if cookie:
                    extra = Gst.Structure.new_empty("extra-headers")
                    extra.set_value("Cookie", cookie)
                    auth = headers.get("Authorization")
                    if auth:
                        extra.set_value("Authorization", auth)
                    try:
                        source.set_property("extra-headers", extra)
                    except Exception as e:
                        print(f"[PLAYER] set extra-headers failed: {e}")
        except Exception as e:
            print(f"[PLAYER] source-setup hook error: {e}")

    def _start_playback(self, uri, cookie_file=None):
        # NULL→URI→PLAYING must happen in order, but set_state(NULL) can
        # block for hundreds of ms while GStreamer flushes the previous
        # stream. Off-load the whole sequence to a worker so the UI
        # thread doesn't pay for it. set_state is thread-safe and serializes
        # against the (now also threaded) NULL transition kicked off in
        # _load_internal via the pipeline's internal state-change lock.
        def _drive():
            try:
                self.player.set_state(Gst.State.NULL)
                self.player.set_property("uri", uri)
                self.player.set_state(Gst.State.PLAYING)
            except Exception as e:
                print(f"[PLAYBACK] start failed: {e}")
        threading.Thread(target=_drive, daemon=True).start()

        self._load_media_api()
        if hasattr(self, "mpris_server"):
            self.mpris_server.publish()

        return False

    def play(self):
        self.player.set_state(Gst.State.PLAYING)
        self._update_logical_state()

    def pause(self):
        self.player.set_state(Gst.State.PAUSED)
        self._update_logical_state()

    def stop(self):
        if hasattr(self, "mpris_server"):
            self.mpris_server.unpublish()
        
        self.player.set_state(Gst.State.NULL)
        self._is_loading = False
        self._pending_gapless_index = None
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

    def _dispatch_spectrum_message(self, structure):
        """Queue the per-band magnitudes keyed by the audio's stream-time
        (position within the current track). Visualizer widgets pull the
        latest entry whose stream-time the audio sink has actually
        reached — this gives free pipeline-clock sync without needing the
        user to calibrate sink latency.

        Stream-time, not running-time: after a seek, running-time keeps
        advancing monotonically but `query_position(Gst.Format.TIME)`
        returns the new (post-seek) stream-time. Mixing the two would
        leave the bars stuck on stale data until the running-time delta
        caught up. Both sides have to agree on the same clock.

        GStreamer's `spectrum` reports `magnitude` as a GstValueList in
        modern (≥1.20) builds and as a GValueArray on older systems.
        `_extract_spectrum_bands` walks all three shapes.
        """
        if not getattr(self, "_visualizer_first_msg_logged", False):
            self._visualizer_first_msg_logged = True
            print("[VISUALIZER] first spectrum message received — data flowing")

        bands = _extract_spectrum_bands(structure)
        if not bands:
            return

        try:
            st_ok, stream_time_ns = structure.get_clock_time("stream-time")
        except Exception:
            st_ok = False
            stream_time_ns = 0
        if not st_ok:
            # Older spectrum builds don't tag stream-time. Mark the entry
            # with -1 so pull_visualizer_bands knows to return it ASAP
            # (no sync available).
            stream_time_ns = -1

        self._viz_queue.append((int(stream_time_ns), bands))
        # Cap the buffer to ~3s at 30Hz. Anything older than that is
        # either past the play-head (will be trimmed on next pull) or
        # so far ahead the user has already navigated past it.
        while len(self._viz_queue) > 90:
            self._viz_queue.popleft()

    def pull_visualizer_bands(self):
        """Return the spectrum bands the audio sink is currently playing,
        or None. Driven by visualizer widgets on their UI tick — non-
        destructive so multiple widgets (main + settings preview) can
        share the same queue.

        Returns None while paused / stopped so the widget's gravity loop
        lets the bars fall to zero. If we returned the latest queued
        entry instead, _ingest_magnitudes would re-snap the levels up to
        it every tick and the bars would visibly freeze.

        Strategy: walk the queue, return the most recent entry whose
        stream_time ≤ current sink position, and drop entries that have
        fallen >1s behind so the deque stays small.
        """
        if not self._viz_queue:
            return None
        try:
            state = self.player.get_state(0)[1]
        except Exception:
            state = None
        if state != Gst.State.PLAYING:
            return None
        try:
            pos_ok, pos_ns = self.player.query_position(Gst.Format.TIME)
        except Exception:
            pos_ok, pos_ns = False, 0
        if not pos_ok or pos_ns < 0:
            # No clock to sync against (pre-roll, between tracks). Hand
            # back the latest available; it'll be approximately right and
            # the next tick will correct.
            return self._viz_queue[-1][1]

        latest = None
        for rt, bands in self._viz_queue:
            if rt < 0 or rt <= pos_ns:
                latest = bands
            else:
                break

        # Trim entries the play-head is more than a second past — old
        # spectrum frames the sink can never reach again after the
        # play-head moved on (e.g. after a seek forward or just normal
        # advance).
        stale_threshold = pos_ns - 1_000_000_000
        while self._viz_queue and 0 <= self._viz_queue[0][0] < stale_threshold:
            self._viz_queue.popleft()

        return latest

    def on_message(self, bus, message):
        t = message.type
        if t == Gst.MessageType.STREAM_START:
            # Fired when playbin starts a new stream — for gapless this is
            # the precise moment the pipeline switched to the uri we set
            # in _on_about_to_finish. Catch up our state on the main thread.
            if self._pending_gapless_index is not None:
                print(
                    f"[GAPLESS] stream-start for pending index="
                    f"{self._pending_gapless_index}",
                    flush=True,
                )
                GLib.idle_add(self._apply_gapless_transition)
                return
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
            if (
                self._track_started_at
                and _time.time() - self._track_started_at < 1.0
            ):
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
        elif t == Gst.MessageType.ELEMENT:
            # Spectrum analyzer posts magnitude data here on every interval.
            structure = message.get_structure()
            if structure is not None and structure.get_name() == "spectrum":
                self._dispatch_spectrum_message(structure)
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

            # Fresh yt-dlp URLs can still 503 because googlevideo rotates
            # hosts and the format we picked sometimes sits behind a
            # flaky one. Invalidate the cache entry and re-resolve —
            # yt-dlp usually lands on a different host the second time.
            # Capped at `_stream_retry_max` so a genuinely dead video
            # can't loop forever.
            vid = self.current_video_id
            if (
                vid
                and self._stream_retry_count < self._stream_retry_max
                and not self._is_loading
            ):
                self._stream_retry_count += 1
                print(
                    f"[PLAYER] stream error (attempt "
                    f"{self._stream_retry_count}/{self._stream_retry_max}), "
                    f"re-resolving {vid}"
                )
                try:
                    self.stream_cache.invalidate(vid)
                except Exception:
                    pass
                self.player.set_state(Gst.State.NULL)
                # Kick off a fresh yt-dlp resolution on a background
                # thread; when it lands, `_fetch_and_play` will call
                # _start_playback with the new URL.
                idx = self.current_queue_index
                if 0 <= idx < len(self.queue):
                    track = self.queue[idx]
                    self._is_loading = True
                    self.load_generation += 1
                    gen = self.load_generation
                    threading.Thread(
                        target=self._fetch_and_play,
                        args=(
                            vid,
                            track.get("title", ""),
                            track.get("artist", ""),
                            track.get("thumb"),
                            track.get("likeStatus", "INDIFFERENT"),
                            gen,
                        ),
                        daemon=True,
                    ).start()
                    return

            self.player.set_state(Gst.State.NULL)
            self._is_loading = False
            self._update_logical_state()
        elif t == Gst.MessageType.STATE_CHANGED:
            if message.src == self.player:
                old, new, pending = message.parse_state_changed()
                if new == Gst.State.PLAYING:
                    if self._user_volume == None:
                        self._user_volume = self.get_volume()

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

    def _publish_mpris_art_pixbuf(self, pixbuf, video_id):
        """Center-crop a pixbuf to a square and upscale small art before
        saving it as the MPRIS cover.

        Covers come in all shapes (video thumbnails are 16:9) and sizes, but
        MPRIS wants a square — and some clients render low-res art poorly — so
        we crop the centre square and upscale anything below MIN_ART_SIZE.
        Saves to the art cache and points mpris_art_url at it. Returns the
        saved path, or None if there was no pixbuf to save."""
        if not pixbuf:
            return None

        MIN_ART_SIZE = 512

        w = pixbuf.get_width()
        h = pixbuf.get_height()
        size = min(w, h)
        pixbuf = pixbuf.new_subpixbuf(
            (w - size) // 2, (h - size) // 2, size, size
        )

        if size < MIN_ART_SIZE:
            pixbuf = pixbuf.scale_simple(
                MIN_ART_SIZE, MIN_ART_SIZE, GdkPixbuf.InterpType.BILINEAR
            )

        cache_dir = os.path.join(GLib.get_user_cache_dir(), "mixtapes")
        os.makedirs(cache_dir, exist_ok=True)

        # Cleanup old art files to prevent bloat and cache issues
        for old_art in glob.glob(os.path.join(cache_dir, "mpris_art_*.jpg")):
            try:
                os.remove(old_art)
            except OSError:
                pass

        # Use unique filename per track to bypass MPRIS client caching
        safe_video_id = video_id.replace("-", "_").replace(".", "_")
        target_path = os.path.join(cache_dir, f"mpris_art_{safe_video_id}.jpg")

        pixbuf.savev(target_path, "jpeg", ["quality"], ["90"])
        self.mpris_art_url = f"file://{target_path}"
        return target_path

    def _sync_mpris_art(self, url, video_id):
        """Downloads, crops, and saves artwork locally for MPRIS with fallback support."""
        def job(current_url, fallbacks=None):
            # Try local cover first (works offline). Downloads embed the cover
            # in the audio file's tags (no sidecar cover.jpg), so reuse the
            # UI's extractor which pulls the embedded art into the cover cache
            # and returns a file:// URL. The cold path reads the audio file +
            # writes a JPEG, so it must stay off the main thread.
            if video_id and self.current_video_id == video_id:
                from ui.utils import resolve_local_cover

                local_cover = resolve_local_cover(video_id)
                if local_cover:
                    try:
                        path = local_cover[len("file://"):]
                        pixbuf = GdkPixbuf.Pixbuf.new_from_file(path)
                        if self._publish_mpris_art_pixbuf(pixbuf, video_id):
                            if hasattr(self, "mpris_events"):
                                GLib.idle_add(self.mpris_events.on_player_all)
                            return
                    except Exception as e:
                        # Fall through to the network path on any failure.
                        print(f"[PLAYER] MPRIS local art failed: {e}")

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

                # 2. Load, crop and upscale
                loader = GdkPixbuf.PixbufLoader()
                loader.write(data)
                loader.close()
                pixbuf = loader.get_pixbuf()

                if self._publish_mpris_art_pixbuf(pixbuf, video_id):
                    # 4. Notify MPRIS to refresh metadata with the NEW local URL
                    if hasattr(self, "mpris_events"):
                        GLib.idle_add(self.mpris_events.on_player_all)

                    # 5. Propagate the working URL back to the track so
                    # Discord RPC (which sends the URL string to Discord
                    # for it to fetch) and any other consumer of
                    # track["thumb"] stop showing the dead maxresdefault.
                    # update_track_thumbnail is a no-op when the URL
                    # already matches, so this is safe to call
                    # unconditionally.
                    if fetch_url and fetch_url != url:
                        GLib.idle_add(
                            self.update_track_thumbnail, video_id, fetch_url
                        )

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
            # 2. Update Duration if it changed (vital for MPRIS progress bar scale).
            # Only trust a POSITIVE duration from GStreamer — some upload-song
            # streams return success=True with dur_nanos=0 on the first ticks
            # (and sometimes throughout, when YT's locker endpoint doesn't
            # advertise a length). Treat those as unknown and fall through to
            # the metadata fallback.
            new_dur = None
            success_dur, dur_nanos = self.player.query_duration(Gst.Format.TIME)
            if success_dur and dur_nanos > 0:
                new_dur = dur_nanos / Gst.SECOND

            if new_dur is not None:
                if abs(new_dur - self.duration) > 0.1:
                    self.duration = new_dur
                    if hasattr(self, "mpris_events"):
                        self.mpris_events.on_title()  # Syncs 'mpris:length'
                    if getattr(self, "discord_rpc", None):
                        self.discord_rpc.update()
            elif self.duration <= 0:
                # GStreamer doesn't know the length yet — use the track's
                # metadata so the seek bar has a range to drag inside.
                # Uploaded songs from get_library_upload_songs() only
                # carry `duration` as "M:SS" string (no `duration_seconds`),
                # so check both.
                if 0 <= self.current_queue_index < len(self.queue):
                    track = self.queue[self.current_queue_index]
                    meta_dur = _parse_track_duration(track)
                    if meta_dur > 0:
                        self.duration = float(meta_dur)
                        if hasattr(self, "mpris_events"):
                            self.mpris_events.on_title()

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

                # 5. Record a listen after the threshold, but only in
                # `after_30s` mode. "immediate" is handled in
                # `_load_internal`; "never" skips recording entirely.
                vid = getattr(self, "current_video_id", None)
                if (
                    self._history_mode == "after_30s"
                    and vid
                    and self._history_recorded_for != vid
                    and state == Gst.State.PLAYING
                    and current_time >= self._history_record_after_sec
                ):
                    self._history_recorded_for = vid
                    print(
                        f"[HISTORY] {self._history_record_after_sec}s threshold "
                        f"hit for {vid} — recording play"
                    )
                    try:
                        self.client.add_history_item_async(vid)
                    except Exception as e:
                        print(f"[HISTORY] failed to record {vid}: {e}")

        return True

    def _load_history_mode(self):
        """Read the user's history-recording preference. Defaults to
        "immediate" so we match YT Music's own behavior out of the box."""
        import json
        import os
        try:
            path = os.path.join(
                GLib.get_user_data_dir(), "muse", "prefs.json"
            )
            if os.path.exists(path):
                with open(path) as f:
                    return json.load(f).get("history_mode", "immediate")
        except Exception:
            pass
        return "immediate"


    def set_history_mode(self, mode):
        """Update the history-recording mode at runtime so the
        preferences switch takes effect on the next track without
        needing a restart."""
        if mode not in ("immediate", "after_30s", "never"):
            return
        self._history_mode = mode

    def seek(self, position, flush=True):
        """Seek to position in seconds. Returns True on success, False on
        failure (e.g. the stream doesn't support range requests — common
        for YT Music upload-locker URLs)."""
        if self.player.get_state(0)[1] == Gst.State.NULL:
            return False

        import time

        self.last_seek_time = time.time()
        # Drop any spectrum entries queued before this seek — their
        # stream-times are now in the past (forward seek) or the future
        # (backward seek), either way they mislead pull_visualizer_bands.
        self._viz_queue.clear()

        # Check whether the pipeline reports as seekable BEFORE attempting.
        # This is cheap, and lets us avoid trying a seek that we know will
        # silently fail — letting on_scale_change_value know not to bother.
        seekable = False
        try:
            q = Gst.Query.new_seeking(Gst.Format.TIME)
            if self.player.query(q):
                _, seekable, _, _ = q.parse_seeking()
        except Exception:
            seekable = True  # be permissive — try anyway

        flags = Gst.SeekFlags.ACCURATE
        if flush:
            flags |= Gst.SeekFlags.FLUSH

        ok = self.player.seek_simple(
            Gst.Format.TIME,
            flags,
            int(position * Gst.SECOND),
        )

        # ACCURATE seeks sometimes get rejected by sources that would accept
        # a keyframe-aligned seek. Fall back to KEY_UNIT so we still move.
        if not ok:
            ok = self.player.seek_simple(
                Gst.Format.TIME,
                (Gst.SeekFlags.KEY_UNIT | Gst.SeekFlags.FLUSH) if flush else Gst.SeekFlags.KEY_UNIT,
                int(position * Gst.SECOND),
            )

        if not ok:
            print(
                f"[PLAYER] seek to {position:.1f}s rejected by pipeline "
                f"(seekable={seekable}, vid={self.current_video_id})"
            )
            return False

        if hasattr(self, "mpris_events"):
            self.mpris_events.on_seek(int(position * 1_000_000))
        return True

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

        # wireplumber is an external call, which should set the volume of last session
        # ensure volume value from external volume change is always listened to if _user_volume is still not set
        # just storing the value from the call, it will be used whenever
        if self._user_volume == None:
            linear = float(element.get_property("volume"))
            self._user_volume = GstAudio.StreamVolume.convert_volume(
                GstAudio.StreamVolumeFormat.LINEAR,
                GstAudio.StreamVolumeFormat.CUBIC,
                linear,
            )

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

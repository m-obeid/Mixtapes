import json
import os
import queue
import socket
import struct
import sys
import threading
import time
import uuid


DISCORD_APP_ID = "1492500060087255231"

# Schedule of reconnect delays in seconds when the Discord IPC pipe isn't
# reachable. After exhausting the list we stay at the final value, so the
# tail is what dominates "Mixtapes opened before Discord" — keep it small
# so a user who launches Discord a few minutes later sees Rich Presence
# light up within ~30s rather than the previous 2-minute worst case. The
# poll is essentially a single failed open()/connect(); cost is negligible.
RECONNECT_BACKOFF = [3, 5, 10, 15, 30]

STATUS_DISPLAY_TYPES = {
    "app_name": 0,
    "artist": 1,
    "song_title": 2,
}
STATUS_DISPLAY_DEFAULT = "artist"


def _get_prefs():
    from gi.repository import GLib
    path = os.path.join(GLib.get_user_data_dir(), "muse", "prefs.json")
    try:
        if os.path.exists(path):
            with open(path) as f:
                return json.load(f)
    except Exception:
        pass
    return {}


def get_rpc_enabled():
    return _get_prefs().get("discord_rpc_enabled", True)


def get_status_display_type():
    return _get_prefs().get("discord_rpc_status_display", STATUS_DISPLAY_DEFAULT)


def get_small_icon_enabled():
    return _get_prefs().get("discord_rpc_small_icon_enabled", True)


def get_hide_pause_enabled():
    return _get_prefs().get("discord_rpc_hide_pause_enabled", False)

OP_HANDSHAKE = 0
OP_FRAME = 1
OP_CLOSE = 2
OP_PING = 3
OP_PONG = 4

IS_WINDOWS = sys.platform == "win32"


def _candidate_ipc_paths():
    """All paths Discord clients (incl. flatpak/snap/Windows variants) expose."""
    if IS_WINDOWS:
        # Windows uses named pipes.
        return [rf"\\.\pipe\discord-ipc-{i}" for i in range(10)]

    bases = []
    xdg = os.environ.get("XDG_RUNTIME_DIR")
    if xdg:
        bases.append(xdg)
        bases.append(os.path.join(xdg, "app", "com.discordapp.Discord"))
        bases.append(os.path.join(xdg, "app", "dev.vencord.Vesktop"))
        bases.append(os.path.join(xdg, "snap.discord"))
    tmpdir = os.environ.get("TMPDIR") or "/tmp"
    bases.append(tmpdir)
    bases.append("/tmp")

    seen = set()
    paths = []
    for base in bases:
        if not base or base in seen:
            continue
        seen.add(base)
        for i in range(10):
            paths.append(os.path.join(base, f"discord-ipc-{i}"))
    return paths


class DiscordRPCAdapter:
    """Discord Rich Presence adapter using the raw IPC protocol.

    All socket I/O is pinned to a single worker thread.
    """

    def __init__(self, player, app_id=DISCORD_APP_ID):
        self.player = player
        self.app_id = app_id
        # Underlying transport: socket on Linux/macOS, file object on Windows
        # (Windows named pipes are accessed as binary files).
        self.transport = None
        self.connected = False
        self._stopping = False
        self._queue = queue.Queue()
        self._connect_attempt = 0
        self._pid = os.getpid()
        self.status = "Disconnected"
        self._enabled = get_rpc_enabled()
        self._reconnect_lock = threading.Lock()
        self._reconnect_timer = None

        if self._enabled:
            self._worker = threading.Thread(target=self._run, daemon=True)
            self._worker.start()
            self._queue.put(("connect", None))
        else:
            self._worker = None
            self.status = "Disabled"

    # ── Public API ────────────────────────────────────────────────────────

    def update(self):
        if not self._stopping and self._enabled:
            self._queue.put(("update", None))

    def set_enabled(self, enabled):
        self._enabled = enabled
        if enabled and self._worker is None:
            self._stopping = False
            self._queue = queue.Queue()
            self.status = "Disconnected"
            self._worker = threading.Thread(target=self._run, daemon=True)
            self._worker.start()
            self._queue.put(("connect", None))
        elif not enabled and self._worker is not None:
            self.stop()
            self._worker = None
            self.status = "Disabled"

    def stop(self):
        self._stopping = True
        self._queue.put(("stop", None))

    # ── Worker ────────────────────────────────────────────────────────────

    def _run(self):
        MIN_UPDATE_INTERVAL = 0.4  # seconds; below Discord's ~5/20s limit
        last_update_at = 0.0

        while not self._stopping:
            try:
                op, _payload = self._queue.get(timeout=1.0)
            except queue.Empty:
                continue

            # Coalesce: drain any queued events and keep the latest "update".
            # Stops/connects always take priority.
            saw_stop = op == "stop"
            saw_connect = op == "connect"
            saw_update = op == "update"
            try:
                while True:
                    next_op, _ = self._queue.get_nowait()
                    if next_op == "stop":
                        saw_stop = True
                    elif next_op == "connect":
                        saw_connect = True
                    elif next_op == "update":
                        saw_update = True
            except queue.Empty:
                pass

            if saw_stop:
                self._do_stop()
                return
            if saw_connect and not self.connected:
                self._do_connect()
            if saw_update:
                if not self.connected:
                    self._do_connect()
                if not self.connected:
                    continue

                # Enforce min interval: if we sent too recently, sleep first.
                # During rapid skips this lets more events pile up so the
                # next iteration coalesces them and sends only the latest.
                elapsed = time.time() - last_update_at
                if elapsed < MIN_UPDATE_INTERVAL:
                    time.sleep(MIN_UPDATE_INTERVAL - elapsed)
                    # Re-drain anything that queued up during the sleep so
                    # we send the freshest possible state.
                    try:
                        while True:
                            next_op, _ = self._queue.get_nowait()
                            if next_op == "stop":
                                self._do_stop()
                                return
                            elif next_op == "connect" and not self.connected:
                                self._do_connect()
                            # extra "update" events collapse into the
                            # single _do_update() below
                    except queue.Empty:
                        pass

                self._do_update()
                last_update_at = time.time()

    # ── Connection ────────────────────────────────────────────────────────

    def _do_connect(self):
        if self.connected or self._stopping:
            return
        # Ignore if a reconnect timer is pending
        with self._reconnect_lock:
            if self._reconnect_timer:
                return
        for path in _candidate_ipc_paths():
            # On Windows the named pipe doesn't appear in os.path.exists for
            # all client variants, so we just try to open it and let the
            # exception path move on.
            if not IS_WINDOWS and not os.path.exists(path):
                continue
            try:
                if IS_WINDOWS:
                    # Open the named pipe as a raw binary file.
                    self.transport = open(path, "r+b", buffering=0)
                else:
                    s = socket.socket(socket.AF_UNIX, socket.SOCK_STREAM)
                    s.connect(path)
                    self.transport = s
                self._send(OP_HANDSHAKE, {"v": 1, "client_id": str(self.app_id)})
                op, data = self._recv()
                print(f"[DiscordRPC] handshake via {path}: op={op} data={data}")
                if op is None:
                    raise OSError("no handshake response")
                self.connected = True
                self.status = "Connected"
                self._connect_attempt = 0
                self._cancel_reconnect()
                self._do_update()
                return
            except Exception as e:
                print(f"[DiscordRPC] connect attempt {path} failed: {e}")
                self._teardown_transport()
                continue
        print("[DiscordRPC] no Discord IPC endpoint reachable")
        self.status = "Disconnected"
        self._schedule_reconnect()

    def _schedule_reconnect(self):
        if self._stopping:
            return
        with self._reconnect_lock:
            if self._reconnect_timer:
                return
            idx = min(self._connect_attempt, len(RECONNECT_BACKOFF) - 1)
            delay = RECONNECT_BACKOFF[idx]
            self._connect_attempt += 1

            def _later():
                with self._reconnect_lock:
                    self._reconnect_timer = None
                if not self._stopping:
                    self._queue.put(("connect", None))

            timer = threading.Timer(delay, _later)
            timer.daemon = True
            self._reconnect_timer = timer
            timer.start()

    def _cancel_reconnect(self):
        with self._reconnect_lock:
            timer = self._reconnect_timer
            self._reconnect_timer = None
        if timer:
            timer.cancel()

    # ── Frame I/O ─────────────────────────────────────────────────────────

    def _send(self, op, payload):
        body = json.dumps(payload).encode("utf-8")
        header = struct.pack("<II", op, len(body))
        frame = header + body
        if IS_WINDOWS:
            self.transport.write(frame)
            self.transport.flush()
        else:
            self.transport.sendall(frame)

    def _recv(self):
        header = self._recv_exact(8)
        if not header:
            return None, None
        op, length = struct.unpack("<II", header)
        body = self._recv_exact(length) if length else b""
        try:
            data = json.loads(body.decode("utf-8")) if body else None
        except Exception:
            data = None
        return op, data

    def _recv_exact(self, n):
        buf = b""
        while len(buf) < n:
            try:
                if IS_WINDOWS:
                    chunk = self.transport.read(n - len(buf))
                else:
                    chunk = self.transport.recv(n - len(buf))
            except Exception:
                return None
            if not chunk:
                return None
            buf += chunk
        return buf

    # ── Activity ──────────────────────────────────────────────────────────

    def _do_update(self):
        if not self.connected or not self.transport:
            return
        activity = self._build_activity()
        try:
            frame = {
                "cmd": "SET_ACTIVITY",
                "args": {"pid": self._pid, "activity": activity},
                "nonce": str(uuid.uuid4()),
            }
            print(f"[DiscordRPC] -> {frame}")
            self._send(OP_FRAME, frame)
            op, data = self._recv()
            print(f"[DiscordRPC] <- op={op} data={data}")
        except Exception as e:
            print(f"[DiscordRPC] update failed: {e}")
            self._teardown_transport()
            self._schedule_reconnect()

    def _do_stop(self):
        self._cancel_reconnect()
        try:
            if self.connected and self.transport:
                frame = {
                    "cmd": "SET_ACTIVITY",
                    "args": {"pid": self._pid, "activity": None},
                    "nonce": str(uuid.uuid4()),
                }
                self._send(OP_FRAME, frame)
        except Exception:
            pass
        self._teardown_transport()

    def _teardown_transport(self):
        if self.transport:
            try:
                self.transport.close()
            except Exception:
                pass
        self.transport = None
        self.connected = False
        if self._enabled:
            self.status = "Disconnected"

    # ── Payload ───────────────────────────────────────────────────────────

    def _build_activity(self):
        player = self.player
        state = player.get_state_string()
        if state == "stopped" or (get_hide_pause_enabled() and state == "paused"):
            return None

        idx = player.current_queue_index
        if idx < 0 or idx >= len(player.queue):
            return None
        track = player.queue[idx]

        title = str(track.get("title") or "Unknown")
        artist = track.get("artist", "")
        if isinstance(artist, list):
            artist = ", ".join([a.get("name", "") for a in artist if a])
        artist = str(artist or "Unknown artist")

        details = (title or "Unknown")[:128]
        if len(details) < 2:
            details = details + " "
        state_text = f"{artist}"[:128]
        if len(state_text) < 2:
            state_text = state_text + " "

        display_key = get_status_display_type()
        display_type = STATUS_DISPLAY_TYPES.get(display_key, 1)

        activity = {
            "details": details,
            "state": state_text,
            "type": 2,
            "status_display_type": display_type,
        }

        mixtapes_logo = (
            "https://raw.githubusercontent.com/m-obeid/Mixtapes/"
            "main/screenshots/omori-mixtape.png"
        )
        show_small_icon = get_small_icon_enabled()
        small_image = "pause" if state == "playing" else "play"
        small_text = "Playing" if state == "playing" else "Paused"

        album = track.get("album", "")
        if isinstance(album, dict):
            album = album.get("name", "")
        album = str(album or "")

        thumb = track.get("thumb") or ""
        if thumb.startswith("http"):
            activity["assets"] = {
                "large_image": thumb,
                "large_text": (album or title or "Mixtapes")[:128] or "Mixtapes",
            }
        else:
            activity["assets"] = {
                "large_image": mixtapes_logo,
                "large_text": "Mixtapes",
            }
        if show_small_icon:
            activity["assets"]["small_image"] = small_image
            activity["assets"]["small_text"] = small_text

        if state == "playing":
            duration = player.duration if player.duration and player.duration > 0 else 0
            pos = 0.0
            try:
                if player.player is not None:
                    from gi.repository import Gst

                    ok, p = player.player.query_position(Gst.Format.TIME)
                    if ok:
                        pos = p / Gst.SECOND
            except Exception:
                pos = 0.0
            now_ms = int(time.time() * 1000)
            start = now_ms - int(pos * 1000)
            activity["timestamps"] = {"start": start}
            if duration > 0:
                activity["timestamps"]["end"] = start + int(duration * 1000)

        return activity

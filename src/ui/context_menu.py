"""Shared context menu builder.

Every right-click menu in the app is built here, so the same kind of item
always offers the same actions in the same order. Pages pass their own
entries (remove from playlist, delete upload, selection helpers) as
`MenuAction` objects; everything else is derived from the item data.
"""

import threading

from gi.repository import Gdk, Gio, GLib, Gtk

from ui.utils import copy_to_clipboard, is_online, show_toast

# Section order. Every menu lays its entries out in this order, so an
# action never moves between pages.
SECTIONS = ("queue", "nav", "actions", "remove", "clipboard", "debug")

SONG_KINDS = ("song", "video", "episode", "upload")


class MenuAction:
    """A page specific entry merged into one of the standard sections.

    `first` puts the entry at the top of its section instead of the end.
    """

    __slots__ = ("label", "callback", "section", "first")

    def __init__(self, label, callback, section="actions", first=False):
        self.label = label
        self.callback = callback
        self.section = section
        self.first = first


# ── Item parsing ───────────────────────────────────────────────────────────


def detect_kind(item):
    """Best guess at what an ytmusicapi item is: song, album, playlist or
    artist. Pages that already know should pass the kind explicitly."""
    if not isinstance(item, dict):
        return "song"

    declared = str(item.get("resultType") or item.get("type") or "").lower()
    if declared in ("song", "video", "episode", "upload"):
        return "song"
    if declared in ("album", "single", "ep"):
        return "album"
    if declared == "playlist":
        return "playlist"
    if declared == "artist":
        return "artist"

    if item.get("videoId"):
        return "song"
    if item.get("audioPlaylistId"):
        return "album"

    browse_id = item.get("browseId") or ""
    if isinstance(browse_id, str):
        if browse_id.startswith(("MPRE", "OLAK")):
            return "album"
        if browse_id.startswith("VL"):
            return "playlist"
        if browse_id.startswith(("UC", "MPLA")):
            return "artist"
    if item.get("playlistId"):
        return "playlist"
    if item.get("channelId"):
        return "artist"
    return "song"


def video_id_of(item):
    if not isinstance(item, dict):
        return None
    vid = item.get("videoId")
    return vid if isinstance(vid, str) and vid else None


def artist_ref(item):
    """Return (channel_id, name) of the item's primary artist, or (None, None)."""
    if not isinstance(item, dict):
        return None, None

    artists = item.get("artists")
    if isinstance(artists, list):
        for entry in artists:
            if isinstance(entry, dict) and entry.get("id"):
                return entry["id"], entry.get("name", "")

    for key in ("artist", "author"):
        value = item.get(key)
        if isinstance(value, dict) and value.get("id"):
            return value["id"], value.get("name", "")
        if isinstance(value, list) and value and isinstance(value[0], dict):
            if value[0].get("id"):
                return value[0]["id"], value[0].get("name", "")
    return None, None


def album_ref(item):
    """Return (browse_id, title) of the item's album, or (None, None)."""
    if not isinstance(item, dict):
        return None, None
    album = item.get("album")
    if isinstance(album, dict) and album.get("id"):
        return album["id"], album.get("name") or album.get("title") or "Album"
    return None, None


def collection_id(item, kind=None):
    """Playlist id used to play or seed a radio from an album or playlist."""
    if not isinstance(item, dict):
        return None
    kind = kind or detect_kind(item)
    if kind == "album":
        pid = item.get("audioPlaylistId")
        if pid:
            return pid
    pid = item.get("playlistId") or item.get("browseId") or ""
    if isinstance(pid, str) and pid.startswith("VL"):
        pid = pid[2:]
    return pid or None


def build_link(item, kind=None):
    """Shareable music.youtube.com URL for an item, or None."""
    if not isinstance(item, dict):
        return None
    kind = kind or detect_kind(item)

    if kind in SONG_KINDS:
        vid = video_id_of(item)
        return f"https://music.youtube.com/watch?v={vid}" if vid else None

    browse_id = item.get("browseId") or ""
    if kind == "album":
        pid = item.get("audioPlaylistId")
        if pid:
            return f"https://music.youtube.com/playlist?list={pid}"
        if isinstance(browse_id, str) and browse_id.startswith("OLAK"):
            return f"https://music.youtube.com/playlist?list={browse_id}"
        if browse_id:
            return f"https://music.youtube.com/browse/{browse_id}"
        return None

    if kind == "playlist":
        pid = item.get("playlistId") or browse_id or ""
        if pid.startswith("VL"):
            pid = pid[2:]
        return f"https://music.youtube.com/playlist?list={pid}" if pid else None

    if kind == "artist":
        cid = item.get("channelId") or browse_id or item.get("id")
        return f"https://music.youtube.com/channel/{cid}" if cid else None
    return None


# ── Menu assembly ──────────────────────────────────────────────────────────


class _Builder:
    def __init__(self, anchor, prefix):
        self.anchor = anchor
        self.prefix = prefix
        self.group = Gio.SimpleActionGroup()
        self.sections = {name: Gio.Menu() for name in SECTIONS}
        self._used = set()

    def add(self, section, label, name, callback, first=False):
        if name in self._used:
            name = f"{name}-{len(self._used)}"
        self._used.add(name)
        action = Gio.SimpleAction.new(name, None)
        action.connect("activate", lambda a, p: callback())
        self.group.add_action(action)
        target = self.sections[section]
        detailed = f"{self.prefix}.{name}"
        if first:
            target.prepend(label, detailed)
        else:
            target.append(label, detailed)

    def add_extras(self, extras):
        for i, entry in enumerate(extras or []):
            if entry is None:
                continue
            section = entry.section if entry.section in self.sections else "actions"
            self.add(section, entry.label, f"extra-{i}", entry.callback, entry.first)

    def build(self):
        model = Gio.Menu()
        for name in SECTIONS:
            section = self.sections[name]
            if section.get_n_items() > 0:
                model.append_section(None, section)
        self.anchor.insert_action_group(self.prefix, self.group)
        return model


def _root(widget):
    return widget.get_root() if widget else None


def _open_artist(anchor, artist_id, name):
    root = _root(anchor)
    if root and hasattr(root, "open_artist"):
        root.open_artist(artist_id, name)


def _open_playlist(anchor, playlist_id, title=None):
    root = _root(anchor)
    if root and hasattr(root, "open_playlist"):
        root.open_playlist(playlist_id, {"title": title} if title else None)


def _copy_link(anchor, url):
    copy_to_clipboard(url)
    show_toast(anchor, "Link copied")


def _queue_tracks(anchor, player, tracks, next_=False):
    items = [dict(t) for t in tracks if t]
    if not items:
        return
    if len(items) == 1:
        player.add_to_queue(items[0], next=next_)
        show_toast(anchor, "Playing next" if next_ else "Added to queue")
    else:
        player.add_tracks_to_queue(items, next=next_)
        show_toast(
            anchor,
            f"Playing {len(items)} tracks next"
            if next_
            else f"Added {len(items)} tracks to queue",
        )


def _add_to_playlist(anchor, player, client, video_ids):
    from ui.widgets.add_to_playlist import AddToPlaylistPopover, mark_playlist_used

    def _on_select(target_pid):
        if not (target_pid and video_ids):
            return
        mark_playlist_used(target_pid)
        count = len(video_ids)

        def _run():
            # add_playlist_items handles the OMV to ATV swap itself.
            ok = client.add_playlist_items(target_pid, list(video_ids))
            plural = "s" if count > 1 else ""
            GLib.idle_add(
                show_toast,
                anchor,
                f"Added {count} track{plural} to playlist"
                if ok
                else "Failed to add to playlist",
            )

        threading.Thread(target=_run, daemon=True).start()

    pop = AddToPlaylistPopover(player, on_select=_on_select, parent=anchor)
    pop.connect("closed", lambda p: GLib.idle_add(p.unparent))
    pop.popup()


def _download(anchor, tracks, album_title=None, album_id=None):
    root = _root(anchor)
    if not root:
        return
    if len(tracks) == 1 and hasattr(root, "download_track"):
        root.download_track(tracks[0], album_title, album_id)
    elif hasattr(root, "download_tracks"):
        root.download_tracks(tracks, album_title, album_id)


def _remove_download(anchor, player, video_id, on_change):
    manager = getattr(player, "download_manager", None)
    if not manager:
        return
    manager.delete_download(video_id)
    show_toast(anchor, "Download removed")
    if on_change:
        on_change()


def _apply_metadata(anchor, player, track, video_id, fresh, on_change):
    if fresh.get("title"):
        track["title"] = fresh["title"]
    if fresh.get("artists"):
        track["artists"] = fresh["artists"]
        track["artist"] = ", ".join(a.get("name", "") for a in fresh["artists"] if a)
    if fresh.get("album"):
        track["album"] = fresh["album"]
    thumbs = fresh.get("thumbnail") or fresh.get("thumbnails")
    if isinstance(thumbs, list) and thumbs:
        track["thumb"] = thumbs[-1].get("url", "")
        track["thumbnails"] = thumbs

    for queued in getattr(player, "queue", None) or []:
        if queued.get("videoId") == video_id:
            queued.update(track)
            break
    if getattr(player, "discord_rpc", None):
        player.discord_rpc.update()
    if on_change:
        on_change()
    show_toast(anchor, "Metadata refreshed")
    return False


def _refresh_metadata(anchor, player, client, track, video_id, on_change):
    show_toast(anchor, "Refreshing metadata...")

    def _fetch():
        try:
            data = client.get_watch_playlist(video_id=video_id, limit=1) or {}
            tracks = data.get("tracks") or []
            if not tracks:
                GLib.idle_add(show_toast, anchor, "No metadata found")
                return
            GLib.idle_add(
                _apply_metadata, anchor, player, track, video_id, tracks[0], on_change
            )
        except Exception as exc:
            print(f"[ContextMenu] Refresh metadata failed: {exc}")
            GLib.idle_add(show_toast, anchor, "Failed to refresh metadata")

    threading.Thread(target=_fetch, daemon=True).start()


def _fetch_collection_tracks(client, item, kind, playlist_id, on_done):
    """Load the tracks of an album or playlist in the background."""

    def _run():
        tracks = []
        try:
            browse_id = item.get("browseId") or ""
            if kind == "album" and isinstance(browse_id, str) and browse_id.startswith("MPRE"):
                data = client.get_album(browse_id) or {}
            else:
                data = client.get_playlist(playlist_id) or {}
            tracks = data.get("tracks") or []
        except Exception as exc:
            print(f"[ContextMenu] Failed to load tracks: {exc}")
        GLib.idle_add(on_done, tracks)

    threading.Thread(target=_run, daemon=True).start()


# ── Songs ──────────────────────────────────────────────────────────────────


def build_song_menu(
    anchor,
    track,
    *,
    player,
    client=None,
    prefix="ctx",
    selection=None,
    extras=None,
    hide=(),
    video_id=None,
    album_title=None,
    album_id=None,
    on_change=None,
):
    """Standard song menu. Returns the Gio.Menu; the action group is
    installed on `anchor` under `prefix`."""
    client = client or getattr(player, "client", None)
    track = track if isinstance(track, dict) else {}
    vid = video_id or video_id_of(track)
    tracks = [t for t in (selection or []) if t]
    multi = len(tracks) > 1
    if not tracks:
        tracks = [track] if track else []
    count = len(tracks)
    online = is_online()

    builder = _Builder(anchor, prefix)

    # Queue
    queueable = bool(vid or multi)
    if tracks and queueable and "play_next" not in hide:
        builder.add(
            "queue",
            f"Play {count} Next" if multi else "Play Next",
            "play-next",
            lambda: _queue_tracks(anchor, player, tracks, True),
        )
    if tracks and queueable and "add_to_queue" not in hide:
        builder.add(
            "queue",
            f"Add {count} to Queue" if multi else "Add to Queue",
            "add-to-queue",
            lambda: _queue_tracks(anchor, player, tracks, False),
        )

    # Navigation
    artist_id, artist_name = artist_ref(track)
    if artist_id and "goto_artist" not in hide:
        builder.add(
            "nav",
            "Go to Artist",
            "goto-artist",
            lambda: _open_artist(anchor, artist_id, artist_name),
        )
    album_id_ref, album_name = album_ref(track)
    if album_id_ref and "goto_album" not in hide:
        builder.add(
            "nav",
            "Go to Album",
            "goto-album",
            lambda: _open_playlist(anchor, album_id_ref, album_name),
        )

    # Actions
    if vid and online and not multi and "start_radio" not in hide:
        builder.add(
            "actions",
            "Start Radio",
            "start-radio",
            lambda: (
                player.start_radio(video_id=vid),
                show_toast(anchor, "Starting radio..."),
            ),
        )

    video_ids = [video_id_of(t) for t in tracks]
    video_ids = [v for v in video_ids if v] or ([vid] if vid else [])
    if (
        video_ids
        and online
        and client
        and client.is_authenticated()
        and "add_to_playlist" not in hide
    ):
        builder.add(
            "actions",
            f"Add {len(video_ids)} to Playlist…" if multi else "Add to Playlist…",
            "add-to-playlist",
            lambda: _add_to_playlist(anchor, player, client, video_ids),
        )

    manager = getattr(player, "download_manager", None)
    if manager and tracks and "download" not in hide:
        if multi:
            pending = [t for t in tracks if not manager.is_downloaded(video_id_of(t))]
            if pending and online:
                builder.add(
                    "actions",
                    f"Download {len(pending)} Songs",
                    "download",
                    lambda: _download(anchor, pending, album_title, album_id),
                )
        elif vid and manager.is_downloaded(vid):
            builder.add(
                "actions",
                "Remove Download",
                "remove-download",
                lambda: _remove_download(anchor, player, vid, on_change),
            )
        elif vid and online:
            builder.add(
                "actions",
                "Download",
                "download",
                lambda: _download(anchor, tracks, album_title, album_id),
            )

    if (
        vid
        and online
        and client
        and not multi
        and "refresh_metadata" not in hide
    ):
        builder.add(
            "actions",
            "Refresh Metadata",
            "refresh-metadata",
            lambda: _refresh_metadata(anchor, player, client, track, vid, on_change),
        )

    builder.add_extras(extras)

    # Clipboard
    url = f"https://music.youtube.com/watch?v={vid}" if vid else None
    if url and "copy_link" not in hide:
        builder.add(
            "clipboard", "Copy Link", "copy-link", lambda: _copy_link(anchor, url)
        )

    return builder.build()


# ── Albums, playlists and artists ──────────────────────────────────────────


def build_collection_menu(
    anchor, item, kind, *, player, client=None, prefix="ctx", extras=None, hide=()
):
    client = client or getattr(player, "client", None)
    online = is_online()
    builder = _Builder(anchor, prefix)
    playlist_id = collection_id(item, kind)

    def _load(mode):
        show_toast(anchor, "Loading...")

        def _done(tracks):
            if not tracks:
                show_toast(anchor, "Nothing to play")
                return False
            if mode == "play":
                player.play_tracks(tracks)
            else:
                _queue_tracks(anchor, player, tracks, mode == "next")
            return False

        _fetch_collection_tracks(client, item, kind, playlist_id, _done)

    if playlist_id and client and online:
        if "play" not in hide:
            builder.add("queue", "Play", "play", lambda: _load("play"))
        if "play_next" not in hide:
            builder.add("queue", "Play Next", "play-next", lambda: _load("next"))
        if "add_to_queue" not in hide:
            builder.add(
                "queue", "Add to Queue", "add-to-queue", lambda: _load("queue")
            )

    artist_id, artist_name = artist_ref(item)
    if artist_id and "goto_artist" not in hide:
        builder.add(
            "nav",
            "Go to Artist",
            "goto-artist",
            lambda: _open_artist(anchor, artist_id, artist_name),
        )

    if playlist_id and online and "start_radio" not in hide:
        radio_id = item.get("radioId") or (
            playlist_id if playlist_id.startswith("RD") else f"RDAMPL{playlist_id}"
        )
        builder.add(
            "actions",
            "Start Radio",
            "start-radio",
            lambda: (
                player.start_radio(playlist_id=radio_id),
                show_toast(anchor, "Starting radio..."),
            ),
        )

    builder.add_extras(extras)

    url = build_link(item, kind)
    if url and "copy_link" not in hide:
        builder.add(
            "clipboard", "Copy Link", "copy-link", lambda: _copy_link(anchor, url)
        )

    return builder.build()


def _artist_radio(anchor, player, client, radio_id, channel_id):
    """Seed a radio from an artist. Falls back to their top song when the
    item carries no radioId, which most search results don't."""
    if radio_id:
        player.start_radio(playlist_id=radio_id)
        show_toast(anchor, "Starting radio...")
        return
    if not (client and channel_id):
        show_toast(anchor, "Radio unavailable")
        return

    show_toast(anchor, "Starting radio...")

    def _run():
        try:
            artist = client.get_artist(channel_id) or {}
        except Exception as exc:
            print(f"[ContextMenu] Artist radio failed: {exc}")
            GLib.idle_add(show_toast, anchor, "Radio unavailable")
            return
        found = artist.get("radioId")
        if found:
            GLib.idle_add(player.start_radio, None, found)
            return
        songs = (artist.get("songs") or {}).get("results") or []
        seed = songs[0].get("videoId") if songs else None
        if seed:
            GLib.idle_add(player.start_radio, seed)
        else:
            GLib.idle_add(show_toast, anchor, "Radio unavailable")

    threading.Thread(target=_run, daemon=True).start()


def build_artist_menu(
    anchor, item, *, player, client=None, prefix="ctx", extras=None, hide=()
):
    builder = _Builder(anchor, prefix)
    online = is_online()
    channel_id = item.get("channelId") or item.get("browseId") or item.get("id")
    name = item.get("artist") or item.get("title") or item.get("name") or ""

    if channel_id and "goto_artist" not in hide:
        builder.add(
            "nav",
            "Go to Artist",
            "goto-artist",
            lambda: _open_artist(anchor, channel_id, name),
        )

    radio_id = item.get("radioId") or item.get("shuffleId")
    if (radio_id or channel_id) and online and "start_radio" not in hide:
        builder.add(
            "actions",
            "Start Radio",
            "start-radio",
            lambda: _artist_radio(anchor, player, client, radio_id, channel_id),
        )

    builder.add_extras(extras)

    url = build_link(item, "artist")
    if url and "copy_link" not in hide:
        builder.add(
            "clipboard", "Copy Link", "copy-link", lambda: _copy_link(anchor, url)
        )

    return builder.build()


def build_item_menu(anchor, item, kind=None, **kwargs):
    """Dispatch to the right builder for any ytmusicapi item."""
    kind = kind or detect_kind(item)
    if kind in SONG_KINDS:
        return build_song_menu(anchor, item, **kwargs)
    if kind == "artist":
        return build_artist_menu(anchor, item, **kwargs)
    return build_collection_menu(anchor, item, kind, **kwargs)


# ── Popup helpers ──────────────────────────────────────────────────────────


def popup_menu(anchor, model, x, y):
    """Show a built menu model at the pointer."""
    if model is None or model.get_n_items() == 0:
        return None
    popover = Gtk.PopoverMenu.new_from_model(model)
    popover.set_parent(anchor)
    popover.set_has_arrow(False)
    rect = Gdk.Rectangle()
    rect.x = int(x)
    rect.y = int(y)
    rect.width = 1
    rect.height = 1
    popover.set_pointing_to(rect)
    # Drop the popover once it closes. Right-clicking a long list otherwise
    # leaves one parented widget per click alive for the page's lifetime.
    popover.connect("closed", lambda p: GLib.idle_add(p.unparent))
    popover.popup()
    return popover


def show_song_menu(anchor, x, y, track, **kwargs):
    return popup_menu(anchor, build_song_menu(anchor, track, **kwargs), x, y)


def show_item_menu(anchor, x, y, item, kind=None, **kwargs):
    return popup_menu(anchor, build_item_menu(anchor, item, kind, **kwargs), x, y)

import threading
from typing import Callable

from gi.repository import Gtk, Adw, GLib, GObject, Pango

from api.client import MusicClient
from ui.utils import AsyncPicture, LikeButton
from ui.context_menu import MenuAction, show_song_menu
from ui.util_classes import ScrolledWindow


class HistoryPage(Adw.Bin):
    """Listening-history page, grouped by the `played` field that
    ytmusicapi's get_history() attaches to each track ("Today",
    "Yesterday", "This week", ...).

    Visually modelled on YT Music's Verlauf/History screen:
    section heading + row per track (thumbnail, title, artist, duration),
    no big play-all header. Clicking a row plays it and queues the
    remaining flat history after it.
    """

    __gsignals__ = {
        "header-title-changed": (GObject.SignalFlags.RUN_FIRST, None, (str,)),
    }

    def __init__(self, player, *args, **kwargs):
        super().__init__(*args, **kwargs)
        self.player = player
        self.client = MusicClient()
        self._tracks = []
        # Collected on each rebuild so set_compact_mode can swap the
        # thumbnail sizing on every row (matches PlaylistPage).
        self._row_imgs = []
        self._compact = False

        self.main_box = Gtk.Box(orientation=Gtk.Orientation.VERTICAL)
        self.set_child(self.main_box)

        self.scrolled = ScrolledWindow()
        self.scrolled.set_policy(Gtk.PolicyType.NEVER, Gtk.PolicyType.AUTOMATIC)
        self.scrolled.set_vexpand(True)

        self.clamp = Adw.Clamp()
        self.clamp.set_maximum_size(1024)
        self.clamp.set_tightening_threshold(600)

        self.content_box = Gtk.Box(orientation=Gtk.Orientation.VERTICAL, spacing=24)
        self.content_box.set_margin_top(24)
        self.content_box.set_margin_bottom(24)
        self.content_box.set_margin_start(24)
        self.content_box.set_margin_end(24)

        # Page title ("Listening History")
        self.title_label = Gtk.Label(label="Listening History")
        self.title_label.add_css_class("title-1")
        self.title_label.set_halign(Gtk.Align.START)
        self.content_box.append(self.title_label)

        # Sections container — emptied and rebuilt on every fetch.
        self.sections_box = Gtk.Box(orientation=Gtk.Orientation.VERTICAL, spacing=16)
        self.content_box.append(self.sections_box)

        # Empty-state label (shown when history returns zero tracks).
        self.empty_label = Gtk.Label(
            label="Your listening history will appear here after you play something."
        )
        self.empty_label.add_css_class("dim-label")
        self.empty_label.set_wrap(True)
        self.empty_label.set_halign(Gtk.Align.CENTER)
        self.empty_label.set_margin_top(48)
        self.empty_label.set_visible(False)
        self.content_box.append(self.empty_label)

        self.clamp.set_child(self.content_box)
        self.scrolled.set_child(self.clamp)

        # Loading spinner overlayed at the Bin level — same reliable
        # centering pattern as LibraryPage: Gtk.Overlay sidesteps the
        # Adw.Clamp/ScrolledWindow sizing fight that breaks vexpand
        # chains inside the viewport.
        self._loading_wrap = Gtk.Box(orientation=Gtk.Orientation.VERTICAL, spacing=12)
        self._loading_wrap.set_valign(Gtk.Align.CENTER)
        self._loading_wrap.set_halign(Gtk.Align.CENTER)
        self._loading_wrap.set_visible(True)
        spinner = Adw.Spinner()
        spinner.set_size_request(48, 48)
        self._loading_wrap.append(spinner)
        lbl = Gtk.Label(label="Loading history...")
        lbl.add_css_class("caption")
        self._loading_wrap.append(lbl)

        overlay = Gtk.Overlay()
        overlay.set_vexpand(True)
        overlay.set_child(self.scrolled)
        overlay.add_overlay(self._loading_wrap)
        self.main_box.append(overlay)

        # Title-in-headerbar behavior matches PlaylistPage so the
        # NavigationPage's title bar stays in sync when scrolling.
        self.scrolled.get_vadjustment().connect("value-changed", self._on_scroll)

    # ── Loading ────────────────────────────────────────────────────────────

    def load(self):
        """Kick off the initial render.

        Cached tracks are loaded and rendered asynchronously. Once cached rendering completes,
        a refresh callback is triggered to fetch fresh history from the server.
        """
        self._load_cached(async_render=True, async_callback=self.refresh_from_server)

    def _load_cached(self, async_render=False, async_callback: Callable[[], None]=lambda:None):
        if not self.client.is_authenticated():
            self._show_empty("Sign in to view listening history.")
            return
        cached = self.client.get_cached_history() or []
        if cached:
            self._normalize_durations(cached)
            self._render(cached, async_render=async_render, async_callback=async_callback)
        else:
            self._clear_sections()
            self.empty_label.set_visible(False)
            self._loading_wrap.set_visible(True)

    def load_cached(self):
        """Paint the cached history immediately. Intended to run before
        the nav animation so the pushed page isn't blank."""
        self._load_cached()

    def refresh_from_server(self, delay_ms=0):
        """Fetch fresh history and re-render. `delay_ms` lets callers
        push the network work past the nav-transition frames."""
        from ui.utils import is_online
        if not self.client.is_authenticated():
            return
        if not is_online():
            if not self._tracks:
                self._show_empty("History requires an internet connection.")
            return

        def _fetch():
            tracks = self.client.get_history() or []
            self._normalize_durations(tracks)
            GLib.idle_add(self._render, tracks)

        def _kick():
            threading.Thread(target=_fetch, daemon=True).start()
            return False

        if delay_ms > 0:
            GLib.timeout_add(delay_ms, _kick)
        else:
            _kick()

    @staticmethod
    def _normalize_durations(tracks):
        """Fill in `duration_seconds` from the "3:42"-style string that
        ytmusicapi returns. The player queue expects numeric seconds.

        Also derives a clean `artist` string from the `artists` list,
        skipping the view-count entries ytmusicapi tacks on for history
        items ("7.2M views"). If we don't set `artist` here, the player
        falls back to joining every `artists[].name` — producing
        "Jamie Paige, 7.2M views" in the player bar."""
        import re
        view_re = re.compile(
            r"^\s*\d+(?:[.,]\d+)?\s*[kKmMbB]?\s*views?\s*$"
        )
        for t in tracks:
            if not t.get("duration_seconds"):
                dstr = t.get("duration") or ""
                parts = dstr.split(":")
                try:
                    if len(parts) == 2:
                        t["duration_seconds"] = int(parts[0]) * 60 + int(parts[1])
                    elif len(parts) == 3:
                        t["duration_seconds"] = (
                            int(parts[0]) * 3600
                            + int(parts[1]) * 60
                            + int(parts[2])
                        )
                except ValueError:
                    pass

            if not t.get("artist"):
                real_artists = [
                    a.get("name", "")
                    for a in (t.get("artists") or [])
                    if isinstance(a, dict)
                    and a.get("name")
                    and not view_re.match(a["name"])
                ]
                if real_artists:
                    t["artist"] = ", ".join(real_artists)

    def _render(self, tracks, async_render=False, async_callback: Callable[[], None]=lambda:None):
        """Renders the given tracks onto the page.

        If async_render is True, rendering is performed incrementally using GLib.idle_add,
        processing one group per idle cycle. async_callback (if provided) is called once
        rendering completes (when idle returns False).

        If async_render is False, all groups are rendered synchronously in a single pass
        without idle scheduling. async_callback will not be called.
        """
        self._loading_wrap.set_visible(False)
        self._tracks = tracks
        self._row_imgs = []
        self._clear_sections()
        if not tracks:
            self.empty_label.set_label(
                "Your listening history will appear here after you play something."
            )
            self.empty_label.set_visible(True)
            return
        self.empty_label.set_visible(False)

        # Group by the `played` header YT Music attaches to each track.
        # Tracks that somehow lack a value fall back to "Recently".
        by_title = {}
        for idx, t in enumerate(tracks):
            t["_history_index"] = idx
            key = t.get("played") or "Recently"
            if key not in by_title:
                by_title[key] = []
            by_title[key].append(t)

        groups = by_title.items()  # [(title, [tracks])], preserves YT's order

        if async_render:
            groups_iter = iter(groups)

            def _render_by_group():
                item = next(groups_iter, None)
                if item is None:
                    async_callback()
                    return False
                title, group_tracks = item
                self.sections_box.append(self._make_section(title, group_tracks))
                return True

            GLib.idle_add(_render_by_group)
        else:
            for title, group_tracks in groups:
                self.sections_box.append(self._make_section(title, group_tracks))

    def _show_empty(self, msg):
        self._loading_wrap.set_visible(False)
        self._clear_sections()
        self.empty_label.set_label(msg)
        self.empty_label.set_visible(True)

    def _clear_sections(self):
        child = self.sections_box.get_first_child()
        while child:
            nxt = child.get_next_sibling()
            self.sections_box.remove(child)
            child = nxt

    # ── Section / row building ─────────────────────────────────────────────

    def _make_section(self, title, tracks):
        box = Gtk.Box(orientation=Gtk.Orientation.VERTICAL, spacing=8)

        header = Gtk.Label(label=title)
        header.add_css_class("title-3")
        header.set_halign(Gtk.Align.START)
        box.append(header)

        listbox = Gtk.ListBox()
        listbox.set_selection_mode(Gtk.SelectionMode.NONE)
        listbox.add_css_class("boxed-list")
        for t in tracks:
            row = self._make_row(t)
            if row is not None:
                listbox.append(row)
        listbox.connect("row-activated", self._on_row_activated)
        box.append(listbox)
        return box

    def _make_row(self, track):
        vid = track.get("videoId")
        if not vid:
            return None

        row = Gtk.ListBoxRow()
        row._track = track

        hb = Gtk.Box(orientation=Gtk.Orientation.HORIZONTAL, spacing=12)
        hb.set_hexpand(True)
        hb.add_css_class("song-row")

        # Thumbnail — matches playlist rows (56px via `song-img` class).
        thumb_url = ""
        thumbnails = track.get("thumbnails") or []
        if thumbnails:
            thumb_url = thumbnails[-1].get("url", "")
        img = AsyncPicture(
            crop_to_square=True, target_size=56, player=self.player
        )
        img.add_css_class("song-img")
        if self._compact:
            img.set_compact(True)
        if thumb_url:
            img.video_id = vid
            img.load_url(thumb_url)
        hb.append(img)
        self._row_imgs.append(img)

        # Title + subtitle
        vb = Gtk.Box(orientation=Gtk.Orientation.VERTICAL, spacing=2)
        vb.set_valign(Gtk.Align.CENTER)
        vb.set_hexpand(True)

        title = track.get("title", "Unknown")
        title_label = Gtk.Label(label=title)
        title_label.set_halign(Gtk.Align.START)
        title_label.set_ellipsize(Pango.EllipsizeMode.END)
        title_label.set_lines(1)
        title_label.set_tooltip_text(title)

        title_box = Gtk.Box(orientation=Gtk.Orientation.HORIZONTAL, spacing=6)
        title_box.append(title_label)

        # Explicit badge (matches playlist row).
        is_explicit = bool(track.get("isExplicit"))
        if is_explicit:
            explicit_badge = Gtk.Label(label="E")
            explicit_badge.add_css_class("explicit-badge")
            explicit_badge.set_valign(Gtk.Align.CENTER)
            title_box.append(explicit_badge)

        # Download indicator, same icon/size as PlaylistPage.
        dl_icon = Gtk.Image.new_from_icon_name("folder-download-symbolic")
        dl_icon.set_pixel_size(14)
        dl_icon.add_css_class("dim-label")
        dl_icon.set_valign(Gtk.Align.CENTER)
        dl_icon.set_visible(
            self.player.download_manager.is_downloaded(vid)
        )
        title_box.append(dl_icon)

        vb.append(title_box)

        artists = track.get("artists") or []
        artist_str = ", ".join(
            a.get("name", "") for a in artists if isinstance(a, dict)
        )
        album = track.get("album") or {}
        if isinstance(album, dict) and album.get("name"):
            subtitle = (
                f"{artist_str} • {album['name']}" if artist_str else album["name"]
            )
        else:
            subtitle = artist_str
        sub_label = Gtk.Label(label=subtitle or "")
        sub_label.add_css_class("dim-label")
        sub_label.add_css_class("caption")
        sub_label.set_halign(Gtk.Align.START)
        sub_label.set_ellipsize(Pango.EllipsizeMode.END)
        sub_label.set_lines(1)
        sub_label.set_tooltip_text(subtitle or "")
        vb.append(sub_label)

        hb.append(vb)

        # Duration
        dur = track.get("duration_seconds") or 0
        dur_str = track.get("duration") or (
            f"{dur // 60}:{dur % 60:02d}" if dur else ""
        )
        dur_label = Gtk.Label(label=dur_str)
        dur_label.add_css_class("caption")
        dur_label.set_valign(Gtk.Align.CENTER)
        dur_label.set_margin_end(6)
        hb.append(dur_label)

        # Like button
        like = LikeButton(self.client, None)
        like.set_valign(Gtk.Align.CENTER)
        like.set_data(vid, track.get("likeStatus", "INDIFFERENT"))
        hb.append(like)

        row.set_child(hb)

        # Right-click / long-press context menu
        click = Gtk.GestureClick()
        click.set_button(3)
        click.connect("released", self._on_row_right_click, row)
        row.add_controller(click)
        lp = Gtk.GestureLongPress()
        lp.connect(
            "pressed",
            lambda g, x, y, r=row: self._on_row_right_click(g, 1, x, y, r),
        )
        row.add_controller(lp)

        return row

    # ── Interactions ───────────────────────────────────────────────────────

    def _on_row_activated(self, listbox, row):
        track = getattr(row, "_track", None)
        if not track or not self._tracks:
            return
        start = track.get("_history_index", 0)
        # Play from start index through the rest of the history, same
        # as clicking inside a flat playlist.
        self.player.set_queue(
            self._tracks,
            start,
            shuffle=False,
            source_id="HISTORY",
            is_infinite=False,
        )

    def _on_row_right_click(self, gesture, n_press, x, y, row):
        track = getattr(row, "_track", None)
        if not track:
            return
        vid = track.get("videoId")
        if not vid:
            return

        extras = [
            MenuAction(
                "Play",
                lambda r=row: self._on_row_activated(None, r),
                section="queue",
                first=True,
            )
        ]

        # Remove from history is optimistic: the row disappears instantly
        # and the cache is patched up front, the API call is fire-and-forget.
        fb_token = track.get("feedbackToken") or track.get("feedbackTokens")
        if fb_token:
            tokens = (
                [fb_token]
                if isinstance(fb_token, str)
                else list(fb_token.values())
                if isinstance(fb_token, dict)
                else list(fb_token)
            )
            extras.append(
                MenuAction(
                    "Remove from History",
                    lambda v=vid, t=tokens: self._remove_track_optimistic(v, t),
                    section="remove",
                )
            )

        show_song_menu(
            row,
            x,
            y,
            track,
            player=self.player,
            client=self.client,
            prefix="row",
            extras=extras,
        )

    def _remove_track_optimistic(self, video_id, tokens):
        """Delete a track row in-place without a full re-render: drop it
        from `self._tracks` + the disk cache, remove the widget from its
        section's ListBox (and the section itself if it becomes empty),
        then fire the API call in the background."""
        if not video_id:
            return

        # 1. Trim in-memory and disk state.
        self._tracks = [t for t in self._tracks if t.get("videoId") != video_id]
        try:
            self.client.invalidate_history_cache_entry(video_id)
        except Exception as e:
            print(f"[HISTORY] cache invalidate failed: {e}")

        # 2. Find and remove the widget row. Each section is a VBox with
        # [header_label, listbox]; walk them until we hit the matching vid,
        # then stop — there's only ever one row per videoId.
        section = self.sections_box.get_first_child()
        while section is not None:
            next_section = section.get_next_sibling()
            listbox = section.get_last_child()
            removed = False
            if isinstance(listbox, Gtk.ListBox):
                row = listbox.get_first_child()
                while row is not None:
                    nxt = row.get_next_sibling()
                    if getattr(row, "_track", {}).get("videoId") == video_id:
                        listbox.remove(row)
                        removed = True
                        # If that emptied the section, drop its header too.
                        if listbox.get_first_child() is None:
                            self.sections_box.remove(section)
                        break
                    row = nxt
            if removed:
                break
            section = next_section

        # 3. Empty-state banner if that was the last entry.
        if not self._tracks:
            self.empty_label.set_label(
                "Your listening history will appear here after you play something."
            )
            self.empty_label.set_visible(True)

        # 4. Fire the API call.
        def _th():
            try:
                self.client.remove_history_items(tokens)
            except Exception as e:
                print(f"[HISTORY] remove failed: {e}")

        threading.Thread(target=_th, daemon=True).start()

    # ── Title-in-headerbar ─────────────────────────────────────────────────

    def _on_scroll(self, vadjust):
        val = vadjust.get_value()
        self.emit("header-title-changed", "" if val <= 50 else "Listening History")

    def set_compact_mode(self, compact):
        self._compact = compact
        # Shrink/expand every row's thumbnail the same way PlaylistPage
        # propagates compact state to `_lv_img.set_compact(...)`.
        for img in self._row_imgs:
            if hasattr(img, "set_compact"):
                img.set_compact(compact)
        if compact:
            self.add_css_class("compact")
            self.content_box.set_margin_start(12)
            self.content_box.set_margin_end(12)
        else:
            self.remove_css_class("compact")
            self.content_box.set_margin_start(24)
            self.content_box.set_margin_end(24)

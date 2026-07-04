from gi.repository import Gtk, Adw, GObject, GLib, Pango, Gdk, Gio
import threading
import weakref
import json
import re
from api.client import MusicClient
from ui.utils import (
    AsyncImage, AsyncPicture, LikeButton, parse_item_metadata,
    attach_playing_highlight,
)
from ui.util_classes import ScrolledWindow


class ArtistPage(Adw.Bin):
    __gsignals__ = {
        "header-title-changed": (GObject.SignalFlags.RUN_FIRST, None, (str,))
    }

    def __init__(self, player, open_playlist_callback, *args, **kwargs):
        super().__init__(*args, **kwargs)
        self.player = player
        weak_self = weakref.ref(self)
        self.open_playlist_callback = open_playlist_callback
        self.client = MusicClient()
        self.artist_name = ""
        self.current_songs = []
        self._artist_data = None
        self._section_limits = {
            "Top Songs": 5,
            "Albums": 10,
            "Singles & EPs": 10,
            "Videos": 10,
        }
        self._section_widgets = {}  # Store section containers

        # Main Layout
        self.main_box = Gtk.Box(orientation=Gtk.Orientation.VERTICAL)

        # Content Scrolled Window
        scrolled = ScrolledWindow()
        scrolled.set_policy(Gtk.PolicyType.NEVER, Gtk.PolicyType.AUTOMATIC)
        scrolled.set_vexpand(True)

        # Monitor scroll for title
        vadjust = scrolled.get_vadjustment()
        self.vadjust = vadjust
        self._scroll_handler_id = vadjust.connect(
            "value-changed", lambda adj: weak_self()._on_scroll(adj) if weak_self() else None
        )

        # Main Clamp
        self.clamp = Adw.Clamp()
        self.clamp.set_maximum_size(1024)
        self.clamp.set_tightening_threshold(600)

        content_box = Gtk.Box(orientation=Gtk.Orientation.VERTICAL, spacing=0)
        content_box.set_margin_bottom(24)
        content_box.set_margin_start(12)
        content_box.set_margin_end(12)
        self.content_box = content_box

        # 1. Header Grid (The new robust layout)
        self.header_grid = Gtk.Grid()
        self.header_grid.set_column_homogeneous(True)
        content_box.append(self.header_grid)

        # 1a. Visual Banner Overlay
        self.banner_overlay = Gtk.Overlay()
        self.banner_overlay.set_vexpand(False)
        self.banner_overlay.set_hexpand(True)
        self.banner_overlay.set_valign(Gtk.Align.START)
        self.banner_overlay.set_size_request(-1, 260)

        # Banner Image
        self.avatar = AsyncPicture(player=self.player)
        self.avatar.set_hexpand(True)
        self.avatar.set_vexpand(True)
        self.avatar.set_halign(Gtk.Align.FILL)
        self.avatar.set_valign(Gtk.Align.FILL)
        self.avatar.set_content_fit(Gtk.ContentFit.COVER)

        from ui.widgets.fade_bottom_bin import FadeBottomBin
        self.banner_wrapper = FadeBottomBin(fade_start=0.55)
        self.banner_wrapper.set_overflow(Gtk.Overflow.HIDDEN)
        self.banner_wrapper.add_css_class("banner-top-rounded")
        self.banner_wrapper.set_hexpand(True)
        self.banner_wrapper.set_vexpand(False)
        self.banner_wrapper.set_size_request(-1, 260)
        self.banner_wrapper.append(self.avatar)
        self.banner_overlay.set_child(self.banner_wrapper)
        # Sync the bottom fade with the window's blur mode. Without the
        # fade in non-blur mode, the existing `.banner-scrim` gradient
        # ramping to opaque @window_bg_color already hides the bottom
        # edge of the photo. In blur mode that scrim can't go fully
        # opaque (would paint a solid band over the blur), so we mask
        # the image itself to alpha 0 instead. Re-check on every realize
        # plus on a notify::css-classes from the root window.
        weak_self = weakref.ref(self)
        def _sync_banner_fade(*_):
            s = weak_self()
            if not s:
                return
            root = s.banner_wrapper.get_root()
            classes = list(root.get_css_classes()) if root else []
            active = "cover-bg-active" in classes
            print(
                f"[FADE-SYNC] root={type(root).__name__ if root else None}"
                f" classes={classes} active={active}"
            )
            s.banner_wrapper.set_fade_active(active)

        self._sync_banner_fade = _sync_banner_fade
        self._banner_fade_root_handler = None

        def _hook_root(*_):
            s = weak_self()
            if not s:
                return
            root = s.banner_wrapper.get_root()
            if root and s._banner_fade_root_handler is None:
                s._banner_fade_root_handler = root.connect(
                    "notify::css-classes", _sync_banner_fade
                )
            _sync_banner_fade()

        self.banner_wrapper.connect("realize", lambda *_: _hook_root())
        self.banner_wrapper.connect(
            "map", lambda *_: GLib.idle_add(lambda: _hook_root())
        )

        # Visual Scrim
        self.banner_scrim = Gtk.Box()
        self.banner_scrim.set_vexpand(True)
        self.banner_scrim.set_hexpand(True)
        self.banner_scrim.add_css_class("banner-scrim")
        self.banner_overlay.add_overlay(self.banner_scrim)

        # Attach banner to the grid
        self.header_grid.attach(self.banner_overlay, 0, 0, 1, 1)

        # 1b. Info Overlay Box
        # This box overlaps the banner bottom naturally within the same grid cell
        self.info_overlay_box = Gtk.Box(orientation=Gtk.Orientation.VERTICAL, spacing=4)
        self.info_overlay_box.set_margin_top(160)  # Standard positive margin offset
        self.info_overlay_box.set_margin_start(16)
        self.info_overlay_box.set_margin_end(16)
        self.info_overlay_box.set_margin_bottom(24)
        self.info_overlay_box.set_vexpand(False)
        self.info_overlay_box.set_valign(Gtk.Align.START)

        # Attach info to the SAME cell in the grid
        self.header_grid.attach(self.info_overlay_box, 0, 0, 1, 1)

        self.name_label = Gtk.Label(label="Artist Name")
        self.name_label.add_css_class("title-1")
        self.name_label.set_halign(Gtk.Align.START)
        self.name_label.add_css_class("banner-text")  # Still white with shadow
        # Long artist names should wrap to a second line instead of being
        # clipped off the edge of the banner.
        self.name_label.set_wrap(True)
        self.name_label.set_wrap_mode(Pango.WrapMode.WORD_CHAR)
        self.name_label.set_xalign(0.0)
        self.info_overlay_box.append(self.name_label)

        self.subscribers_label = Gtk.Label(label="")
        self.subscribers_label.add_css_class("caption")
        self.subscribers_label.set_opacity(0.85)
        self.subscribers_label.set_halign(Gtk.Align.START)
        self.subscribers_label.add_css_class("banner-text")
        self.info_overlay_box.append(self.subscribers_label)

        # Actions row
        actions = Gtk.Box(orientation=Gtk.Orientation.HORIZONTAL, spacing=12)
        actions.set_margin_top(8)
        actions.set_valign(Gtk.Align.CENTER)
        self.info_overlay_box.append(actions)

        self.play_btn = Gtk.Button(label="Play")
        self.play_btn.add_css_class("suggested-action")
        self.play_btn.add_css_class("pill")
        self.play_btn.connect("clicked", lambda btn: weak_self().on_play_clicked(btn) if weak_self() else None)
        actions.append(self.play_btn)

        self.shuffle_btn = Gtk.Button()
        self.shuffle_btn.set_icon_name("media-playlist-shuffle-symbolic")
        self.shuffle_btn.add_css_class("circular")
        self.shuffle_btn.set_valign(Gtk.Align.CENTER)
        self.shuffle_btn.set_size_request(48, 48)
        self.shuffle_btn.set_tooltip_text("Shuffle")
        self.shuffle_btn.connect("clicked", lambda btn: weak_self().on_shuffle_clicked(btn) if weak_self() else None)
        actions.append(self.shuffle_btn)

        self.radio_btn = Gtk.Button()
        self.radio_btn.set_icon_name("triangular-antenna-symbolic")
        self.radio_btn.add_css_class("circular")
        self.radio_btn.set_valign(Gtk.Align.CENTER)
        self.radio_btn.set_size_request(48, 48)
        self.radio_btn.set_tooltip_text("Start Radio")
        self.radio_btn.connect("clicked", lambda btn: weak_self().on_radio_clicked(btn) if weak_self() else None)
        actions.append(self.radio_btn)

        self.subscribe_btn = Gtk.Button()
        self.subscribe_btn.set_icon_name("non-starred-symbolic")
        self.subscribe_btn.add_css_class("circular")
        self.subscribe_btn.add_css_class("flat")
        self.subscribe_btn.set_valign(Gtk.Align.CENTER)
        self.subscribe_btn.set_size_request(48, 48)
        self.subscribe_btn.set_tooltip_text("Subscribe")
        self.subscribe_btn.connect("clicked", lambda btn: weak_self().on_subscribe_clicked(btn) if weak_self() else None)
        actions.append(self.subscribe_btn)

        # Description
        self.description_label = Gtk.Label(label="")
        self.description_label.add_css_class("body")
        self.description_label.set_wrap(True)
        self.description_label.set_wrap_mode(Pango.WrapMode.WORD_CHAR)
        self.description_label.set_halign(Gtk.Align.START)
        self.description_label.set_xalign(0)
        self.description_label.set_lines(0)
        self.description_label.set_ellipsize(Pango.EllipsizeMode.NONE)
        self.description_label.set_margin_top(12)

        self.read_more_btn = Gtk.Label()
        self.read_more_btn.set_use_markup(True)
        self.read_more_btn.set_markup("<a href='toggle'>Read more</a>")
        self.read_more_btn.add_css_class("caption")
        self.read_more_btn.set_halign(Gtk.Align.START)
        self.read_more_btn.set_visible(False)
        self.read_more_btn.connect(
            "activate-link", lambda label, uri: weak_self()._on_read_more_link(label, uri) if weak_self() else None
        )
        self._description_expanded = False

        self.description_box = Gtk.Box(orientation=Gtk.Orientation.VERTICAL, spacing=4)
        self.description_box.set_margin_start(16)
        self.description_box.set_margin_end(16)
        self.description_box.set_margin_top(0)
        self.description_box.set_margin_bottom(16)
        self.description_box.append(self.description_label)
        self.description_box.append(self.read_more_btn)
        content_box.append(self.description_box)

        # 2. Sections
        self.sections_box = Gtk.Box(orientation=Gtk.Orientation.VERTICAL, spacing=32)
        self.sections_box.set_margin_top(12)
        content_box.append(self.sections_box)

        self.clamp.set_child(content_box)
        scrolled.set_child(self.clamp)
        self.main_box.append(scrolled)

        # Stack for Loading vs Content
        self.stack = Adw.ViewStack()

        loading_box = Gtk.Box(orientation=Gtk.Orientation.VERTICAL, spacing=12)
        loading_box.set_valign(Gtk.Align.CENTER)
        loading_box.set_halign(Gtk.Align.CENTER)

        self.spinner = Adw.Spinner()
        self.spinner.set_size_request(32, 32)
        loading_box.append(self.spinner)

        self.stack.add_named(loading_box, "loading")
        self.stack.add_named(self.main_box, "content")

        self.set_child(self.stack)

    def load_artist(self, channel_id, initial_name=None):
        self.channel_id = channel_id
        if initial_name:
            self.artist_name = initial_name
            self.name_label.set_label(initial_name)

        self.stack.set_visible_child_name("loading")

        thread = threading.Thread(target=self._fetch_artist, args=(channel_id,))
        thread.daemon = True
        thread.start()

    def _fetch_artist(self, channel_id):
        client = self.client
        if not client:
            return
        try:
            self._artist_data = client.get_artist(channel_id)

            # Fetch missing sections (playlists, featured on, live) from raw API
            if self._artist_data and not self._artist_data.get("_is_channel"):
                try:
                    raw = client.api._send_request(
                        "browse", {"browseId": channel_id}
                    )
                    tabs = (
                        raw.get("contents", {})
                        .get("singleColumnBrowseResultsRenderer", {})
                        .get("tabs", [])
                    )
                    if tabs:
                        sections = (
                            tabs[0]
                            .get("tabRenderer", {})
                            .get("content", {})
                            .get("sectionListRenderer", {})
                            .get("contents", [])
                        )
                        for section in sections:
                            for rkey in ["musicCarouselShelfRenderer"]:
                                r = section.get(rkey, {})
                                if not r:
                                    continue
                                title = ""
                                browse_id = None
                                params = None
                                for hk in r.get("header", {}):
                                    hr = r["header"][hk]
                                    if isinstance(hr, dict):
                                        runs = hr.get("title", {}).get("runs", [])
                                        if runs:
                                            title = runs[0].get("text", "")
                                        # Get browse endpoint for "View All"
                                        more_ep = (
                                            hr.get("moreContentButton", {})
                                            .get("buttonRenderer", {})
                                            .get("navigationEndpoint", {})
                                            .get("browseEndpoint", {})
                                        )
                                        if more_ep.get("browseId"):
                                            browse_id = more_ep["browseId"]
                                            params = more_ep.get("params")
                                # Add sections that ytmusicapi misses
                                if (
                                    title
                                    and "playlist" in title.lower()
                                    and "playlists" not in self._artist_data
                                ):
                                    items = []
                                    for raw_item in r.get("contents", []):
                                        parsed = client._parse_channel_item(
                                            raw_item
                                        )
                                        if parsed:
                                            items.append(parsed)
                                    if items:
                                        self._artist_data["playlists"] = {
                                            "results": items,
                                            "browseId": browse_id,
                                            "params": params,
                                        }
                                elif (
                                    title
                                    and "featured" in title.lower()
                                    and "featured_on" not in self._artist_data
                                ):
                                    items = []
                                    for raw_item in r.get("contents", []):
                                        parsed = client._parse_channel_item(
                                            raw_item
                                        )
                                        if parsed:
                                            items.append(parsed)
                                    if items:
                                        self._artist_data["featured_on"] = {
                                            "results": items,
                                            "browseId": browse_id,
                                            "params": params,
                                        }
                except Exception:
                    pass

            # Deep fetch sections that are usually truncated (Albums, Singles, EPs)
            # This ensures we get high-quality metadata (year, type, explicit) from the start.
            detail_threads = []

            def detail_fetch(key, browse_id, params):
                try:
                    # Limit to 10 for the initial view as requested
                    detailed_items = client.get_artist_albums(
                        browse_id, params, limit=10
                    )
                    if detailed_items:
                        # Merge into _artist_data
                        self._artist_data[key]["results"] = detailed_items
                except Exception as e:
                    print(f"Error deep fetching {key}: {e}")

            for key in ["songs", "albums", "singles"]:
                if key in self._artist_data and isinstance(
                    self._artist_data[key], dict
                ):
                    section = self._artist_data[key]
                    b_id = section.get("browseId")
                    p_params = section.get("params")
                    if b_id:
                        # For songs, we use get_playlist to get the full list
                        fetch_target = (
                            client.get_playlist if key == "songs" else detail_fetch
                        )
                        target_args = (
                            (b_id,) if key == "songs" else (key, b_id, p_params)
                        )

                        def wrap_fetch(k=key, ft=fetch_target, ta=target_args):
                            try:
                                if k == "songs":
                                    res = ft(ta[0])
                                    if res and res.get("tracks"):
                                        self._artist_data[k]["results"] = res["tracks"]
                                else:
                                    ft(*ta)
                            except Exception as ex:
                                print(f"Error deep fetching {k}: {ex}")

                        t = threading.Thread(target=wrap_fetch)
                        t.daemon = True
                        t.start()
                        detail_threads.append(t)

            # Wait for all deep fetches to complete (timed out)
            for t in detail_threads:
                t.join(timeout=10.0)  # Generous timeout for multiple deep fetches

            self._is_ui_init = False  # Fresh load
            GLib.idle_add(self.update_ui, self._artist_data)
        except Exception as e:
            print(f"Error fetching artist: {e}")

    def update_ui(self, data):
        if not data:
            return

        self.stack.set_visible_child_name("content")

        # Header
        self.artist_name = data.get("name") or "Unknown Artist"
        self.name_label.set_label(self.artist_name)
        description = data.get("description") or ""
        self._description_expanded = False
        if description:
            # Strip trailing Wikipedia attribution ("From Wikipedia, ...")
            clean = re.sub(
                r"\s*From Wikipedia[^\n]*", "", description, flags=re.IGNORECASE
            ).strip()
            # Collapse only 2+ SPACES, preserve single \n
            clean = re.sub(r"[^\S\n]{2,}", " ", clean)
            # Collapse only 3+ \n into 2 \n
            clean = re.sub(r"\n{3,}", "\n\n", clean)
            self._description_clean = clean
            if len(clean) > 280:
                preview = clean[:280].rsplit(" ", 1)[0] + "…"
                self.description_label.set_label(preview)
                self.read_more_btn.set_markup("<a href='toggle'>Read more</a>")
                self.read_more_btn.set_visible(True)
            else:
                self.description_label.set_label(clean)
                self.read_more_btn.set_visible(False)
            self.description_label.set_visible(True)
        else:
            self._description_clean = ""
            self.description_label.set_label("")
            self.description_label.set_visible(False)
            self.read_more_btn.set_visible(False)

        subs = data.get("subscribers") or ""
        if subs:
            subs = str(subs) + " subscribers"

        views = data.get("views")
        if views:
            views = str(views)
            if subs:
                subs += " • " + views
            else:
                subs = views

        self.subscribers_label.set_label(str(subs))

        # Subscription Status
        self._is_subscribed = data.get("subscribed", False)
        # Fallback: Check local cache if it's already in our Library
        if not self._is_subscribed and self.channel_id:
            if self.client.is_subscribed_artist(self.channel_id):
                self._is_subscribed = True

        self._update_subscribe_button()

        # Store radio/shuffle IDs for the action buttons
        self._radio_id = data.get("radioId")
        self._shuffle_id = data.get("shuffleId")

        # Hide play/shuffle/radio for channels (no songs)
        is_channel = data.get("_is_channel", False)
        self.play_btn.set_visible(not is_channel)
        self.shuffle_btn.set_visible(not is_channel)
        self.radio_btn.set_visible(not is_channel and bool(self._radio_id))

        # Use banner for the header image if available, otherwise avatar/thumbnail
        banner = data.get("banner", [])
        thumbnails = data.get("thumbnails", [])
        if banner:
            self.avatar.load_url(banner[-1]["url"])
        elif thumbnails:
            self.avatar.load_url(thumbnails[-1]["url"])

        # Determine if we are updating or doing a fresh load
        is_refresh = getattr(self, "_is_ui_init", False)
        if not is_refresh:
            self._section_widgets = {}
            child = self.sections_box.get_first_child()
            while child:
                next_child = child.get_next_sibling()
                self.sections_box.remove(child)
                child = next_child
            self._is_ui_init = True

        # Songs
        if "songs" in data:
            self.add_songs_section("Top Songs", data["songs"])

        # Albums
        if "albums" in data:
            self.add_grid_section("Albums", data["albums"])

        # Singles
        if "singles" in data:
            self.add_grid_section("Singles & EPs", data["singles"])

        # Videos
        if "videos" in data:
            self.add_grid_section("Videos", data["videos"])

        # Playlists
        if "playlists" in data:
            playlists = data["playlists"]
            # get_user returns {"browseId": ..., "results": [...]}
            if isinstance(playlists, dict) and playlists.get("results"):
                self.add_grid_section("Playlists", playlists)
            elif isinstance(playlists, list) and playlists:
                self.add_grid_section("Playlists", {"results": playlists})

        # Featured on
        if "featured_on" in data:
            self.add_grid_section("Featured On", data["featured_on"])

        # Fans might also like (related artists)
        if "related" in data:
            related = data["related"]
            if isinstance(related, dict) and related.get("results"):
                self.add_grid_section("Fans Might Also Like", related)
            elif isinstance(related, list) and related:
                self.add_grid_section("Fans Might Also Like", {"results": related})

    def add_songs_section(self, title, section_dict):
        items = section_dict.get("results", [])
        if not items:
            return
        self.current_songs = items  # Store for queue
        if title in self._section_widgets:
            box = self._section_widgets[title]
            # Content box is the first child of 'box'
            section_box = box.get_first_child()
            # Clear it
            child = section_box.get_first_child()
            while child:
                next_child = child.get_next_sibling()
                section_box.remove(child)
                child = next_child
        else:
            box = Gtk.Box(orientation=Gtk.Orientation.VERTICAL, spacing=16)
            self._section_widgets[title] = box
            self.sections_box.append(box)
            section_box = Gtk.Box(orientation=Gtk.Orientation.VERTICAL, spacing=8)
            box.append(section_box)

        label = Gtk.Label(label=title)
        label.add_css_class("heading")
        label.set_halign(Gtk.Align.START)
        section_box.append(label)

        list_box = Gtk.ListBox()
        list_box.add_css_class("boxed-list")
        list_box.set_selection_mode(Gtk.SelectionMode.NONE)
        weak_self = weakref.ref(self)
        list_box.connect("row-activated", lambda lb, r: weak_self().on_song_activated(lb, r) if weak_self() else None)

        limit = self._section_limits.get(title, 5)
        showing_items = items[:limit]

        for item in showing_items:
            row = Gtk.ListBoxRow()
            box = Gtk.Box(orientation=Gtk.Orientation.HORIZONTAL, spacing=12)
            box.add_css_class("song-row")
            row.set_child(box)
            # Highlight while this track is playing — artist's Top
            # Songs rows are built ad-hoc (not SongRowWidget) so they
            # need an explicit subscription.
            if item.get("videoId"):
                attach_playing_highlight(box, self.player, item["videoId"])

            # Thumbnail
            thumbnails = item.get("thumbnails", [])
            thumb_url = thumbnails[-1]["url"] if thumbnails else None
            from ui.utils import AsyncPicture

            img = AsyncPicture(
                url=thumb_url,
                target_size=56,
                crop_to_square=True,
                player=self.player,
            )
            img.video_id = item.get("videoId")
            img.add_css_class("song-img")
            root = self.get_root()
            img.set_compact(getattr(root, "_is_compact", False) if root else False)
            box.append(img)

            song_title = item.get("title", "Unknown")

            # Artists
            artist_list = item.get("artists", [])
            if isinstance(artist_list, list):
                artist_names = ", ".join(
                    [
                        a.get("name", "Unknown")
                        for a in artist_list
                        if isinstance(a, dict)
                    ]
                )
            else:
                artist_names = ""

            # Album
            album_name = (
                item.get("album", {}).get("name")
                if isinstance(item.get("album"), dict)
                else item.get("album")
            )
            if album_name == song_title:
                album_name = "Single"

            subtitle = artist_names
            if album_name:
                if subtitle:
                    subtitle += f" • {album_name}"
                else:
                    subtitle = album_name

            vbox = Gtk.Box(orientation=Gtk.Orientation.VERTICAL, spacing=2)
            vbox.set_valign(Gtk.Align.CENTER)
            vbox.set_hexpand(True)

            title_label = Gtk.Label(label=song_title)
            title_label.set_halign(Gtk.Align.START)
            title_label.set_ellipsize(Pango.EllipsizeMode.END)
            title_label.set_lines(1)

            subtitle_label = Gtk.Label(label=subtitle or "")
            subtitle_label.set_halign(Gtk.Align.START)
            subtitle_label.set_ellipsize(Pango.EllipsizeMode.END)
            subtitle_label.set_lines(1)
            subtitle_label.add_css_class("dim-label")
            subtitle_label.add_css_class("caption")

            title_box = Gtk.Box(orientation=Gtk.Orientation.HORIZONTAL, spacing=6)
            title_box.append(title_label)

            # Explicit Badge
            meta = parse_item_metadata(item)
            if meta["is_explicit"]:
                explicit_badge = Gtk.Label(label="E")
                explicit_badge.add_css_class("explicit-badge")
                explicit_badge.set_valign(Gtk.Align.CENTER)
                title_box.append(explicit_badge)

            vbox.append(title_box)
            vbox.append(subtitle_label)
            box.append(vbox)

            # Duration Suffix
            duration = item.get("duration") or ""
            if not duration and "duration_seconds" in item:
                ds = item["duration_seconds"]
                duration = f"{ds // 60}:{ds % 60:02d}"

            if duration:
                dur_label = Gtk.Label(label=duration)
                dur_label.add_css_class("caption")
                dur_label.set_opacity(0.7)
                dur_label.set_valign(Gtk.Align.CENTER)
                dur_label.set_margin_end(6)
                box.append(dur_label)

            # Like Button
            if item.get("videoId"):
                like_btn = LikeButton(
                    self.client, item["videoId"], item.get("likeStatus", "INDIFFERENT")
                )
                like_btn.set_valign(Gtk.Align.CENTER)
                box.append(like_btn)
                row._like_btn = like_btn

            row.item_data = item
            list_box.append(row)

            # Context Menu
            gesture = Gtk.GestureClick()
            gesture.set_button(3)
            gesture.connect(
                "pressed", lambda g, n, x, y: weak_self().on_song_right_click(g, n, x, y, g.get_widget()) if weak_self() and g.get_widget() is not None else None
            )
            row.add_controller(gesture)

            # Long Press for touch
            lp = Gtk.GestureLongPress()
            lp.connect(
                "pressed", lambda g, x, y: weak_self().on_song_right_click(g, 1, x, y, g.get_widget()) if weak_self() and g.get_widget() is not None else None
            )
            row.add_controller(lp)

        section_box.append(list_box)

        # Load More Button
        has_more_online = section_dict.get("browseId") or section_dict.get("params")
        if len(items) > limit or has_more_online:
            load_more_btn = Gtk.Button(label="Load More")
            load_more_btn.add_css_class("pill")
            load_more_btn.set_halign(Gtk.Align.CENTER)
            load_more_btn.set_margin_top(12)

            # Create a box to hold the spinner and button securely
            btn_box = Gtk.Box(orientation=Gtk.Orientation.HORIZONTAL, spacing=8)
            btn_box.set_halign(Gtk.Align.CENTER)
            btn_box.append(load_more_btn)

            spinner = Adw.Spinner()
            spinner.set_visible(False)
            btn_box.append(spinner)

            load_more_btn.connect(
                "clicked",
                lambda btn, t=title, sd=section_dict, s=spinner, lmb=load_more_btn: (
                    weak_self().on_load_more_clicked(lmb, t, sd, s, lmb) if weak_self() else None
                ),
            )
            section_box.append(btn_box)

    def on_subscribe_clicked(self, btn):
        if not self.channel_id:
            return

        new_status = not self._is_subscribed
        old_status = self._is_subscribed

        # Optimistic UI update
        self._is_subscribed = new_status
        self._update_subscribe_button()

        def thread_func():
            if new_status:
                success = self.client.subscribe_artist(self.channel_id)
            else:
                success = self.client.unsubscribe_artist(self.channel_id)

            if not success:
                print(f"Failed to toggle subscription for {self.channel_id}")

                def revert():
                    self._is_subscribed = old_status
                    self._update_subscribe_button()

                GLib.idle_add(revert)

        thread = threading.Thread(target=thread_func, daemon=True)
        thread.start()

    def _update_subscribe_button(self):
        if self._is_subscribed:
            self.subscribe_btn.set_icon_name("starred-symbolic")
            self.subscribe_btn.set_tooltip_text("Unsubscribe")
            self.subscribe_btn.add_css_class(
                "liked-button"
            )  # Consistent with LikeButton
        else:
            self.subscribe_btn.set_icon_name("non-starred-symbolic")
            self.subscribe_btn.set_tooltip_text("Subscribe")
            self.subscribe_btn.remove_css_class("liked-button")

    def add_grid_section(self, title, section_dict):
        weak_self = weakref.ref(self)
        items = section_dict.get("results", [])
        if not items:
            return

        if title in self._section_widgets:
            box = self._section_widgets[title]
            child = box.get_first_child()
            while child:
                next_child = child.get_next_sibling()
                box.remove(child)
                child = next_child
        else:
            box = Gtk.Box(orientation=Gtk.Orientation.VERTICAL, spacing=16)
            self._section_widgets[title] = box
            self.sections_box.append(box)

        label = Gtk.Label(label=title)
        label.add_css_class("heading")
        label.set_halign(Gtk.Align.START)
        box.append(label)

        from ui.widgets.scroll_box import HorizontalScrollBox

        scrolled = HorizontalScrollBox()

        inner_box = Gtk.Box(orientation=Gtk.Orientation.HORIZONTAL, spacing=16)
        scrolled.set_content(inner_box)
        box.append(scrolled)

        limit = self._section_limits.get(title, 10)
        showing_items = items[:limit]

        for item in showing_items:
            thumb_url = (
                item.get("thumbnails", [])[-1]["url"]
                if item.get("thumbnails")
                else None
            )

            item_box = Gtk.Box(orientation=Gtk.Orientation.VERTICAL, spacing=4)
            item_box.add_css_class("artist-horizontal-item")
            item_box.item_data = item
            item_box.set_cursor(Gdk.Cursor.new_from_name("pointer", None))

            img = AsyncImage(url=thumb_url, size=140, player=self.player)
            img.video_id = (
                item.get("videoId") or item.get("playlistId") or item.get("browseId")
            )

            wrapper = Gtk.Box()
            wrapper.set_overflow(Gtk.Overflow.HIDDEN)
            wrapper.add_css_class("card")
            wrapper.set_halign(Gtk.Align.CENTER)
            wrapper.append(img)

            item_box.append(wrapper)

            lbl = Gtk.Label(label=item.get("title", ""))
            lbl.set_ellipsize(Pango.EllipsizeMode.END)
            lbl.set_wrap(True)
            lbl.set_wrap_mode(Pango.WrapMode.WORD_CHAR)
            lbl.set_lines(2)
            lbl.set_justify(Gtk.Justification.LEFT)
            lbl.set_halign(Gtk.Align.START)

            text_clamp = Adw.Clamp(maximum_size=140)
            text_clamp.set_child(lbl)
            item_box.append(text_clamp)

            # Subtitle (Year / Type / Explicit)
            meta = parse_item_metadata(item)
            parts = []
            if meta["year"]:
                parts.append(meta["year"])
            if meta["type"] and meta["type"].lower() not in [p.lower() for p in parts]:
                parts.append(meta["type"])

            subtitle_text = " • ".join(parts)

            subtitle_box = Gtk.Box(orientation=Gtk.Orientation.HORIZONTAL, spacing=4)
            subtitle_box.set_halign(Gtk.Align.START)

            if meta["is_explicit"]:
                explicit_lbl = Gtk.Label(label="E")
                explicit_lbl.set_justify(Gtk.Justification.CENTER)
                explicit_lbl.set_halign(Gtk.Align.CENTER)
                explicit_lbl.add_css_class("explicit-badge")
                subtitle_box.append(explicit_lbl)

            if subtitle_text:
                subtitle_lbl = Gtk.Label(label=subtitle_text)
                subtitle_lbl.add_css_class("caption")
                subtitle_lbl.add_css_class("dim-label")
                subtitle_lbl.set_ellipsize(Pango.EllipsizeMode.END)
                subtitle_box.append(subtitle_lbl)

            if subtitle_text or meta["is_explicit"]:
                subtitle_clamp = Adw.Clamp(maximum_size=140)
                subtitle_clamp.set_child(subtitle_box)
                item_box.append(subtitle_clamp)

            inner_box.append(item_box)



            # Left Click Activation
            click_gesture = Gtk.GestureClick()
            click_gesture.set_button(1)
            click_gesture.connect(
                "pressed", lambda g, n, x, y: weak_self()._on_grid_item_pressed(g, n, x, y, g.get_widget()) if weak_self() and g.get_widget() is not None else None
            )
            click_gesture.connect(
                "released", lambda g, n, x, y: weak_self()._on_grid_item_clicked(g, n, x, y, g.get_widget()) if weak_self() and g.get_widget() is not None else None
            )
            item_box.add_controller(click_gesture)

            # Right Click Menu
            gesture = Gtk.GestureClick()
            gesture.set_button(3)
            gesture.connect(
                "released", lambda g, n, x, y: weak_self().on_grid_right_click(g, n, x, y, g.get_widget()) if weak_self() and g.get_widget() is not None else None
            )
            item_box.add_controller(gesture)

            # Long Press for touch
            lp = Gtk.GestureLongPress()
            lp.connect(
                "pressed", lambda g, x, y: weak_self().on_grid_right_click(g, 1, x, y, g.get_widget()) if weak_self() and g.get_widget() is not None else None
            )
            item_box.add_controller(lp)

        # Load More Cell
        limit = self._section_limits.get(title, 10)
        has_more_online = section_dict.get("params")
        if len(items) > limit or has_more_online:
            load_more_cell = Gtk.Box(orientation=Gtk.Orientation.VERTICAL)
            load_more_cell.set_valign(Gtk.Align.CENTER)
            load_more_cell.set_halign(Gtk.Align.CENTER)
            load_more_cell.set_margin_start(16)
            load_more_cell.set_margin_end(16)

            more_btn = Gtk.Button(label="View All")
            more_btn.add_css_class("pill")
            more_btn.set_cursor(Gdk.Cursor.new_from_name("pointer", None))

            # Click handler for Load More using the button directly
            more_btn.connect(
                "clicked",
                lambda btn, t=title, sd=section_dict: (
                    weak_self().on_load_more_clicked(btn, t, sd, None, btn) if weak_self() else None
                ),
            )

            load_more_cell.append(more_btn)
            inner_box.append(load_more_cell)

    def on_load_more_clicked(self, *args):
        # Extremely flexible signature to handle any signal (Button.clicked, Gesture.pressed, etc.)
        title = None
        section_dict = None
        spinner = None
        btn_widget = None

        # Greedy search for arguments
        for arg in args:
            if isinstance(arg, str):
                title = arg
            elif isinstance(arg, dict):
                section_dict = arg
            elif isinstance(arg, Adw.Spinner):
                spinner = arg
            elif isinstance(arg, Gtk.Widget) and not isinstance(arg, Adw.Spinner):
                btn_widget = arg

        if not title or not section_dict:
            # Fallback for very specific cases if greedy search fails
            if len(args) >= 3:
                if isinstance(args[1], str):
                    title = args[1]
                if isinstance(args[2], dict):
                    section_dict = args[2]

        if not title or not section_dict:
            return

        limit = self._section_limits.get(title, 10)
        results = section_dict.get("results", [])

        # For grid sections (Albums, Singles, Videos), navigate to DiscographyPage
        if title != "Top Songs":
            params = section_dict.get("params")
            browse_id = section_dict.get("browseId")

            if not browse_id:
                # No browseId - show all results inline
                self._section_limits[title] = len(results)
                self.update_ui(self._artist_data)
                return

            # Find the main window to call context navigation
            window = self.get_root()
            if hasattr(window, "open_discography"):
                page_title = f"{self.artist_name} - {title}"
                window.open_discography(
                    self.channel_id, page_title, browse_id, params, None
                )
            return

        # For Top Songs (which is inline and not Navigational), continue inline loading
        if len(results) > limit:
            self._section_limits[title] = limit + 20
            self.update_ui(self._artist_data)
            return

        browse_id = section_dict.get("browseId")
        if not browse_id:
            return

        if spinner:
            spinner.set_visible(True)
        if btn_widget:
            btn_widget.set_sensitive(False)

        def thread_func():
            try:
                res = self.client.get_playlist(browse_id)
                new_items = res.get("tracks", [])

                if not new_items:

                    def reset_ui():
                        if spinner:
                            spinner.set_visible(False)
                        if btn_widget:
                            btn_widget.set_sensitive(True)

                    GLib.idle_add(reset_ui)
                    return

                def update_cb():
                    if not self._artist_data:
                        if spinner:
                            spinner.set_visible(False)
                        if btn_widget:
                            btn_widget.set_sensitive(True)
                        return

                    target_category = None
                    for key in ["songs", "albums", "singles", "videos"]:
                        if key in self._artist_data and isinstance(
                            self._artist_data[key], dict
                        ):
                            s_data = self._artist_data[key]
                            if s_data.get("browseId") == browse_id and browse_id:
                                target_category = key
                                break

                    if not target_category:
                        target_category = "songs"

                    if target_category in self._artist_data:
                        self._artist_data[target_category]["results"] = new_items
                        self._artist_data[target_category].pop("params", None)
                        self._artist_data[target_category].pop("browseId", None)

                    self._section_limits[title] = len(new_items)
                    self.update_ui(self._artist_data)

                    # update_ui resets the spinner/button state naturally by rebuilding

                GLib.idle_add(update_cb)
            except Exception as e:
                print(f"Error loading more for {title}: {e}")

                def error_ui():
                    if spinner:
                        spinner.set_visible(False)
                    if btn_widget:
                        btn_widget.set_sensitive(True)

                GLib.idle_add(error_ui)

        threading.Thread(target=thread_func, daemon=True).start()

    def on_grid_right_click(self, gesture, n_press, x, y, item_box=None):
        if item_box is None:
            item_box = gesture.get_widget()
        if not hasattr(item_box, "item_data"):
            return
        data = item_box.item_data
        group = Gio.SimpleActionGroup()
        item_box.insert_action_group("item", group)

        url = None
        if "videoId" in data:
            url = f"https://music.youtube.com/watch?v={data['videoId']}"
        elif "audioPlaylistId" in data:
            url = f"https://music.youtube.com/playlist?list={data['audioPlaylistId']}"
        elif "browseId" in data:
            bid = data["browseId"]
            if bid.startswith("MPRE") or bid.startswith("OLAK"):
                url = f"https://music.youtube.com/playlist?list={bid}"
            else:
                url = f"https://music.youtube.com/browse/{bid}"
        elif "playlistId" in data:
            url = f"https://music.youtube.com/playlist?list={data['playlistId']}"

        def copy_link_action(action, param):
            if url:
                try:
                    clipboard = Gdk.Display.get_default().get_clipboard()
                    clipboard.set(url)
                except Exception:
                    pass

        # Add to Playlist Action
        def add_to_playlist_action(action, param):
            target_pid = param.get_string()
            target_vid = data.get("videoId")
            if target_pid and target_vid:

                def thread_func():
                    success = self.client.add_playlist_items(target_pid, [target_vid])
                    if success:
                        print(f"Added {target_vid} to {target_pid}")
                    else:
                        print(f"Failed to add {target_vid} to {target_pid}")

                threading.Thread(target=thread_func, daemon=True).start()

        action_add = Gio.SimpleAction.new("add_to_playlist", GLib.VariantType.new("s"))
        action_add.connect("activate", add_to_playlist_action)
        group.add_action(action_add)

        action_copy = Gio.SimpleAction.new("copy_link", None)
        action_copy.connect("activate", copy_link_action)
        group.add_action(action_copy)

        def copy_json_action(action, param):
            try:
                json_str = json.dumps(data, indent=2)
                clipboard = Gdk.Display.get_default().get_clipboard()
                clipboard.set(json_str)
            except Exception:
                pass

        action_json = Gio.SimpleAction.new("copy_json", None)
        action_json.connect("activate", copy_json_action)
        group.add_action(action_json)

        menu = Gio.Menu()
        if url:
            menu.append("Copy Link", "item.copy_link")
        menu.append("Copy JSON (Debug)", "item.copy_json")

        # Add to Playlist Submenu
        if "videoId" in data:
            playlists = self.client.get_editable_playlists()
            if playlists:
                playlist_menu = Gio.Menu()
                for p in playlists:
                    p_title = p.get("title", "Unknown")
                    p_id = p.get("playlistId")
                    if p_id:
                        playlist_menu.append(p_title, f"item.add_to_playlist('{p_id}')")
                menu.append_submenu("Add to Playlist", playlist_menu)

        if menu.get_n_items() > 0:
            popover = Gtk.PopoverMenu.new_from_model(menu)
            popover.set_parent(item_box)
            popover.set_has_arrow(False)
            rect = Gdk.Rectangle()
            rect.x = int(x)
            rect.y = int(y)
            rect.width = 1
            rect.height = 1
            popover.set_pointing_to(rect)
            popover.popup()

    def on_song_right_click(self, gesture, n_press, x, y, row):
        if not hasattr(row, "item_data"):
            return
        data = row.item_data
        group = Gio.SimpleActionGroup()
        row.insert_action_group("row", group)

        # Determine URL
        url = None
        if "videoId" in data:
            url = f"https://music.youtube.com/watch?v={data['videoId']}"

        # Copy Link Action
        def copy_link_action(action, param):
            if url:
                try:
                    clipboard = Gdk.Display.get_default().get_clipboard()
                    clipboard.set(url)
                except Exception:
                    pass

        # Add to Playlist Action
        def add_to_playlist_action(action, param):
            target_pid = param.get_string()
            target_vid = data.get("videoId")
            if target_pid and target_vid:

                def thread_func():
                    success = self.client.add_playlist_items(target_pid, [target_vid])
                    if success:
                        print(f"Added {target_vid} to {target_pid}")
                    else:
                        print(f"Failed to add {target_vid} to {target_pid}")

                threading.Thread(target=thread_func, daemon=True).start()

        action_add = Gio.SimpleAction.new("add_to_playlist", GLib.VariantType.new("s"))
        action_add.connect("activate", add_to_playlist_action)
        group.add_action(action_add)

        action_copy = Gio.SimpleAction.new("copy_link", None)
        action_copy.connect("activate", copy_link_action)
        group.add_action(action_copy)

        menu = Gio.Menu()
        if url:
            menu.append("Copy Link", "row.copy_link")

        # Add to Playlist Submenu
        if "videoId" in data:
            playlists = self.client.get_editable_playlists()
            if playlists:
                playlist_menu = Gio.Menu()
                for p in playlists:
                    p_title = p.get("title", "Unknown")
                    p_id = p.get("playlistId")
                    if p_id:
                        playlist_menu.append(p_title, f"row.add_to_playlist('{p_id}')")
                menu.append_submenu("Add to Playlist", playlist_menu)

        if menu.get_n_items() > 0:
            popover = Gtk.PopoverMenu.new_from_model(menu)
            popover.set_parent(row)
            popover.set_has_arrow(False)
            rect = Gdk.Rectangle()
            rect.x = int(x)
            rect.y = int(y)
            rect.width = 1
            rect.height = 1
            popover.set_pointing_to(rect)
            popover.popup()

    def on_song_activated(self, listbox, row):
        data = getattr(row, "item_data", None)
        if not data:
            return

        if "videoId" in data:
            # Use full results for queue to ensure all songs are included
            full_items = (
                self._artist_data.get("songs", {}).get("results", [])
                if self._artist_data
                else getattr(self, "current_songs", [])
            )
            start_index = 0
            for i, song in enumerate(full_items):
                if song.get("videoId") == data.get("videoId"):
                    start_index = i
                    break

            self.player.set_queue(self._build_queue_tracks(), start_index)
        else:
            print("No videoId in song data", data)

    def _on_grid_item_pressed(self, gesture, n_press, x, y, item_box):
        item_box._start_x = x
        item_box._start_y = y

    def _on_grid_item_clicked(self, gesture, n_press, x, y, item_box):
        if hasattr(item_box, "_start_x"):
            dx = abs(x - item_box._start_x)
            dy = abs(y - item_box._start_y)
            if dx > 10 or dy > 10:
                return
        self.on_grid_child_activated(None, item_box)

    def on_load_more_clicked_with_check(self, gesture, x, y, cell, title, section_dict):
        if hasattr(cell, "_start_x"):
            dx = abs(x - cell._start_x)
            dy = abs(y - cell._start_y)
            if dx > 10 or dy > 10:
                return
        self.on_load_more_clicked(None, title, section_dict, None, None)

    def on_grid_child_activated(self, flowbox, child):
        item_box = child.get_child() if hasattr(child, "get_child") else child
        if hasattr(item_box, "item_data"):
            data = item_box.item_data
            pid = data.get("browseId") or data.get("playlistId")
            if "videoId" in data:
                self.player.play_tracks([data])
            elif pid and pid.startswith("UC"):
                # Artist browse ID - open artist page
                root = self.get_root()
                if root and hasattr(root, "open_artist"):
                    root.open_artist(pid, data.get("title"))
            elif pid:
                self.open_playlist_callback(
                    pid,
                    {
                        "title": data.get("title"),
                        "thumb": data.get("thumbnails", [])[-1]["url"]
                        if data.get("thumbnails")
                        else None,
                        "author": self.artist_name,
                    },
                )


    def _build_queue_tracks(self):
        queue_tracks = []
        # Use full results from _artist_data to ensure all songs are added to queue
        songs_section = self._artist_data.get("songs", {}) if self._artist_data else {}
        items = songs_section.get("results", []) or getattr(self, "current_songs", [])

        for song in items:
            artist_name = ", ".join(
                [a.get("name", "") for a in song.get("artists", [])]
            )
            # Fallback for artist name if not in "artists" list
            if not artist_name:
                artist_name = self.artist_name

            thumb = (
                song.get("thumbnails", [])[-1]["url"]
                if song.get("thumbnails")
                else None
            )
            queue_tracks.append(
                {
                    "videoId": song.get("videoId"),
                    "title": song.get("title"),
                    "artist": artist_name,
                    "thumb": thumb,
                }
            )
        return queue_tracks

    def on_play_clicked(self, btn):
        if hasattr(self, "current_songs") and self.current_songs:
            self.player.set_queue(self._build_queue_tracks(), 0)

    def on_shuffle_clicked(self, btn):
        if hasattr(self, "current_songs") and self.current_songs:
            self.player.set_queue(self._build_queue_tracks(), -1, shuffle=True)

    def on_radio_clicked(self, btn):
        radio_id = getattr(self, "_radio_id", None)
        if radio_id:
            self.player.start_radio(playlist_id=radio_id)
        elif hasattr(self, "current_songs") and self.current_songs:
            # Fallback: start radio from the first song
            first_vid = self.current_songs[0].get("videoId")
            if first_vid:
                self.player.start_radio(video_id=first_vid)

    def _on_read_more_link(self, label, uri):
        GLib.idle_add(self._toggle_description)
        return True

    def _toggle_description(self):
        self._description_expanded = not self._description_expanded
        if self._description_expanded:
            self.description_label.set_label(self._description_clean)
            self.description_label.set_lines(0)
            text = "Show less"
        else:
            preview = self._description_clean[:280].rsplit(" ", 1)[0] + "…"
            self.description_label.set_label(preview)
            self.description_label.set_lines(3)
            text = "Read more"
        parent = self.read_more_btn.get_parent()
        parent.remove(self.read_more_btn)
        self.read_more_btn = Gtk.Label()
        self.read_more_btn.set_use_markup(True)
        self.read_more_btn.set_markup(f"<a href='toggle'>{text}</a>")
        self.read_more_btn.add_css_class("caption")
        self.read_more_btn.set_halign(Gtk.Align.START)
        self.read_more_btn.connect("activate-link", self._on_read_more_link)
        parent.append(self.read_more_btn)
        return False

    def on_banner_right_click(self, gesture, n_press, x, y):
        url = getattr(self.avatar, "url", None)
        if not url:
            return

        menu = Gio.Menu()
        menu.append("Copy Banner URL", "banner.copy_url")

        action = Gio.SimpleAction.new("copy_url", None)
        action.set_enabled(True)
        from ui.utils import copy_to_clipboard

        action.connect("activate", lambda *_: copy_to_clipboard(url))

        group = Gio.SimpleActionGroup()
        group.add_action(action)
        self.banner_overlay.insert_action_group("banner", group)

        popover = Gtk.PopoverMenu.new_from_model(menu)
        popover.set_parent(self.banner_overlay)
        # Point to x,y relative to gesture target
        rect = Gdk.Rectangle()
        rect.x, rect.y, rect.width, rect.height = x, y, 1, 1
        popover.set_pointing_to(rect)
        popover.popup()

    def _propagate_compact(self, widget, compact):
        if hasattr(widget, "set_compact") and hasattr(widget, "target_size"):
            widget.set_compact(compact)
        child = widget.get_first_child() if hasattr(widget, "get_first_child") else None
        while child:
            self._propagate_compact(child, compact)
            child = child.get_next_sibling()

    def set_compact_mode(self, compact):
        self._compact = compact
        self._propagate_compact(self.content_box, compact)

        if compact:
            self.add_css_class("compact")
            self.banner_overlay.set_size_request(-1, 200)
            self.banner_wrapper.set_size_request(-1, 200)
            self.info_overlay_box.set_margin_top(120)

            self.info_overlay_box.set_halign(Gtk.Align.START)
            self.banner_wrapper.set_halign(Gtk.Align.FILL)
            self.avatar.set_halign(Gtk.Align.FILL)
            self.avatar.set_hexpand(True)

            self.name_label.set_halign(Gtk.Align.START)
            self.subscribers_label.set_halign(Gtk.Align.START)
            self.description_label.set_halign(Gtk.Align.START)
            self.read_more_btn.set_halign(Gtk.Align.START)
        else:
            self.remove_css_class("compact")
            self.banner_overlay.set_size_request(-1, 260)
            self.banner_wrapper.set_size_request(-1, 260)
            self.info_overlay_box.set_margin_top(160)

            self.info_overlay_box.set_halign(Gtk.Align.START)
            self.banner_wrapper.set_halign(Gtk.Align.FILL)
            self.avatar.set_halign(Gtk.Align.FILL)
            self.avatar.set_hexpand(True)

            self.name_label.set_halign(Gtk.Align.START)
            self.subscribers_label.set_halign(Gtk.Align.START)
            self.description_label.set_halign(Gtk.Align.START)
            self.read_more_btn.set_halign(Gtk.Align.START)

    def _on_scroll(self, vadjust):
        if vadjust.get_value() > 100:
            self.emit("header-title-changed", self.artist_name)
        else:
            self.emit("header-title-changed", "")


from gi.repository import Gtk, Adw, Pango
from ui.utils import AsyncImage, parse_item_metadata, bind_weak_signal

CARD_SIZE_DEFAULT = 150
CARD_SIZE_COMPACT = 130


# Gap between columns. The cards carry 10px of padding each, so the covers
# still sit 32px apart. Every px here is a px the columns cannot use, and at
# 24 a 904px grid lost a whole column.
GRID_SPACING = 12
# Matched to the column gap. The card's own padding lands on both axes, so
# equal values here read as an even gutter in both directions.
GRID_LINE_SPACING = 12
# .artist-horizontal-item padding, both sides.
CARD_PADDING = 20
# Outer limits for a card in a grid. Absolute rather than relative to the
# base size, so crossing the compact breakpoint (base 130 to 150) cannot take
# a column count away from a row that held it a moment earlier. The grid picks
# the count landing nearest the base, so these are edges, not usual results.
CARD_SIZE_MIN = 110
CARD_SIZE_MAX = 190
# Gap between cards in a horizontal strip. Every page that builds one shares
# these so a strip on the artist page matches a strip on home.
STRIP_SPACING = 16
STRIP_SPACING_COMPACT = 8


class CardWrapLayout(Adw.WrapLayout):
    """Wrap layout that shares the row width between its card columns.

    A WrapBox packs children at their natural width, so fixed 150px cards
    leave the wrap remainder, up to a full column, as dead space on the right.
    Sizing the cards to the row keeps every row aligned, the short last one
    included, which justify-mode would not.

    It lives on the layout manager because GTK skips a widget's size_allocate
    vfunc entirely once that widget has one.
    """

    __gtype_name__ = "MuseCardWrapLayout"

    def do_measure(self, widget, orientation, for_size):
        # A layout pass runs measure(H, -1), measure(V, final width), allocate.
        # Sizing the cards here means the heights this pass reports already
        # account for the new cover size, so the frame paints once, correctly.
        # Doing it in do_allocate instead leaves the cards a frame behind the
        # window and the grid visibly flickers while the user drags.
        if orientation == Gtk.Orientation.VERTICAL:
            self._sync(widget, for_size)
        return Adw.WrapLayout.do_measure(self, widget, orientation, for_size)

    def do_allocate(self, widget, width, height, baseline):
        # Safety net for cards added after this pass measured. It is a no-op
        # whenever do_measure already sized them.
        self._sync(widget, width)
        Adw.WrapLayout.do_allocate(self, widget, width, height, baseline)

    def _column_size(self, widget, width):
        """Card size for the column count that suits this width best.

        Taking the most columns that fit and stretching them to the row was
        worse than it sounds: a window one step wider could drop from 3
        columns to 2 fat ones. Scoring each count by how far its card lands
        from the base size keeps the count rising with the width.
        """
        child = widget.get_first_child()
        if child is None or width <= 0:
            return None
        base = getattr(child, "_base_size", CARD_SIZE_DEFAULT)

        best = None
        columns = 1
        while True:
            size = (width - GRID_SPACING * (columns - 1)) // columns - CARD_PADDING
            if size < CARD_SIZE_MIN and columns > 1:
                break
            if best is None or abs(size - base) < abs(best - base):
                best = size
            columns += 1
        return max(CARD_SIZE_MIN, min(best, CARD_SIZE_MAX))

    def _sync(self, widget, width):
        size = self._column_size(widget, width)
        if size is None:
            return
        child = widget.get_first_child()
        while child is not None:
            if hasattr(child, "set_card_size"):
                child.set_card_size(size)
            child = child.get_next_sibling()


class CardBinLayout(Gtk.BinLayout):
    """Bin layout that holds a card to the width it was given.

    Measured against a known height, a wrapping title label asks for more
    width than the card's size request, and a strip with room to spare hands
    it over, pulling a short row of cards apart. A card is only ever as wide
    as the size set on it, so its natural width is its minimum.

    It lives on the layout manager because gtk_widget_measure() asks the
    layout manager and never reaches the widget's own measure vfunc.
    """

    __gtype_name__ = "MuseCardBinLayout"

    def do_measure(self, widget, orientation, for_size):
        width, _n, _mb, _nb = Gtk.BinLayout.do_measure(
            self, widget, Gtk.Orientation.HORIZONTAL, -1
        )
        if orientation == Gtk.Orientation.HORIZONTAL:
            # Measured against a set height, the title asks for the width that
            # fits its text in that many lines, which is what pulls a row of
            # cards apart. Width comes from the size request alone, so it is
            # measured unconstrained and reported as minimum and natural both.
            # Baselines are vertical-only, and GTK warns about a horizontal one.
            return width, width, -1, -1
        # Height is measured against that same width, whatever for_size says.
        # A card's height changes with the width it is asked about - a title
        # that fits on one line at the card's width takes two when the label
        # is asked at its own natural width - and a parent that measures its
        # minimum and its natural at different widths then reports a minimum
        # taller than its natural. GTK resolves that by believing the natural,
        # which leaves the row a line short and the card clipped.
        minimum, natural, min_base, nat_base = Gtk.BinLayout.do_measure(
            self, widget, orientation, width
        )
        height = max(minimum, natural)
        return height, height, min_base, nat_base


class MediaCardWidget(Gtk.Button):
    __gtype_name__ = "MuseMediaCardWidget"

    def __init__(
        self,
        item,
        player=None,
        title_lines=1,
        subtitle_text=None,
        custom_icon=None,
        fallback_icon="media-playlist-audio-symbolic",
        on_clicked=None,
        *args,
        **kwargs,
    ):
        super().__init__(*args, **kwargs)
        self.item_data = item
        self.player = player
        self._compact = False

        self.target_size = None
        self._base_size = CARD_SIZE_DEFAULT
        self._grid_size = None
        self.icon_box = None
        self._cover_icon = None

        self.set_layout_manager(CardBinLayout())

        self.add_css_class("activatable")
        self.add_css_class("artist-horizontal-item")
        self.add_css_class("flat")
        self.set_hexpand(False)
        self.set_halign(Gtk.Align.START)

        self.main_box = Gtk.Box(orientation=Gtk.Orientation.VERTICAL, spacing=4)
        self.main_box.set_size_request(CARD_SIZE_DEFAULT, -1)
        self.set_child(self.main_box)

        self.wrapper = Gtk.Box()
        self.wrapper.set_overflow(Gtk.Overflow.HIDDEN)
        self.wrapper.add_css_class("card-cover")
        self.wrapper.set_halign(Gtk.Align.START)
        self.wrapper.set_valign(Gtk.Align.START)
        self.wrapper.set_vexpand(False)

        if custom_icon:
            self.icon_box = Gtk.Box(orientation=Gtk.Orientation.VERTICAL)
            self.icon_box.add_css_class("card-download-icon")
            # The icon expands to centre itself in the box, but that expand
            # must not propagate. An expanding cover swallows the slack a
            # homogeneous grid line hands the card and drops its title below
            # the image-cover neighbours. Setting expand explicitly on
            # icon_box stops the flag climbing further.
            icon = Gtk.Image.new_from_icon_name(custom_icon)
            icon.set_hexpand(True)
            icon.set_vexpand(True)
            self.icon_box.set_hexpand(False)
            self.icon_box.set_vexpand(False)
            self.icon_box.append(icon)
            self._cover_icon = icon
            self.wrapper.append(self.icon_box)
            self._cover_img = None
        else:
            thumbnails = item.get("thumbnails", [])
            thumb_url = thumbnails[-1].get("url") if thumbnails else None

            self._cover_img = AsyncImage(url=thumb_url, width=CARD_SIZE_DEFAULT, height=CARD_SIZE_DEFAULT, player=player)
            self._cover_img.add_css_class("card-cover-img")
            self._cover_img.set_size_request(-1, -1)

            self._cover_img.video_id = (
                item.get("videoId") or item.get("playlistId") or item.get("browseId")
            )
            if not thumb_url and fallback_icon:
                self._cover_img.set_from_icon_name(fallback_icon)
            self.wrapper.append(self._cover_img)

        self.main_box.append(self.wrapper)

        title = item.get("title", "")
        self.title_label = Gtk.Label(label=title)
        self.title_label.set_halign(Gtk.Align.START)
        self.title_label.set_xalign(0.0)
        self.title_label.set_hexpand(True)
        self.title_label.set_width_chars(1)
        # Without a max, a wrapping label reports its whole text as its
        # natural width and drags the card past target_size, leaving the grid
        # with ragged columns.
        self.title_label.set_max_width_chars(1)
        self.title_label.set_ellipsize(Pango.EllipsizeMode.END)
        self.title_label.set_wrap(True)
        self.title_label.set_wrap_mode(Pango.WrapMode.WORD_CHAR)
        self.title_label.set_lines(title_lines)
        self.title_label.set_justify(Gtk.Justification.LEFT)
        self.title_label.set_tooltip_text(title)
        self.main_box.append(self.title_label)

        meta = parse_item_metadata(item)
        final_subtitle = subtitle_text if subtitle_text is not None else self._resolve_subtitle(item, meta)

        subtitle_box = Gtk.Box(orientation=Gtk.Orientation.HORIZONTAL, spacing=4)
        # FILL, not START. The capped subtitle label has a 1-char natural
        # width, so a START box shrinks to it and ellipsizes everything.
        subtitle_box.set_halign(Gtk.Align.FILL)
        subtitle_box.set_hexpand(True)

        if meta.get("is_explicit"):
            explicit_lbl = Gtk.Label(label="E")
            explicit_lbl.add_css_class("explicit-badge")
            explicit_lbl.set_valign(Gtk.Align.CENTER)
            subtitle_box.append(explicit_lbl)

        if final_subtitle:
            self.subtitle_label = Gtk.Label(label=final_subtitle)
            self.subtitle_label.add_css_class("caption")
            self.subtitle_label.add_css_class("dim-label")
            self.subtitle_label.set_ellipsize(Pango.EllipsizeMode.END)
            self.subtitle_label.set_lines(1)
            self.subtitle_label.set_width_chars(1)
            self.subtitle_label.set_max_width_chars(1)
            self.subtitle_label.set_hexpand(True)
            # FILL plus xalign 0 keeps the text left-aligned while the label
            # takes the card's full width. START allocates 1 char.
            self.subtitle_label.set_halign(Gtk.Align.FILL)
            self.subtitle_label.set_xalign(0.0)
            subtitle_box.append(self.subtitle_label)

        if final_subtitle or meta.get("is_explicit"):
            self.main_box.append(subtitle_box)

        if player and item.get("videoId"):
            self._attach_playing_state(item["videoId"])

        if on_clicked:
            self.connect("clicked", lambda btn: on_clicked(btn, self.item_data))

        # The CSS minimums are only a floor, so the cover needs its real size
        # set before the first measure, not on map.
        self._apply_size()

        self.connect("map", self._on_map)

    def set_compact_mode(self, compact: bool):
        self._compact = bool(compact)
        self._base_size = CARD_SIZE_COMPACT if compact else CARD_SIZE_DEFAULT
        # Drop the width the old layout handed out. The grid re-syncs
        # against the new base on its next allocation.
        self._grid_size = None

        if compact:
            self.add_css_class("compact")
        else:
            self.remove_css_class("compact")

        self._apply_size()

    def set_card_size(self, size):
        """Grow the card to the width its grid column offers.

        Grids call this on every allocation so the cards share the row evenly
        instead of leaving the wrap remainder as dead space on the right.
        """
        self._grid_size = size
        self._apply_size()

    def _apply_size(self):
        # A grid's figure wins outright, since it may sit just under the base
        # size so one more column fits.
        size = self._grid_size or self._base_size
        if size == self.target_size:
            return
        self.target_size = size

        self.main_box.set_size_request(size, -1)

        if self._cover_img is not None:
            self._cover_img.set_size_request(size, size)
            # GtkImage paints a paintable at its icon size, not at its
            # allocation, so without this the art stays 150px in a grown
            # wrapper.
            self._cover_img.set_pixel_size(size)
            # target_w is deliberately left alone. It feeds get_high_res_url,
            # so moving it rewrites the thumbnail URL, and both the memory and
            # disk caches are keyed by URL. Every column change would then
            # refetch every cover the next time the grid remaps.

        if self.icon_box is not None:
            self.icon_box.set_size_request(size, size)
            # 72px in the 150px CSS box. Hold that ratio as the box grows.
            self._cover_icon.set_pixel_size(round(size * 0.48))

    def set_compact(self, compact: bool):
        self.set_compact_mode(compact)

    def _is_ancestor_compact(self):
        widget = self.get_parent()
        while widget:
            if hasattr(widget, "has_css_class") and widget.has_css_class("compact"):
                return True
            widget = widget.get_parent()
        return False

    def _on_map(self, _widget):
        root = self.get_root()
        is_compact = (
            bool(getattr(root, "_is_compact", False))
            or self._is_ancestor_compact()
        )
        self.set_compact_mode(is_compact)

    def _resolve_subtitle(self, item, meta):
        parts = []
        if meta.get("year"):
            parts.append(str(meta["year"]))
        if meta.get("type") and meta.get("type").lower() not in [p.lower() for p in parts]:
            parts.append(meta.get("type"))

        if parts:
            return " • ".join(parts)
        if item.get("artists"):
            artists = item.get("artists")
            if isinstance(artists, list):
                return ", ".join([a.get("name", "") for a in artists if isinstance(a, dict)])
            return str(artists)
        return item.get("subtitle") or item.get("description") or ""

    def _attach_playing_state(self, video_id):
        def update_state(*_):
            current_id = getattr(self.player, "current_video_id", None)
            if current_id and current_id == video_id:
                self.add_css_class("playing")
                self.remove_css_class("flat")
            else:
                self.remove_css_class("playing")
                self.add_css_class("flat")

        update_state()
        bind_weak_signal(self.player, "metadata-changed", self, update_state)
        bind_weak_signal(self.player, "state-changed", self, update_state)

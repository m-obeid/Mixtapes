import os
import json
import re
from ytmusicapi import YTMusic
import ytmusicapi.navigation
from gi.repository import GLib

# Monkeypatch ytmusicapi.navigation.nav to handle UI changes like musicImmersiveHeaderRenderer
_original_nav = ytmusicapi.navigation.nav


def robust_nav(root, items, none_if_absent=False):
    if root is None:
        return None
    try:
        current = root
        for i, k in enumerate(items):
            # Fallback for musicVisualHeaderRenderer -> musicImmersiveHeaderRenderer
            if (
                k == "musicVisualHeaderRenderer"
                and isinstance(current, dict)
                and k not in current
                and "musicImmersiveHeaderRenderer" in current
            ):
                k = "musicImmersiveHeaderRenderer"
            # Fallback for musicDetailHeaderRenderer -> musicResponsiveHeaderRenderer
            if (
                k == "musicDetailHeaderRenderer"
                and isinstance(current, dict)
                and k not in current
                and "musicResponsiveHeaderRenderer" in current
            ):
                k = "musicResponsiveHeaderRenderer"
            if k == "runs" and isinstance(current, dict) and k not in current:
                if none_if_absent:
                    return None
                if i < len(items) - 1 and items[i + 1] == 0:
                    current = [{"text": ""}]
                    continue
                else:
                    current = []
                    continue

            current = current[k]
        return current
    except (KeyError, IndexError, TypeError):
        if none_if_absent:
            return None
        return _original_nav(root, items, none_if_absent)


ytmusicapi.navigation.nav = robust_nav


def _extract_shelf_title(row):
    """Pull the heading text out of a raw home/browse shelf row, mirroring
    ytmusicapi.parsers.browsing.parse_mixed_content so titles line up with
    the parsed sections."""
    if not isinstance(row, dict):
        return None
    if "musicDescriptionShelfRenderer" in row:
        try:
            return row["musicDescriptionShelfRenderer"]["header"]["runs"][0]["text"]
        except (KeyError, IndexError, TypeError):
            return None
    for shelf in row.values():
        if not isinstance(shelf, dict):
            continue
        try:
            return (
                shelf["header"]["musicCarouselShelfBasicHeaderRenderer"]
                     ["title"]["runs"][0]["text"]
            )
        except (KeyError, IndexError, TypeError):
            pass
        try:
            return (
                shelf["header"]["musicImmersiveCarouselShelfBasicHeaderRenderer"]
                     ["title"]["runs"][0]["text"]
            )
        except (KeyError, IndexError, TypeError):
            pass
    return None


def _extract_video_types(row):
    """Walk a raw home shelf row and return ``{videoId: musicVideoType}`` for
    every item that exposes one.

    ytmusicapi's mixed-content parser only keeps ``videoId`` for songs/videos
    — it drops the ``musicVideoType`` flag that YouTube uses to mark a watch
    endpoint as a song (``MUSIC_VIDEO_TYPE_ATV``) vs. a music video
    (``OMV`` / ``UGC`` / ``OFFICIAL_SOURCE_MUSIC``). We grab it back so the
    home page can render correct kind indicators instead of guessing.
    """
    out = {}
    if not isinstance(row, dict):
        return out

    def _grab(endpoint):
        if not isinstance(endpoint, dict):
            return None, None
        vid = endpoint.get("videoId")
        mc = (
            endpoint.get("watchEndpointMusicSupportedConfigs", {})
                    .get("watchEndpointMusicConfig", {})
        )
        return vid, mc.get("musicVideoType")

    def _walk_item(item):
        if not isinstance(item, dict):
            return
        for renderer_key in (
            "musicTwoRowItemRenderer",
            "musicResponsiveListItemRenderer",
        ):
            renderer = item.get(renderer_key)
            if not isinstance(renderer, dict):
                continue
            vid, vtype = _grab(
                renderer.get("navigationEndpoint", {}).get("watchEndpoint")
            )
            if not vid:
                # Row-style items put the watch endpoint on the play overlay.
                vid, vtype = _grab(
                    renderer.get("overlay", {})
                            .get("musicItemThumbnailOverlayRenderer", {})
                            .get("content", {})
                            .get("musicPlayButtonRenderer", {})
                            .get("playNavigationEndpoint", {})
                            .get("watchEndpoint")
                )
            if vid and vtype:
                out[vid] = vtype

    for shelf in row.values():
        if not isinstance(shelf, dict):
            continue
        for item in shelf.get("contents") or []:
            _walk_item(item)
    return out


def _extract_shelf_header_meta(row):
    """Pull per-shelf header extras (strapline thumbnail + text) out of a raw
    home/browse row. ytmusicapi's parser drops these. Returns a dict to merge
    into the parsed section, or None if nothing useful is present."""
    if not isinstance(row, dict):
        return None
    for shelf in row.values():
        if not isinstance(shelf, dict):
            continue
        header = shelf.get("header")
        if not isinstance(header, dict):
            continue
        basic = (
            header.get("musicCarouselShelfBasicHeaderRenderer")
            or header.get("musicImmersiveCarouselShelfBasicHeaderRenderer")
        )
        if not isinstance(basic, dict):
            continue
        out = {}
        strapline = basic.get("strapline")
        if isinstance(strapline, dict):
            runs = strapline.get("runs") or []
            text = "".join(r.get("text", "") for r in runs if isinstance(r, dict))
            if text:
                out["strapline_text"] = text
        thumb = (
            basic.get("thumbnail", {})
                 .get("musicThumbnailRenderer", {})
                 .get("thumbnail", {})
                 .get("thumbnails", [])
        )
        if thumb:
            out["strapline_thumbnail"] = thumb[-1].get("url")
        return out or None
    return None


def _ttml_time_to_seconds(value):
    """Parse a TTML time expression to seconds. Handles the formats the
    BetterLyrics endpoint emits: bare seconds ("24.111"), M:SS.sss
    ("1:03.364"), and H:MM:SS.sss. Returns ``None`` on parse failure
    so callers can degrade to unsynced lines."""
    if not value:
        return None
    try:
        parts = str(value).split(":")
        if len(parts) == 1:
            return float(parts[0])
        if len(parts) == 2:
            return int(parts[0]) * 60 + float(parts[1])
        if len(parts) == 3:
            return (
                int(parts[0]) * 3600
                + int(parts[1]) * 60
                + float(parts[2])
            )
    except (TypeError, ValueError):
        pass
    return None


def _result_rank(res):
    """Score a lyrics-fetch result so the chain can pick the richest hit.
    3 = word-level (line carries ``parts``); 2 = line-synced; 1 = plain
    text; 0 = nothing usable."""
    if not res or not res.get("lines"):
        return 0
    if any(l.get("parts") for l in res.get("lines", [])):
        return 3
    if res.get("synced"):
        return 2
    return 1


def _norm_artist_for_match(s):
    """Strip everything but alphanumerics + lowercase. Lets us compare
    artists across providers that format the same name differently
    (e.g. ``Daft Punk`` vs ``daft-punk`` vs ``Daft Punk feat. Pharrell``)."""
    if not s:
        return ""
    import re as _re
    return _re.sub(r"[^a-z0-9]", "", s.lower())


def _artist_matches(expected, found):
    """True if ``found`` plausibly belongs to the same artist as
    ``expected``. Generous so feat. credits and minor punctuation drift
    don't cost us a real match; tight enough to reject "Intro" by a
    completely unrelated artist."""
    e = _norm_artist_for_match(expected)
    f = _norm_artist_for_match(found)
    if not e or not f:
        # If either side is missing we can't verify — let the caller
        # decide. Callers that REQUIRE verification (generic-title path)
        # should reject when this returns False.
        return False
    return e in f or f in e


_GENERIC_TITLE_WORDS = {
    "intro", "outro", "interlude", "skit", "prelude", "overture",
    "untitled", "bonus", "bonustrack", "instrumental", "reprise",
    "epilogue", "prologue",
}


def _is_generic_title(title):
    """True when the title alone is too common to identify the track
    (e.g. ``Intro``, ``Track 01``). Tells the lyrics chain to refuse
    any provider result whose artist field doesn't match — otherwise
    we'd happily pick up some random album's `Intro` lyrics whenever
    LRCLIB's exact-match endpoint 404s."""
    if not title:
        return True
    import re as _re
    norm = _re.sub(r"[^a-z0-9 ]", "", title.lower()).strip()
    if not norm:
        return True
    compact = norm.replace(" ", "")
    if compact in _GENERIC_TITLE_WORDS:
        return True
    if _re.fullmatch(r"track\s*0*\d+", norm):
        return True
    return False


def _title_variants(title):
    """Return a deduplicated, order-preserving list of title strings to
    try when looking up lyrics. Catches the common YouTube Music shapes
    that lose us matches:

    - ``Original - Translation`` (often kanji/kana followed by an English
      gloss); both halves are valid lookups on lyrics DBs.
    - ``Song (feat. Artist)`` / ``Song (Remastered 2009)``; the
      parenthetical isn't part of the canonical title.
    """
    import re
    if not title:
        return []

    out = []
    def _add(t):
        t = (t or "").strip()
        if t and t not in out:
            out.append(t)

    _add(title)

    # Split on the ``-`` separator that YT Music uses for translations.
    # Real songs that legitimately contain dashes ("U-Turn", "Brain-Stew")
    # use a hyphen without surrounding spaces, so the spaced variant is a
    # safe signal for the translation pattern.
    if " - " in title:
        parts = [p.strip() for p in title.split(" - ") if p.strip()]
        # The translation is usually the latter half (e.g. ``イガク - Medicine``),
        # which is what English lyrics DBs key on; prefer it before the original.
        for p in reversed(parts):
            _add(p)

    # Strip parenthetical/bracket suffixes once at a time so we get both
    # ``Song`` and ``Song (Bonus Track)``.
    stripped = re.sub(r"\s*[\(\[][^)\]]*[\)\]]\s*$", "", title).strip()
    _add(stripped)

    return out


def _paxsenix_to_lines(data):
    """Convert a Paxsenix ``/apple-music/lyrics`` JSON response into our
    normalized ``{lines, synced, source}`` format.

    Schema:
        type     = "Syllable" (word-level), "Line" (line-level), or "None" (plain)
        content  = [{ timestamp, endtime, text: [{text, timestamp, endtime}, ...] }]
    """
    if not isinstance(data, dict):
        return None
    kind = data.get("type")
    content = data.get("content") or []
    if not content:
        return None

    lines = []
    for entry in content:
        if not isinstance(entry, dict):
            continue
        words = entry.get("text") or []
        # Each word entry: {text, timestamp, endtime}
        text_join = " ".join(
            (w.get("text") or "").strip()
            for w in words
            if isinstance(w, dict) and (w.get("text") or "").strip()
        ).strip()
        if not text_join:
            continue
        start_ms = entry.get("timestamp")
        start = float(start_ms) / 1000.0 if isinstance(start_ms, (int, float)) else None
        line = {"start": start, "text": text_join}
        end_ms = entry.get("endtime")
        if isinstance(end_ms, (int, float)):
            line["end"] = float(end_ms) / 1000.0

        # Attach word-level parts only when each word has its own timing
        # (i.e. type == "Syllable"). Skipped for "Line" / "None".
        if kind == "Syllable":
            parts = []
            for w in words:
                if not isinstance(w, dict):
                    continue
                wt = (w.get("text") or "").strip()
                if not wt:
                    continue
                ws = w.get("timestamp")
                we = w.get("endtime")
                if isinstance(ws, (int, float)):
                    parts.append({
                        "start": float(ws) / 1000.0,
                        "end": float(we) / 1000.0 if isinstance(we, (int, float)) else None,
                        "text": wt,
                    })
            if parts:
                line["parts"] = parts
        lines.append(line)

    if not lines:
        return None
    synced = all(l.get("start") is not None for l in lines)
    # type=="None" → plain text only; we already drop "synced" when starts are missing
    if kind == "None":
        # Strip timings so the chain ranks this as plain (rank 1) — Apple's
        # "no timing" tracks shouldn't preempt a line-synced source.
        for l in lines:
            l["start"] = None
        synced = False
    return {
        "lines": lines,
        "synced": synced,
        "source": "Apple Music",
    }


def _strip_leading_credits(lines):
    """Drop the production-credit lines that NetEase and a few other
    sources prepend to their LRC (e.g. ``作词 : X`` / ``Lyricist: X``).
    Only strips contiguously from the start so a credit-shaped lyric in
    the middle of the song stays put."""
    credit_keywords = (
        "作词", "作曲", "编曲", "編曲", "制作人", "製作人", "出品人",
        "混音", "母带", "監製", "监制", "演唱", "和声", "录音",
        "lyricist", "composer", "arranger", "producer", "mixed by",
    )
    out = []
    skipping = True
    for line in lines:
        text = (line.get("text") or "").strip()
        if skipping:
            text_low = text.lower()
            looks_like_credit = (":" in text or "：" in text) and any(
                kw in text_low for kw in credit_keywords
            )
            if looks_like_credit:
                continue
            skipping = False
        out.append(line)
    return out


def _parse_lrc_text(lrc_string):
    """Parse an LRC string ([mm:ss.xx] text) into our normalized line list.

    LRCLIB returns standard line-timed LRC. Each line may carry one or more
    ``[mm:ss.xx]`` timestamps followed by the lyric text; instrumental
    sections show up as empty-text timestamps which we keep so the active
    line still advances during those gaps."""
    import re

    if not lrc_string:
        return []

    timestamp_re = re.compile(r"\[(\d+):(\d{1,2})(?:[.:](\d{1,3}))?\]")
    out = []
    for raw_line in lrc_string.splitlines():
        # Collect all timestamps at the start of the line.
        starts = []
        idx = 0
        while True:
            m = timestamp_re.match(raw_line, idx)
            if not m:
                break
            minutes = int(m.group(1))
            seconds = int(m.group(2))
            frac = m.group(3)
            frac_s = float("0." + frac) if frac else 0.0
            starts.append(minutes * 60 + seconds + frac_s)
            idx = m.end()
        if not starts:
            # LRC metadata tags like [ar:...] / [ti:...] / [length:...] also
            # match the bracket pattern but aren't actual timestamps; skip
            # any line without a numeric timestamp.
            continue
        text = raw_line[idx:].strip()
        for start in starts:
            out.append({"start": start, "text": text})
    # LRC repeats can be in any order; sort by start time.
    out.sort(key=lambda l: l["start"])
    return out


def _ttml_to_lines(ttml_string):
    """Turn a BetterLyrics TTML payload into our normalized line list.

    Each line carries a start time and the full text. When the TTML has
    word-level ``<span>`` children with their own ``begin``/``end``, we
    also attach a ``parts`` list of ``{start, end, text}`` so the UI can
    do karaoke-style per-word highlighting.
    """
    import xml.etree.ElementTree as ET

    try:
        root = ET.fromstring(ttml_string)
    except ET.ParseError as e:
        print(f"[LYRICS] TTML parse failed: {e}")
        return []

    def _local(tag):
        return tag.rsplit("}", 1)[-1] if "}" in tag else tag

    lines = []
    for elem in root.iter():
        if _local(elem.tag) != "p":
            continue
        line_start = _ttml_time_to_seconds(elem.get("begin"))
        line_end = _ttml_time_to_seconds(elem.get("end"))

        # Walk direct children to preserve order. Each <span> with begin/end
        # is a word. Any text outside spans is treated as filler.
        parts = []
        text_chunks = []
        if elem.text:
            text_chunks.append(elem.text)
        for child in list(elem):
            if _local(child.tag) == "span":
                word = (child.text or "").strip()
                if word:
                    w_start = _ttml_time_to_seconds(child.get("begin"))
                    w_end = _ttml_time_to_seconds(child.get("end"))
                    parts.append({"start": w_start, "end": w_end, "text": word})
                    text_chunks.append(word)
            elif child.text:
                text_chunks.append(child.text)
            if child.tail:
                text_chunks.append(child.tail)

        text = " ".join(("".join(text_chunks)).split())
        if not text:
            continue

        line = {"start": line_start, "text": text}
        if line_end is not None:
            line["end"] = line_end
        # Only attach parts if at least one had real timing data — otherwise
        # the UI falls back to line-level rendering.
        if any(p.get("start") is not None for p in parts):
            line["parts"] = parts
        lines.append(line)
    return lines


class MusicClient:
    _instance = None

    def __new__(cls):
        if cls._instance is None:
            cls._instance = super(MusicClient, cls).__new__(cls)
            cls._instance._init()
        return cls._instance

    def _init(self):
        self.api = None
        data_dir = os.path.join(GLib.get_user_data_dir(), "muse")
        self.auth_path = os.path.join(data_dir, "headers_auth.json")
        self._is_authed = False
        self._playlist_cache = {}  # Cache fully-fetched playlists
        self._sort_metric_cache = {}  # (metric, browseId) -> {videoId: number}
        self._download_db = None  # Lazy-loaded DownloadDB for offline cache
        self._user_info = None  # Cache for account info
        self._subscribed_artists = set()  # Set of channel IDs
        self._library_playlists = []  # Cache for editable playlists
        self._library_playlist_ids = set()  # IDs of all library playlists
        self._library_album_ids = set()  # Browse IDs of all library albums
        self.try_login(skip_validation=True)

    @property
    def _offline_db(self):
        if self._download_db is None:
            try:
                from player.downloads import DownloadDB

                self._download_db = DownloadDB()
            except Exception:
                pass
        return self._download_db

    def try_login(self, skip_validation=False):
        # 1. Try saved headers_auth.json (Preferred)
        if os.path.exists(self.auth_path):
            try:
                print(f"Loading saved auth from {self.auth_path}")
                # Load headers to check/fix them before init
                with open(self.auth_path, "r") as f:
                    headers = json.load(f)

                # Normalize keys for ytmusicapi and remove Bearer tokens
                headers = self._normalize_headers(headers)

                self.api = YTMusic(auth=headers)
                if skip_validation:
                    # Assume valid for now; caller will validate asynchronously
                    print("Auth loaded (validation deferred).")
                    self._is_authed = True
                    return True
                if self.validate_session():
                    print("Authenticated via saved session.")
                    self._is_authed = True
                    return True
                else:
                    print("Saved session invalid.")
            except Exception as e:
                print(f"Failed to load saved session: {e}")

        # 2. Check for browser.json in cwd (Manually provided)
        browser_path = os.path.join(os.getcwd(), "browser.json")
        if os.path.exists(browser_path):
            print(f"Found browser.json at {browser_path}. Importing...")
            if self.login(browser_path):
                return True

        # 3. Fallback
        print("Falling back to unauthenticated mode.")
        self.api = YTMusic()
        self._is_authed = False
        return False

    def _normalize_headers(self, headers):
        """
        Ensures headers match what ytmusicapi expects for a browser session.
        Preserves Authorization (if not Bearer) and ensures required keys exist.
        """
        print("Standardizing headers for ytmusicapi...")
        normalized = {}
        for k, v in headers.items():
            lk = k.lower().replace("-", "_")

            # Whitelist standard browser headers with Title-Case
            if lk == "cookie":
                normalized["Cookie"] = v
            elif lk == "user_agent":
                normalized["User-Agent"] = v
            elif lk == "accept_language":
                normalized["Accept-Language"] = v
            elif lk == "content_type":
                normalized["Content-Type"] = v
            elif lk == "authorization":
                # Only keep if it's NOT an OAuth Bearer token
                if v.lower().startswith("bearer"):
                    print("  [Security] Dropping OAuth Bearer token.")
                else:
                    normalized["Authorization"] = v
            elif lk == "x_goog_authuser":
                normalized["X-Goog-AuthUser"] = v
            # Blacklist OAuth-triggering keys
            elif lk in [
                "oauth_credentials",
                "client_id",
                "client_secret",
                "access_token",
                "refresh_token",
                "token_type",
                "expires_at",
                "expires_in",
            ]:
                print(f"  [Security] Dropping OAuth-triggering field: {k}")
                continue
            else:
                # Title-Case other headers as a safe default
                nk = "-".join([part.capitalize() for part in k.split("-")])
                if nk.lower().startswith("x-"):
                    nk = k  # Preserve X-Goog etc. original casing
                normalized[nk] = v

        # Cleanup duplicates that might have been created by normalization
        final = {}
        for k, v in normalized.items():
            if k in [
                "Cookie",
                "User-Agent",
                "Accept-Language",
                "Content-Type",
                "Authorization",
                "X-Goog-AuthUser",
            ]:
                final[k] = v
            elif k.lower() not in [
                "cookie",
                "user-agent",
                "accept-language",
                "content-type",
                "authorization",
                "x-goog-authuser",
            ]:
                final[k] = v

        # Ensure minimal required headers for stability
        if "Accept-Language" not in final:
            final["Accept-Language"] = "en-US,en;q=0.9"
        if "Content-Type" not in final:
            final["Content-Type"] = "application/json"

        print(f"Finalized headers: {list(final.keys())}")
        return final

    def is_authenticated(self):
        return self._is_authed and self.api is not None

    def login(self, auth_input):
        """
        Robust login method for browser.json or headers dict.
        """
        try:
            headers = None
            if isinstance(auth_input, str):
                if os.path.exists(auth_input):
                    with open(auth_input, "r") as f:
                        headers = json.load(f)
                else:
                    # Try parsing as JSON string
                    try:
                        headers = json.loads(auth_input)
                    except json.JSONDecodeError:
                        # Legacy raw headers string support
                        from ytmusicapi.auth.browser import setup_browser

                        headers = json.loads(
                            setup_browser(filepath=None, headers_raw=auth_input)
                        )
            elif isinstance(auth_input, dict):
                headers = auth_input

            if not headers:
                print("Invalid auth input.")
                return False

            # CRITICAL: Enforce Headers for Stability
            # 1. Accept-Language must be English to avoid parsing errors
            headers["Accept-Language"] = "en-US,en;q=0.9"

            # 2. Ensure User-Agent is consistent/modern if missing
            if "User-Agent" not in headers:
                headers["User-Agent"] = (
                    "Mozilla/5.0 (Windows NT 10.0; Win64; x64) AppleWebKit/537.36 (KHTML, like Gecko) Chrome/115.0.0.0 Safari/537.36"
                )

            # 3. Content-Type often needed for JSON payloads
            if "Content-Type" not in headers:
                headers["Content-Type"] = "application/json; charset=UTF-8"

            # 4. Standardize headers and remove Bearer tokens
            headers = self._normalize_headers(headers)

            # Save to data/headers_auth.json (Overwrite)
            os.makedirs(os.path.dirname(self.auth_path), exist_ok=True)
            if os.path.exists(self.auth_path):
                try:
                    os.remove(self.auth_path)
                except Exception:
                    pass
            with open(self.auth_path, "w") as f:
                json.dump(headers, f)

            # Initialize API with dict directly
            print(f"Initializing YTMusic with headers: {list(headers.keys())}")
            # Drop any cached state from a previous session before the
            # new account's requests start landing — otherwise the
            # avatar menu, library indexes, and channel-handle map all
            # keep showing the old user's data.
            self._clear_account_state()
            self.api = YTMusic(auth=headers)

            # Validate
            if self.validate_session():
                self._is_authed = True
                print("Login successful and saved.")
                return True
            else:
                print("Login failed: Session invalid after init.")
                self.api = YTMusic()
                self._is_authed = False
                return False

        except Exception as e:
            import traceback

            print(f"Login exception: {e}")
            traceback.print_exc()
            self.api = YTMusic()
            self._is_authed = False
            return False

    def search(self, query, *args, **kwargs):
        if not self.api:
            return []
        return self.api.search(query, *args, **kwargs)

    def find_audio_version(self, video_id, title=None, artists=None, *_, **__):
        """Given a music-video (OMV/UGC/etc.) videoId, return a dict
        describing its audio (ATV) counterpart — the album/song
        version of the same release — with keys
        ``{"videoId", "title", "artists", "thumb"}``, or ``None`` if
        no counterpart exists or the original is already an audio
        track.

        The ``thumb`` field matters: the album-cover image lives on a
        different CDN than the music-video still, so the caller needs
        the swapped thumb to avoid showing the video frame next to
        the swapped-in audio.

        Two-step lookup:

        1. `get_watch_playlist().tracks[0].counterpart` — YT Music's
           own pairing (the data behind the "Switch to song version"
           button). Authoritative when the counterpart is ATV. We
           also harvest title/artists from this response so callers
           don't need to pass them, and so we use the same names YT
           Music uses.

        2. Songs-filtered search — fallback when YT Music didn't pair
           the song/video in its catalog (happens fairly often;
           pairing seems to be curated per release rather than
           automatic). Conservative match: normalized title equality
           + artist name in result's artists.

        Extra positional/keyword args are accepted and ignored so
        legacy call sites that passed `verify=...` don't break."""
        if not self.api or not video_id:
            return None

        cur_type = ""
        cp_id = ""
        cp_type = ""
        cp_title = ""
        cp_thumb = ""
        cp_artists = None
        try:
            result = self.api.get_watch_playlist(videoId=video_id, limit=1)
        except Exception as e:
            print(f"[swap-version] get_watch_playlist({video_id}) failed: {e}")
            result = {}
        tracks = result.get("tracks") or []
        if tracks:
            cur = tracks[0]
            cur_type = (cur.get("videoType") or "").upper()
            # Prefer YT Music's track names over what the caller passed
            # in — they're normalized for the catalog, which matches
            # better against search results below.
            if not title:
                title = cur.get("title")
            if not artists and cur.get("artists"):
                artists = cur.get("artists")
            counterpart = cur.get("counterpart")
            if isinstance(counterpart, dict):
                cp_id = counterpart.get("videoId") or ""
                cp_type = (counterpart.get("videoType") or "").upper()
                cp_title = counterpart.get("title") or "?"
                cp_artists = counterpart.get("artists")
                cp_thumb = self._best_thumb(
                    counterpart.get("thumbnail")
                    or counterpart.get("thumbnails")
                )

        print(
            f"[swap-version] cur={video_id} type={cur_type or '?'} title={title!r}"
            f" | counterpart={cp_id or '(none)'} type={cp_type or '?'} title={cp_title!r}"
        )

        if cur_type == "MUSIC_VIDEO_TYPE_ATV":
            return None
        # Only follow the counterpart link when it actually points at
        # the audio (ATV) side. Without this check, an unknown
        # `cur_type` on a track that was secretly ATV would swap us
        # *to* the music video — the opposite of what we want.
        if cp_id and cp_id != video_id and cp_type == "MUSIC_VIDEO_TYPE_ATV":
            return {
                "videoId": cp_id,
                "title": cp_title if cp_title != "?" else title,
                "artists": cp_artists or artists,
                "thumb": cp_thumb,
            }

        # Fallback: songs-filtered search. Only attempt if we know
        # the current video isn't ATV (either confirmed by
        # get_watch_playlist or unknown — both worth trying for an
        # OMV/UGC) and we have a title to match against.
        if not title:
            return None
        if cur_type and cur_type == "MUSIC_VIDEO_TYPE_ATV":
            return None  # shouldn't reach here but defensive
        return self._search_audio_version(video_id, title, artists)

    @staticmethod
    def _norm_title(s):
        """Lowercase, strip punctuation, collapse whitespace so titles
        like "Foo (feat. Bar)" and "foo  feat bar" compare equal."""
        import re
        s = (s or "").lower()
        s = re.sub(r"[^\w\s]", " ", s)
        s = re.sub(r"\s+", " ", s).strip()
        return s

    @staticmethod
    def _best_thumb(thumbs):
        """Pull the highest-resolution URL out of a ytmusicapi
        thumbnail field. Accepts the list form, the {"thumbnails": [...]}
        wrapper, or None."""
        if isinstance(thumbs, dict):
            thumbs = thumbs.get("thumbnails")
        if not isinstance(thumbs, list) or not thumbs:
            return ""
        last = thumbs[-1]
        return last.get("url", "") if isinstance(last, dict) else ""

    def _search_audio_version(self, video_id, title, artists):
        artist_name = ""
        if isinstance(artists, list) and artists:
            first = artists[0]
            artist_name = (
                first.get("name", "") if isinstance(first, dict) else str(first)
            )
        elif isinstance(artists, str):
            artist_name = artists

        query = f"{title} {artist_name}".strip()
        try:
            results = self.api.search(query, filter="songs", limit=10)
        except Exception as e:
            print(f"[swap-version] search('{query}') failed: {e}")
            return None

        title_norm = self._norm_title(title)
        artist_norm = self._norm_title(artist_name)
        for r in results:
            r_vid = r.get("videoId")
            if not r_vid or r_vid == video_id:
                continue
            r_title = r.get("title")
            if self._norm_title(r_title) != title_norm:
                continue
            r_artists = r.get("artists") or []
            r_artist_names = {
                self._norm_title(a.get("name"))
                for a in r_artists
                if isinstance(a, dict)
            }
            if artist_norm and artist_norm not in r_artist_names:
                continue
            print(
                f"[swap-version] search fallback {video_id} → {r_vid}"
                f" ({r_title!r})"
            )
            return {
                "videoId": r_vid,
                "title": r_title,
                "artists": r_artists,
                "thumb": self._best_thumb(r.get("thumbnails")),
            }
        print(
            f"[swap-version] no search match for {title!r} / {artist_name!r}"
        )
        return None

    def get_song(self, video_id):
        if not self.api:
            return None
        try:
            res = self.api.get_song(video_id)
            return res
        except Exception as e:
            print(f"Error getting song details: {e}")
            return None

    def get_history(self):
        """Return the authenticated user's listening history as a list of
        ytmusicapi track dicts. Empty list if unavailable.

        Writes through to the local cache on success so the next open
        (or an offline load) can render instantly from disk."""
        if not self.is_authenticated():
            if self._offline_db:
                cached = self._offline_db.get_cached_history()
                if cached:
                    return cached
            return []
        try:
            tracks = self.api.get_history() or []
            if tracks and self._offline_db:
                try:
                    self._offline_db.cache_history(tracks)
                except Exception as e:
                    print(f"[HISTORY] cache write failed: {e}")
            return tracks
        except Exception as e:
            print(f"Error getting history: {e}")
            if self._offline_db:
                cached = self._offline_db.get_cached_history()
                if cached:
                    return cached
            return []

    def get_cached_history(self):
        """Pull the cached history directly, bypassing the network. Used
        for the optimistic first paint of HistoryPage."""
        if self._offline_db:
            return self._offline_db.get_cached_history() or []
        return []

    def invalidate_history_cache_entry(self, video_id):
        """Remove a single track from the cached history list — used
        after an optimistic 'Remove from History' so the disk cache
        stays in sync with the server-side removal."""
        if self._offline_db and video_id:
            try:
                self._offline_db.remove_from_history_cache(video_id)
            except Exception as e:
                print(f"[HISTORY] cache invalidate failed: {e}")

    def add_history_item_async(self, video_id):
        """Fire-and-forget record of a listen to the signed-in YT Music
        account. Requires the song's `get_song` response — ytmusicapi's
        add_history_item uses the `videostatsPlaybackUrl` inside it to
        ping YT's playback tracker.

        Also optimistically prepends the track to the on-disk history
        cache so HistoryPage reflects the play before YT's server-side
        roll-up catches up."""
        if not video_id:
            print(f"[HISTORY] skip: no video id")
            return
        if not self.is_authenticated():
            print(f"[HISTORY] skip: not authenticated ({video_id})")
            return
        import threading

        def _record():
            try:
                song = self.api.get_song(video_id)
                if not song:
                    print(f"[HISTORY] get_song returned None for {video_id}")
                    return
                pb = song.get("playbackTracking") or {}
                if not pb.get("videostatsPlaybackUrl"):
                    print(
                        f"[HISTORY] {video_id} has no playbackTracking URL — "
                        f"can't add to history"
                    )
                    return
                resp = self.api.add_history_item(song)
                status = getattr(resp, "status_code", "?")
                print(
                    f"[HISTORY] add_history_item({video_id}) -> HTTP {status}"
                )
                # Server-side histories take a moment to include the new
                # entry. Prepend locally so the Verlauf-style UI reacts
                # immediately on the next open.
                try:
                    self._prepend_to_history_cache(video_id, song)
                except Exception as e:
                    print(f"[HISTORY] cache prepend failed: {e}")
            except Exception as e:
                print(f"[HISTORY] add_history_item failed for {video_id}: {e}")

        threading.Thread(target=_record, daemon=True).start()

    def _prepend_to_history_cache(self, video_id, song_data):
        """Patch the cached history with an optimistic 'Today' entry for
        the just-played track. Drops any existing entry for the same
        videoId so the track bubbles to the top instead of duplicating."""
        if not self._offline_db:
            return
        cached = self._offline_db.get_cached_history() or []

        # Build a minimal track dict from the get_song response. The
        # shape mirrors what `get_history` returns so HistoryPage can
        # render it without special-casing.
        video_details = song_data.get("videoDetails") or {}
        title = video_details.get("title", "")
        author = video_details.get("author", "")
        length_seconds = 0
        try:
            length_seconds = int(video_details.get("lengthSeconds", 0))
        except (TypeError, ValueError):
            length_seconds = 0
        thumbs = (
            video_details.get("thumbnail", {}).get("thumbnails", []) or []
        )

        new_entry = {
            "videoId": video_id,
            "title": title,
            "artists": [{"name": author, "id": video_details.get("channelId")}]
            if author else [],
            "album": {"name": "", "id": None},
            "duration": (
                f"{length_seconds // 60}:{length_seconds % 60:02d}"
                if length_seconds else ""
            ),
            "duration_seconds": length_seconds,
            "thumbnails": thumbs,
            "played": "Today",
            "likeStatus": "INDIFFERENT",
        }

        filtered = [t for t in cached if t.get("videoId") != video_id]
        filtered.insert(0, new_entry)
        try:
            self._offline_db.cache_history(filtered)
        except Exception as e:
            print(f"[HISTORY] cache_history write failed: {e}")

    def remove_history_items(self, feedback_tokens):
        """Remove history entries by their feedback tokens (each history
        item exposes one). Returns True on success."""
        if not self.is_authenticated() or not feedback_tokens:
            return False
        try:
            self.api.remove_history_items(feedback_tokens)
            return True
        except Exception as e:
            print(f"[HISTORY] remove failed: {e}")
            return False

    def get_library_playlists(self):
        if not self.is_authenticated():
            # Offline fallback
            if self._offline_db:
                cached = self._offline_db.get_cached_library_playlists()
                if cached:
                    return cached
            return []
        try:
            playlists = self.api.get_library_playlists(limit=None)
            self._library_playlists = playlists
            self._library_playlist_ids = {
                p.get("playlistId") for p in playlists if p.get("playlistId")
            }
            # Cache for offline use
            if self._offline_db and playlists:
                self._offline_db.cache_library_playlists(playlists)
            return playlists
        except Exception as e:
            print(f"Error fetching library playlists: {e}")
            # Fall back to cache
            if self._offline_db:
                cached = self._offline_db.get_cached_library_playlists()
                if cached:
                    return cached
            return []

    def get_library_albums(self, limit=100):
        if not self.is_authenticated():
            if self._offline_db:
                cached = self._offline_db.get_cached_library_albums()
                if cached:
                    return cached
            return []
        try:
            albums = self.api.get_library_albums(limit=limit)
            self._library_album_ids = set()
            for a in albums:
                if a.get("browseId"):
                    self._library_album_ids.add(a["browseId"])
                if a.get("audioPlaylistId"):
                    self._library_album_ids.add(a["audioPlaylistId"])
                if a.get("playlistId"):
                    self._library_album_ids.add(a["playlistId"])
            if self._offline_db and albums:
                self._offline_db.cache_library_albums(albums)
            return albums
        except Exception as e:
            print(f"Error fetching library albums: {e}")
            if self._offline_db:
                cached = self._offline_db.get_cached_library_albums()
                if cached:
                    return cached
            return []

    def is_in_library(self, playlist_id, on_cache_warmed=None):
        """Check if a playlist or album is saved. NEVER blocks on network.

        Previously this fetched library playlists + albums synchronously on
        first call. That ran inside update_ui on the main thread, so a cold
        playlist open hit two YT requests on the UI thread — fine online,
        but a 30+ second TCP-timeout freeze with wifi off. Now we only
        consult the in-memory cache; if it's cold, we kick off a background
        populate and (optionally) invoke ``on_cache_warmed`` on the main
        thread when it lands, so the caller can re-check and refresh UI.
        """
        if not playlist_id or not self.is_authenticated():
            return False

        if not self._library_playlist_ids and not self._library_album_ids:
            self._populate_library_cache_async(on_cache_warmed)
            return False  # optimistic; corrected when populate completes

        pid = playlist_id
        if pid.startswith("VL"):
            pid = pid[2:]
        if pid in self._library_playlist_ids:
            return True
        if pid in self._library_album_ids:
            return True
        return False

    def _populate_library_cache_async(self, on_complete=None):
        """Fill ``_library_playlist_ids`` / ``_library_album_ids`` on a
        worker thread. Coalesces overlapping calls; ``on_complete`` (if
        given) fires on the main thread once the cache is warm."""
        import threading as _t
        if not hasattr(self, "_library_populate_lock"):
            self._library_populate_lock = _t.Lock()
            self._library_populate_in_flight = False
            self._library_populate_callbacks = []

        with self._library_populate_lock:
            if on_complete is not None:
                self._library_populate_callbacks.append(on_complete)
            if self._library_populate_in_flight:
                return
            self._library_populate_in_flight = True

        def _populate():
            try:
                playlists = self.api.get_library_playlists()
                self._library_playlists = playlists
                self._library_playlist_ids = {
                    p.get("playlistId") for p in playlists if p.get("playlistId")
                }
            except Exception:
                pass
            try:
                albums = self.api.get_library_albums(limit=100)
                ids = set()
                for a in albums:
                    if a.get("browseId"):
                        ids.add(a["browseId"])
                    if a.get("audioPlaylistId"):
                        ids.add(a["audioPlaylistId"])
                    if a.get("playlistId"):
                        ids.add(a["playlistId"])
                self._library_album_ids = ids
            except Exception:
                pass

            with self._library_populate_lock:
                callbacks = list(self._library_populate_callbacks)
                self._library_populate_callbacks.clear()
                self._library_populate_in_flight = False

            if callbacks:
                try:
                    from gi.repository import GLib
                    for cb in callbacks:
                        GLib.idle_add(cb)
                except Exception:
                    pass

        _t.Thread(target=_populate, daemon=True).start()

    def rate_playlist(self, playlist_id, rating="LIKE"):
        """Rate a playlist/album: 'LIKE' to save, 'INDIFFERENT' to remove from library.
        Strips VL prefix and converts MPRE browse IDs to playlist IDs automatically."""
        if not self.is_authenticated():
            return False
        try:
            # Strip VL prefix (browse ID → playlist ID)
            pid = playlist_id
            if pid.startswith("VL"):
                pid = pid[2:]
            # Convert MPRE browse ID to audio playlist ID
            if pid.startswith("MPRE"):
                try:
                    album_data = self.api.get_album(pid)
                    pid = album_data.get("audioPlaylistId", pid)
                except Exception:
                    pass
            self.api.rate_playlist(pid, rating)
            return True
        except Exception as e:
            print(f"Error rating playlist: {e}")
            return False

    def edit_song_library_status(self, feedback_tokens):
        """Add/remove songs from library using feedback tokens."""
        if not self.is_authenticated():
            return False
        try:
            self.api.edit_song_library_status(feedback_tokens)
            return True
        except Exception as e:
            print(f"Error editing song library status: {e}")
            return False

    def get_library_upload_songs(self, limit=100, order=None):
        if not self.is_authenticated():
            return []
        try:
            return self.api.get_library_upload_songs(limit=limit, order=order)
        except Exception as e:
            print(f"Error fetching uploaded songs: {e}")
            return []

    def get_library_upload_albums(self, limit=100, order=None):
        if not self.is_authenticated():
            return []
        try:
            return self.api.get_library_upload_albums(limit=limit, order=order)
        except Exception as e:
            print(f"Error fetching uploaded albums: {e}")
            return []

    def get_library_upload_artists(self, limit=100, order=None):
        if not self.is_authenticated():
            return []
        try:
            return self.api.get_library_upload_artists(limit=limit, order=order)
        except Exception as e:
            print(f"Error fetching uploaded artists: {e}")
            return []

    def upload_song(self, filepath):
        """Upload a song file (mp3, m4a, wma, flac, ogg) to YouTube Music."""
        if not self.is_authenticated():
            return None
        try:
            return self.api.upload_song(filepath)
        except Exception as e:
            print(f"Error uploading song: {e}")
            return None

    def delete_upload_entity(self, entity_id):
        """Delete a previously uploaded song or album."""
        if not self.is_authenticated():
            return False
        try:
            self.api.delete_upload_entity(entity_id)
            return True
        except Exception as e:
            print(f"Error deleting upload: {e}")
            return False

    def get_library_upload_album(self, browse_id):
        """Get tracks for an uploaded album."""
        if not self.is_authenticated():
            return None
        try:
            return self.api.get_library_upload_album(browse_id)
        except Exception as e:
            print(f"Error fetching upload album: {e}")
            return None

    def get_library_upload_artist(self, browse_id, limit=100):
        """Get uploaded songs by a specific artist."""
        if not self.is_authenticated():
            return []
        try:
            return self.api.get_library_upload_artist(browse_id, limit=limit)
        except Exception as e:
            print(f"Error fetching upload artist songs: {e}")
            return []

    def get_library_subscriptions(self, limit=None):
        if not self.is_authenticated():
            if self._offline_db:
                cached = self._offline_db.get_cached_library_artists()
                if cached:
                    return cached
            return []
        try:
            subs = self.api.get_library_subscriptions(limit=limit)
            if subs:
                for s in subs:
                    bid = s.get("browseId")
                    if bid:
                        self._subscribed_artists.add(bid)
                if self._offline_db:
                    self._offline_db.cache_library_artists(subs)
            return subs
        except Exception as e:
            print(f"Error fetching library subscriptions: {e}")
            if self._offline_db:
                cached = self._offline_db.get_cached_library_artists()
                if cached:
                    return cached
            return []

    def get_account_info(self):
        """
        Fetches the current user's account info. Caches the result.
        """
        if not self.is_authenticated():
            return None
        if self._user_info:
            return self._user_info

        try:
            self._user_info = self.api.get_account_info()
            return self._user_info
        except Exception as e:
            print(f"Error fetching account info: {e}")
            return None

    def resolve_channel_handle(self, handle):
        """Turn a YT Music @handle into an artist browseId (UC…) via
        the internal `navigation/resolve_url` endpoint. Returns the
        browseId string, or None. Results are cached on the client
        instance to avoid repeated lookups."""
        if not handle or not self.api:
            return None
        if not hasattr(self, "_channel_handle_cache"):
            self._channel_handle_cache = {}
        if handle in self._channel_handle_cache:
            return self._channel_handle_cache[handle]

        h = handle.lstrip("@")
        try:
            resp = self.api._send_request(
                "navigation/resolve_url",
                {"url": f"https://music.youtube.com/@{h}"},
            )
            # The response's `endpoint.browseEndpoint.browseId` is the
            # channel id; dig defensively, the key nesting has shifted
            # between YT internal revisions.
            ep = (resp or {}).get("endpoint") or {}
            browse = ep.get("browseEndpoint") or ep.get("browse") or {}
            bid = browse.get("browseId")
            if bid:
                self._channel_handle_cache[handle] = bid
                return bid
        except Exception as e:
            print(f"[CLIENT] resolve_channel_handle({handle}) failed: {e}")
        return None

    def is_own_playlist(self, playlist_metadata, playlist_id=None):
        """
        Determines if a playlist is owned/editable by the current user.
        Excludes collaborative playlists where the user is only a collaborator.
        """
        if not self.is_authenticated():
            return False

        pid = (
            playlist_id
            or playlist_metadata.get("id")
            or playlist_metadata.get("playlistId")
            or ""
        )

        # 1. Liked Music and special system playlists are NOT owned
        if pid in ["LM", "SE", "VLLM"]:
            return False

        # 2. Strict prefix check: must start with PL or VL
        if not pid.startswith("PL") and not pid.startswith("VL"):
            return False

        author = playlist_metadata.get("author")

        if not author and not playlist_metadata.get("collaborators"):
            return True
        elif playlist_metadata.get("collaborators"):
            author = playlist_metadata.get("collaborators", {}).get("text", "")
        else:
            # Handle list or dict for author
            if isinstance(author, list) and len(author) > 0:
                author = author[0].get("name", "")
            elif isinstance(author, dict):
                author = author.get("name", "")
            else:
                author = str(author)

        user_info = self.get_account_info()
        user_name = user_info.get("accountName", "") if user_info else ""

        # If it contains user's name and is collaborators, it is owned
        if user_name and user_name in author and playlist_metadata.get("collaborators"):
            return True

        # If it matches the user's name, it is owned
        if author == user_name:
            return True

        return False

    def get_playlist(self, playlist_id, limit=None):
        if not self.api:
            # Offline: try cache
            if self._offline_db:
                cached = self._offline_db.get_cached_playlist(playlist_id)
                if cached:
                    return cached
            return None
        try:
            result = self.api.get_playlist(playlist_id, limit=limit)
            # Cache the full result for offline use
            if result and self._offline_db:
                raw_author = result.get("author", "")
                # ytmusicapi returns author as a dict {"name": ..., "id": ...}
                # for some playlists; str()-ing it produces "{'name': ...}"
                # which later gets dumped into the UI as literal JSON-looking
                # text. Normalize to the display name only.
                if isinstance(raw_author, dict):
                    author_str = raw_author.get("name", "")
                elif isinstance(raw_author, list):
                    author_str = ", ".join(
                        a.get("name", "") for a in raw_author if isinstance(a, dict)
                    )
                else:
                    author_str = str(raw_author or "")

                # modify title and description field to a hardcoded value for Liked Music playlist
                # this modification with a hardcoded value used to exist in `_fetch_playlist_details`
                # we move it down to the get_playlist level so that the cache and display value doesnt mismatch
                if playlist_id == "LM":
                    if "title" in result:
                        result["title"] = "Your Likes"
                    if "description" in result:
                        result["description"] = "Your liked songs from YouTube Music."

                # Store the rich metadata so PlaylistPage can re-render the
                # full header (year, privacy, author-as-link, etc.) on the
                # next open before the live fetch completes.
                meta = {
                    "description": result.get("description", "") or "",
                    "year": result.get("year", "") or "",
                    "privacy": result.get("privacy", "") or "",
                    "duration_seconds": result.get("duration_seconds"),
                    "thumbnails": result.get("thumbnails", []) or [],
                    "author_raw": raw_author if isinstance(raw_author, (dict, list)) else None,
                }
                # Defer the cache write off the critical path. It's a
                # ``json.dumps(tracks)`` over ~1000 dicts (200-500 KB),
                # plus a second ``json.loads`` for the regression check —
                # all running under the GIL and visibly stalling the UI
                # thread during playlist open (~100-200 ms blocked frames).
                # The cache is a next-time optimization, not a correctness
                # requirement; it can wait a couple of seconds for the
                # page to finish rendering.
                self._schedule_playlist_cache_write(
                    playlist_id,
                    result.get("title", ""),
                    author_str,
                    result.get("trackCount", len(result.get("tracks", []))),
                    result.get("tracks", []),
                    meta,
                )
            return result
        except Exception as e:
            print(f"Error fetching playlist: {e}")
            if self._offline_db:
                cached = self._offline_db.get_cached_playlist(playlist_id)
                if cached:
                    return cached
            return None

    def get_watch_playlist(
        self, video_id=None, playlist_id=None, limit=25, radio=False
    ):
        if not self.api:
            return {}
        try:
            res = self.api.get_watch_playlist(
                videoId=video_id, playlistId=playlist_id, limit=limit, radio=radio
            )
            return res
        except Exception as e:
            print(f"Error getting watch playlist: {e}")
            return {}

    # Provider catalog — display name -> (fetcher, takes_video_id_only).
    # Listed in the order we try them in the chain.
    _LYRIC_PROVIDERS = [
        ("Apple Music", "_fetch_lyrics_paxsenix", False),
        ("BetterLyrics",           "_fetch_lyrics_betterlyrics", False),
        ("BiniLyrics",             "_fetch_lyrics_binilyrics", False),
        ("NetEase",                "_fetch_lyrics_netease", False),
        ("LRCLIB",                 "_fetch_lyrics_lrclib", False),
        ("YouTube Music",          "_fetch_lyrics_ytm", True),
    ]

    @property
    def _lyrics_cache(self):
        cache = getattr(self, "_lyrics_cache_inst", None)
        if cache is None:
            from player.lyrics_cache import LyricsCache
            cache = LyricsCache()
            self._lyrics_cache_inst = cache
        return cache

    def get_lyrics(self, video_id, title=None, artist=None, duration=None):
        """Return the lyrics for a track as a normalized dict, or
        ``None`` if nothing was found. Reads the disk cache first
        (instant), then runs the provider chain on miss and caches what
        it finds. The user can pin a particular provider for a track
        via :meth:`set_preferred_lyrics_source`; when a preference
        exists the cache returns that instead of the ranking-chosen
        best.

            {
                "lines": [{"start": float_seconds_or_None, "text": str,
                           "parts": [{...}]?}, ...],
                "synced": bool,   # True iff every line has a real start time
                "source": str,    # human-readable provider name
            }
        """
        if not video_id:
            return None

        cached = self._lyrics_cache.get_result(video_id)
        if cached:
            return cached

        result = self._run_lyrics_chain(video_id, title, artist, duration)
        if result:
            self._lyrics_cache.add_result(video_id, result)
        return result

    def get_lyrics_alternatives(self, video_id):
        """Return ``[(source_name, result), ...]`` for every provider we
        already have cached for this video. Ordered richest-first. Use
        :meth:`fetch_lyrics_alternatives_async` to populate uncached
        providers."""
        return self._lyrics_cache.get_alternatives(video_id)

    def get_preferred_lyrics_source(self, video_id):
        """The user-pinned provider for this track, or ``None`` if none."""
        return self._lyrics_cache.get_preferred(video_id)

    def set_preferred_lyrics_source(self, video_id, source):
        """Pin ``source`` (one of the provider display names) as the
        result the next :meth:`get_lyrics` call should return for this
        track. Pass ``None`` to clear the preference."""
        self._lyrics_cache.set_preferred(video_id, source)

    def fetch_lyrics_alternatives_async(
        self, video_id, title, artist, duration, on_result,
    ):
        """Fire every provider in parallel (on background threads) and
        call ``on_result(source_name, result_or_None)`` for each as it
        completes. Cache hits emit synchronously before the function
        returns. ``on_result`` is called from worker threads — wrap any
        UI work in ``GLib.idle_add``."""
        import threading

        if not video_id:
            return
        title_variants = _title_variants(title) if title else []

        # Emit anything we already have cached up-front.
        for src, res in self._lyrics_cache.get_alternatives(video_id):
            on_result(src, res)

        cached_sources = {
            src for src, _ in self._lyrics_cache.get_alternatives(video_id)
        }

        def _runner(source_name, fetcher_attr, takes_vid_only):
            try:
                fetcher = getattr(self, fetcher_attr)
                if takes_vid_only:
                    res = fetcher(video_id)
                else:
                    res = None
                    for variant in title_variants:
                        res = fetcher(variant, artist, duration)
                        if res:
                            break
                if res:
                    # Make sure the result's source label matches the
                    # display name, since some fetchers carry a more
                    # specific string (e.g. "BetterLyrics").
                    res = dict(res)
                    res["source"] = source_name
                    self._lyrics_cache.add_result(video_id, res)
            except Exception as e:
                print(f"[LYRICS] alt-fetch {source_name} failed: {e}")
                res = None
            on_result(source_name, res)

        for source_name, fetcher_attr, takes_vid_only in self._LYRIC_PROVIDERS:
            if source_name in cached_sources:
                continue
            if not self._lyrics_provider_ready(fetcher_attr):
                continue
            t = threading.Thread(
                target=_runner,
                args=(source_name, fetcher_attr, takes_vid_only),
                daemon=True,
            )
            t.start()

    def _lyrics_provider_ready(self, key):
        """False while a provider is in a back-off window. A source that
        just returned HTTP 429 or kept timing out is skipped for all tracks
        until the window expires, instead of being re-hit on every track
        change (which only deepens the rate-limit / timeout spiral)."""
        import time

        cooldowns = getattr(self, "_lyrics_cooldowns", None)
        if not cooldowns:
            return True
        return time.monotonic() >= cooldowns.get(key, 0)

    def _trip_lyrics_cooldown(self, key, seconds, reason=""):
        """Stop calling a lyrics provider for ``seconds`` after it signals
        it's overloaded (HTTP 429) or unreachable (timeouts)."""
        import time

        cooldowns = getattr(self, "_lyrics_cooldowns", None)
        if cooldowns is None:
            cooldowns = {}
            self._lyrics_cooldowns = cooldowns
        deadline = time.monotonic() + seconds
        # Never let a fresh short cooldown shorten a longer active one.
        if deadline > cooldowns.get(key, 0):
            cooldowns[key] = deadline
            print(f"[LYRICS] backing off {key} for {seconds}s ({reason})")

    def _run_lyrics_chain(self, video_id, title, artist, duration):
        """Walk the provider chain in priority order, returning the
        richest result we can find. Word-level providers go first;
        line-synced next; plain text last. ``rank`` encodes the order:
        3=word, 2=line-synced, 1=plain.

        YouTube Music titles for international tracks frequently arrive
        as ``Original - Translation`` (e.g. ``イガク - Medicine``) or
        carry ``(feat. X)`` / ``(Remastered)`` suffixes that don't match
        the canonical title in lyrics DBs. We try a few normalized
        variants so a single source dropout doesn't lose us the song.
        """
        title_variants = _title_variants(title) if title else []

        result = None
        best_rank = 0

        for fetcher, args_list, target_rank in [
            (self._fetch_lyrics_paxsenix,
             [(v, artist, duration) for v in title_variants], 3),
            (self._fetch_lyrics_betterlyrics,
             [(v, artist, duration) for v in title_variants], 3),
            (self._fetch_lyrics_binilyrics,
             [(v, artist, duration) for v in title_variants], 3),
            (self._fetch_lyrics_netease,
             [(v, artist, duration) for v in title_variants], 2),
            (self._fetch_lyrics_lrclib,
             [(v, artist, duration) for v in title_variants], 2),
            (self._fetch_lyrics_ytm, [(video_id,)], 1),
        ]:
            if best_rank >= target_rank:
                continue
            if not self._lyrics_provider_ready(fetcher.__name__):
                continue
            for args in args_list:
                res = fetcher(*args)
                rank = _result_rank(res)
                if rank > best_rank:
                    result = res
                    best_rank = rank
                if best_rank >= target_rank:
                    break

        return result

    def _fetch_lyrics_binilyrics(self, title, artist, duration):
        """Query BiniLyrics (https://lyrics-api.binimum.org/) — an open
        TTML database used by the BetterLyrics extension that has good
        word-level coverage for English/Western pop and Japanese tracks.

        The endpoint takes a free-text query, returns a list of matches
        with ISRC + timing_type, and a ``lyricsUrl`` pointing at TTML on
        their static-storage subdomain."""
        import urllib.parse
        import urllib.request
        import urllib.error
        import json as _json

        headers = {"User-Agent": "BetterLyrics/1.0"}

        # Search by "title artist" — exact match isn't required, the
        # backend does fuzzy lookup.
        query = title.strip() + (" " + artist.strip() if artist else "")
        search_url = (
            "https://lyrics-api.binimum.org/getLyrics?"
            + urllib.parse.urlencode({"q": query})
        )
        try:
            req = urllib.request.Request(search_url, headers=headers)
            with urllib.request.urlopen(req, timeout=6) as resp:
                body = resp.read().decode("utf-8", errors="replace")
        except urllib.error.HTTPError as e:
            if e.code != 404:
                print(f"[LYRICS] BiniLyrics HTTP {e.code}: {e.reason}")
            return None
        except Exception as e:
            print(f"[LYRICS] BiniLyrics search failed: {e}")
            return None

        try:
            data = _json.loads(body)
        except _json.JSONDecodeError:
            return None
        results = (data.get("results") if isinstance(data, dict) else None) or []
        if not results:
            return None

        # Hard-filter to artist matches before scoring — keeps "Intro" from
        # one album from claiming to be lyrics for an entirely unrelated
        # track that happens to share the name.
        if artist:
            artist_filtered = [
                r for r in results
                if _artist_matches(artist, r.get("artist_name") or "")
            ]
            if artist_filtered:
                results = artist_filtered
            elif _is_generic_title(title):
                return None

        # Score: prefer results with word-level timing, then by duration
        # closeness (within 5 s).
        def _score(item):
            r = 0
            if item.get("timing_type") in ("word", "syllable"):
                r += 100
            d = item.get("duration") or 0
            if duration and d:
                r -= min(abs(int(duration) - int(d)), 30)
            return -r
        results.sort(key=_score)
        best = results[0]

        lyrics_url = best.get("lyricsUrl")
        if not lyrics_url:
            return None
        try:
            req = urllib.request.Request(lyrics_url, headers=headers)
            with urllib.request.urlopen(req, timeout=6) as resp:
                ttml = resp.read().decode("utf-8", errors="replace")
        except Exception as e:
            print(f"[LYRICS] BiniLyrics ttml fetch failed: {e}")
            return None

        lines = _ttml_to_lines(ttml)
        if not lines:
            return None
        synced = all(l.get("start") is not None for l in lines)
        # If the search promised word-level but the parsed TTML doesn't
        # have any parts, fall through — line-synced LRCLIB will likely
        # be a better match.
        has_parts = any(l.get("parts") for l in lines)
        if best.get("timing_type") in ("word", "syllable") and not has_parts:
            return None
        if not synced and not has_parts:
            # Unsynced plain TTML — let the line-synced providers try first;
            # we'll only fall back to this if everything else misses.
            return {"lines": lines, "synced": False, "source": "BiniLyrics"}
        return {"lines": lines, "synced": synced, "source": "BiniLyrics"}

    def _fetch_lyrics_netease(self, title, artist, duration):
        """Query NetEase Music (music.163.com) — by far the broadest
        source for Japanese, Vocaloid, K-pop, and other Asian tracks.
        Uses the unencrypted ``cloudsearch/pc`` search endpoint and the
        ``song/lyric`` lyric endpoint, both of which return plain JSON
        with no auth required."""
        import urllib.parse
        import urllib.request
        import urllib.error
        import json as _json

        headers = {
            "User-Agent": "Mozilla/5.0",
            "Referer": "https://music.163.com/",
        }

        # 1. Search for the track. The `cloudsearch/pc` variant returns
        # unencrypted JSON (unlike the regular `cloudsearch/get/web` one).
        query = title.strip() + (" " + artist.strip() if artist else "")
        try:
            search_url = (
                "https://music.163.com/api/cloudsearch/pc?"
                + urllib.parse.urlencode({
                    "s": query, "type": 1, "limit": 8, "offset": 0,
                })
            )
            req = urllib.request.Request(search_url, headers=headers)
            with urllib.request.urlopen(req, timeout=5) as resp:
                search_body = resp.read().decode("utf-8", errors="replace")
        except Exception as e:
            print(f"[LYRICS] NetEase search failed: {e}")
            return None

        try:
            search_data = _json.loads(search_body)
        except _json.JSONDecodeError:
            return None
        songs = (
            (search_data.get("result") or {}).get("songs") or []
            if isinstance(search_data, dict) else []
        )
        if not songs:
            return None

        # 2. Hard-filter to artist matches first. For generic titles like
        # "Intro" the closest-duration tiebreaker is a coin toss across
        # whoever else has an Intro track on NetEase — artist mismatch
        # has to disqualify the result, not just dock its score.
        if artist:
            artist_filtered = []
            for song in songs:
                song_artists = " ".join(
                    (a.get("name") or "")
                    for a in (song.get("ar") or [])
                )
                if _artist_matches(artist, song_artists):
                    artist_filtered.append(song)
            if artist_filtered:
                songs = artist_filtered
            elif _is_generic_title(title):
                # Generic title + no artist match = certain false positive.
                # Better to return nothing than wrong lyrics.
                return None

        # 3. Pick the best match: closest duration wins, with a small
        # bonus for exact-title match.
        title_low = title.lower().strip()

        def _score(song):
            score = 0
            # NetEase returns duration in ms (the `dt` field).
            d_ms = song.get("dt") or 0
            if duration and d_ms:
                delta = abs(int(duration) - (d_ms // 1000))
                score -= min(delta, 30)
            name = (song.get("name") or "").lower()
            if title_low and (title_low in name or name in title_low):
                score += 5
            return -score
        songs.sort(key=_score)
        best = songs[0]
        song_id = best.get("id")
        if not song_id:
            return None

        # 3. Fetch the lyric. NetEase returns an LRC string in
        # `result.lrc.lyric` — exactly the format our LRC parser
        # already understands.
        try:
            lyric_url = (
                f"https://music.163.com/api/song/lyric?id={song_id}"
                "&lv=1&kv=1&tv=-1"
            )
            req = urllib.request.Request(lyric_url, headers=headers)
            with urllib.request.urlopen(req, timeout=5) as resp:
                lyric_body = resp.read().decode("utf-8", errors="replace")
        except Exception as e:
            print(f"[LYRICS] NetEase lyric fetch failed: {e}")
            return None

        try:
            lyric_data = _json.loads(lyric_body)
        except _json.JSONDecodeError:
            return None
        lrc = ((lyric_data.get("lrc") or {}).get("lyric") or "").strip() \
            if isinstance(lyric_data, dict) else ""
        if not lrc:
            return None

        lines = _parse_lrc_text(lrc)
        if not lines:
            return None
        # NetEase usually pads the start with credit metadata (e.g.
        # ``[00:00.000] 作词 : XXX``, ``[00:01.000] 作曲 : XXX``). Strip
        # those leading credit lines so the lyrics column doesn't open on
        # production notes instead of the actual song.
        lines = _strip_leading_credits(lines)
        if not lines:
            return None
        synced = all(l.get("start") is not None for l in lines)
        return {"lines": lines, "synced": synced, "source": "NetEase"}

    def _fetch_lyrics_lrclib(self, title, artist, duration):
        """Query LRCLIB (https://lrclib.net) for synced LRC. Tries the exact
        ``/get`` endpoint first (matches by title + artist + duration ±2 s);
        if that misses, falls back to ``/get`` without duration and then to
        ``/search`` so a minor metadata mismatch (e.g. a (Remastered) suffix
        or a duration off by a few seconds) doesn't lose us the lyrics."""
        import urllib.parse
        import urllib.request
        import urllib.error
        import json as _json

        headers = {"User-Agent": "Mixtapes (https://github.com/m-obeid/Mixtapes)"}

        def _hit(path, params):
            # Once a 429/timeout has tripped the cooldown, skip the remaining
            # /get and /search probes (and any later title variants) instead
            # of firing them into a server that just told us to back off.
            if not self._lyrics_provider_ready("_fetch_lyrics_lrclib"):
                return None
            url = "https://lrclib.net/api/" + path + "?" + urllib.parse.urlencode(params)
            try:
                req = urllib.request.Request(url, headers=headers)
                # 3 s is enough for a successful hit; capping low here keeps
                # the worst case at a few seconds when the chain has to try
                # multiple title variants and the API is sluggish.
                with urllib.request.urlopen(req, timeout=3) as resp:
                    return _json.loads(resp.read())
            except urllib.error.HTTPError as e:
                if e.code == 429:
                    self._trip_lyrics_cooldown(
                        "_fetch_lyrics_lrclib", 120, "HTTP 429"
                    )
                elif e.code != 404:
                    print(f"[LYRICS] LRCLIB HTTP {e.code}: {e.reason} for {path}")
                return None
            except Exception as e:
                if "timed out" in str(e).lower():
                    self._trip_lyrics_cooldown(
                        "_fetch_lyrics_lrclib", 60, "timeout"
                    )
                print(f"[LYRICS] LRCLIB fetch failed: {e}")
                return None

        candidates = []
        base = {"track_name": title, "artist_name": artist}
        if duration and duration > 0:
            data = _hit("get", {**base, "duration": int(duration)})
            if isinstance(data, dict):
                candidates.append(data)
        data = _hit("get", base)
        if isinstance(data, dict):
            candidates.append(data)
        if not candidates:
            results = _hit("search", base)
            if isinstance(results, list) and results:
                # Filter to results whose artist matches ours. Without this
                # an unfilled exact-match request followed by /search would
                # happily return some random album's "Intro" lyrics for any
                # track called "Intro" — the closest-duration tiebreaker
                # gives the wrong answer immediately.
                if artist:
                    results = [
                        r for r in results
                        if _artist_matches(artist, r.get("artistName") or "")
                    ]
                if results:
                    def _score(item):
                        d = item.get("duration") or 0
                        return abs((duration or 0) - d) if duration else 0
                    results.sort(key=_score)
                    candidates.append(results[0])

        for cand in candidates:
            synced = cand.get("syncedLyrics")
            plain = cand.get("plainLyrics")
            if synced and isinstance(synced, str) and synced.strip():
                lines = _parse_lrc_text(synced)
                if lines:
                    return {
                        "lines": lines,
                        "synced": all(l.get("start") is not None for l in lines),
                        "source": "LRCLIB",
                    }
            if plain and isinstance(plain, str) and plain.strip():
                lines = [
                    {"start": None, "text": ln.strip()}
                    for ln in plain.splitlines()
                    if ln.strip()
                ]
                if lines:
                    return {
                        "lines": lines,
                        "synced": False,
                        "source": "LRCLIB",
                    }
        return None

    def _fetch_lyrics_paxsenix(self, title, artist, duration):
        """Query the Paxsenix proxy (``lyrics.paxsenix.org``) which
        re-serves Apple Music's lyric database. Apple Music ships
        syllable-level timing for an enormous chunk of Western pop, and
        Paxsenix's ``/apple-music/lyrics`` endpoint returns it as plain
        JSON keyed by Apple Music's catalog song ID.

        We have to scrape an Apple Music developer token from the Apple
        Music web app (the same trick Metrolist uses) to do the catalog
        search. The token is cached process-wide and re-fetched only
        when Apple Music returns 401."""
        import urllib.parse
        import urllib.request
        import urllib.error
        import json as _json

        # 1. Apple Music search for matching tracks.
        token = self._apple_music_token(force_new=False)
        songs = self._apple_music_search(token, title, artist) if token else None
        if songs is None and token:
            # The token might have expired; try once with a fresh one.
            token = self._apple_music_token(force_new=True)
            songs = self._apple_music_search(token, title, artist) if token else None
        if not songs:
            return None

        # 2. Hard-filter to artist matches. Generic-title tracks would
        # otherwise pick up Apple Music's "Intro" by anyone with the
        # closest duration.
        if artist:
            artist_filtered = [
                s for s in songs
                if _artist_matches(
                    artist,
                    (s.get("attributes") or {}).get("artistName") or "",
                )
            ]
            if artist_filtered:
                songs = artist_filtered
            elif _is_generic_title(title):
                return None

        # 3. Score candidates by duration + name match.
        title_low = title.lower().strip()

        def _score(song):
            attr = song.get("attributes") or {}
            score = 0
            d_ms = attr.get("durationInMillis") or 0
            if duration and d_ms:
                delta = abs(int(duration) - (d_ms // 1000))
                if delta <= 2:
                    score += 100
                elif delta <= 5:
                    score += 50
                elif delta <= 10:
                    score += 10
                else:
                    score -= 50
            name_low = (attr.get("name") or "").lower()
            if title_low and (title_low == name_low):
                score += 80
            elif title_low and (title_low in name_low or name_low in title_low):
                score += 40
            return -score
        songs.sort(key=_score)

        # 3. Walk the top few matches looking for the richest lyric type.
        best = None
        best_rank = 0
        for song in songs[:5]:
            data = self._paxsenix_fetch(song.get("id"))
            if not data:
                continue
            res = _paxsenix_to_lines(data)
            if res is None:
                continue
            rank = _result_rank(res)
            if rank > best_rank:
                best = res
                best_rank = rank
            if best_rank >= 3:
                break
        return best

    def _apple_music_token(self, force_new=False):
        """Scrape an Apple Music JWT from the Apple Music web app and
        cache it process-wide. Used to authorize catalog search calls."""
        import urllib.request
        import re as _re

        if not force_new:
            tok = getattr(self, "_am_token", None)
            if tok:
                return tok

        try:
            req = urllib.request.Request(
                "https://beta.music.apple.com",
                headers={"User-Agent": "Mozilla/5.0"},
            )
            with urllib.request.urlopen(req, timeout=6) as resp:
                main_html = resp.read().decode("utf-8", errors="replace")
            m = _re.search(r"/assets/index[~\-][^/\"' ]+\.js", main_html)
            if not m:
                print("[LYRICS] Paxsenix: couldn't locate Apple Music bundle")
                return None
            js_url = "https://beta.music.apple.com" + m.group(0)
            req = urllib.request.Request(
                js_url, headers={"User-Agent": "Mozilla/5.0"},
            )
            with urllib.request.urlopen(req, timeout=8) as resp:
                js_body = resp.read().decode("utf-8", errors="replace")
            tok_match = _re.search(r"eyJh[A-Za-z0-9._-]+", js_body)
            if not tok_match:
                print("[LYRICS] Paxsenix: couldn't extract Apple Music token")
                return None
            tok = tok_match.group(0)
            self._am_token = tok
            return tok
        except Exception as e:
            print(f"[LYRICS] Paxsenix: token fetch failed: {e}")
            return None

    def _apple_music_search(self, token, title, artist):
        import urllib.parse
        import urllib.request
        import urllib.error
        import json as _json

        if not token:
            return None
        term = title.strip() + (" " + artist.strip() if artist else "")
        url = (
            "https://amp-api.music.apple.com/v1/catalog/us/search?"
            + urllib.parse.urlencode({
                "term": term, "types": "songs", "limit": 8,
                "l": "en-US", "platform": "web",
            })
        )
        headers = {
            "Authorization": f"Bearer {token}",
            "Origin": "https://music.apple.com",
            "Referer": "https://music.apple.com/",
            "User-Agent": "Mozilla/5.0",
        }
        try:
            req = urllib.request.Request(url, headers=headers)
            with urllib.request.urlopen(req, timeout=6) as resp:
                data = _json.loads(resp.read())
        except urllib.error.HTTPError as e:
            if e.code == 401:
                # Caller will retry with a freshly-scraped token.
                self._am_token = None
                return None
            print(f"[LYRICS] Apple Music search HTTP {e.code}")
            return None
        except Exception as e:
            print(f"[LYRICS] Apple Music search failed: {e}")
            return None
        return (
            (data.get("results") or {}).get("songs", {}).get("data") or []
            if isinstance(data, dict) else []
        )

    def _paxsenix_fetch(self, song_id):
        import urllib.request
        import json as _json
        if not song_id:
            return None
        try:
            url = f"https://lyrics.paxsenix.org/apple-music/lyrics?id={song_id}"
            req = urllib.request.Request(
                url, headers={"User-Agent": "Mixtapes/1.0"},
            )
            with urllib.request.urlopen(req, timeout=8) as resp:
                return _json.loads(resp.read())
        except Exception as e:
            print(f"[LYRICS] Paxsenix lyric fetch failed: {e}")
            return None

    def _fetch_lyrics_betterlyrics(self, title, artist, duration):
        import urllib.parse
        import urllib.request
        import json as _json

        # BetterLyrics' response is just the lyric body — no artist field
        # to verify against. If our title is generic ("Intro" etc.) and
        # the server's fuzzy match goes wide, we'd silently return a
        # totally different track's lyrics. Skip rather than risk it;
        # Paxsenix (also Apple Music-backed) covers the same source and
        # we *do* check artist there.
        if _is_generic_title(title) and artist:
            return None

        params = {"s": title, "a": artist}
        if duration and duration > 0:
            params["d"] = str(int(duration))
        url = (
            "https://lyrics-api.boidu.dev/getLyrics?"
            + urllib.parse.urlencode(params)
        )
        # The endpoint returns 403 to generic user agents — the BetterLyrics
        # browser extension identifies itself this way and the API mirrors
        # that as a soft gate.
        try:
            req = urllib.request.Request(
                url, headers={"User-Agent": "BetterLyrics/1.0"}
            )
            with urllib.request.urlopen(req, timeout=6) as resp:
                body = resp.read().decode("utf-8", errors="replace")
        except urllib.error.HTTPError as e:
            # 401/404 just mean "no lyrics for this track" — common and
            # not actionable. Anything else (5xx, timeout) is worth logging.
            if e.code not in (401, 404):
                print(f"[LYRICS] BetterLyrics HTTP {e.code}: {e.reason}")
            return None
        except Exception as e:
            print(f"[LYRICS] BetterLyrics fetch failed: {e}")
            return None

        try:
            data = _json.loads(body)
        except _json.JSONDecodeError:
            return None

        # Newer responses are wrapped as {"ttml": "<tt …>…</tt>"} — Apple
        # Music's TTML schema with word-level <span> timing. Older shapes
        # are kept as a fallback in case the deployment ever rolls back.
        if isinstance(data, dict) and isinstance(data.get("ttml"), str):
            lines = _ttml_to_lines(data["ttml"])
            if lines:
                synced = all(l.get("start") is not None for l in lines)
                return {
                    "lines": lines,
                    "synced": synced,
                    "source": "BetterLyrics",
                }
            return None

        raw_lines = data.get("lyrics") if isinstance(data, dict) else data
        if not isinstance(raw_lines, list) or not raw_lines:
            return None

        lines = []
        synced = True
        for item in raw_lines:
            if not isinstance(item, dict):
                continue
            text = item.get("words") or item.get("text") or ""
            text = str(text).strip()
            if not text:
                continue
            start_ms = item.get("startTimeMs")
            start = None
            try:
                if start_ms is not None:
                    start = float(start_ms) / 1000.0
            except (TypeError, ValueError):
                start = None
            if start is None:
                synced = False
            lines.append({"start": start, "text": text})

        if not lines:
            return None
        return {"lines": lines, "synced": synced, "source": "BetterLyrics"}

    def _fetch_lyrics_ytm(self, video_id):
        if not self.api:
            return None
        try:
            wp = self.api.get_watch_playlist(videoId=video_id, limit=1)
        except Exception as e:
            print(f"[LYRICS] watch_playlist failed: {e}")
            return None
        browse_id = wp.get("lyrics") if isinstance(wp, dict) else None
        if not browse_id:
            return None

        # Try timed lyrics first; older ytmusicapi releases don't accept
        # the kwarg, so silently fall back to plain text on TypeError.
        data = None
        try:
            data = self.api.get_lyrics(browse_id, timestamps=True)
        except TypeError:
            try:
                data = self.api.get_lyrics(browse_id)
            except Exception as e:
                print(f"[LYRICS] get_lyrics failed: {e}")
                return None
        except Exception as e:
            print(f"[LYRICS] get_lyrics(timestamps) failed: {e}")
            try:
                data = self.api.get_lyrics(browse_id)
            except Exception as e2:
                print(f"[LYRICS] get_lyrics fallback failed: {e2}")
                return None

        if not data:
            return None

        raw = data.get("lyrics")
        source = (data.get("source") or "YouTube Music").replace("Source: ", "")
        if data.get("hasTimestamps") and isinstance(raw, list):
            lines = []
            for ll in raw:
                # LyricLine: attribute access on the dataclass; fall back to
                # dict access in case ytmusicapi changes the type.
                text = getattr(ll, "text", None) or (
                    ll.get("text") if isinstance(ll, dict) else None
                )
                start_ms = getattr(ll, "start_time", None)
                if start_ms is None and isinstance(ll, dict):
                    start_ms = ll.get("start_time")
                if not text:
                    continue
                start = None
                try:
                    if start_ms is not None:
                        start = float(start_ms) / 1000.0
                except (TypeError, ValueError):
                    start = None
                lines.append({"start": start, "text": str(text).strip()})
            if not lines:
                return None
            return {"lines": lines, "synced": True, "source": source}

        # Plain text fallback — split on newlines, no timing.
        if isinstance(raw, str) and raw.strip():
            lines = [
                {"start": None, "text": ln.strip()}
                for ln in raw.splitlines()
                if ln.strip()
            ]
            if not lines:
                return None
            return {"lines": lines, "synced": False, "source": source}

        return None

    def _schedule_playlist_cache_write(
        self, playlist_id, title, author, track_count, tracks, meta
    ):
        """Write the playlist into the on-disk cache on a low-priority
        thread after a delay. The serialization (``json.dumps`` over
        ~1000 track dicts) holds the GIL for hundreds of milliseconds,
        which stalls UI binds when it runs on the same fetch thread
        right after the API call returns. Doing it later means the page
        finishes rendering first.

        Also de-duplicates rapid back-to-back writes for the same
        playlist (e.g. cache-then-live fetches) — the latest call wins.
        """
        import threading as _t

        pending = getattr(self, "_pending_cache_writes", None)
        if pending is None:
            pending = {}
            self._pending_cache_writes = pending
            self._pending_cache_lock = _t.Lock()

        with self._pending_cache_lock:
            pending[playlist_id] = (
                title, author, track_count, tracks, meta,
            )

        def _worker(pid):
            import time as _time
            _time.sleep(1.5)  # let the page render before grabbing the GIL
            with self._pending_cache_lock:
                args = self._pending_cache_writes.pop(pid, None)
            if not args or not self._offline_db:
                return
            t, a, tc, tr, m = args
            try:
                self._offline_db.cache_playlist(pid, t, a, tc, tr, m)
            except Exception as e:
                print(f"[CACHE] deferred playlist write failed: {e}")

        _t.Thread(target=_worker, args=(playlist_id,), daemon=True).start()

    def get_cached_playlist_tracks(self, playlist_id):
        return self._playlist_cache.get(playlist_id)

    def set_cached_playlist_tracks(self, playlist_id, tracks):
        self._playlist_cache[playlist_id] = tracks

    def get_album(self, browse_id):
        if not self.api:
            if self._offline_db:
                cached = self._offline_db.get_cached_playlist(browse_id)
                if cached:
                    return cached
            return None
        try:
            result = self.api.get_album(browse_id)
            if result and self._offline_db:
                self._offline_db.cache_playlist(
                    browse_id,
                    result.get("title", ""),
                    str(result.get("artists", "")),
                    result.get("trackCount", len(result.get("tracks", []))),
                    result.get("tracks", []),
                )
            return result
        except Exception as e:
            print(f"Error fetching album: {e}")
            if self._offline_db:
                cached = self._offline_db.get_cached_playlist(browse_id)
                if cached:
                    return cached
            return None

    def get_artist(self, channel_id):
        if not self.api:
            return None
        try:
            res = self.api.get_artist(channel_id)
            return res
        except Exception as e:
            print(f"Error getting artist details: {e}")
            # Fallback: try as a regular YouTube channel
            try:
                user_data = self.api.get_user(channel_id)
                if user_data:
                    # Normalize to artist-like format
                    user_data["_is_channel"] = True
                    if "name" in user_data and "subscribers" not in user_data:
                        user_data["subscribers"] = ""
                    # Fetch avatar and banner from raw API
                    try:
                        raw = self.api._send_request("browse", {"browseId": channel_id})
                        header = raw.get("header", {})
                        for hkey in [
                            "musicVisualHeaderRenderer",
                            "musicImmersiveHeaderRenderer",
                        ]:
                            h = header.get(hkey, {})
                            if h:
                                # Avatar (foregroundThumbnail)
                                fg = (
                                    h.get("foregroundThumbnail", {})
                                    .get("musicThumbnailRenderer", {})
                                    .get("thumbnail", {})
                                    .get("thumbnails", [])
                                )
                                if fg:
                                    user_data["thumbnails"] = fg
                                # Banner (thumbnail)
                                bg = (
                                    h.get("thumbnail", {})
                                    .get("musicThumbnailRenderer", {})
                                    .get("thumbnail", {})
                                    .get("thumbnails", [])
                                )
                                if bg:
                                    user_data["banner"] = bg
                                # Subscriber count from subscriptionButton
                                sub_btn = h.get("subscriptionButton", {}).get(
                                    "subscribeButtonRenderer", {}
                                )
                                sub_count = sub_btn.get("subscriberCountText", {}).get(
                                    "runs", []
                                )
                                if sub_count:
                                    user_data["subscribers"] = sub_count[0].get(
                                        "text", ""
                                    )
                                break
                    except Exception:
                        pass
                    # Fallback: use first content thumbnail if no avatar found
                    if "thumbnails" not in user_data:
                        for section_key in ["playlists", "videos", "songs"]:
                            section = user_data.get(section_key, {})
                            results = (
                                section.get("results", [])
                                if isinstance(section, dict)
                                else (section if isinstance(section, list) else [])
                            )
                            if results and results[0].get("thumbnails"):
                                user_data["thumbnails"] = results[0]["thumbnails"]
                                break
                    return user_data
            except Exception as e2:
                print(f"Error getting channel details: {e2}")
            return None

    def get_artist_albums(self, channel_id, params=None, limit=100):
        if not self.api:
            return []
        try:
            result = self.api.get_artist_albums(channel_id, params=params, limit=limit)
            if result:
                return result
        except Exception:
            pass
        # Fallback: try as channel content
        try:
            result = self.api.get_user_playlists(channel_id, params)
            if result:
                return result
        except Exception:
            pass
        # Last resort: raw parse
        try:
            result = self._raw_parse_channel_content(channel_id, params)
            if result:
                return result
        except Exception:
            pass
        return []

    def get_playlist_full(self, playlist_id, limit=None):
        """Fetch a playlist's full track list. Uses ytmusicapi first, then
        our raw-continuation fallback, then yt_dlp's flat-playlist
        extraction as a last resort.

        ytmusicapi's internal paginator sometimes stops before hitting
        trackCount on large playlists (drops a continuation token mid-
        stream). The raw-continuation fallback hits the same pagination
        API though, so it often caps at the same point. yt_dlp uses a
        different underlying request flow that reliably walks YT's full
        playlist enumeration — the metadata it returns is lighter than
        ytmusicapi's, so we use it to fill IN the gaps (matching by
        videoId) rather than replacing what we have."""
        data = self.get_playlist(playlist_id, limit=limit) or {}
        tracks = list(data.get("tracks") or [])
        track_count = data.get("trackCount")
        print(
            f"[PLAYLIST] get_playlist_full: ytmusicapi returned "
            f"{len(tracks)}/{track_count} tracks for {playlist_id}"
        )
        # 5-song tolerance for off-by-one counting in the API's trackCount.
        if track_count and len(tracks) < track_count - 5:
            # Stage 1: raw-continuation on the VL<id> browse endpoint.
            try:
                browse_id = playlist_id if playlist_id.startswith("VL") else f"VL{playlist_id}"
                raw_items = self._raw_parse_playlist(browse_id) or []
                seen = {t.get("videoId") for t in tracks if t.get("videoId")}
                added = 0
                for item in raw_items:
                    vid = item.get("videoId")
                    if vid and vid not in seen:
                        tracks.append(item)
                        seen.add(vid)
                        added += 1
                print(
                    f"[PLAYLIST] raw-continuation added {added} tracks "
                    f"(total now {len(tracks)})"
                )
                data["tracks"] = tracks
            except Exception as e:
                print(f"[PLAYLIST] raw-continuation fallback failed: {e}")

        # Stage 2: if we're STILL short, use yt_dlp to enumerate all
        # videoIds on the playlist and fill in whatever we don't have
        # yet with minimal stubs. These will lack rich metadata but will
        # at least be queueable — the player fetches per-track details
        # on demand.
        if track_count and len(tracks) < track_count - 5:
            ytdlp_items = self._yt_dlp_flat_playlist(playlist_id)
            if ytdlp_items:
                seen = {t.get("videoId") for t in tracks if t.get("videoId")}
                added = 0
                for item in ytdlp_items:
                    vid = item.get("videoId")
                    if vid and vid not in seen:
                        tracks.append(item)
                        seen.add(vid)
                        added += 1
                print(
                    f"[PLAYLIST] yt_dlp added {added} tracks "
                    f"(total now {len(tracks)})"
                )
                data["tracks"] = tracks

        # Persist the (possibly-augmented) track list. The regression
        # guard in cache_playlist() prevents overwriting a richer cache
        # with a poorer one, so this is safe to call unconditionally.
        if self._offline_db and tracks:
            try:
                raw_author = data.get("author")
                if isinstance(raw_author, dict):
                    author_str = raw_author.get("name", "")
                elif isinstance(raw_author, list):
                    author_str = ", ".join(
                        a.get("name", "") for a in raw_author if isinstance(a, dict)
                    )
                else:
                    author_str = str(raw_author or "")
                meta = {
                    "description": data.get("description", "") or "",
                    "year": data.get("year", "") or "",
                    "privacy": data.get("privacy", "") or "",
                    "duration_seconds": data.get("duration_seconds"),
                    "thumbnails": data.get("thumbnails", []) or [],
                    "author_raw": raw_author,
                }
                self._offline_db.cache_playlist(
                    playlist_id,
                    data.get("title", ""),
                    author_str,

                    # dont use track_count; YouTube Music sometimes give a trackCount value higher than there actually is
                    # if the value is higher than there actually is, it causes cache regression when there isnt any
                    len(tracks), 
                    
                    tracks,
                    meta,
                )
            except Exception as e:
                print(f"[PLAYLIST] re-cache after augment failed: {e}")
        return data

    # ── Playlist sort metrics (views / date added) ────────────────────────────
    #
    # A YTM playlist track carries neither a view count nor the date it was
    # added, so the two extra sort orders need a side fetch:
    #
    #   views  — the same playlist on the youtube.com side lists "36M views"
    #            per entry (lockupViewModel), 100 per browse call.
    #   added  — `playlistItemData.voteSortValue` on the YTM response is the
    #            add timestamp (epoch seconds); it's what YTM's own "Newest
    #            first" sorts on. ytmusicapi's parser drops it, so we walk
    #            the raw response ourselves.
    #
    # Both are fetched only when the user picks the matching sort, and
    # memoized per playlist for the session.

    _MAX_METRIC_PAGES = 60  # 100 rows per page
    _YT_WEB_BROWSE_URL = "https://www.youtube.com/youtubei/v1/browse?prettyPrint=false"

    @staticmethod
    def _normalize_browse_playlist_id(playlist_id):
        pid = (playlist_id or "").strip()
        if not pid:
            return None
        return pid if pid.startswith("VL") else f"VL{pid}"

    @staticmethod
    def _walk_renderers(response, key):
        """Yield every value stored under `key` anywhere in a response.

        Iterative rather than recursive — a full playlist page nests
        deeply enough for that to matter."""
        stack = [response]
        while stack:
            node = stack.pop()
            if isinstance(node, dict):
                found = node.get(key)
                if found is not None:
                    yield found
                stack.extend(node.values())
            elif isinstance(node, list):
                stack.extend(node)

    @classmethod
    def _continuation_token(cls, response, row_keys):
        """Continuation token of the shelf whose rows use one of `row_keys`.

        A playlist response carries more than one continuation: the row
        list has its own, and the section list wrapping it has another
        that only pulls in the trailing "related playlists" shelf.
        Following the wrong one silently truncates the fetch after the
        first page, so match on the shelf that holds the rows."""
        stack = [response]
        while stack:
            node = stack.pop()
            if isinstance(node, dict):
                stack.extend(node.values())
            elif isinstance(node, list):
                token = None
                has_rows = False
                for entry in node:
                    if not isinstance(entry, dict):
                        continue
                    if any(key in entry for key in row_keys):
                        has_rows = True
                    renderer = entry.get("continuationItemRenderer")
                    if isinstance(renderer, dict):
                        # The endpoint is sometimes a commandExecutorCommand
                        # wrapping the real continuation, so search the
                        # subtree rather than assuming a fixed path.
                        for command in cls._walk_renderers(
                            renderer, "continuationCommand"
                        ):
                            if isinstance(command, dict) and command.get("token"):
                                token = command["token"]
                                break
                if token and has_rows:
                    return token
                stack.extend(node)
        return None

    def get_playlist_added_dates(self, playlist_id):
        """Map videoId -> epoch seconds the track was added to the playlist.

        Returns {} when the playlist has no such data (albums, radios,
        the local DOWNLOADS/HISTORY lists) or the fetch fails."""
        browse_id = self._normalize_browse_playlist_id(playlist_id)
        if not self.api or not browse_id:
            return {}
        key = ("added", browse_id)
        if key in self._sort_metric_cache:
            return self._sort_metric_cache[key]

        dates = {}
        try:
            response = self.api._send_request("browse", {"browseId": browse_id})
            for _ in range(self._MAX_METRIC_PAGES):
                for item in self._walk_renderers(response, "playlistItemData"):
                    if not isinstance(item, dict):
                        continue
                    vid = item.get("videoId")
                    added = item.get("voteSortValue")
                    if vid and isinstance(added, int):
                        dates.setdefault(vid, added)
                token = self._continuation_token(
                    response, ("musicResponsiveListItemRenderer",)
                )
                if not token:
                    break
                response = self.api._send_request("browse", {"continuation": token})
        except Exception as e:
            print(f"[PLAYLIST] added-date fetch failed for {browse_id}: {e}")
            if not dates:
                return {}

        print(f"[PLAYLIST] added dates: {len(dates)} tracks for {browse_id}")
        self._sort_metric_cache[key] = dates
        return dates

    # "36M views", "1.8B views", "1,234 views", "1 view", "No views"
    _VIEWS_RE = re.compile(r"^([\d.,]+)\s*([KMB])?\s+views?$", re.IGNORECASE)
    _VIEW_MULTIPLIERS = {"K": 1_000, "M": 1_000_000, "B": 1_000_000_000}

    @classmethod
    def _parse_view_count(cls, text):
        if not isinstance(text, str):
            return None
        text = text.strip()
        if text.lower().startswith("no view"):
            return 0
        match = cls._VIEWS_RE.match(text)
        if not match:
            return None
        number, suffix = match.groups()
        try:
            value = float(number.replace(",", ""))
        except ValueError:
            return None
        return int(value * cls._VIEW_MULTIPLIERS.get((suffix or "").upper(), 1))

    def get_playlist_view_counts(self, playlist_id):
        """Map videoId -> view count for every video on the playlist.

        Reads youtube.com's copy of the playlist, which — unlike the
        YouTube Music one — puts a view count on each row. Returns {} if
        the playlist isn't reachable there (private playlists need the
        session cookie; DOWNLOADS/HISTORY have no YouTube counterpart)."""
        browse_id = self._normalize_browse_playlist_id(playlist_id)
        if not browse_id:
            return {}
        key = ("views", browse_id)
        if key in self._sort_metric_cache:
            return self._sort_metric_cache[key]

        import requests

        # hl/gl pin the response to English so "36M views" stays parseable
        # regardless of the user's locale.
        body = {
            "context": {
                "client": {
                    "clientName": "WEB",
                    "clientVersion": "2.20240101.00.00",
                    "hl": "en",
                    "gl": "US",
                }
            },
            "browseId": browse_id,
        }
        views = {}
        try:
            session = requests.Session()
            session.headers.update(self._yt_web_headers())
            response = session.post(
                self._YT_WEB_BROWSE_URL, json=body, timeout=20
            ).json()
            for _ in range(self._MAX_METRIC_PAGES):
                self._harvest_view_counts(response, views)
                token = self._continuation_token(response, self._VIEW_ROW_KEYS)
                if not token:
                    break
                body.pop("browseId", None)
                body["continuation"] = token
                response = session.post(
                    self._YT_WEB_BROWSE_URL, json=body, timeout=20
                ).json()
        except Exception as e:
            print(f"[PLAYLIST] view-count fetch failed for {browse_id}: {e}")
            if not views:
                return {}

        print(f"[PLAYLIST] view counts: {len(views)} tracks for {browse_id}")
        self._sort_metric_cache[key] = views
        return views

    def _yt_web_headers(self):
        """Browser-ish headers for www.youtube.com. Carries the signed-in
        session when we have one so private playlists resolve too — the
        SAPISIDHASH is origin-bound, so YTM's own Authorization header
        can't be reused as-is."""
        headers = {
            "Content-Type": "application/json",
            "Accept-Language": "en-US,en;q=0.9",
            "User-Agent": (
                "Mozilla/5.0 (Windows NT 10.0; Win64; x64) AppleWebKit/537.36 "
                "(KHTML, like Gecko) Chrome/120.0.0.0 Safari/537.36"
            ),
            "Origin": "https://www.youtube.com",
            "Referer": "https://www.youtube.com/",
        }
        cookie = None
        try:
            cookie = self.api.headers.get("Cookie") if self.api else None
        except Exception:
            cookie = None
        if not cookie:
            return headers
        try:
            from ytmusicapi.helpers import get_authorization, sapisid_from_cookie

            sapisid = sapisid_from_cookie(cookie)
            headers["Cookie"] = cookie
            headers["Authorization"] = get_authorization(
                sapisid + " " + "https://www.youtube.com"
            )
            headers["X-Origin"] = "https://www.youtube.com"
        except Exception:
            pass
        return headers

    # youtube.com renders a playlist two ways: the read-only lockup grid, and
    # the classic editable list for playlists the signed-in user owns. Both
    # carry the view count, in different places.
    _VIEW_ROW_KEYS = ("lockupViewModel", "playlistVideoRenderer")

    @classmethod
    def _harvest_view_counts(cls, response, out):
        for lockup in cls._walk_renderers(response, "lockupViewModel"):
            if not isinstance(lockup, dict):
                continue
            if lockup.get("contentType") != "LOCKUP_CONTENT_TYPE_VIDEO":
                continue
            vid = lockup.get("contentId")
            if not vid or vid in out:
                continue
            rows = (
                lockup.get("metadata", {})
                .get("lockupMetadataViewModel", {})
                .get("metadata", {})
                .get("contentMetadataViewModel", {})
                .get("metadataRows", [])
            )
            for row in rows:
                for part in row.get("metadataParts", []):
                    count = cls._parse_view_count(part.get("text", {}).get("content"))
                    if count is not None:
                        out[vid] = count
                        break
                if vid in out:
                    break

        for item in cls._walk_renderers(response, "playlistVideoRenderer"):
            if not isinstance(item, dict):
                continue
            vid = item.get("videoId")
            if not vid or vid in out:
                continue
            for run in item.get("videoInfo", {}).get("runs", []):
                count = cls._parse_view_count(run.get("text"))
                if count is not None:
                    out[vid] = count
                    break

    def _yt_dlp_flat_playlist(self, playlist_id):
        """Use yt_dlp's flat-playlist extraction to enumerate every
        videoId on a YouTube playlist. Returns minimal track dicts
        compatible with the rest of the app (videoId, title, duration,
        thumbnails, artists). Returns [] on any failure."""
        try:
            from yt_dlp import YoutubeDL
        except ImportError:
            return []

        url = f"https://music.youtube.com/playlist?list={playlist_id}"
        opts = {
            "quiet": True,
            "no_warnings": True,
            "extract_flat": "in_playlist",
            "skip_download": True,
            "ignoreerrors": True,
        }
        try:
            with YoutubeDL(opts) as ydl:
                info = ydl.extract_info(url, download=False)
        except Exception as e:
            print(f"[PLAYLIST] yt_dlp flat-playlist raised: {e}")
            return []
        if not info:
            return []
        result = []
        for entry in info.get("entries") or []:
            if not entry:
                continue
            vid = entry.get("id")
            if not vid:
                continue
            duration = entry.get("duration")
            thumbs = []
            for t in entry.get("thumbnails") or []:
                u = t.get("url") if isinstance(t, dict) else None
                if u:
                    thumbs.append({"url": u})
            artists = []
            uploader = entry.get("uploader") or entry.get("channel")
            if uploader:
                artists.append({"name": uploader})
            result.append({
                "videoId": vid,
                "title": entry.get("title", "") or "Unknown",
                "duration_seconds": int(duration) if duration else 0,
                "thumbnails": thumbs,
                "artists": artists,
            })
        return result

    def _raw_parse_playlist(self, browse_id):
        """Parse a playlist from raw API when ytmusicapi can't handle it (e.g. chart playlists)."""
        body = {"browseId": browse_id}
        response = self.api._send_request("browse", body)

        # Try secondaryContents (twoColumnBrowseResultsRenderer format)
        tcbr = response.get("contents", {}).get("twoColumnBrowseResultsRenderer", {})
        sc = (
            tcbr.get("secondaryContents", {})
            .get("sectionListRenderer", {})
            .get("contents", [])
        )

        items = []
        for section in sc:
            for rkey in ["musicPlaylistShelfRenderer", "musicShelfRenderer"]:
                renderer = section.get(rkey, {})
                if renderer:
                    for raw_item in renderer.get("contents", []):
                        cont = raw_item.get("continuationItemRenderer")
                        if cont:
                            token = (
                                cont.get("continuationEndpoint", {})
                                .get("continuationCommand", {})
                                .get("token")
                            )
                            if token:
                                items.extend(self._fetch_continuation(token))
                        else:
                            parsed = self._parse_channel_item(raw_item)
                            if parsed:
                                items.append(parsed)

        # Fallback: try singleColumnBrowseResultsRenderer
        if not items:
            items = self._raw_parse_channel_content(browse_id, None)

        # Get title from header
        header = response.get("header", {})
        for hkey in header:
            h = header[hkey]
            if isinstance(h, dict):
                title_runs = h.get("title", {}).get("runs", [])
                if title_runs:
                    for item in items:
                        item["_playlist_title"] = title_runs[0].get("text", "")
                    break

        return items

    def _raw_parse_channel_content(self, browse_id, params):
        """Parse channel content from raw API response when ytmusicapi can't."""
        body = {"browseId": browse_id}
        if params:
            body["params"] = params
        response = self.api._send_request("browse", body)

        tabs = (
            response.get("contents", {})
            .get("singleColumnBrowseResultsRenderer", {})
            .get("tabs", [])
        )
        if not tabs:
            return []

        sections = (
            tabs[0]
            .get("tabRenderer", {})
            .get("content", {})
            .get("sectionListRenderer", {})
            .get("contents", [])
        )
        items = []
        for section in sections:
            for renderer_key in [
                "gridRenderer",
                "musicShelfRenderer",
                "musicPlaylistShelfRenderer",
                "musicCarouselShelfRenderer",
            ]:
                renderer = section.get(renderer_key, {})
                content_key = "items" if "items" in renderer else "contents"
                for raw_item in renderer.get(content_key, []):
                    parsed = self._parse_channel_item(raw_item)
                    if parsed:
                        items.append(parsed)
                    # Check for continuation token
                    cont = raw_item.get("continuationItemRenderer", {})
                    if cont:
                        token = (
                            cont.get("continuationEndpoint", {})
                            .get("continuationCommand", {})
                            .get("token")
                        )
                        if token:
                            items.extend(self._fetch_continuation(token))
        return items

    def _fetch_continuation(self, token, max_pages=100):
        """Follow continuation tokens to get all paginated results."""
        items = []
        for _ in range(max_pages):
            if not token:
                break
            try:
                response = self.api._send_request("browse", {"continuation": token})
                token = None  # Reset for next iteration

                # Format 1: onResponseReceivedActions (common for playlists)
                for action in response.get("onResponseReceivedActions", []):
                    if not isinstance(action, dict):
                        continue
                    cont_action = action.get("appendContinuationItemsAction", {})
                    for raw_item in cont_action.get("continuationItems", []):
                        cont_item = raw_item.get("continuationItemRenderer")
                        if cont_item:
                            token = (
                                cont_item.get("continuationEndpoint", {})
                                .get("continuationCommand", {})
                                .get("token")
                            )
                        else:
                            parsed = self._parse_channel_item(raw_item)
                            if parsed:
                                items.append(parsed)

                # Format 2: continuationContents (older format)
                cont_contents = response.get("continuationContents", {})
                for renderer_key in [
                    "musicPlaylistShelfContinuation",
                    "gridContinuation",
                    "musicShelfContinuation",
                ]:
                    renderer = cont_contents.get(renderer_key, {})
                    if not renderer:
                        continue
                    for raw_item in renderer.get("contents", []) + renderer.get(
                        "items", []
                    ):
                        cont_item = raw_item.get("continuationItemRenderer")
                        if cont_item:
                            token = (
                                cont_item.get("continuationEndpoint", {})
                                .get("continuationCommand", {})
                                .get("token")
                            )
                        else:
                            parsed = self._parse_channel_item(raw_item)
                            if parsed:
                                items.append(parsed)
            except Exception:
                break
        return items

    def _parse_channel_item(self, raw_item):
        """Best-effort parse of a channel content item."""
        for item_key in ["musicTwoRowItemRenderer", "musicResponsiveListItemRenderer"]:
            renderer = raw_item.get(item_key)
            if not renderer:
                continue
            result = {}
            # Title - check both direct title.runs and flexColumns
            title_runs = renderer.get("title", {}).get("runs", [])
            if not title_runs:
                # flexColumns format (used in musicResponsiveListItemRenderer)
                for col in renderer.get("flexColumns", []):
                    col_renderer = col.get(
                        "musicResponsiveListItemFlexColumnRenderer", {}
                    )
                    runs = col_renderer.get("text", {}).get("runs", [])
                    if runs and not result.get("title"):
                        title_runs = runs
                        break

            if title_runs:
                result["title"] = title_runs[0].get("text", "")
                nav = title_runs[0].get("navigationEndpoint", {})
                browse_ep = nav.get("browseEndpoint", {})
                watch_ep_title = nav.get("watchEndpoint", {})
                if browse_ep.get("browseId"):
                    result["browseId"] = browse_ep["browseId"]
                if watch_ep_title.get("videoId"):
                    result["videoId"] = watch_ep_title["videoId"]
                    result["playlistId"] = watch_ep_title.get("playlistId", "")

            # Artists from flexColumns (second column usually)
            for col in renderer.get("flexColumns", [])[1:]:
                col_renderer = col.get("musicResponsiveListItemFlexColumnRenderer", {})
                runs = col_renderer.get("text", {}).get("runs", [])
                artists = []
                for r in runs:
                    browse_nav = r.get("navigationEndpoint", {}).get(
                        "browseEndpoint", {}
                    )
                    if browse_nav.get("browseId"):
                        artists.append(
                            {"name": r.get("text", ""), "id": browse_nav["browseId"]}
                        )
                    elif r.get("text", "").strip() and r["text"].strip() not in (
                        "•",
                        "&",
                        ",",
                    ):
                        artists.append({"name": r["text"].strip()})
                if artists:
                    result["artists"] = artists
                    break

            # Duration from fixedColumns
            for col in renderer.get("fixedColumns", []):
                col_renderer = col.get("musicResponsiveListItemFixedColumnRenderer", {})
                runs = col_renderer.get("text", {}).get("runs", [])
                if runs:
                    dur_text = runs[0].get("text", "")
                    result["duration"] = dur_text
                    # Convert "M:SS" or "H:MM:SS" to seconds
                    try:
                        parts = dur_text.split(":")
                        if len(parts) == 2:
                            result["duration_seconds"] = int(parts[0]) * 60 + int(
                                parts[1]
                            )
                        elif len(parts) == 3:
                            result["duration_seconds"] = (
                                int(parts[0]) * 3600
                                + int(parts[1]) * 60
                                + int(parts[2])
                            )
                    except (ValueError, IndexError):
                        pass
                    break

            # Thumbnail
            thumb_renderer = renderer.get("thumbnailRenderer", {}).get(
                "musicThumbnailRenderer", {}
            )
            if not thumb_renderer:
                thumb_renderer = renderer.get("thumbnail", {}).get(
                    "musicThumbnailRenderer", {}
                )
            thumbs = thumb_renderer.get("thumbnail", {}).get("thumbnails", [])
            if thumbs:
                result["thumbnails"] = thumbs

            # VideoId from overlay play button (fallback if not from title)
            if not result.get("videoId"):
                overlay = renderer.get("overlay", {}).get(
                    "musicItemThumbnailOverlayRenderer", {}
                )
                play_btn = overlay.get("content", {}).get("musicPlayButtonRenderer", {})
                watch_ep = play_btn.get("playNavigationEndpoint", {}).get(
                    "watchEndpoint", {}
                )
                if watch_ep.get("videoId"):
                    result["videoId"] = watch_ep["videoId"]
                    result["playlistId"] = watch_ep.get("playlistId", "")

            # Subtitle (for musicTwoRowItemRenderer)
            subtitle_runs = renderer.get("subtitle", {}).get("runs", [])
            if subtitle_runs:
                parts = [r.get("text", "") for r in subtitle_runs]
                result["subtitle"] = "".join(parts)
                if not result.get("artists"):
                    artists = []
                    for r in subtitle_runs:
                        nav = r.get("navigationEndpoint", {}).get("browseEndpoint", {})
                    if nav.get("browseId"):
                        artists.append({"name": r["text"], "id": nav["browseId"]})
                if artists:
                    result["artists"] = artists

            if result.get("title"):
                return result
        return None

    def get_liked_songs(self, limit=100):
        if not self.is_authenticated():
            return []
        # Liked songs is actually a playlist 'LM'
        res = self.get_playlist("LM", limit=limit)
        return res

    def get_charts(self, country="US"):
        if not self.api:
            return {}
        return self.api.get_charts(country=country)

    def get_explore(self):
        if not self.api:
            return {}
        return self.api.get_explore()

    def get_home(self, limit=25):
        if not self.api:
            return []
        try:
            return self.api.get_home(limit=limit)
        except Exception as e:
            print(f"Error fetching home feed: {e}")
            return []

    def get_home_full(self, limit=25):
        """Same as get_home, but also attaches per-shelf header metadata
        (strapline thumbnail/text) that ytmusicapi's parser drops.

        ytmusicapi only surfaces ``title`` + ``contents`` for each shelf. For
        "Based on …" / "Because you played …" rows YouTube also returns a
        small thumbnail of the seed (an album cover, artist photo, playlist
        thumb) — we annotate first-page shelves with that thumbnail by title
        so the home page can show it next to the heading.

        We deliberately delegate the actual section fetch to ytmusicapi
        (which handles continuations) and only do a single raw call for the
        metadata sidecar, so continuation shelves still come through —
        they just don't get a strapline thumbnail.
        """
        if not self.api:
            return []

        try:
            sections = self.api.get_home(limit=limit) or []
        except Exception as e:
            print(f"Error fetching home feed: {e}")
            return []

        try:
            from ytmusicapi.navigation import SINGLE_COLUMN_TAB, SECTION_LIST

            response = self.api._send_request(
                "browse", {"browseId": "FEmusic_home"}
            )
            cur = response
            for step in SINGLE_COLUMN_TAB + SECTION_LIST:
                cur = cur[step]

            meta_by_title = {}
            video_type_map = {}
            for row in cur:
                title = _extract_shelf_title(row)
                if title:
                    extras = _extract_shelf_header_meta(row)
                    if extras and title not in meta_by_title:
                        meta_by_title[title] = extras
                video_type_map.update(_extract_video_types(row))

            for sec in sections:
                t = sec.get("title")
                if t and t in meta_by_title:
                    sec.update(meta_by_title[t])
                for it in sec.get("contents") or []:
                    if not isinstance(it, dict):
                        continue
                    vid = it.get("videoId")
                    if vid and vid in video_type_map:
                        it["videoType"] = video_type_map[vid]
        except Exception as e:
            print(f"[HOME] strapline metadata pass failed: {e}")

        return sections

    def get_mood_playlists(self, params):
        if not self.api:
            return []
        try:
            return self.api.get_mood_playlists(params=params)
        except Exception as e:
            print(f"Error fetching mood playlists: {e}")
            return []

    def get_mood_categories(self):
        if not self.api:
            return {}
        try:
            return self.api.get_mood_categories()
        except Exception as e:
            print(f"Error fetching mood categories: {e}")
            return {}

    def get_category_page(self, params):
        if not self.api:
            return []
        try:
            response = self.api._send_request(
                "browse",
                {"browseId": "FEmusic_moods_and_genres_category", "params": params},
            )

            sections = []
            if (
                "contents" in response
                and "singleColumnBrowseResultsRenderer" in response["contents"]
            ):
                tabs = response["contents"]["singleColumnBrowseResultsRenderer"]["tabs"]
                results = tabs[0]["tabRenderer"]["content"]["sectionListRenderer"][
                    "contents"
                ]

                for section in results:
                    if "musicCarouselShelfRenderer" in section:
                        carousel = section["musicCarouselShelfRenderer"]
                        title = carousel["header"][
                            "musicCarouselShelfBasicHeaderRenderer"
                        ]["title"]["runs"][0]["text"]
                        contents = carousel["contents"]

                        parsed_items = []
                        for item in contents:
                            try:
                                data = {}
                                if "musicResponsiveListItemRenderer" in item:
                                    renderer = item["musicResponsiveListItemRenderer"]
                                    runs = renderer["flexColumns"][0][
                                        "musicResponsiveListItemFlexColumnRenderer"
                                    ]["text"]["runs"]
                                    data["title"] = runs[0]["text"]

                                    if "navigationEndpoint" in renderer:
                                        ep = renderer["navigationEndpoint"]
                                        if "watchEndpoint" in ep:
                                            data["videoId"] = ep["watchEndpoint"][
                                                "videoId"
                                            ]
                                        elif "browseEndpoint" in ep:
                                            data["browseId"] = ep["browseEndpoint"][
                                                "browseId"
                                            ]
                                    elif "navigationEndpoint" in runs[0]:
                                        ep = runs[0]["navigationEndpoint"]
                                        if "watchEndpoint" in ep:
                                            data["videoId"] = ep["watchEndpoint"][
                                                "videoId"
                                            ]
                                        elif "browseEndpoint" in ep:
                                            data["browseId"] = ep["browseEndpoint"][
                                                "browseId"
                                            ]

                                    if "thumbnail" in renderer:
                                        data["thumbnails"] = renderer["thumbnail"][
                                            "musicThumbnailRenderer"
                                        ]["thumbnail"]["thumbnails"]

                                    if len(renderer["flexColumns"]) > 1:
                                        sub_runs = renderer["flexColumns"][1][
                                            "musicResponsiveListItemFlexColumnRenderer"
                                        ]["text"]["runs"]
                                        artists = []
                                        for r in sub_runs:
                                            if (
                                                "navigationEndpoint" in r
                                                and "browseEndpoint"
                                                in r["navigationEndpoint"]
                                            ):
                                                if (
                                                    "browseEndpointContextSupportedConfigs"
                                                    in r["navigationEndpoint"][
                                                        "browseEndpoint"
                                                    ]
                                                ):
                                                    if (
                                                        r["navigationEndpoint"][
                                                            "browseEndpoint"
                                                        ][
                                                            "browseEndpointContextSupportedConfigs"
                                                        ][
                                                            "browseEndpointContextMusicConfig"
                                                        ]["pageType"]
                                                        == "MUSIC_PAGE_TYPE_ARTIST"
                                                    ):
                                                        artists.append(
                                                            {
                                                                "name": r["text"],
                                                                "id": r[
                                                                    "navigationEndpoint"
                                                                ]["browseEndpoint"][
                                                                    "browseId"
                                                                ],
                                                            }
                                                        )
                                        data["artists"] = artists

                                elif "musicTwoRowItemRenderer" in item:
                                    renderer = item["musicTwoRowItemRenderer"]
                                    runs = renderer["title"]["runs"]
                                    data["title"] = runs[0]["text"]

                                    if "navigationEndpoint" in renderer:
                                        ep = renderer["navigationEndpoint"]
                                        if "watchEndpoint" in ep:
                                            data["videoId"] = ep["watchEndpoint"][
                                                "videoId"
                                            ]
                                        elif "browseEndpoint" in ep:
                                            data["browseId"] = ep["browseEndpoint"][
                                                "browseId"
                                            ]
                                    elif "navigationEndpoint" in runs[0]:
                                        ep = runs[0]["navigationEndpoint"]
                                        if "watchEndpoint" in ep:
                                            data["videoId"] = ep["watchEndpoint"][
                                                "videoId"
                                            ]
                                        elif "browseEndpoint" in ep:
                                            data["browseId"] = ep["browseEndpoint"][
                                                "browseId"
                                            ]

                                    if (
                                        "thumbnailRenderer" in renderer
                                        and "musicThumbnailRenderer"
                                        in renderer["thumbnailRenderer"]
                                    ):
                                        data["thumbnails"] = renderer[
                                            "thumbnailRenderer"
                                        ]["musicThumbnailRenderer"]["thumbnail"][
                                            "thumbnails"
                                        ]

                                    if (
                                        "subtitle" in renderer
                                        and "runs" in renderer["subtitle"]
                                    ):
                                        sub_runs = renderer["subtitle"]["runs"]
                                        artists = []
                                        year = None
                                        type_ = None
                                        for r in sub_runs:
                                            if (
                                                "navigationEndpoint" in r
                                                and "browseEndpoint"
                                                in r["navigationEndpoint"]
                                            ):
                                                ep = r["navigationEndpoint"][
                                                    "browseEndpoint"
                                                ]
                                                if (
                                                    "browseEndpointContextSupportedConfigs"
                                                    in ep
                                                ):
                                                    pt = ep[
                                                        "browseEndpointContextSupportedConfigs"
                                                    ][
                                                        "browseEndpointContextMusicConfig"
                                                    ]["pageType"]
                                                    if pt == "MUSIC_PAGE_TYPE_ARTIST":
                                                        artists.append(
                                                            {
                                                                "name": r["text"],
                                                                "id": ep["browseId"],
                                                            }
                                                        )
                                            elif (
                                                "text" in r and r["text"].strip() != "•"
                                            ):
                                                txt = r["text"].strip()
                                                if txt.isdigit() and len(txt) == 4:
                                                    year = txt
                                                elif txt in [
                                                    "Album",
                                                    "Single",
                                                    "EP",
                                                    "Playlist",
                                                ]:
                                                    type_ = txt
                                        data["artists"] = artists
                                        if year:
                                            data["year"] = year
                                        if type_:
                                            data["type"] = type_

                                if data:
                                    parsed_items.append(data)
                            except Exception as e:
                                print("Error parsing item in category page:", e)

                        if parsed_items:
                            sections.append({"title": title, "items": parsed_items})
            return sections
        except Exception as e:
            print(f"Error fetching category page: {e}")
            return []

    def get_album_browse_id(self, audio_playlist_id):
        if not self.api:
            return None
        return self.api.get_album_browse_id(audio_playlist_id)

    def rate_song(self, video_id, rating="LIKE"):
        """
        Rate a song: 'LIKE', 'DISLIKE', or 'INDIFFERENT'.
        """
        if not self.is_authenticated():
            return False
        try:
            self.api.rate_song(video_id, rating)
            return True
        except Exception as e:
            print(f"Error rating song: {e}")
            return False

    def validate_session(self):
        """
        Check if the current session is valid by attempting an authenticated request.
        """
        if self.api is None:
            return False

        try:
            # Try to fetch liked songs (requires auth)
            # Just metadata is enough
            self.api.get_liked_songs(limit=1)
            return True
        except Exception as e:
            print(f"Session validation failed: {e}")
            return False

    def logout(self):
        """
        Log out by deleting the saved auth file and resetting the API.
        """
        if os.path.exists(self.auth_path):
            try:
                os.remove(self.auth_path)
                print(f"Removed auth file at {self.auth_path}")
            except Exception as e:
                print(f"Could not remove auth file: {e}")

        self.api = YTMusic()
        self._is_authed = False
        self._clear_account_state()
        print("Logged out. API reset to unauthenticated mode.")
        return True

    def _clear_account_state(self):
        """Drop every per-account cache so account switches can't leak
        data from the previous user. Covers in-memory state (account
        info, library indexes, subscribed artists, playlist/channel
        caches) and the per-user on-disk caches (history + library
        playlists)."""
        self._user_info = None
        self._subscribed_artists = set()
        self._library_playlists = []
        self._library_playlist_ids = set()
        self._library_album_ids = set()
        self._playlist_cache = {}
        if hasattr(self, "_channel_handle_cache"):
            self._channel_handle_cache = {}

        # Wipe the on-disk per-user caches via DownloadDB helpers if
        # they're available. Silent on failure — this is a cleanup
        # pass, not a correctness-critical operation.
        db = self._offline_db
        if db is None:
            return
        try:
            import sqlite3
            with db._lock:
                conn = db._connect()
                try:
                    conn.execute("DELETE FROM history_cache")
                except sqlite3.OperationalError:
                    pass
                try:
                    conn.execute("DELETE FROM library_playlists_cache")
                except sqlite3.OperationalError:
                    pass
                conn.commit()
                conn.close()
        except Exception as e:
            print(f"[CLIENT] per-account cache wipe failed: {e}")

    def edit_playlist(
        self, playlist_id, title=None, description=None, privacy=None, moveItem=None
    ):
        if not self.is_authenticated():
            return False
        try:
            self.api.edit_playlist(
                playlist_id,
                title=title,
                description=description,
                privacyStatus=privacy,
                moveItem=moveItem,
            )
            return True
        except Exception as e:
            print(f"Error editing playlist: {e}")
            return False

    def delete_playlist(self, playlist_id):
        if not self.is_authenticated():
            return False
        try:
            self.api.delete_playlist(playlist_id)
            return True
        except Exception as e:
            print(f"Error deleting playlist: {e}")
            return False

    def add_playlist_items(
        self, playlist_id, video_ids, duplicates=False, swap_to_audio=None
    ):
        """Add tracks to a playlist.

        When `swap_to_audio` is True, each music-video videoId is
        transparently replaced with its audio (ATV) counterpart via
        `find_audio_version`. Default behavior (`None`): auto-enable
        for single-item adds (right-click → Add to Playlist) and
        disable for bulk adds (Add all to Playlist, Add queue to
        playlist) where N extra API calls would noticeably stall the
        action. Callers can override either way explicitly."""
        if not self.is_authenticated():
            return False
        if swap_to_audio is None:
            swap_to_audio = len(video_ids) == 1
        if swap_to_audio and video_ids:
            swapped = []
            for vid in video_ids:
                try:
                    alt = self.find_audio_version(vid)
                except Exception as e:
                    print(f"[swap-version] auto-swap failed for {vid}: {e}")
                    alt = None
                alt_id = alt.get("videoId") if isinstance(alt, dict) else None
                swapped.append(alt_id or vid)
            video_ids = swapped
        try:
            self.api.add_playlist_items(playlist_id, video_ids, duplicates=duplicates)
            return True
        except Exception as e:
            print(f"Error adding to playlist: {e}")
            return False

    def remove_playlist_items(self, playlist_id, videos):
        if not self.is_authenticated():
            return False
        try:
            self.api.remove_playlist_items(playlist_id, videos)
            return True
        except Exception as e:
            print(f"Error removing from playlist: {e}")
            return False

    def get_editable_playlists(self):
        """
        Returns a list of playlists that the user can add songs to.
        Includes owned playlists and collaborative playlists.
        """
        if not self.is_authenticated():
            return []
        try:
            playlists = (
                self._library_playlists
                if self._library_playlists
                else self.get_library_playlists()
            )

            user_info = self.get_account_info()
            user_name = user_info.get("accountName", "").lower() if user_info else ""

            editable = []
            for p in playlists:
                pid = p.get("playlistId") or ""
                # Exclude radio/mixes/system playlists
                if not pid.startswith("PL") and not pid.startswith("VL"):
                    continue
                if pid in ["LM", "SE", "VLLM"]:
                    continue

                # Ownership Check:
                # items created by the user often have author="You" or their name, or no author field.
                # items subscribed to have a specific author name.
                # collaborative ones might have both, but usually can be added to.

                author = p.get("author") or p.get("creator")
                if isinstance(author, list) and author:
                    author = author[0].get("name", "")
                elif isinstance(author, dict):
                    author = author.get("name", "")

                author_str = str(author or "").lower()

                # If author is missing, empty, "you", or your name, it's yours
                is_mine = False
                if (
                    not author_str
                    or author_str == "you"
                    or (user_name and author_str == user_name)
                ):
                    is_mine = True

                # Collaborative check: ytmusicapi identifies these in some objects,
                # but if we are following it and it's in the library, we can try.
                # Actually, the most reliable way in the library list is seeing if there is NOT an external author.

                if is_mine or p.get("collaborative"):
                    editable.append(p)
            return editable
        except Exception as e:
            print(f"Error filtering editable playlists: {e}")
            return []

    def subscribe_artist(self, channel_id):
        if not self.is_authenticated():
            return False
        try:
            self.api.subscribe_artists([channel_id])
            self._subscribed_artists.add(channel_id)
            return True
        except Exception as e:
            print(f"Error subscribing to artist: {e}")
            return False

    def unsubscribe_artist(self, channel_id):
        if not self.is_authenticated():
            return False
        try:
            self.api.unsubscribe_artists([channel_id])
            if channel_id in self._subscribed_artists:
                self._subscribed_artists.remove(channel_id)
            return True
        except Exception as e:
            print(f"Error unsubscribing from artist: {e}")
            return False

    def is_subscribed_artist(self, channel_id):
        """Checks if an artist is in the local subscription cache."""
        return channel_id in self._subscribed_artists

    def create_playlist(
        self, title, description="", privacy_status="PRIVATE", video_ids=None
    ):
        """
        Creates a new playlist.
        """
        if not self.is_authenticated():
            return None
        try:
            return self.api.create_playlist(
                title, description, privacy_status=privacy_status, video_ids=video_ids
            )
        except Exception as e:
            print(f"Error creating playlist: {e}")
            return None

    def set_playlist_thumbnail(self, playlist_id, image_path):
        """
        Sets a custom thumbnail for a playlist.
        Uses internal YouTube resumable upload endpoints. Resizes to 1024x1024 max.
        """
        if not self.is_authenticated():
            print("Not authenticated.")
            return False

        import requests

        try:
            with open(image_path, "rb") as f:
                img_data = f.read()

            # Use base ytmusicapi headers, but remove Content-Type for binary upload steps
            base_headers = self.api.headers.copy()
            base_headers.pop("Content-Type", None)

            # --- STEP 1: INITIATE UPLOAD ---
            headers_start = base_headers.copy()
            headers_start.update(
                {
                    "Content-Type": "application/x-www-form-urlencoded;charset=utf-8",
                    "Content-Length": "0",
                    "X-Goog-Upload-Command": "start",
                    "X-Goog-Upload-Protocol": "resumable",
                    "X-Goog-Upload-Header-Content-Length": str(len(img_data)),
                    "Origin": "https://music.youtube.com",
                    "Referer": f"https://music.youtube.com/playlist?list={playlist_id}",
                }
            )

            init_res = requests.post(
                "https://music.youtube.com/playlist_image_upload/playlist_custom_thumbnail",
                headers=headers_start,
                data=b"",
            )

            upload_id = init_res.headers.get("x-guploader-uploadid")

            if not upload_id:
                raise Exception(
                    f"Failed to obtain upload ID. Status={init_res.status_code}, Body={init_res.text[:500]}"
                )

            # --- STEP 2: UPLOAD BINARY DATA ---
            upload_url = init_res.headers.get("X-Goog-Upload-URL")

            headers_upload = base_headers.copy()
            headers_upload.update(
                {
                    "Content-Type": "application/x-www-form-urlencoded;charset=utf-8",
                    "X-Goog-Upload-Command": "upload, finalize",
                    "X-Goog-Upload-Offset": "0",
                    "Origin": "https://music.youtube.com",
                    "Referer": f"https://music.youtube.com/playlist?list={playlist_id}",
                }
            )
            # Remove any encoding headers that cause "Could not decompress" errors
            headers_upload.pop("Accept-Encoding", None)
            headers_upload.pop("Content-Encoding", None)

            import urllib.request

            req = urllib.request.Request(
                upload_url,
                data=img_data,
                headers=headers_upload,
                method="POST",
            )
            with urllib.request.urlopen(req) as resp:
                upload_body = resp.read().decode("utf-8")
                upload_status = resp.status

            if not upload_body.strip():
                raise Exception(
                    f"Upload returned empty response. Status={upload_status}"
                )

            import json as _json

            blob_data = _json.loads(upload_body)
            blob_id = blob_data.get("encryptedBlobId")

            if not blob_id:
                raise Exception(
                    f"Failed to obtain encryptedBlobId. Response: {blob_data}"
                )

            # --- STEP 3: BIND BLOB TO PLAYLIST ---
            clean_playlist_id = (
                playlist_id[2:] if playlist_id.startswith("VL") else playlist_id
            )

            payload = {
                "playlistId": clean_playlist_id,
                "actions": [
                    {
                        "action": "ACTION_SET_CUSTOM_THUMBNAIL",
                        "addedCustomThumbnail": {
                            "imageKey": {
                                "type": "PLAYLIST_IMAGE_TYPE_CUSTOM_THUMBNAIL",
                                "name": "studio_square_thumbnail",
                            },
                            "playlistScottyEncryptedBlobId": blob_id,
                        },
                    }
                ],
            }

            # _send_request natively handles putting "Content-Type: application/json" back
            edit_res = self.api._send_request("browse/edit_playlist", payload)

            if edit_res.get("status") == "STATUS_SUCCEEDED":
                print("Thumbnail successfully updated!")
                return True
            else:
                print(f"Failed to bind thumbnail. API Response: {edit_res}")
                return False

        except Exception as e:
            print(f"Error setting playlist thumbnail: {e}")
            return False

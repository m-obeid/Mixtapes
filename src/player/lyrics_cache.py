"""Disk-backed cache for lyrics fetched from any provider.

One JSON file per videoId at ``~/.cache/muse/lyrics/<video_id>.json``.
Schema:

    {
        "preferred_source": "Paxsenix (Apple Music)" | null,
        "results": {
            "<source name>": {
                "lines": [{"start": float|null, "text": str, ...}, ...],
                "synced": bool,
                "source": str
            },
            ...
        }
    }

``preferred_source`` is the user's pinned choice for this track (``null``
means "use whatever the chain returned best"). The lyrics view's source
picker writes to this field; ``get_lyrics`` reads it before falling back
to the highest-ranked result.

Lyrics don't change once written so entries live forever; the cache is
bounded by a soft cap that evicts the least-recently-used file on insert.
"""

import json
import os
import threading
import time
from gi.repository import GLib


# Bumped whenever the fetch/parse pipeline starts producing better data
# for input it already handled. Entries written by an older pipeline are
# dropped on read and refetched, so a fix reaches tracks the user has
# already played without them knowing to clear anything.
#
#   2 — Apple Music parsed from its TTML (word timing, romanization,
#       translations, background vocals); Unicode-aware artist matching;
#       NetEase romanization/translation tracks.
#   3 — Search hits gated on duration and version before the
#       richest-lyrics walk, so a different song can no longer be served
#       in place of the track.
#   4 — Romanization merged in from a second provider when the one that
#       won the lyrics doesn't carry one.
#   5 — Korean romanization generated locally.
#   6 — Apple Music catalog resolved through the public iTunes Search
#       API, so tracks the JWT scrape couldn't reach are found now.
#   7 — Cyrillic romanization generated locally; NetEase search hits
#       gated the same way Apple's and BiniLyrics' are.
#   8 — Duet voice recorded per line, so the second singer's lines can
#       sit against the opposite edge.
#   9 — Artist takes precedence over title when picking a search hit, so
#       a same-title track by someone else can't win.
#  10 — Romanization gaps completed from a local dictionary.
#  11 — Second line applied to manually picked sources too, not just the
#       one the chain settled on.
#  12 — Store watermarks stripped from the top of a lyric.
PIPELINE_VERSION = 12

_CACHE_DIR = None


def _cache_dir():
    global _CACHE_DIR
    if _CACHE_DIR is None:
        d = os.path.join(GLib.get_user_cache_dir(), "muse", "lyrics")
        os.makedirs(d, exist_ok=True)
        _CACHE_DIR = d
    return _CACHE_DIR


def _path_for(video_id):
    return os.path.join(_cache_dir(), f"{video_id}.json")


class LyricsCache:
    # Soft cap on cached files; oldest mtimes get evicted on insert.
    MAX_ENTRIES = 2000

    def __init__(self):
        self._lock = threading.Lock()
        # Per-process in-memory mirror so the lyrics view's repeated reads
        # don't hit disk on every progression tick.
        self._mem = {}

    def load(self, video_id):
        """Return the entire cache entry for ``video_id``, or ``None`` if
        we've never written one. The returned dict has ``results``
        (provider-name → normalized result) and ``preferred_source``."""
        if not video_id:
            return None
        if video_id in self._mem:
            return self._mem[video_id]
        path = _path_for(video_id)
        try:
            if not os.path.exists(path):
                return None
            with self._lock:
                with open(path, "r") as f:
                    data = json.load(f)
        except (OSError, json.JSONDecodeError):
            return None
        if not isinstance(data, dict):
            return None
        if data.get("pipeline") != PIPELINE_VERSION:
            # Stale shape. Drop the cached lyrics so the chain refetches
            # them through the current parsers, but keep the user's pinned
            # provider and anything they picked by hand — those are
            # deliberate choices, and re-deriving them would silently
            # replace the match they went to the trouble of selecting.
            kept = {
                name: res
                for name, res in (data.get("results") or {}).items()
                if isinstance(res, dict) and res.get("user_choice")
            }
            data = {
                "preferred_source": data.get("preferred_source"),
                "results": kept,
                "pipeline": PIPELINE_VERSION,
            }
        data.setdefault("results", {})
        data.setdefault("preferred_source", None)
        self._mem[video_id] = data
        return data

    def get_result(self, video_id, order=None, accept_rank=1):
        """Return the cached lyrics result to show, or ``None``.

        The user-pinned ``preferred_source`` always wins when it's
        present. Otherwise this mirrors what the live chain would have
        picked: walk the user's provider queue (``order``) and take the
        first source whose result is at least ``accept_rank``, holding a
        weaker one as a fallback.

        With ``order`` given and none of its providers cached, this
        returns ``None`` on purpose — the caller then runs the chain,
        which is what should happen when the only cached sources are
        ones the user has since switched off. Callers that want whatever
        is on disk regardless (the source picker) use
        :meth:`get_alternatives`."""
        entry = self.load(video_id)
        if not entry or not entry.get("results"):
            return None
        results = entry["results"]
        pref = entry.get("preferred_source")
        if pref and pref in results:
            return results[pref]

        if order:
            fallback = None
            fallback_rank = 0
            for name in order:
                res = results.get(name)
                if not res:
                    continue
                rank = _rank_score(res)
                if rank >= accept_rank:
                    return res
                if rank > fallback_rank:
                    fallback, fallback_rank = res, rank
            return fallback

        # Pick the richest result by rank.
        ranked = sorted(results.values(), key=_rank_score, reverse=True)
        return ranked[0] if ranked else None

    def get_alternatives(self, video_id):
        """Return ``[(source_name, result), ...]`` for every cached
        provider, sorted from richest to weakest."""
        entry = self.load(video_id)
        if not entry or not entry.get("results"):
            return []
        items = list(entry["results"].items())
        items.sort(key=lambda kv: _rank_score(kv[1]), reverse=True)
        return items

    def get_preferred(self, video_id):
        entry = self.load(video_id)
        return entry.get("preferred_source") if entry else None

    def has_source(self, video_id, source):
        entry = self.load(video_id)
        if not entry:
            return False
        return source in (entry.get("results") or {})

    def add_result(self, video_id, result, user_choice=False):
        """Save a single provider's result under the source name it
        already carries in ``result["source"]``. No-op if ``result`` is
        falsy or doesn't carry lines.

        ``user_choice`` marks a result the listener selected themselves,
        which survives the pipeline-version wipe that clears everything
        else."""
        if not video_id or not result or not result.get("lines"):
            return
        result = dict(result)
        if user_choice:
            result["user_choice"] = True
        source = result.get("source") or "Unknown"
        entry = self.load(video_id) or {
            "preferred_source": None, "results": {},
        }
        entry["results"][source] = result
        self._write(video_id, entry)

    def add_results(self, video_id, results):
        """Bulk-add several provider results. Atomic write at the end."""
        if not video_id or not results:
            return
        entry = self.load(video_id) or {
            "preferred_source": None, "results": {},
        }
        for res in results:
            if res and res.get("lines"):
                entry["results"][res.get("source") or "Unknown"] = res
        self._write(video_id, entry)

    def set_preferred(self, video_id, source):
        """Pin the user's preferred provider for this track. Pass
        ``None`` to clear the preference and fall back to ranked order."""
        if not video_id:
            return
        entry = self.load(video_id) or {
            "preferred_source": None, "results": {},
        }
        entry["preferred_source"] = source
        self._write(video_id, entry)

    def clear_user_choice(self, video_id):
        """Undo a hand-picked source: drop the pin and any result the
        listener selected, so the chain's own choice applies again.

        Only the picked results go — whatever the chain had already
        cached stays, so undoing is instant rather than a refetch."""
        entry = self.load(video_id)
        if not entry:
            return
        entry["preferred_source"] = None
        entry["results"] = {
            name: res for name, res in (entry.get("results") or {}).items()
            if not (isinstance(res, dict) and res.get("user_choice"))
        }
        self._write(video_id, entry)

    def invalidate(self, video_id):
        """Wipe the cache for a single track (e.g. user explicitly asks
        to refresh)."""
        if not video_id:
            return
        self._mem.pop(video_id, None)
        try:
            os.remove(_path_for(video_id))
        except OSError:
            pass

    def clear_all(self):
        """Wipe every cached track. Used by Settings after the provider
        queue changes — reordering only decides which cached source gets
        shown, so a track already cached from a now-lower-priority
        provider needs the chain re-run to pick up a better one."""
        self._mem.clear()
        removed = 0
        try:
            d = _cache_dir()
            for fname in os.listdir(d):
                if not fname.endswith(".json"):
                    continue
                try:
                    os.remove(os.path.join(d, fname))
                    removed += 1
                except OSError:
                    pass
        except OSError:
            pass
        return removed

    def _write(self, video_id, entry):
        path = _path_for(video_id)
        entry["pipeline"] = PIPELINE_VERSION
        try:
            with self._lock:
                with open(path, "w") as f:
                    json.dump(entry, f)
            os.utime(path, None)
            self._mem[video_id] = entry
            self._evict_old()
        except OSError as e:
            print(f"[LYRICS-CACHE] write failed for {video_id}: {e}")

    def _evict_old(self):
        try:
            d = _cache_dir()
            files = []
            for fname in os.listdir(d):
                if not fname.endswith(".json"):
                    continue
                fpath = os.path.join(d, fname)
                try:
                    files.append((os.path.getmtime(fpath), fpath))
                except OSError:
                    pass
            if len(files) <= self.MAX_ENTRIES:
                return
            files.sort()
            for _, fpath in files[: len(files) - self.MAX_ENTRIES]:
                try:
                    os.remove(fpath)
                except OSError:
                    pass
                # Drop from memory mirror too.
                vid = os.path.splitext(os.path.basename(fpath))[0]
                self._mem.pop(vid, None)
        except OSError:
            pass


def _rank_score(result):
    """Higher is better. Mirrors the chain's ranking in api/client.py:
    word-level > line-synced > plain text."""
    if not result or not result.get("lines"):
        return 0
    if any(l.get("parts") for l in result.get("lines", [])):
        return 3
    if result.get("synced"):
        return 2
    return 1

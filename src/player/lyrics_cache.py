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

    # Cap on the in-memory mirror. The disk cache holds MAX_ENTRIES, but the
    # `_mem` dict would otherwise grow once per *played* track for a whole
    # session (each entry is a full lyrics payload, ~5-50KB) and never shrink.
    MAX_MEM_ENTRIES = 64

    def __init__(self):
        self._lock = threading.Lock()
        # Per-process in-memory mirror so the lyrics view's repeated reads
        # don't hit disk on every progression tick. Bounded LRU-ish: oldest
        # insertion is dropped past MAX_MEM_ENTRIES (see _mem_put).
        self._mem = {}

    def _mem_put(self, video_id, entry):
        # Refresh recency: re-insert at the end so eviction drops genuinely
        # stale entries, then trim from the front (oldest insertion).
        self._mem.pop(video_id, None)
        self._mem[video_id] = entry
        while len(self._mem) > self.MAX_MEM_ENTRIES:
            self._mem.pop(next(iter(self._mem)))

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
        data.setdefault("results", {})
        data.setdefault("preferred_source", None)
        self._mem_put(video_id, data)
        return data

    def get_result(self, video_id):
        """Return the cached lyrics result to show: the user-pinned
        ``preferred_source`` if set and present, otherwise the
        highest-ranked one available. ``None`` if nothing usable."""
        entry = self.load(video_id)
        if not entry or not entry.get("results"):
            return None
        results = entry["results"]
        pref = entry.get("preferred_source")
        if pref and pref in results:
            return results[pref]
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

    def add_result(self, video_id, result):
        """Save a single provider's result under the source name it
        already carries in ``result["source"]``. No-op if ``result`` is
        falsy or doesn't carry lines."""
        if not video_id or not result or not result.get("lines"):
            return
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

    def _write(self, video_id, entry):
        path = _path_for(video_id)
        try:
            with self._lock:
                with open(path, "w") as f:
                    json.dump(entry, f)
            os.utime(path, None)
            self._mem_put(video_id, entry)
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

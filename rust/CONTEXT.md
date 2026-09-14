# Domain language

Terms this codebase uses. Keep the names in code, comments and commits the same as here.

## Playback

**Queue** (`src/queue.rs`): the tracks lined up, what plays now, and the rules
for moving between them. Pure data: no GTK, no GStreamer, no network. Shuffle
keeps the original order so it can be restored. A queue with a `source_id` and
`infinite` set is a radio and extends itself.

**Step**: what the queue decided should happen next: `Load(index)`, `Restart`,
`Stop`, `Extend` (fetch more radio tracks) or `Stay`. The player carries it
out; nothing else decides.

**Bounds**: whether Next and Previous have anywhere to go. The transport bar
and the system media controls both read this rather than re-deriving it.

**Generation**: a counter stamped on every load and on every event from the
audio thread, so a late event from a track the listener already skipped is
discarded instead of applied.

**Gapless arming**: handing the next URI to playbin before the current track
ends, so the switch has no gap. Armed is disarmed only when the track that was
armed is no longer the one coming up.

## Network

**Browse** (`src/net/browse.rs`): the InnerTube transport. One call: post a
body to an endpoint, get JSON back. Endpoints take this, not a client, so the
same parsing runs against the live service and against captured fixtures.

**Continuation**: YouTube returns long lists a page at a time behind a token.
`Continuation` follows those tokens for every endpoint, in both response
shapes, under one page cap and one row limit.

**Fixtures**: captured InnerTube responses under `rust/fixtures/innertube`,
not in git. `cargo test -- --ignored capture_fixtures` records them; the
offline tests skip while they are missing.

## Playlist page

**TrackList** (`src/ui/pages/track_list.rs`): the rows behind a playlist page.
`fetched` is everything the page has in the order it arrived; `rendered` is
what the list shows, sorted and cut to the chunk rendered so far. It also owns
the search text, the selection and the sort metrics.

**Sort metric**: a number YouTube keeps off the track itself (view count, date
added). Fetched once per playlist, then rows the metric does not know about
keep their order at the end.

## Home

**Shelf** (`src/net/home.rs`): one titled row of the feed, its cards, and the
seed picture a "Based on ..." heading shows beside its title.

**Bucket**: the four rows that lead the feed whatever order they arrived in:
your library, listen again, discover, forgotten.

**Dial**: the quick-picks grid at the top. It takes the quick-picks shelf out
of the feed, or borrows Listen again when the feed has none.

**Stamp**: the id `play_then_radio` puts on a queue while its radio is being
fetched, so a reply that arrives after the listener moved on can tell.

## Explore

**Category** (`src/net/explore.rs`): one mood or genre pill. A title and the
`params` that open its page; the pill row on Explore and the View All list are
both made of these.

**Chart artist**: a charted artist plus where the chart put them, the rank and
the trend arrow. Both are absent when the request is unauthenticated, and
neither belongs on a `MediaItem`.

## Listening history

**Play** (`src/net/history.rs`): one entry of the history: the track, the
heading YouTube filed it under, and the feedback token that forgets it.

**History mode**: the shared `history_mode` pref deciding when a play is
written to the account: immediate, after_30s, or never.

## Downloads

**Store** (`src/downloads/store.rs`): the download library, one SQLite table
shared with the Python app. Holds one row per downloaded track: where the file
is, what it was tagged with, and when it arrived.

**Job**: a track queued for download, plus the album it came from and its
position in it.

**Run**: what every download in one queue pass shares: the audio format, the
cookie jar written once, and the session.

**Mirror**: the `.m3u8` file under `<music>/Playlists` that tracks a downloaded
playlist. Only songs on disk are listed, and paths are relative.

**Layout**: how downloads are arranged on disk, from the folder structure
preference: Artist/Album/Song, Artist/Song, or no folders, optionally under a
`Songs` subfolder. Changing it moves existing files (`migrate_layout`).

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

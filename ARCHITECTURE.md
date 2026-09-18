# Mixtapes: architecture

Mixtapes was a Python/GTK4 app, ported to Rust one layer at a time in 2026. The
Python sources were removed once the port reached parity and are in git history
up to commit 180c3fe. This document keeps the references to them (`player/player.py`
and so on) because they explain why each part of the Rust code is shaped the way it is.

Target stack: gtk4-rs 0.11, libadwaita-rs 0.9, gstreamer-rs 0.25, tokio 1, reqwest 0.13. All GNOME crates share glib 0.22.

## What the Python app did

State is spread across three objects with no ownership boundary:

- `Player` (player/player.py) is a GObject on the GTK thread. It owns the playbin, the queue (`queue`, `original_queue`, `current_queue_index`), `shuffle_mode`, `repeat_mode`, `load_generation` and `_is_loading`. Worker threads mutate these fields directly and call `GLib.idle_add` to emit signals back on the main thread.
- `MusicClient` (api/client.py) is a process-wide singleton wrapping `ytmusicapi`. Auth state is `_is_authed` plus the `YTMusic` instance. Every page thread calls it synchronously.
- UI widgets subscribe to five Player signals: `state-changed`, `progression`, `metadata-changed`, `volume-changed`, `track-error`.

The race conditions the Python code guards against by hand (stale EOS, stale yt-dlp results, clear-while-loading) all come from that shared mutation.

## Three threads, three ownership rules

| Thread | Owns | Never touches |
|---|---|---|
| GTK main | `PlayerState` (GObject), `Player` controller, the queue, every widget | playbin, sockets |
| Audio (`mixtapes-audio`) | playbin, bus watch, position ticker | the queue, widgets, HTTP |
| tokio workers (`mixtapes-net`) | `reqwest::Client`, session headers, yt-dlp subprocesses, disk caches | widgets, playbin |

Rule 1: mutable playback state has exactly one owner, the `Player` on the GTK thread.
Rule 2: the audio thread receives commands and emits events. It knows URIs and generation numbers, never tracks.
Rule 3: the network runtime is request/response. It holds one piece of shared state, the session, behind a `std::sync::RwLock` whose critical sections never await.

## Channels versus Rc<RefCell>

`glib::Sender` and `glib::MainContext::channel` were removed in glib 0.19. The current tools:

- `Rc<RefCell<T>>` only inside the GTK thread. `Player` holds the queue in a `RefCell<Queue>`; widgets hold `Rc<Player>` or `Rc<App>`.
- `async_channel` plus `glib::spawn_future_local` for audio thread to GTK thread. The audio thread's `Sender` is `send_blocking` on an unbounded channel for control events, and `force_send` on a bounded channel of 64 for telemetry (position, spectrum). Old telemetry is evicted, control events are never lost.
- `async_channel` unbounded for GTK thread to audio thread commands. The audio thread runs a private `glib::MainContext` and polls the receiver with `ctx.spawn_local`.
- `tokio::task::JoinHandle` for GTK thread to network and back. `NetHandle::spawn(fut)` runs the future on tokio; the GTK side awaits the handle inside `glib::spawn_future_local`. `JoinHandle` is a plain `Future`, so no channel is needed and `abort()` cancels the request. Search-as-you-type keeps one `AbortHandle` and aborts it on each keystroke.
- `tokio::sync::watch<AuthState>` for unsolicited network to GTK updates. The client publishes into it; `Player::start` awaits `changed()` on the GTK executor and mirrors the value into `PlayerState`.
- `Arc<Mutex<_>>` only for the two GStreamer callbacks that run on streaming threads: `about-to-finish` reads the armed gapless URI, `source-setup` reads the cookie snapshot. Both are a lock, a property set, and a return.

`glib::idle_add` is not used anywhere. If you reach for it, you are on the wrong thread.

## Load generations

Every `AudioCommand::Load` carries a `u64` generation from a monotonic counter in `Player`. Every event the audio thread emits echoes the generation the pipeline believes it is on. The controller drops anything whose generation is not `current`. This single check replaces Python's `load_generation`, `_is_loading`, `_track_started_at` and the one second EOS grace hack.

Gapless uses the same counter. `Player::arm_gapless` allocates a generation, resolves the next track on tokio, then sends `ArmNext { uri, generation }`. On `about-to-finish` the audio thread hands the URI to playbin and stores the generation. On `STREAM_START` it switches to that generation and emits `StreamStarted`. The controller then advances the queue index without touching the pipeline. Any queue mutation sends `DisarmNext`.

## Modules

```
src/main.rs        boot order, App context, adw::Application wiring
src/bootstrap.rs   mallopt arena cap, fd limit, GSK renderer pref, tracing
src/paths.rs       XDG paths, identical to the Python app
src/model.rs       Track, VideoId, PlaybackStatus, RepeatMode, StreamInfo, HttpAuth
src/state/mod.rs   PlayerState GObject: properties, queue ListStore, signals
src/player.rs      Player controller: queue logic, generations, event application
src/audio/mod.rs   audio thread: playbin, spectrum, bus, ticker, commands
src/net/mod.rs     NetHandle: tokio runtime handle, client, resolver
src/net/ytmusic.rs session layer on the ytmusicapi crate: AuthState watch, headers_auth.json, media auth, ratings
src/net/search.rs  search endpoint on the crate's send_request plus the shelf and card parser
src/net/explore.rs explore feed, mood and genre categories, charts, and the page behind a pill
src/net/home.rs    the home feed: shelves, their cards and the order they are shown in
src/net/history.rs listening history: the plays, the token that forgets one, the ping that records one
src/net/stream.rs  StreamResolver trait, yt-dlp subprocess impl, DemoResolver, StreamCache
src/lyrics/        Lyrics service: six providers, the chain, TTML and LRC parsing, romanization, disk cache, prefs
src/state/queue_entry.rs  QueueEntry GObject, one per queue row
src/ui/mod.rs      CSS loading (style.css verbatim plus the player bar rules)
src/ui/window.rs   MainWindow shell: header, switcher, search, split view, breakpoints, actions
src/ui/player_bar.rs  PlayerBar: marquee title, artist links, like, overflow folding, compact gestures
src/ui/queue_panel.rs queue sidebar: header with count, shuffle/repeat, DnD reorder, context menu
src/ui/cover.rs    load_texture plus CoverImage: async fetch and texture cache
src/ui/like_button.rs LikeButton: click likes, hold or right-click dislikes, rates through Player
src/ui/marquee.rs  MarqueeLabel: scrolling title
src/ui/context_menu.rs song menu builder with sections, prefix action groups, popup at pointer
src/demo.rs        MIXTAPES_DEMO queue, autoplay and in-app PNG snapshots
src/ui/context.rs  UiContext (player, net, paths, compact flag) and Navigator
src/ui/pages/      home, explore (feed plus search results), category, all moods, library, history
src/ui/widgets/    scroll box, media card, song list rows, SongRow, transport, visualizer, cover picture, playing tracker
src/ui/expanded_player.rs  phone sheet: carousel, transport over visualizer, queue and lyrics tabs
src/ui/cover_view.rs       desktop cover view: Player / Lyrics toggle, split with lyrics sidebar
```

## Navigation

Each tab in the AdwViewStack is an AdwNavigationView whose root page is tagged "root". Pages never push directly: they call `Navigator::go` with a `NavRequest` (playlist, album, artist, category, all moods, search), and the window pushes onto the active tab, dismisses the cover view, and closes the search bar. A pushed page's struct is stored on its NavigationPage under the "pushed" key: that is what keeps it alive, and it is how the window finds the page the search bar should filter and the refresh button should act on.

The cover carousel keeps one page per queue entry, placeholder included: a
hidden page shifts every page index after it, which is how a tap on the cover
used to send the player back to the first track. The page a settle lands on is
resolved back to its cover widget, and the player only follows a move of at
least half a page, so a tap that snaps back changes nothing. The playing track
falls back to the artwork the bar is showing when its queue row carries none.

The cover only scrolls into place once the carousel has been laid out, which
happens when the sheet opens, so a centre asked for while it is closed is
repeated on the first frame after it appears.

A settled carousel changes the track when three things hold: the listener
touched it recently, it came to rest on a page, and the position moved from
where the gesture began. The clock runs from the last movement rather than the
gesture's start, because a slow swipe can take seconds and would otherwise be
dropped, and the baseline is taken once per gesture, so a stream of scroll
events cannot drag it along. Distance is not capped: a flick carries several
covers and plays the one it lands on. A settle that never moved means the
carousel is out of step with the queue, so it is put back on the playing
track rather than followed.

The three-dot menu is built whenever the track changes, not when it opens: a
menu button with no model is insensitive, so a lazily built menu can never be
clicked. Both player views carry it, with the song entries minus the ones they
already show (play next, add to queue, go to artist, go to album) plus Stream
Info, which asks the audio thread for the live pipeline state through
`AudioCommand::Describe` and renders it in the dialog `present_stream_info`
builds for either view.

Expansion follows the Python split: on desktop the main GtkStack crossfades from "browser" to the cover view and the back button dismisses it; under 500px the player bar and switcher move into AdwBottomSheet's bottom bar and the expanded player becomes the sheet, so the bar's tap and drag-up (and the sheet's own swipe) open it.

## How the UI will consume state

Widgets bind to `PlayerState` properties with `bind_property`, `connect_notify_local`, or `gtk::PropertyExpression`. The queue is a `gio::ListStore` of `QueueEntry` GObjects (index, title, artist, playing). Row factories bind with expressions rooted at the list item, so recycling needs no unbind code; `Player::mark_current` flips the `playing` flag on the two affected entries. Two signals remain for things that are events rather than state: `track-error` (toast) and `queue-changed` (structural rebuild).

Widgets call methods on `Rc<Player>` for every action: `play_tracks`, `next`, `previous`, `toggle_play`, `seek`, `set_volume`, queue edits. They never send `AudioCommand` themselves and never hold a `NetHandle` for playback.

Pages that fetch data get the `NetHandle`, call `net.spawn(client.some_endpoint(args))`, and await the handle in `glib::spawn_future_local`. Results are plain `serde` structs; the page converts them to GObjects for list models on the GTK thread.

## Network client

The `ytmusicapi` crate supplies browser-cookie auth (SAPISIDHASH), the WEB_REMIX context and `send_request`, plus playlists, liked songs, ratings and `get_song`. It has no search, browse or account endpoints, so those live in `net/` on top of `send_request`. `YtMusic` wraps an `Arc<YTMusicClient>` behind an `RwLock` and rebuilds it on login and logout, since the crate client is immutable. The crate reads the same `headers_auth.json` the Python app wrote: keys are normalized to Title-Case and the crate matches them case-insensitively. The crate brings reqwest 0.12 with native TLS alongside the app's reqwest 0.13 with rustls, so OpenSSL headers are a build dependency now.

Search runs the unfiltered call plus songs, artists, community playlists and albums concurrently on tokio, merges them in that order and drops duplicates by id, as search.py did. Songs under a top-result artist card inherit the card's artist. `cargo test -- --ignored live_search --nocapture` prints a live parse for inspection.

## Auth state machine

`AuthState` lives in `net::ytmusic`:

- `Anonymous`: no `headers_auth.json`.
- `Unverified`: headers loaded at startup, no round trip yet. Requests are sent with the cookie. Mirrors `try_login(skip_validation=True)`.
- `Authenticated(AccountInfo)`: `account/account_menu` returned an active account.
- `Invalid(reason)`: the server rejected the session. The UI opens the login dialog.

`validate()` runs once from `Player::start` and again whenever the network comes back. Offline keeps `Unverified` so the cached library still works. `login()` accepts a browser.json path, a JSON object or a raw header block, normalizes it exactly like `_normalize_headers`, and saves it only after the server confirms it.

## Stream resolution

`StreamResolver` is the seam that replaces yt-dlp. `YtDlpResolver` shells out with the same format policy and PO-token setup the Python app uses, so playback works from day one. `StreamCache` reads and writes the exact JSON files the Python app left in `~/.cache/muse/streams`.

`PlayerEndpointResolver` (`net/player_endpoint.rs`) sits in front of it since 2026-09-18. It posts one `player` request as the VISIONOS client, which returns direct opus URLs that need no signature, no PO token and no cookies, and serve open-ended ranges. Measured: 0.26 s to resolve and 0.84 s from click to sound, against about 6 s through yt-dlp (a Python start, seven player clients, player.js under node). It needs a visitor id, read off the music.youtube.com landing page and warmed at startup, and it probes two bytes at offset 200000 before trusting a URL, because a client YouTube gates serves only a 100 KB preview (ANDROID_VR does, tested with and without botguard tokens). Whatever VISIONOS declines, such as uploads and age-gated or private videos, goes to yt-dlp with the session as before. The idea came from limusic, which uses the same client as its first fallback.

## Verifying playback without pages

`MIXTAPES_DEMO=1` stages a queue at startup from audio files under ~/Music (or `MIXTAPES_DEMO_URI`), `MIXTAPES_DEMO_VIDEO=<id>` adds a real YouTube track through yt-dlp, `MIXTAPES_DEMO_AUTOPLAY=1` presses play after three seconds, `MIXTAPES_DEMO_QUEUE=1` opens the sidebar, `MIXTAPES_DEMO_EXPAND=1` opens the expanded player, `MIXTAPES_DEMO_TAB` and `MIXTAPES_DEMO_SEARCH` pick a tab or run a live search, `MIXTAPES_DEMO_ACTIVATE=1` plays the first result, `MIXTAPES_DEMO_WIDTH=420` starts in the phone layout, `MIXTAPES_DEMO_LOGIN=1` opens the sign-in dialog and snapshots it, and `MIXTAPES_DEMO_SNAPSHOT=<prefix>` writes PNGs of the window from inside GTK. `demo:` ids are served by `DemoResolver` in front of the real resolver, so the controller path is identical to production.

## Comparing against the Python app

The Python app was removed from the tree on 2026-09-18, after the parity audit
below. While both existed, `tools/pysnap.py` rendered the Python window to a
PNG under the same scenario as a Rust demo run, and the two were diffed with
PIL, element by element. That is how the pages, the preferences dialog and the
cover theming were checked pixel for pixel. `tools/cover_effects_ref.py` asked
Pillow for the expected values in the `cover_effects` tests. Both scripts need
the Python sources, so they went with them: they are in git history up to
commit 180c3fe, along with `src/`.

## Library and sign-in

`net/library.rs` ports the library calls the Python page used. `browse_all` fetches a browse id and follows every continuation the way ytmusicapi's `get_continuations` does: the token sits under `continuations[0].nextContinuationData`, the next page arrives under `continuationContents.gridContinuation` or `musicShelfContinuation`. Automatic playlists ("Sounds from Shorts", "Episodes for Later") only exist on later pages, so a single request loses them. Two-letter ids sort first like library.py. `playlist()` maps the `LM` id to `get_liked_songs` and everything else to `get_playlist`.

`ui/pages/library.rs` keeps one `gio::ListStore<MediaObject>` per section. The list mode binds rows with `bind_model`, the grid mode rebuilds cards on `items-changed`, and `widgets/card_grid.rs` ports `CardWrapLayout`: it scores candidate column counts per width and resizes cards so a row fills the page, which is why five columns appear on the desktop and two on a phone. The Downloads entry is spliced in at index 1 after Liked Music, the view mode round-trips through `library_view_mode` in prefs.json, and the page loads on the first `authenticated` notification and clears on logout.

`widgets/card_grid.rs` holds two layout managers instead of a tick callback. `CardGridLayout` sizes the cards inside measure(VERTICAL, width), so the heights that pass reports already account for the new cover size and a resize paints once; sizing after allocation left the cards a frame behind the window and the grid flickered while dragging, which is the pitfall media_card.py's CardWrapLayout documents. `CardGrid` is the widget that carries it. `CardLayout` is CardBinLayout: a card's width is its size request and its height is measured against that width, so a strip never pulls a short row apart.

## Connectivity

`net/online.rs` ports the probe in ui/utils.py. Gio.NetworkMonitor stays silent for some transitions and calls a link available when DNS still fails, so the window only lets it trigger a probe after a 1.5 s settle; a TCP connect to music.youtube.com:443 on tokio decides, and a 5 s tick backstops the monitor (every tick while offline, every 30 s while online). `is_online` never blocks: it answers from the last probe, optimistic before the first, and honours the `force_offline` pref that both apps share. Listeners fire once per real transition: `MainWindow::apply_network_state` toasts, reloads Library, Explore and Home when back online and revalidates the session, and greys the library lists and reloads Explore and Home into their offline screens when the link drops. Home and Explore own the load state machine from their Python pages (`load_home_data`, `load_explore_data`): one fetch at a time, offline straight to the status page, transient errors retried with growing delays before a Retry button.

`ui/login.rs` is login.py: an `adw::Window` with three `ViewStack` pages, a WebKitGTK view of the Google sign-in page (`resource-load-started` sniffs the browse request headers for SAPISID, falling back to the cookie jar), a browser.json import, and a manual header paste. Every path ends in `YtMusic::login`, which writes headers_auth.json, rebuilds the crate client and flips the auth watch channel. `MainWindow::check_auth_on_startup` waits half a second after presenting and opens the dialog when the saved session is missing or invalid and the network monitor reports connectivity, otherwise it toasts. `MediaObject` in `state/media_object.rs` wraps `MediaItem` so stores and expressions see GObject properties without the widgets becoming subclasses.

## Playlist and album pages

`net/playlists.rs` and `net/items.rs` carry the endpoints the crate lacks, each a port of the MusicClient method of the same name: playlist details parsed from the raw browse response (the crate's parser drops the like status the rows show), `get_album` with the 2024 responsive header, uploaded albums, the watch panel behind radios and the song-version swap, `get_album_browse_id` read off the playlist page HTML, rating, editing and row removal, the youtube.com view counts and the date-added values behind the last two sort entries, the artist albums grid, and the raw channel parsers used as fallbacks. Paging follows `get_continuations_2025`: the limit bounds the rows continuations add, never the first page. `net/cache.rs` holds what MusicClient cached in memory: full track lists, sort metrics and the saved-playlist ids behind the Add to Library toggle.

`ui/pages/playlist.rs` is playlist.py, which album.py only subclassed to force the album view; one page serves user playlists, Liked Music, MPRE and OLAK albums, uploaded albums, radios and the virtual Downloads list, and the window routes `NavRequest::Album` to it. The track list is a `ListView` over a flattened model, a one-item header store carrying the whole header widget ahead of the filtered track store, so the header scrolls with the rows and the view stays virtualized. `widgets/track_row.rs` is the factory row with its lazily created check box, track number, badges and download icon; it reaches the page through a `Weak<dyn TrackRowHost>`. Rows populate in chunks after the page transition, the search bar filters the visible page instead of running a search, the sort dropdown fetches its metric once per playlist, multi-select drives the selection bar and the row menu's selection entries, and the more menu rebuilds lazily when its popover opens. Play routes through the live queue when it came from this page. The header-bar refresh button now targets a visible user playlist as well as the library root, polling the page's inline spinner. `widgets/add_to_playlist.rs` is the popover with covers, search and recents in `playlist_recents.json`; `crop_dialog.rs` is the square cropper behind the edit dialog's cover picker. `pages/discography.rs` is the artist albums grid on the same justified layout.

The playlist disk cache is `PlaylistDiskCache` in `net/cache.rs`, a JSON file per playlist under the data directory standing in for DownloadDB's library_cache table: header fields, author, count and rows. The page reads it on the runtime when it opens (header and rows render before the live fetch, which then waits the two seconds the Python page waited), serves the whole page from it offline, writes it 1.5 s after each fetch with the same no-regression guard, and deletes it when rows are removed or the page is refreshed. The library's offline greying keeps rows live when a cached copy with rows exists.

Two deliberate differences: the Python edit dialog's save job never called the edit endpoint, only mirrored the cover and reloaded, so this port sends changed title, description and privacy too. Download badges and Download All have no data source until the download manager lands, and the yt-dlp flat enumeration stage of the full fetch is not ported.

## Playback details that matched the Python player late

A gapless handoff used to report `Loading` the moment `about-to-finish` handed playbin the armed URI, a second before the switch. Audio kept playing, but the slider froze and the visualizer stopped ticking for that second, since both follow the Playing status. The switch reports itself through `StreamStart` instead, and the player drops the previous stream's spectrum frames there: they are keyed by stream time, which restarts, so they would sit in front of the queue with times the new play-head never reaches. Starting a load also tears the pipeline down at once, as `set_queue` and `play_queue_index` did, so an uncached stream resolving for ten seconds no longer plays the old track underneath; the gapless path never goes through there.

Seeks are `FLUSH | ACCURATE` with a key-unit fallback, as in `Player.seek`; a key-unit seek lands on the previous cluster, which showed as the slider jumping back after a seek. The visualizer no longer draws the newest spectrum frame: the audio thread tags each frame with its stream time, the player queues them like `_viz_queue`, and `pull_visualizer_bands` releases the latest frame the play-head has reached, interpolating between the 100 ms position ticks. Seeks and new streams clear the queue. Every page scroller gets `suppress_hover_while_scrolling`, the Python trick that disables hit-testing on the content while it moves so no row is restyled under a stationary pointer. Covers decode on the runtime, since `GdkTexture` is thread-safe. The list view builds the same number of rows Python does (409 for a 945-track list), so the remaining smoothness difference is the debug build; use `cargo build --release` to compare.

## Artist pages and radio

`net/artist.rs` is MusicClient.get_artist and ArtistPage._fetch_artist together: the immersive header (name, subscriber count, subscribed flag, radio and shuffle ids, the channel id the subscribe endpoints take), the description shelf, the top songs shelf parsed as playlist rows, and the carousels matched by their English titles the way ytmusicapi's parse_channel_contents does ("Albums", "Singles & EPs", "Videos", "Playlists", "Fans might also like"), with the raw scan for the playlist and featured carousels it skips. The deep fetches the Python page joined before rendering, the full top songs playlist and the detailed albums and singles grids, run concurrently with the same ten-second bound. Plain channels fall back to the visual header like get_user did. Subscribing and unsubscribing hit the subscription endpoints, and the library load feeds the subscription set the page checks first.

`ui/pages/artist.rs` is artist.py: banner in a `FadeBottomBin` (the mask-node fade, active only in blur mode) under the scrim, the info box overlaid in the same grid cell, Play, Shuffle, Radio and Subscribe, the description with Read more, then the sections in order with their limits, Load More for top songs and View All into the discography page for the rest. Rows and cards route through the shared menus, and every artist navigation in the app (menus, cards, the playlist header's artist links, the player views) opens this page; a player-bar artist without an id resolves its channel from the player response first.

Radio is `Player::start_radio` for songs, playlists and artists, replacing the queue with the watch panel's fifty tracks as an infinite queue sourced from the radio id. `maybe_extend_infinite` runs after every advance and fetches more from the queue's tail when fewer than fifteen rows remain or the play-head is past halfway, retrying from the playing track when the batch was all duplicates; `force_radio_extend` is the last resort when the queue runs dry and accepts repeats over silence, as the Python player did.

## System media controls

`mpris.rs` is player/mpris.py, which handed mprisify an adapter onto the
player. The split matches the audio thread: `Server` runs on the tokio
runtime, so its implementation is `Send` and never touches the player. It
answers from a snapshot the GTK thread writes under a mutex, and posts
commands back over an async-channel the GTK thread drains, applying them
through the controller so seeks and volume reach GStreamer the usual way.

Every `PlayerState` notification mirrors into the snapshot. Changes are held
until the next idle and deduplicated by kind, so a track change sends one
`PropertiesChanged` carrying Metadata with CanGoNext and CanGoPrevious rather
than four signals. Position is excluded from that, as the spec requires: the
snapshot stores its last sample with an `Instant`, so a poll between the
100 ms ticks reads a live value, and `PlayerState` gained a `seeked` signal
that `Player::seek` emits and the bridge forwards as `Seeked`.

Cover art is a local file, not the address the track carries: shells load
`file://` reliably and the carried address is often a dead ytimg quality.
When the track changes, the art is fetched through the same fallback chain the
covers use, centre-cropped to a square, upscaled past 512 px and written to
`<cache>/mpris/mpris_art_<id>.jpg`, one file at a time. The metadata keeps the
remote address until the file lands, and the file is held separately from it
so a later metadata refresh cannot put the address back.

Details that follow the Python adapter: Loading reports as Playing so the
shell does not blink between tracks, the track id is the sanitized video id
under `/com/pocoguy/Muse/track`, CanGoPrevious is answered live because
Previous restarts the track past three seconds, and an idle player gets its
own path rather than `NO_TRACK`, which is reserved for track lists. Play on a
stopped player loads the staged track, which the shell expects and the
controller's plain `play()` does not do. The bus name is released and the
server dropped from the application's shutdown handler, which is also what
closing the last window reaches when background play is off; with it on, the
window hides, the app stays up and the shell keeps its controls.

## Explore

`net/explore.rs` is the Explore feed's whole network side, ported from
ytmusicapi's `get_explore`, `get_mood_categories` and `get_charts` plus
MusicClient.get_category_page. `load_explore` is the shape `_fetch_explore`
had: the feed decides whether the page has anything to show, while the
categories and the charts run beside it and are dropped if they fail. The feed
itself carries a mood and genre row of its own, which the page falls back to
when the categories call is the one that failed.

Charts read their shelves by position, the way ytmusicapi does: the country
menu first, then the video playlists, a genre row on US only, and the artists.
A premium account gets daily and weekly rows in place of the one video row,
which is the extra shelf that tells them apart. Chart artists keep the rank and
the trend arrow beside the item, since neither belongs on a `MediaItem`, and
both are absent when the request is unauthenticated.

The country menu writes `charts_country` into the prefs both apps share and
reloads the feed. Python saved the same key but called `load_explore_data()`
without forcing, which its own guard turned into a no-op, so the charts never
followed the menu there.

A pill opens `pages/category.rs`, which keeps the page struct on its
NavigationPage: the fetch renders through a weak reference, so without that the
page is dropped the moment the push returns and nothing appears. Past twenty
pills the row ends in View All, which opens `pages/all_moods.rs`, the one page
the search bar filters rather than searching from.

## Home

`net/home.rs` is the feed: `get_home` posts `FEmusic_home`, parses the shelves
and follows the section-list token until it has the 25 rows the page asks for.
A page that fails keeps what came before it, since a short feed beats an error
screen.

Python parsed that response twice. ytmusicapi's `parse_mixed_content` gave it
the rows, then `get_home_full` re-read the raw response by hand for two things
that parser drops: the strapline thumbnail on a "Based on ..." heading, and the
`musicVideoType` that says whether a card is a song or a music video. The two
were stitched together by shelf title. This parses once and keeps all three, so
a shelf cannot take another's art by sharing its title, and the video type is
there for every shelf rather than only the ones in the first response. That
second part changes what is drawn: Python guesses at the kind of a card past
the first page, and a shelf of songs it guesses wrong about renders as cards
instead of the list it should be.

`items::parse_mixed_item` is the dispatch those shelves share with a category
page: the page a card's title opens says what the card is, and a card that only
plays is told apart by its video type, then by the shelf it sits in, then by
its thumbnail address — home.py's `_detect_kind`, in the parser rather than at
every use.

`home::arrange` is the ordering from `_populate_feed`: podcast shelves and
empty ones go, the quick-picks row leaves the feed for the dial (borrowing
Listen again when there is none), and the four named rows lead whatever order
YouTube sent them in.

Activating a song on Home plays its whole shelf and then keeps going, which is
`Player::play_then_radio`: the queue is stamped with an id of its own, the
watch playlist for the shelf's last track is fetched, and when it lands the
stamp is replaced by the real playlist id so the infinite extender takes over.
A listener who has moved on is left alone: the reply is dropped unless the
stamp is still the one on the queue.

## Listening history

`net/history.rs` is the account's plays: `get_history` reads the shelves of
`FEmusic_history`, keeping each row's heading ("Today", "Last week") and the
feedback token that forgets it. A `HistoryEntry` is the track plus those two,
since neither belongs on a `Track` that the queue and the player also carry.

The page keeps its rows in one flat list. Activating one plays from there
through the rest of the history under the source id `HISTORY`, and its menu
carries the two entries history.py adds: Play, and Remove from History when
the account is one YouTube offers that for. A removal is optimistic — the row
and the cache entry go at once, the account is told afterwards — because the
alternative is a page that sits still after a click.

The cache is the `history_cache` row in the same SQLite file the downloads
live in, which the Python app reads and writes too. It is stored in
ytmusicapi's dict shape rather than this crate's types, so neither app can
hand the other something it cannot read. It is what the page paints first,
and what an offline open shows.

Plays are recorded by `Player::record_play`, port of add_history_item_async:
the `player` endpoint hands out a playback tracker URL for the video, and a
GET to it with the session's cookies is what puts the play in the history.
`history_mode` in the shared prefs decides when — "immediate" as the track
loads, "after_30s" once it has played that long, "never" not at all — and the
play is prepended to the cache so the page shows it before YouTube's own
roll-up catches up.

"Your Channel" is the same artist page as any other: the account's `@handle`
resolves through `navigation/resolve_url` to a channel id, once per session.

## Playing the audio version

A music video and the song it belongs to are two videos, and the song is the
cleaner master. `spawn_resolve` therefore looks for the twin before yt-dlp
runs, so the id the resolver and the stream cache see is the one that will
play. What comes back replaces the playing queue entry under its own id
(`Queue::swap_current`, which `refine_current` refuses on purpose since a
changed id normally means the queue moved on), keeping the album, duration and
rating the entry already had.

Two things keep the cost down. A track that already says it is the audio
version is not asked about, and any id is asked about once per session.
Python asks unconditionally and remembers only the hits, which is a request
per play for every song whose twin does not exist. Gapless arming skips the
lookup entirely: the swap belongs to the load path, as it does in Python.

## Following the output device

`pulsesink` fills its own `device` property in with the sink it landed on, and
from its second connection onwards asks for that device by name. PipeWire
never moves a stream that names its device, so plugging in headphones moved
every other application's audio across and left this one on the speakers until
it was restarted. Only the second track onwards was affected, which is why it
looked like the app simply ignored the new device.

The engine therefore owns its `autoaudiosink` rather than letting playbin make
one, and clears every string `device` property under it before each load. A
stream that asks for the default is moved with all the others, and the next
track opens on whatever is default by then. Gapless transitions never pass
through here, and do not need to: the stream they continue is already one that
asked for the default.

## Not ported yet, and where it attaches

Audited against the Python tree on 2026-09-14. The two smallest entries,
library search and the audio-version swap, were closed the same day. The
settings dialog, scrobbling, Discord Rich Presence, cover theming and the
lyrics view landed on 2026-09-18, see "Settings, presence and theming" below.

- Search drops podcast, episode and profile rows. Python lists them under
  "More results" in the Other tab. `ItemKind` has no kind for them, and the
  app has no page that opens one.
- Non-seekable streams: fragmented m4a, which is what uploads come as, seeks
  through playbin's download flag (`set_download_buffering`). Python's tmpfs
  staging and its `noseek_vids.json` memory are not ported and not needed for
  that case. A stream that refuses to seek for another reason stays unseekable.
- Windows support: system media controls, a tray icon and a sign-in webview.
  The Python app had them (`player/smtc.py`, `ui/tray_win.py`,
  `ui/login_webview_win.py`, in git history). `fonts/` is kept for this port.
  `discord.rs` uses a Unix socket and needs a named-pipe transport there.
- The Nix flake builds the Rust crate but was written without a working Nix
  store to test it on.
- Podcast shows in search results. Episodes are listed and play, a show has no
  page to open.

Dead in the Python tree, deliberately skipped: `ui/pages/mix.py`,
`ui/pages/mood.py`, `ui/pages/album.py` and `ui/queue.py` are never
constructed by anything.

## Queue, transport and track list

Three modules own rules the rest of the app used to restate.

`src/queue.rs` holds the queue: tracks, the play head, shuffle and repeat, and
the radio source. It answers with a `Step` (`Load`, `Restart`, `Stop`,
`Extend`, `Stay`) and with `Bounds` (can next, can previous). The player
carries out the step and tells the audio thread; the transport bar and MPRIS
read the bounds. Shuffle keeps the original order so toggling it back restores
it, repeat-all wraps on Next but repeat-track does not, and a failed track
never wraps or extends a radio, because every track could fail. The module is
pure data and covered by unit tests.

`src/net/browse.rs` is the InnerTube seam. `Browse` is one call: post a body to
an endpoint, get JSON. The `ytmusicapi` client implements it for the app, and
`Fixtures` replays captured responses for tests. Endpoints take `&dyn Browse`.
`Continuation` follows continuation tokens for all of them: both response
shapes, both legacy token spellings, one page cap, one row limit, rows kept
when a page fails. Capture with `cargo test -- --ignored capture_fixtures`;
`fixtures/` is gitignored and the offline tests skip when it is absent.

`src/ui/pages/track_list.rs` holds the playlist page's rows: fetched order,
rendered order, search text, selection and sort metrics. The page renders what
it returns and mirrors it into the GTK store. The "which list is the source"
question and the sort rules live there, not at fifteen call sites.

## Cover sizes

A cover is asked for at twice the size it will be drawn at, which is what
`get_high_res_url(url, target_size)` did. Asking for the largest copy and
letting GTK scale it down looks worse, not better: the scaling happens in sRGB,
so a 544 px cover squeezed into a 56 px row comes out measurably darker and
duller than the same picture fetched at 112 px (mean saturation 88 against 95,
channels off by up to 95 against the Python render). It is also fifteen times
the bytes per row, which is why history and library rows used to fill in slowly.

`CoverImage` passes its own size, so a resize or a compact toggle changes what
the next load asks for. `None` means the largest copy and is for the three
places that want it: the cover view's full-window picture, the art written for
the media controls, and the art embedded in a download.

## Playlist covers

Setting a cover goes through YouTube's resumable upload, ported from
set_playlist_thumbnail: ask `playlist_image_upload/playlist_custom_thumbnail`
for an upload URL with the browser session headers, send the bytes, then bind
the blob id it returns through `browse/edit_playlist` with
ACTION_SET_CUSTOM_THUMBNAIL. The crop is capped at 1024 pixels first.

The chosen image is also mirrored under `<music>/Playlists` so the page shows
it at once, and its `.jpg.url` sidecar is left on the address of the cover
being replaced. The mirror then leaves the file alone until YouTube serves a
different address, which is the uploaded image. Deleting that sidecar instead,
as the port first did, made the mirror pull the old cover straight back over
the new one, which looked like the change had no effect.

A mirrored cover keeps its path when its picture changes, and the texture cache
is keyed by path, so replacing the file is not enough: the page would keep
drawing the old image a few seconds later, when the reload pulled that path
back in. `forget_texture` drops the cached decode after an edit and after the
mirror replaces a file, and the cover reloads if it is the one on screen.

A playlist is not readable the moment it is created, so `await_playlist` polls
until the browse endpoint serves it before the new page is opened.

## Uploads

`src/net/uploads.rs` is the uploaded library: `upload_song` sends a file to
upload.youtube.com in the two resumable steps ytmusicapi uses, `delete_entity`
removes a song or an album, `artist_songs` lists one artist's uploads, and
`upload_stream` asks the player endpoint where an upload plays from.

Uploads answer browse requests with two tabs, Library and Uploads, and the rows
are in the second one. `items::library_sections` takes the first tab that has
sections, which is why the uploaded library reads at all: pointing at the first
tab returned nothing and the whole library looked empty.

The upload queue lives in `ui/upload_queue.rs` behind the header's upload pie,
one file at a time, and the library reloads when it drains. Uploaded albums can
be deleted from their card, and an uploaded artist opens a page of their songs.

Uploaded tracks stream and download like anything else, through `net/potoken.rs`.
YouTube serves them to the `web_music` client alone, and gates that client's
audio formats behind a GVS PO token bound to the video id. Without one, yt-dlp
finds no formats and reports the track as "Video unavailable", which is what
made the uploaded library look unplayable. `rustypipe-botguard` mints a token
in about 30ms once it has a snapshot; tokens are cached until shortly before
they expire and passed as `--extractor-args youtube:po_token=web_music.gvs+...`
by both the resolver and the download path. The `youtubepot-rustypipebotguard`
argument the Python app passes does nothing here, because that provider is a
yt-dlp plugin and only the binary is installed. Minting our own token sidesteps
the plugin entirely, and it also restores the PO-token gated Opus formats for
ordinary songs.

## Library actions

`sync_store` brings a section's model in line with a fetch without disturbing
what did not change: matching rows at each end stay, and the span between them
is replaced in one splice. It also ignores the signature on a thumbnail
address, because YouTube signs those per request and comparing them raw marks
almost every row changed, which rebuilt the grid and made every cover blink on
each reload. The cost is that a new cover keeps its old address, so the page
that set one tells the library outright through `refresh_library_card`.

The library reloads when it comes back into view, so a rename, a new cover or
a deletion made on a playlist page is there without reaching for the refresh
button. A page edit also asks for a reload, but that one runs while YouTube is
still serving the old card: the address of a changed cover takes a second or
two to turn over. A library shown again within two seconds is left alone, so
flicking between tabs does not refetch.

Right-clicking a library playlist offers what the Python library offered: one
of yours can be deleted, one you saved can be dropped again. Ownership is read
from the card the way `is_own_playlist` reads it from a page, so the same rule
covers both, and a playlist page opened from the disk cache knows it too rather
than waiting for the live fetch to enable Edit and Delete.

The + beside Playlists opens the same dialog Python had (title, description,
visibility, private by default), creates the playlist through
`playlists::create_playlist`, refreshes the library and opens the new page.
The uploads tab's all-songs button pushes a virtual playlist page over
`get_upload_songs`, the way the Downloads page sits over the download library.

## Downloads

`src/downloads/` is the offline half of the app, a port of downloads.py.

`store.rs` owns the library: one SQLite table at `<music>/.mixtapes/library.db`,
the same file and columns the Python app writes, so a song downloaded in either
app is known to both. It also renames a pre-rename `~/Music/YouTube Music`
folder on first open and rewrites the rows that pointed into it. `is_downloaded`
answers from an in-memory map because list rows ask it once per row, and a row
whose file vanished is dropped as it is read.

`mod.rs` is the manager. A job is a track plus the album it came from. It fills
in missing metadata (`get_watch_playlist`, then `get_album` for album artist,
year and track number), hands the audio to yt-dlp with the format from prefs,
embeds tags and cover art with lofty, moves the file into
`<music>/[Songs/]<artist>/<album>/`, and records it. Three downloads run at
once. Progress comes back through an `async_channel` of `Event`, which the GTK
side pumps in `UiContext::pump_downloads` and hands to listeners: the header
pie and its popover rows, and the playlist page's row badges.

`naming.rs` holds the layout rules (format table, folder structure, the
filename byte budget against the filesystem limit), `tags.rs` the tag and cover
writing, `m3u.rs` the `.m3u8` mirrors under `<music>/Playlists` that follow a
downloaded playlist and are pruned or repointed when files go or move.

The player checks the library before resolving a stream, so a downloaded track
plays instantly and offline, gapless arming included. The cover of a downloaded
track is cached under `<cache>/local_covers` and rows prefer it, which is what
makes art appear with no connection.

Settings for format and folder layout are not ported yet: the values come from
the shared prefs.json, so changing them in the Python app changes both.
`Downloads::migrate_layout` is ready for the settings page to call.

## Lyrics

`src/lyrics/` is the lyrics backend: the lyrics half of api/client.py plus
player/lyrics_cache.py and player/lyrics_prefs.py. No widgets live there.
`Lyrics` is the one handle the UI needs, built from `Paths`, a
`reqwest::Client` and the `YtMusic` session. It is `Send + Sync`, cheap to
clone, and every call is an async fn for `NetHandle::spawn`.

`providers/` holds the six sources behind the `Source` trait: Apple Music
through the Paxsenix proxy (iTunes Search first, the scraped amp-api token as
the fallback), BetterLyrics, BiniLyrics, NetEase, LRCLIB and YouTube Music.
Each file keeps its parsing in pure functions over `serde_json::Value`, so the
tests run on hand-written responses, and `chain.rs` is tested against a
scripted `Source`. `ttml.rs` reads the TTML three of them serve (word spans,
background vocals, duet voices, Apple's translation and transliteration
blocks), `lrc.rs` the LRC the other two serve, `matching.rs` the gate that
keeps a different song out, and `romanize.rs` the second line: Hangul and
Cyrillic by table, Japanese and Chinese from a provider when one has the
reading and from the `kakasi` and `pinyin` dictionaries when none does.

`chain.rs` walks the queue from `prefs.rs`, skips a provider that cannot beat
the result in hand or that is in a back-off window, and gives the winner its
second line. `cache.rs` is one JSON file per video id under
`<cache>/lyrics`, the Python app's own files with its pipeline version, and
`prefs.rs` reads and writes the `lyrics_*` keys of the shared prefs.json, so
either app shows what the other fetched, pinned and configured.

YouTube Music timed lyrics come from the Android client, as in ytmusicapi.
That request is sent with no session: InnerTube answered 400 when the browser
session's cookie and SAPISIDHASH went out with the Android client, and lyrics
are the same for every account. The web client stays the plain-text fallback.

## Settings, presence and theming

Landed 2026-09-18. Each one writes or reads the files the Python app uses, so
both apps stay interchangeable.

- `ui/preferences.rs` and `ui/preferences_lyrics.rs` build the preferences
  dialog. Every row saves to the shared `prefs.json` and applies live through
  small `MainWindow` methods (`set_sidebar_on_right`, `force_offline_changed`,
  `appearance_pref_changed`, `visualizers`, `lyrics_views`). Debug logs flip a
  reloadable tracing filter in `bootstrap.rs`.
- `scrobbler.rs` is `Send + Sync` behind a mutex. The GTK thread feeds the
  listening clock from position ticks, and one tokio task owns the network and
  the on-disk backlog (`scrobbler.json`, `scrobble_queue.json`).
- `discord.rs` pins the IPC socket to a std thread. The GTK thread builds the
  activity JSON and sends it over a channel. The worker coalesces updates and
  resends the last one after a reconnect.
- `presence.rs` wires both to the player. `Player::on_play` tells a fresh play
  (`Started`) from a metadata correction (`Refined`), so the scrobble clock
  never restarts on the audio-version swap.
- A demo run mutes scrobbling and Discord. `MIXTAPES_DEMO_PRESENCE=1` lifts it.
- `ui/appearance.rs` owns three display-wide CSS providers: the blurred cover
  background, the cover accent with its optional tint, and the derived colors
  (`playing_fg`, `blur_sidebar_bg`, `visualizer_bar`). `ui/cover_effects.rs`
  ports the PIL pipeline by hand, because the `image` crate rounds differently.
  `ui/color_utils.rs` is the OKLCH and WCAG math.
- `ui/widgets/lyric_rows.rs` holds `LyricRow` and `InterludeRow`, GObject
  subclasses that paint the sung line in `snapshot`. `ui/widgets/lyrics_view.rs`
  is the container: fetch generations, activation, autoscroll and the source
  picker. Both live views register in a thread-local list, so a display pref
  change reaches both.
- Pitfall hit again: `css_classes([...])` on a builder replaces the classes an
  icon button brings (`image-button`), which shifts its size. Add classes after
  `build()`.

## Parity audit, 2026-09-18

Three passes over the Python tree by slice (playback and backend, window and
widgets, pages and API client), each reported gap checked against both trees
before it was fixed. Closed the same day:

- Album and playlist card menus: Play, Play Next, Add to Queue, Go to Artist
  and Start Radio, with the tracks fetched when asked for. Artist cards: Start
  Radio, from the artist's own radio or their top song (`ui/context_menu.rs`).
- Song menu: Refresh Metadata, through `Player::refresh_track_metadata` and
  `Queue::refresh_metadata`.
- Offline library: `library_cache.json` in the data dir holds the five library
  sections as they last loaded. The page fills from it before any fetch, so it
  is also what shows at startup. A signed-out app deletes it. Python kept the
  same thing in library.db tables in ytmusicapi's dict shape, which the port
  does not read or write.
- Offline search: `local_results` in `ui/pages/explore.rs` searches downloads.
- The player bar's album button asks the watch panel when the track has no album.
- `PlayerState::source-video-id` and `is_playing_id`: a row holding a music
  video's id keeps its highlight after the audio-version swap.
- `Player::precache_neighbours`: three tracks either side resolve into the
  stream cache once a track plays.
- "Top Result" only heads a card YouTube sent as one.
- Error toasts are trimmed (`summarize_error`), a missing album is backfilled
  for Discord and scrobbles (`backfill_album`), Previous restarts after 5 s,
  and Ctrl+slash opens the shortcuts dialog.

## Resources and packaging

`build.rs` compiles `resources/resources.gresource.xml` into the binary with
`glib-build-tools`: `resources/style.css` and the symbolic icons from the
repository's `assets/icons`, which stays the single copy because the README
and the Flatpak repo file link to it. Nothing is looked up on disk at run
time. The icon theme's resource path names the theme folder
(`/com/pocoguy/muse/icons/hicolor`), since GTK looks under
`<path>/scalable/actions`. The folder is `resources/` and not `data/` because
the root already has an ignored `data/` holding a saved browser session.

The crate sits at the repository root since 2026-09-18. It lived in `rust/`
while the Python app occupied `src/`.

The AUR PKGBUILD and the Flatpak manifest build the crate with cargo and
install one binary (`mixtapes`, with `muse` as a link), the desktop file, the
metainfo and the app icon. `rustypipe-botguard` still ships beside it
(`/usr/lib/mixtapes/bin`, or `/app/bin`), and yt-dlp with Node stays a runtime
dependency for the fallback resolver and downloads.

## Stutter, measured with sysprof

Method: a release build with `-C force-frame-pointers=yes` in its own target
dir (`target-prof`), `sysprof-cli -- ./target-prof/release/mixtapes` with a
demo scenario, then `sysprof-cat` and a small script that merges the callgraph
by symbol. `MIXTAPES_DEMO_FRAMES=1` logs frame pacing, and these hooks repeat
an action so it dominates a capture: `MIXTAPES_DEMO_TOGGLE`,
`MIXTAPES_DEMO_RESIZE`, `MIXTAPES_DEMO_NEXT_EVERY`. `MIXTAPES_DEMO_CLASSES`
dumps every widget's classes and state after each toggle, to diff.
`MIXTAPES_DEMO_PHASES` times the frame clock phases, and
`MIXTAPES_DEMO_WATCHDOG` raises SIGUSR2 when the GTK thread stops answering,
for a backtrace under gdb. Only a release build is worth measuring: the debug
build stalls for its own reasons.

- The cover view toggle dropped frames of 116 to 230 ms. 57% of the frame time
  was CSS style recomputation. Cause: hiding the player bar gave the bottom bar
  zero height, `AdwToolbarView` answered by putting `undershoot-bottom` on
  itself, and a class change on the ancestor of the whole browser restyles
  every widget under it. The revealer now sits in a slot with a height request
  of one pixel. Toggle frames are 17 to 33 ms. Hiding the browser page itself
  was not the cost: that was tested both ways.
- A display-wide CSS provider reload restyles the whole window, about 270 ms in
  release with a full home feed. The appearance code did that up to four times
  per track change. It is one provider now, loaded once per change, and it
  waits up to 1.5 s for a blur and an accent that are still being computed.
  One reload per track remains while the dynamic accent is on. That is what
  the feature costs: an accent change moves named colors every widget uses.
- `high_res_url` compiled three regexes on every cover load, on the GTK thread:
  7% of main-thread time. They are statics now.
- Local cover files were decoded and scaled on the GTK thread. They go through
  the runtime like remote ones.
- The expanded player and the cover view each rebuilt their labels and menu
  five times per track change, once per property. Coalesced to one idle.
- PNG covers with alpha were re-encoded for the disk cache at the default
  compression, which cost more worker time than the decode it saved. Level 2.
- The compact flip cost one frame of 100 to 180 ms, 58% of it style. Three
  causes. The `compact` class sat on the window, which restyles every widget:
  each page now puts it on the widgets that read it, and cards carry no class
  at all since their sizes are set in code. The home feed resized every card
  in one frame: strips and song lists on screen flip at once, the rest one
  per frame from a tick callback. The sheet was unparented on the way back to
  desktop and restyled whole on the next flip: it stays parented, with
  `can-open` off on desktop. `AdwViewStack` is no longer homogeneous, so the
  hidden tabs are not measured on a resize. A flip is 73 to 97 ms now, and a
  plain resize of the same window is 45 to 80 ms (render and label reshaping
  of the visible page), which is the floor without a lazier home feed.
- Measure without touching the desktop: `mutter --headless --virtual-monitor
  1400x900 --wayland-display mx-headless --no-x11`, then run the demo with
  `WAYLAND_DISPLAY=mx-headless`. `MIXTAPES_DEMO_FRAMES` logs `worst_frame_ms`,
  the time inside one frame, which the frame gap count understates.
- The queue model was replaced whole on every sync, 951 rows at each track
  start, and the list view showed blank rows while scrolling to the playing
  one. `sync_queue_model` splices only the changed span and updates kept
  entries in place. Not reproduced, so not confirmed.
- Open: a skip still shows one long frame gap in release (several hundred ms)
  that the frame phases do not account for.

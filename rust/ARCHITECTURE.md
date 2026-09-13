# Mixtapes Rust port: core architecture

Target stack: gtk4-rs 0.11, libadwaita-rs 0.9, gstreamer-rs 0.25, tokio 1, reqwest 0.13. All GNOME crates share glib 0.22.

## What the Python app does today

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
src/net/stream.rs  StreamResolver trait, yt-dlp subprocess impl, DemoResolver, StreamCache
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
src/mock.rs        mock catalog with real video ids, stands in for the endpoints
src/ui/context.rs  UiContext (player, net, paths, compact flag) and Navigator
src/ui/pages/      home, explore (feed plus search results), library, stub page
src/ui/widgets/    scroll box, media card, song list rows, SongRow, transport, visualizer, cover picture, playing tracker
src/ui/expanded_player.rs  phone sheet: carousel, transport over visualizer, queue and lyrics tabs
src/ui/cover_view.rs       desktop cover view: Player / Lyrics toggle, split with lyrics sidebar
```

## Navigation

Each tab in the AdwViewStack is an AdwNavigationView whose root page is tagged "root". Pages never push directly: they call `Navigator::go` with a `NavRequest` (playlist, album, artist, category, search), and the window pushes onto the active tab, dismisses the cover view, and closes the search bar. Until those pages are ported the window pushes a stub NavigationPage, so back, Escape and the re-click-to-root gesture already work.

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

`StreamResolver` is the seam that replaces yt-dlp. `YtDlpResolver` shells out with the same format policy and PO-token setup the Python app uses, so playback works from day one. A native InnerTube `player` endpoint resolver implements the same trait later. `StreamCache` reads and writes the exact JSON files the Python app left in `~/.cache/muse/streams`.

## Verifying playback without pages

`MIXTAPES_DEMO=1` stages a queue at startup from audio files under ~/Music (or `MIXTAPES_DEMO_URI`), `MIXTAPES_DEMO_VIDEO=<id>` adds a real YouTube track through yt-dlp, `MIXTAPES_DEMO_AUTOPLAY=1` presses play after three seconds, `MIXTAPES_DEMO_QUEUE=1` opens the sidebar, `MIXTAPES_DEMO_EXPAND=1` opens the expanded player, `MIXTAPES_DEMO_TAB` and `MIXTAPES_DEMO_SEARCH` pick a tab or run a live search, `MIXTAPES_DEMO_ACTIVATE=1` plays the first result, `MIXTAPES_DEMO_WIDTH=420` starts in the phone layout, `MIXTAPES_DEMO_LOGIN=1` opens the sign-in dialog and snapshots it, and `MIXTAPES_DEMO_SNAPSHOT=<prefix>` writes PNGs of the window from inside GTK. `demo:` ids are served by `DemoResolver` in front of the real resolver, so the controller path is identical to production.

## Comparing against the Python app

`tools/pysnap.py` runs the Python app from the checkout, types a search, and saves a PNG of its window through the same in-app render path the Rust demo uses. Pair it with the Rust demo at the same width and query, then view both files:

```
PY_WIDTH=420 PY_QUERY="bohemian rhapsody" PY_OUT=/tmp/py ../.venv/bin/python tools/pysnap.py
MIXTAPES_DEMO=1 MIXTAPES_DEMO_URI="" MIXTAPES_DEMO_WIDTH=420 MIXTAPES_DEMO_SEARCH="bohemian rhapsody" MIXTAPES_DEMO_SNAPSHOT=/tmp/rs ./target/debug/mixtapes
```

Both apps register the same application id, so run them one after the other. Phone-layout sizing in Rust comes from two places, as in Python: the `.compact` class on the window for the CSS rules, and `UiContext::set_compact` notifying every `CoverImage::in_context` so thumbnail-sized art drops to 44 px like `AsyncImage.set_compact`.

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

Seeks are `FLUSH | ACCURATE` with a key-unit fallback, as in `Player.seek`; a key-unit seek lands on the previous cluster, which showed as the slider jumping back after a seek. The visualizer no longer draws the newest spectrum frame: the audio thread tags each frame with its stream time, the player queues them like `_viz_queue`, and `pull_visualizer_bands` releases the latest frame the play-head has reached, interpolating between the 100 ms position ticks. Seeks and new streams clear the queue. Every page scroller gets `suppress_hover_while_scrolling`, the Python trick that disables hit-testing on the content while it moves so no row is restyled under a stationary pointer. Covers decode on the runtime, since `GdkTexture` is thread-safe. The list view builds the same number of rows Python does (409 for a 945-track list), so the remaining smoothness difference is the debug build; use `cargo build --release` to compare.

## Artist pages and radio

`net/artist.rs` is MusicClient.get_artist and ArtistPage._fetch_artist together: the immersive header (name, subscriber count, subscribed flag, radio and shuffle ids, the channel id the subscribe endpoints take), the description shelf, the top songs shelf parsed as playlist rows, and the carousels matched by their English titles the way ytmusicapi's parse_channel_contents does ("Albums", "Singles & EPs", "Videos", "Playlists", "Fans might also like"), with the raw scan for the playlist and featured carousels it skips. The deep fetches the Python page joined before rendering, the full top songs playlist and the detailed albums and singles grids, run concurrently with the same ten-second bound. Plain channels fall back to the visual header like get_user did. Subscribing and unsubscribing hit the subscription endpoints, and the library load feeds the subscription set the page checks first.

`ui/pages/artist.rs` is artist.py: banner in a `FadeBottomBin` (the mask-node fade, active only in blur mode) under the scrim, the info box overlaid in the same grid cell, Play, Shuffle, Radio and Subscribe, the description with Read more, then the sections in order with their limits, Load More for top songs and View All into the discography page for the rest. Rows and cards route through the shared menus, and every artist navigation in the app (menus, cards, the playlist header's artist links, the player views) opens this page; a player-bar artist without an id resolves its channel from the player response first.

Radio is `Player::start_radio` for songs, playlists and artists, replacing the queue with the watch panel's fifty tracks as an infinite queue sourced from the radio id. `maybe_extend_infinite` runs after every advance and fetches more from the queue's tail when fewer than fifteen rows remain or the play-head is past halfway, retrying from the playing track when the batch was all duplicates; `force_radio_extend` is the last resort when the queue runs dry and accepts repeats over silence, as the Python player did.

## Not ported yet, and where it attaches

- Upload tracks and non-seekable streams staged in tmpfs: a second `StreamResolver` impl that downloads to `/dev/shm` and returns a `file://` URI.
- Mood and history pages: replace `stub::page` in `MainWindow::navigate`; playlists, albums, artists and discographies are live.
- Home and Explore feed data: swap the remaining `mock::*` calls for browse endpoint parsers; search and the library are live already.
- Audio-version swap (OMV to ATV): inside `spawn_resolve` before resolution, on `playlists::find_audio_version`.
- History recording, scrobbling, Discord: subscribers to `PlayerState` property notifications, each an `Rc` on the GTK thread that spawns its own network work.
- MPRIS: `mpris-server` with the tokio feature, fed from `PlayerState` notifications and calling `Player` methods. Dependency is already declared.
- Downloads: a `DownloadManager` on tokio with its own progress `watch` channel.
- GResource and style.css: `build.rs` with `glib-build-tools`, when the first real widget lands.

//! Port of ui/context_menu.py for songs. Sections keep the Python order:
//! queue, nav, actions, remove, clipboard. Entries whose backing API is
//! not ported yet (metadata refresh) are added when those land, so the
//! builder never offers a dead item.

use std::rc::Rc;

use gtk::{gdk, gio, glib, prelude::*};

use crate::model::{ItemKind, MediaItem, Track};
use crate::player::Player;
use crate::ui::context::{NavRequest, Navigator, UiContext};
use crate::ui::{copy_to_clipboard, toast};

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum Section {
    Queue,
    Nav,
    Actions,
    Remove,
    Clipboard,
    /// Diagnostics, last in the menu like the Python section of the same name.
    Debug,
}

const SECTIONS: [Section; 6] = [Section::Queue, Section::Nav, Section::Actions, Section::Remove, Section::Clipboard, Section::Debug];

/// A page-specific entry merged into one of the standard sections.
pub struct MenuAction {
    pub label: String,
    pub section: Section,
    pub first: bool,
    pub callback: Rc<dyn Fn()>,
}

impl MenuAction {
    pub fn new(label: &str, section: Section, callback: impl Fn() + 'static) -> Self {
        Self { label: label.to_owned(), section, first: false, callback: Rc::new(callback) }
    }
}

#[derive(Default)]
pub struct SongMenuOptions {
    pub prefix: &'static str,
    /// Entry names to leave out: play_next, add_to_queue, goto_artist, goto_album, start_radio, add_to_playlist, copy_link.
    pub hide: &'static [&'static str],
    pub extras: Vec<MenuAction>,
    /// With a navigator, Go to Artist / Album push pages instead of toasting.
    pub nav: Option<Rc<Navigator>>,
    /// A multi-selection the queue and playlist entries act on instead of the one track.
    pub selection: Vec<Track>,
    /// Needed for the entries that talk to the network: Start Radio and Add to Playlist.
    pub ctx: Option<Rc<UiContext>>,
    /// The playlist or album the rows came from, so a download is tagged with it.
    pub album: Option<(String, String)>,
}

struct Builder<'a> {
    anchor: &'a gtk::Widget,
    prefix: &'a str,
    group: gio::SimpleActionGroup,
    sections: Vec<(Section, gio::Menu)>,
    used: Vec<String>,
}

impl<'a> Builder<'a> {
    fn new(anchor: &'a gtk::Widget, prefix: &'a str) -> Self {
        Self { anchor, prefix, group: gio::SimpleActionGroup::new(), sections: SECTIONS.iter().map(|s| (*s, gio::Menu::new())).collect(), used: Vec::new() }
    }

    fn add(&mut self, section: Section, label: &str, name: &str, first: bool, callback: Rc<dyn Fn()>) {
        let name = if self.used.iter().any(|n| n == name) { format!("{name}-{}", self.used.len()) } else { name.to_owned() };
        self.used.push(name.clone());
        let action = gio::SimpleAction::new(&name, None);
        action.connect_activate(move |_, _| callback());
        self.group.add_action(&action);
        let detailed = format!("{}.{name}", self.prefix);
        let menu = &self.sections.iter().find(|(s, _)| *s == section).expect("known section").1;
        if first {
            menu.prepend(Some(label), Some(&detailed));
        } else {
            menu.append(Some(label), Some(&detailed));
        }
    }

    fn build(self) -> Option<gio::Menu> {
        let model = gio::Menu::new();
        for (_, section) in &self.sections {
            if section.n_items() > 0 {
                model.append_section(None, section);
            }
        }
        if model.n_items() == 0 {
            return None;
        }
        self.anchor.insert_action_group(self.prefix, Some(&self.group));
        Some(model)
    }
}

pub fn build_song_menu(anchor: &impl IsA<gtk::Widget>, track: &Track, player: &Rc<Player>, opts: SongMenuOptions) -> Option<gio::Menu> {
    let anchor = anchor.upcast_ref::<gtk::Widget>();
    let prefix = if opts.prefix.is_empty() { "ctx" } else { opts.prefix };
    let hidden = |name: &str| opts.hide.contains(&name);
    let mut builder = Builder::new(anchor, prefix);
    let vid = track.video_id.as_str().to_owned();
    let tracks: Vec<Track> = if !opts.selection.is_empty() { opts.selection.iter().filter(|t| !t.video_id.0.is_empty()).cloned().collect() } else if !vid.is_empty() { vec![track.clone()] } else { Vec::new() };
    let multi = tracks.len() > 1;
    let count = tracks.len();
    let online = opts.ctx.as_ref().map(|c| c.online.is_online()).unwrap_or(true);
    let queueable = !vid.is_empty() || multi;

    if !tracks.is_empty() && queueable && !hidden("play_next") {
        let (player, tracks) = (player.clone(), tracks.clone());
        let anchor = anchor.clone();
        let label = if multi { format!("Play {count} Next") } else { "Play Next".to_owned() };
        builder.add(Section::Queue, &label, "play-next", false, Rc::new(move || {
            player.add_to_queue(tracks.clone(), true);
            toast(&anchor, &if multi { format!("Playing {count} tracks next") } else { "Playing next".to_owned() });
        }));
    }
    if !tracks.is_empty() && queueable && !hidden("add_to_queue") {
        let (player, tracks) = (player.clone(), tracks.clone());
        let anchor = anchor.clone();
        let label = if multi { format!("Add {count} to Queue") } else { "Add to Queue".to_owned() };
        builder.add(Section::Queue, &label, "add-to-queue", false, Rc::new(move || {
            player.add_to_queue(tracks.clone(), false);
            toast(&anchor, &if multi { format!("Added {count} tracks to queue") } else { "Added to queue".to_owned() });
        }));
    }

    if let Some(artist) = track.artists.iter().find(|a| a.id.is_some()) {
        if !hidden("goto_artist") {
            let anchor = anchor.clone();
            let (id, name) = (artist.id.clone(), artist.name.clone());
            let nav = opts.nav.clone();
            builder.add(Section::Nav, "Go to Artist", "goto-artist", false, Rc::new(move || match &nav {
                Some(nav) => nav.go(NavRequest::Artist { id: id.clone(), name: name.clone() }),
                None => toast(&anchor, &format!("Artist pages not ported yet ({name})")),
            }));
        }
    }
    if let Some(album) = track.album.as_ref().filter(|a| a.id.is_some()) {
        if !hidden("goto_album") {
            let anchor = anchor.clone();
            let (id, name) = (album.id.clone().unwrap_or_default(), album.name.clone());
            let nav = opts.nav.clone();
            builder.add(Section::Nav, "Go to Album", "goto-album", false, Rc::new(move || match &nav {
                Some(nav) => nav.go(NavRequest::Album { id: id.clone(), title: name.clone(), thumb: None }),
                None => toast(&anchor, &format!("Album pages not ported yet ({name})")),
            }));
        }
    }

    if let Some(ctx) = opts.ctx.clone() {
        if !vid.is_empty() && online && !multi && !hidden("start_radio") {
            let anchor = anchor.clone();
            let (player, vid_c) = (player.clone(), vid.clone());
            builder.add(Section::Actions, "Start Radio", "start-radio", false, Rc::new(move || {
                player.start_radio(Some(vid_c.clone()), None);
                toast(&anchor, "Starting radio...");
            }));
        }
        let video_ids: Vec<String> = if tracks.is_empty() { if vid.is_empty() { Vec::new() } else { vec![vid.clone()] } } else { tracks.iter().map(|t| t.video_id.0.clone()).collect() };
        if !video_ids.is_empty() && online && ctx.net.client().is_authenticated() && !hidden("add_to_playlist") {
            let anchor = anchor.clone();
            let label = if multi { format!("Add {} to Playlist…", video_ids.len()) } else { "Add to Playlist…".to_owned() };
            builder.add(Section::Actions, &label, "add-to-playlist", false, Rc::new(move || {
                add_to_playlist_via_popover(&ctx, &anchor, video_ids.clone());
            }));
        }
    }

    if let Some(ctx) = opts.ctx.clone() {
        if !tracks.is_empty() && !hidden("download") {
            let (album_title, album_id) = opts.album.clone().unwrap_or_default();
            let pending: Vec<Track> = tracks.iter().filter(|t| !ctx.downloads.is_downloaded(&t.video_id.0)).cloned().collect();
            let downloaded_single = !multi && !vid.is_empty() && ctx.downloads.is_downloaded(&vid);
            if multi && !pending.is_empty() && online {
                let label = format!("Download {} Songs", pending.len());
                let ctx = ctx.clone();
                builder.add(Section::Actions, &label, "download", false, Rc::new(move || ctx.download(pending.clone(), &album_title, &album_id)));
            } else if downloaded_single {
                let anchor = anchor.clone();
                let (ctx, vid_c) = (ctx.clone(), vid.clone());
                builder.add(Section::Actions, "Remove Download", "remove-download", false, Rc::new(move || {
                    ctx.downloads.delete(&vid_c);
                    toast(&anchor, "Download removed");
                }));
            } else if !multi && !vid.is_empty() && online {
                let ctx = ctx.clone();
                let tracks = tracks.clone();
                builder.add(Section::Actions, "Download", "download", false, Rc::new(move || ctx.download(tracks.clone(), &album_title, &album_id)));
            }
        }
    }

    for (i, extra) in opts.extras.into_iter().enumerate() {
        builder.add(extra.section, &extra.label, &format!("extra-{i}"), extra.first, extra.callback);
    }

    if !vid.is_empty() && !hidden("copy_link") {
        let url = format!("https://music.youtube.com/watch?v={vid}");
        let anchor = anchor.clone();
        builder.add(Section::Clipboard, "Copy Link", "copy-link", false, Rc::new(move || {
            copy_to_clipboard(&url);
            toast(&anchor, "Link copied");
        }));
    }

    builder.build()
}

/// Port of _add_to_playlist: pick a playlist in the popover, then add on the runtime.
pub fn add_to_playlist_via_popover(ctx: &Rc<UiContext>, anchor: &gtk::Widget, video_ids: Vec<String>) {
    let ctx_c = ctx.clone();
    let anchor_c = anchor.clone();
    crate::ui::widgets::add_to_playlist::AddToPlaylistPopover::show(ctx, anchor, move |playlist_id| {
        crate::ui::widgets::add_to_playlist::mark_playlist_used(&ctx_c.paths, &playlist_id);
        let api = ctx_c.net.client().api();
        let ids = video_ids.clone();
        let count = ids.len();
        let handle = ctx_c.net.spawn(async move { crate::net::playlists::add_playlist_items(&api, &playlist_id, ids, None).await });
        let anchor = anchor_c.clone();
        glib::spawn_future_local(async move {
            match handle.await {
                Ok(Ok(())) => toast(&anchor, &if count > 1 { format!("Added {count} tracks to playlist") } else { "Added to playlist".to_owned() }),
                Ok(Err(err)) => {
                    tracing::warn!(%err, "add to playlist failed");
                    toast(&anchor, "Failed to add to playlist");
                }
                Err(_) => {}
            }
        });
    });
}

/// Show a built menu at the pointer and drop the popover once closed.
pub fn popup_menu(anchor: &impl IsA<gtk::Widget>, model: &gio::Menu, x: f64, y: f64) {
    if model.n_items() == 0 {
        return;
    }
    let popover = gtk::PopoverMenu::from_model(Some(model));
    popover.set_parent(anchor);
    popover.set_has_arrow(false);
    popover.set_pointing_to(Some(&gdk::Rectangle::new(x as i32, y as i32, 1, 1)));
    popover.connect_closed(|p| {
        let p = p.clone();
        glib::idle_add_local_once(move || p.unparent());
    });
    popover.popup();
}

pub fn show_song_menu(anchor: &impl IsA<gtk::Widget>, x: f64, y: f64, track: &Track, player: &Rc<Player>, opts: SongMenuOptions) {
    if let Some(model) = build_song_menu(anchor, track, player, opts) {
        popup_menu(anchor, &model, x, y);
    }
}

/// Menu for a browse item: the song menu for playable kinds, Open plus Copy Link for collections.
pub fn show_item_menu(anchor: &impl IsA<gtk::Widget>, x: f64, y: f64, item: &MediaItem, ctx: &Rc<UiContext>) {
    show_item_menu_with(anchor, x, y, item, ctx, Vec::new());
}

/// `show_item_menu` with page-specific entries merged in.
pub fn show_item_menu_with(anchor: &impl IsA<gtk::Widget>, x: f64, y: f64, item: &MediaItem, ctx: &Rc<UiContext>, extras: Vec<MenuAction>) {
    if let Some(track) = item.to_track() {
        let opts = SongMenuOptions { prefix: "item", nav: Some(ctx.nav.clone()), ctx: Some(ctx.clone()), extras, ..SongMenuOptions::default() };
        show_song_menu(anchor, x, y, &track, &ctx.player, opts);
        return;
    }
    let anchor_w = anchor.upcast_ref::<gtk::Widget>();
    let mut builder = Builder::new(anchor_w, "item");
    let request = match item.kind {
        ItemKind::Album => NavRequest::Album { id: item.id.clone(), title: item.title.clone(), thumb: item.thumb.clone() },
        ItemKind::Playlist => NavRequest::Playlist { id: item.id.clone(), title: item.title.clone(), thumb: item.thumb.clone() },
        _ => NavRequest::Artist { id: Some(item.id.clone()), name: item.title.clone() },
    };
    let label = match item.kind {
        ItemKind::Album => "Open Album",
        ItemKind::Playlist => "Open Playlist",
        _ => "Go to Artist",
    };
    let nav = ctx.nav.clone();
    builder.add(Section::Nav, label, "open", false, Rc::new(move || nav.go(request.clone())));
    let url = match item.kind {
        ItemKind::Playlist => format!("https://music.youtube.com/playlist?list={}", item.id),
        ItemKind::Album => format!("https://music.youtube.com/browse/{}", item.id),
        _ => format!("https://music.youtube.com/channel/{}", item.id),
    };
    let anchor_c = anchor_w.clone();
    builder.add(Section::Clipboard, "Copy Link", "copy-link", false, Rc::new(move || {
        copy_to_clipboard(&url);
        toast(&anchor_c, "Link copied");
    }));
    for (i, extra) in extras.into_iter().enumerate() {
        builder.add(extra.section, &extra.label, &format!("extra-{i}"), extra.first, extra.callback);
    }
    if let Some(model) = builder.build() {
        popup_menu(anchor, &model, x, y);
    }
}

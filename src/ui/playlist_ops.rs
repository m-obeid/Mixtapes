//! Adding tracks to a playlist, from wherever a menu offers it. A `LOCAL_`
//! id goes to the local library, anything else to the network. One place, so
//! the song menu, the queue and the playlist page agree on the toasts.

use std::rc::Rc;

use adw::prelude::*;
use gtk::glib;

use crate::local_library::is_local;
use crate::model::Track;
use crate::ui::context::UiContext;
use crate::ui::toast;
use crate::ui::widgets::add_to_playlist::mark_playlist_used;

pub fn add_tracks(ctx: &Rc<UiContext>, anchor: &gtk::Widget, playlist_id: String, tracks: Vec<Track>) {
    let tracks: Vec<Track> = tracks.into_iter().filter(|t| !t.video_id.0.is_empty()).collect();
    if playlist_id.is_empty() || tracks.is_empty() {
        return;
    }
    mark_playlist_used(&ctx.paths, &playlist_id);
    let count = tracks.len();
    if is_local(&playlist_id) {
        let added = ctx.local.add_tracks(&playlist_id, &tracks);
        toast(anchor, &match (added, count) {
            (0, _) => tr!("Already in that playlist"),
            (1, 1) => tr!("Added to playlist"),
            (added, _) => trn!("Added {n} track to playlist", "Added {n} tracks to playlist", added),
        });
        ctx.nav.refresh_library();
        return;
    }
    let api = ctx.net.client().api();
    let ids: Vec<String> = tracks.iter().map(|t| t.video_id.0.clone()).collect();
    let handle = ctx.net.spawn(async move { crate::net::playlists::add_playlist_items(&api, &playlist_id, ids, None).await });
    let anchor = anchor.clone();
    glib::spawn_future_local(async move {
        match handle.await {
            Ok(Ok(())) => toast(&anchor, &if count > 1 { trn!("Added {n} track to playlist", "Added {n} tracks to playlist", count) } else { tr!("Added to playlist") }),
            Ok(Err(err)) => {
                tracing::warn!(%err, "add to playlist failed");
                toast(&anchor, &tr!("Failed to add to playlist"));
            }
            Err(_) => {}
        }
    });
}

/// Port of on_new_playlist_clicked: title, description and, signed in, where
/// to keep it and its visibility. `on_created` gets the new id and title; a
/// YouTube playlist is reported once the browse endpoint serves it.
pub fn ask_new_playlist(ctx: &Rc<UiContext>, parent: &impl IsA<gtk::Widget>, on_created: impl Fn(String, String) + 'static) {
    let signed_in = ctx.net.client().is_authenticated();
    let dialog = adw::Dialog::builder().title(tr!("New Playlist")).content_width(500).build();
    let main_box = gtk::Box::builder().orientation(gtk::Orientation::Vertical).build();
    let header = adw::HeaderBar::builder().css_classes(["flat"]).build();
    let create_btn = gtk::Button::builder().label(tr!("Create")).css_classes(["suggested-action"]).build();
    header.pack_start(&create_btn);
    main_box.append(&header);

    let prefs_page = adw::PreferencesPage::new();
    let group = adw::PreferencesGroup::builder().title(tr!("Playlist Details")).margin_start(12).margin_end(12).margin_top(12).margin_bottom(12).build();
    let title_row = adw::EntryRow::builder().title(tr!("Title")).activates_default(true).build();
    let desc_row = adw::EntryRow::builder().title(tr!("Description")).build();
    let privacy_row = adw::ComboRow::builder().title(tr!("Visibility")).model(&gtk::StringList::new(&[&tr!("Public"), &tr!("Private"), &tr!("Unlisted")])).selected(1).build();
    // Signed out, the only place is this device. Signed in, the account is the default.
    let where_row = adw::ComboRow::builder().title(tr!("Save To")).model(&gtk::StringList::new(&["YouTube Music", &tr!("This device")])).selected(if signed_in { 0 } else { 1 }).visible(signed_in).build();
    {
        let privacy_row = privacy_row.clone();
        where_row.connect_selected_notify(move |row| privacy_row.set_visible(row.selected() == 0));
    }
    privacy_row.set_visible(signed_in);
    group.add(&title_row);
    group.add(&desc_row);
    group.add(&where_row);
    group.add(&privacy_row);
    prefs_page.add(&group);
    main_box.append(&prefs_page);
    dialog.set_child(Some(&main_box));

    let ctx = ctx.clone();
    let anchor: gtk::Widget = parent.clone().upcast();
    let on_created: Rc<dyn Fn(String, String)> = Rc::new(on_created);
    let dialog_c = dialog.clone();
    let (title_c, desc_c, privacy_c, where_c) = (title_row.clone(), desc_row.clone(), privacy_row.clone(), where_row.clone());
    create_btn.connect_clicked(move |_| {
        let title = title_c.text().trim().to_owned();
        if title.is_empty() {
            return;
        }
        let description = desc_c.text().trim().to_owned();
        dialog_c.close();
        if where_c.selected() == 1 {
            let id = ctx.local.create(&title, &description);
            tracing::info!(id, title, "local playlist created");
            ctx.nav.refresh_library();
            on_created(id, title);
            return;
        }
        let privacy = ["PUBLIC", "PRIVATE", "UNLISTED"][privacy_c.selected().min(2) as usize];
        let api = ctx.net.client().api();
        let title_c = title.clone();
        let handle = ctx.net.spawn(async move {
            let id = crate::net::playlists::create_playlist(&api, &title_c, &description, privacy).await?;
            // The browse endpoint needs a moment before it will serve it.
            crate::net::playlists::await_playlist(&api, &id).await;
            Ok::<String, crate::net::ytmusic::NetError>(id)
        });
        let (anchor, on_created) = (anchor.clone(), on_created.clone());
        glib::spawn_future_local(async move {
            match handle.await {
                Ok(Ok(id)) => {
                    tracing::info!(id, title, "playlist created");
                    on_created(id, title);
                }
                Ok(Err(err)) => {
                    tracing::warn!(%err, "playlist creation failed");
                    toast(&anchor, &tr!("Could not create the playlist"));
                }
                Err(_) => {}
            }
        });
    });
    dialog.present(Some(parent));
    title_row.grab_focus();
}

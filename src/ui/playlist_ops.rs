//! Adding tracks to a playlist, from wherever a menu offers it. A `LOCAL_`
//! id goes to the local library, anything else to the network. One place, so
//! the song menu, the queue and the playlist page agree on the toasts.

use std::rc::Rc;

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
            (0, _) => "Already in that playlist".to_owned(),
            (1, 1) => "Added to playlist".to_owned(),
            (added, _) => format!("Added {added} tracks to playlist"),
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
            Ok(Ok(())) => toast(&anchor, &if count > 1 { format!("Added {count} tracks to playlist") } else { "Added to playlist".to_owned() }),
            Ok(Err(err)) => {
                tracing::warn!(%err, "add to playlist failed");
                toast(&anchor, "Failed to add to playlist");
            }
            Err(_) => {}
        }
    });
}

/// Whether any playlist accepts tracks right now: a local one always does.
pub fn can_add_to_playlist(ctx: &UiContext) -> bool {
    !ctx.local.playlist_items().is_empty() || ctx.net.client().is_authenticated()
}

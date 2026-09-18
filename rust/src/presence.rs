//! Wires the player to the services that publish what is playing: the
//! scrobbler and Discord Rich Presence. Both follow `PlayerState` on the GTK
//! thread and do their network work elsewhere.

use std::rc::Rc;

use glib::prelude::*;

use crate::App;
use crate::discord::{self, Options, Snapshot};
use crate::model::PlaybackStatus;
use crate::player::PlayEvent;

pub fn wire(ctx: &Rc<App>) {
    wire_scrobbler(ctx);
    wire_discord(ctx);
}

fn wire_scrobbler(ctx: &Rc<App>) {
    let state = ctx.player.state().clone();
    {
        let scrobbler = ctx.scrobbler.clone();
        ctx.player.on_play(move |event| match event {
            PlayEvent::Started(t) => {
                let album = t.album.as_ref().map(|a| a.name.as_str()).unwrap_or_default();
                let duration = t.duration_seconds.map(f64::from).unwrap_or(0.0);
                scrobbler.on_track_started(&t.video_id.0, &t.title, &t.artist, album, duration);
            }
            PlayEvent::Refined(t) => scrobbler.refine_current_track(&t.video_id.0, &t.title, &t.artist),
        });
    }
    {
        let scrobbler = ctx.scrobbler.clone();
        state.connect_status_notify(move |state| scrobbler.on_state_changed(state.status() == PlaybackStatus::Playing));
    }
    // The clock hangs off the position tick, not the status: a queue played
    // straight through never reports a transition back into playing.
    let scrobbler = ctx.scrobbler.clone();
    state.connect_position_notify(move |state| scrobbler.on_progress(state.duration(), state.status() == PlaybackStatus::Playing));
}

/// Build the activity from the player as it stands and hand it to the worker.
pub fn update_discord(ctx: &App) {
    if !ctx.discord.is_enabled() {
        return;
    }
    let state = ctx.player.state();
    let track = ctx.player.current_track();
    let snapshot = Snapshot {
        status: state.status(),
        has_track: track.is_some(),
        title: state.title(),
        artist: state.artist(),
        album: track.as_ref().and_then(|t| t.album.as_ref()).map(|a| a.name.clone()).unwrap_or_default(),
        thumb: track.as_ref().and_then(|t| t.thumb.clone()).unwrap_or_else(|| state.thumbnail_url()),
        position: state.position(),
        duration: state.duration(),
    };
    let options = Options::read(&ctx.paths.read_prefs());
    ctx.discord.update(discord::build_activity(&snapshot, &options, discord::now_ms()));
}

fn wire_discord(ctx: &Rc<App>) {
    let state = ctx.player.state().clone();
    for property in ["status", "title", "artist", "thumbnail-url", "video-id", "duration"] {
        let weak = Rc::downgrade(ctx);
        state.connect_notify_local(Some(property), move |_, _| {
            if let Some(ctx) = weak.upgrade() {
                update_discord(&ctx);
            }
        });
    }
    let weak = Rc::downgrade(ctx);
    state.connect_seeked(move |_| {
        if let Some(ctx) = weak.upgrade() {
            update_discord(&ctx);
        }
    });
}

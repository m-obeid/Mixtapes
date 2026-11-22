//! Keeps the system awake while music plays (#185). Only suspend is held off:
//! the screen still blanks and locks on the user's own settings.
//!
//! Linux only. Windows and macOS already stay awake while an audio stream is open.

use std::cell::Cell;
use std::rc::Rc;

use adw::prelude::*;

use crate::App;
use crate::model::PlaybackStatus;
use crate::state::PlayerState;

pub fn start(app: &adw::Application, ctx: &Rc<App>) {
    let cookie = Cell::new(0u32);
    let weak_app = app.downgrade();
    let follow = move |state: &PlayerState| {
        let Some(app) = weak_app.upgrade() else { return };
        // Loading between tracks keeps whatever is held, so a skip does not drop and retake it.
        let playing = match state.status() {
            PlaybackStatus::Playing => true,
            PlaybackStatus::Paused | PlaybackStatus::Stopped => false,
            PlaybackStatus::Loading => return,
        };
        match (playing, cookie.get()) {
            (true, 0) => {
                // GTK asks the session manager, or the portal inside Flatpak. 0 means nobody took it.
                let id = app.inhibit(app.active_window().as_ref(), gtk::ApplicationInhibitFlags::SUSPEND, Some(&tr!("Playing music")));
                tracing::debug!(id, "suspend inhibited");
                cookie.set(id);
            }
            (false, id) if id != 0 => {
                app.uninhibit(id);
                cookie.set(0);
                tracing::debug!("suspend allowed again");
            }
            _ => {}
        }
    };
    // Play on Startup may already be playing by now.
    let state = ctx.player.state();
    follow(state);
    state.connect_status_notify(follow);
}

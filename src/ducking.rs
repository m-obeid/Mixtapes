//! Turn the music down while another media player plays, such as a voice message in a browser.
//!
//! Players announce themselves over MPRIS. Asking each one every so often is enough for
//! something that lasts seconds, and it needs no subscription to every player that comes and goes.

use std::rc::Rc;
use std::time::Duration;

use mpris_server::zbus::zvariant::OwnedValue;
use mpris_server::zbus::{Connection, fdo};

use crate::App;

/// The pref behind the setting.
pub const PREF: &str = "duck_for_other_players";
const POLL: Duration = Duration::from_millis(1500);
const PREFIX: &str = "org.mpris.MediaPlayer2.";

/// Whether a bus name is another player. playerctld and KDE Connect mirror other players, this one included.
fn is_other_player(name: &str) -> bool {
    name.strip_prefix(PREFIX).is_some_and(|rest| !rest.starts_with("Mixtapes") && !rest.starts_with("playerctld") && !rest.starts_with("kdeconnect"))
}

async fn other_player_playing(connection: &Connection) -> bool {
    let Ok(bus) = fdo::DBusProxy::new(connection).await else { return false };
    let Ok(names) = bus.list_names().await else { return false };
    for name in names.iter().map(|n| n.as_str()).filter(|n| is_other_player(n)) {
        let reply = connection.call_method(Some(name), "/org/mpris/MediaPlayer2", Some("org.freedesktop.DBus.Properties"), "Get", &("org.mpris.MediaPlayer2.Player", "PlaybackStatus")).await;
        let Ok(reply) = reply else { continue };
        let Ok(value) = reply.body().deserialize::<OwnedValue>() else { continue };
        if String::try_from(value).is_ok_and(|status| status == "Playing") {
            return true;
        }
    }
    false
}

/// Watch for other players while the setting is on.
pub fn start(ctx: &Rc<App>) {
    let (tx, rx) = async_channel::unbounded();
    let paths = ctx.paths.clone();
    ctx.net.spawn(async move {
        let Ok(connection) = Connection::session().await else { return };
        let mut ducked = false;
        loop {
            tokio::time::sleep(POLL).await;
            let wanted = paths.read_prefs().get(PREF).and_then(|v| v.as_bool()).unwrap_or(false);
            let playing = wanted && other_player_playing(&connection).await;
            if playing != ducked {
                ducked = playing;
                if tx.send(playing).await.is_err() {
                    return;
                }
            }
        }
    });
    let weak = Rc::downgrade(&ctx.player);
    glib::spawn_future_local(async move {
        while let Ok(ducked) = rx.recv().await {
            let Some(player) = weak.upgrade() else { return };
            player.set_ducked(ducked);
        }
    });
}

#[cfg(test)]
mod tests {
    use super::is_other_player;

    #[test]
    fn only_real_other_players_count() {
        assert!(is_other_player("org.mpris.MediaPlayer2.firefox.instance_1_23"));
        assert!(is_other_player("org.mpris.MediaPlayer2.spotify"));
        assert!(!is_other_player("org.mpris.MediaPlayer2.Mixtapes"));
        assert!(!is_other_player("org.mpris.MediaPlayer2.playerctld"));
        assert!(!is_other_player("org.freedesktop.Notifications"));
    }
}

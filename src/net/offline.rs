//! Offline mode as a seal: while it is on, nothing in the app opens a connection.
//!
//! The pages already hide what needs the network, but a page check does not
//! stop the player, the scrobbler or a cover fetch. So the seal sits at the
//! bottom, twice. Every entry point to the network asks `sealed` and refuses
//! at once, and the HTTP clients resolve no host name while it is on, which
//! stops any request that an entry point missed.

use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};

use reqwest::dns::{Addrs, Name, Resolve, Resolving};

use crate::net::ytmusic::NetError;
use crate::paths::Paths;

/// The pref behind Force Offline Mode.
pub const PREF: &str = "force_offline";
/// What a refused request says, in a log or under a failed row.
pub const REFUSAL: &str = "offline mode is on";

static SEALED: AtomicBool = AtomicBool::new(false);

/// Read the pref before the first client is built, so not even the warm-up requests go out.
pub fn init(paths: &Paths) {
    set_sealed(paths.read_prefs().get(PREF).and_then(|v| v.as_bool()).unwrap_or(false));
}

pub fn sealed() -> bool {
    SEALED.load(Ordering::Relaxed)
}

pub fn set_sealed(on: bool) {
    if SEALED.swap(on, Ordering::Relaxed) != on {
        tracing::info!(sealed = on, "offline mode");
    }
}

/// `Err` while sealed, for an entry point to return with `?`.
pub fn check() -> Result<(), NetError> {
    if sealed() { Err(NetError::Message(REFUSAL.to_owned())) } else { Ok(()) }
}

/// The resolver every HTTP client here is built with. Sealed, no name resolves, so no connection is opened.
struct Gate;

impl Resolve for Gate {
    fn resolve(&self, name: Name) -> Resolving {
        Box::pin(async move {
            if sealed() {
                return Err(REFUSAL.into());
            }
            let found: Vec<std::net::SocketAddr> = tokio::net::lookup_host((name.as_str(), 0)).await?.collect();
            Ok(Box::new(found.into_iter()) as Addrs)
        })
    }
}

/// A client builder behind the gate. Every client the app keeps starts here.
pub fn client_builder() -> reqwest::ClientBuilder {
    reqwest::Client::builder().dns_resolver(Arc::new(Gate))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn a_sealed_client_resolves_nothing() {
        // The flag is shared by every test in the process, so this one puts it back at once.
        set_sealed(true);
        let refused = Gate.resolve("localhost".parse().unwrap()).await;
        let checked = check();
        set_sealed(false);
        assert!(refused.is_err());
        assert!(checked.is_err());
        assert!(check().is_ok());
    }
}

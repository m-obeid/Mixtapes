//! Network layer: a tokio runtime plus the InnerTube client and stream resolver.
//!
//! The GTK thread never awaits inside tokio. It calls `NetHandle::spawn`,
//! gets a `JoinHandle`, and awaits that from `glib::spawn_future_local`.
//! The result therefore lands back on the GTK thread with no channel and
//! no `idle_add`. Dropping or aborting the `JoinHandle` cancels the request.

pub mod artist;
pub mod browse;
pub mod cache;
pub mod covers;
pub mod explore;
pub mod history;
pub mod home;
pub mod items;
pub mod library;
pub mod online;
pub mod playlists;
pub mod potoken;
pub mod search;
pub mod stream;
pub mod uploads;
pub mod ytmusic;

use std::future::Future;
use std::sync::Arc;

use tokio::task::JoinHandle;

use crate::paths::Paths;
use cache::Caches;
use stream::{StreamResolver, YtDlpResolver};
use ytmusic::YtMusic;

#[derive(Clone)]
pub struct NetHandle {
    rt: tokio::runtime::Handle,
    client: Arc<YtMusic>,
    resolver: Arc<dyn StreamResolver>,
    caches: Arc<Caches>,
    /// PO tokens, shared with whatever else shells out to yt-dlp.
    tokens: Arc<potoken::PoTokens>,
}

impl NetHandle {
    pub fn new(rt: tokio::runtime::Handle, paths: &Paths) -> anyhow::Result<Self> {
        let client = YtMusic::new(paths)?;
        let tokens = Arc::new(potoken::PoTokens::new());
        let resolver: Arc<dyn StreamResolver> = Arc::new(YtDlpResolver::new(paths, tokens.clone()));
        Ok(Self { rt, client, resolver, caches: Arc::new(Caches::new(paths)), tokens })
    }

    /// Run a future on the tokio runtime. Await the returned handle from the GTK thread.
    pub fn spawn<F>(&self, fut: F) -> JoinHandle<F::Output>
    where
        F: Future + Send + 'static,
        F::Output: Send + 'static,
    {
        self.rt.spawn(fut)
    }

    /// Swap the resolver, for the demo queue or a future native resolver.
    pub fn with_resolver(mut self, resolver: Arc<dyn StreamResolver>) -> Self {
        self.resolver = resolver;
        self
    }

    pub fn client(&self) -> &Arc<YtMusic> {
        &self.client
    }

    pub fn resolver(&self) -> &Arc<dyn StreamResolver> {
        &self.resolver
    }

    /// PO tokens for the yt-dlp calls outside the resolver, such as downloads.
    pub fn tokens(&self) -> &Arc<potoken::PoTokens> {
        &self.tokens
    }

    /// Playlist track lists, sort metrics and library ids, shared with tokio tasks.
    pub fn caches(&self) -> &Arc<Caches> {
        &self.caches
    }
}

//! Network layer: a tokio runtime plus the InnerTube client and stream resolver.
//!
//! The GTK thread never awaits inside tokio. It calls `NetHandle::spawn`,
//! gets a `JoinHandle`, and awaits that from `glib::spawn_future_local`.
//! The result therefore lands back on the GTK thread with no channel and
//! no `idle_add`. Dropping or aborting the `JoinHandle` cancels the request.

pub mod artist;
pub mod cache;
pub mod covers;
pub mod items;
pub mod library;
pub mod online;
pub mod playlists;
pub mod search;
pub mod stream;
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
}

impl NetHandle {
    pub fn new(rt: tokio::runtime::Handle, paths: &Paths) -> anyhow::Result<Self> {
        let client = YtMusic::new(paths)?;
        let resolver: Arc<dyn StreamResolver> = Arc::new(YtDlpResolver::new(paths));
        Ok(Self { rt, client, resolver, caches: Arc::new(Caches::new(paths)) })
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

    /// Playlist track lists, sort metrics and library ids, shared with tokio tasks.
    pub fn caches(&self) -> &Arc<Caches> {
        &self.caches
    }
}

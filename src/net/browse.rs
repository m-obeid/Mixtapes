//! The InnerTube transport seam and the paging that rides on it.
//!
//! Every endpoint in `net` posts a JSON body to an InnerTube endpoint and
//! parses the JSON that comes back. `Browse` is that one call. The app runs
//! it against the `ytmusicapi` client; tests run it against `Fixtures`,
//! captured responses on disk, so parsing is checked without the network.
//!
//! `Continuation` owns paging. YouTube returns long lists a page at a time
//! behind a token, in two response shapes and with three token spellings.
//! Endpoints hand it the first token and a parser and get the rest of the
//! rows back.

use std::future::Future;
#[cfg(test)]
use std::path::PathBuf;
use std::pin::Pin;

use serde_json::{json, Value};
use ytmusicapi::YTMusicClient;

use super::items::{array_at, item_continuation_token, next_continuation, owned_at};
use super::ytmusic::NetError;

/// A boxed response future, so `Browse` works behind `dyn`.
pub type Response<'a> = Pin<Box<dyn Future<Output = Result<Value, NetError>> + Send + 'a>>;

/// The InnerTube transport: post a body to an endpoint, get JSON back.
///
/// Endpoints take this rather than a client, so the same parsing runs against
/// the live service, against captured fixtures, and against pages written by
/// hand in a test.
pub trait Browse: Send + Sync {
    fn post<'a>(&'a self, endpoint: &'a str, body: Value) -> Response<'a>;
}

/// A shared handle is an adapter too, so callers holding an `Arc` pass it straight through.
impl<T: Browse + ?Sized> Browse for std::sync::Arc<T> {
    fn post<'a>(&'a self, endpoint: &'a str, body: Value) -> Response<'a> {
        (**self).post(endpoint, body)
    }
}

impl Browse for YTMusicClient {
    fn post<'a>(&'a self, endpoint: &'a str, body: Value) -> Response<'a> {
        Box::pin(async move { Ok(self.send_request(endpoint, body).await?) })
    }
}

/// Safety cap on continuation pages. A YouTube list is finite, a token loop is not.
pub const MAX_PAGES: usize = 100;

/// Continuation renderers under the 2024 response shape.
const APPEND_ACTIONS: &str = "/onResponseReceivedActions";

/// Every legacy `continuationContents` node the app has seen. A response
/// carries at most one, so trying them all costs nothing and spares callers
/// from naming the one their endpoint happens to use.
const LEGACY_KEYS: [&str; 5] = [
    "musicPlaylistShelfContinuation",
    "musicShelfContinuation",
    "gridContinuation",
    "playlistPanelContinuation",
    "sectionListContinuation",
];

/// Rows gathered before paging stopped.
///
/// A failed page keeps what came before it: a long playlist that dies on page
/// nine still shows eight pages. `strict` is for callers that would rather
/// report the failure than show a short list.
pub struct Paged<T> {
    pub items: Vec<T>,
    pub error: Option<NetError>,
}

impl<T> Paged<T> {
    /// Fail if any page failed.
    pub fn strict(self) -> Result<Vec<T>, NetError> {
        match self.error {
            Some(err) => Err(err),
            None => Ok(self.items),
        }
    }
}

/// Where the endpoint carries its continuation token.
enum Carrier {
    /// `browse` and friends take it in the request body.
    Body,
    /// `next` takes it in the query string and wants the original body resent.
    Query { endpoint: String, body: Value },
}

/// Follows continuation tokens until the rows run out.
///
/// Paging stops at the first of: no token, the row limit, the page cap, a page
/// that parses to nothing, or a failed request. Rows survive all of them.
pub struct Continuation<'a> {
    api: &'a dyn Browse,
    carrier: Carrier,
    token: Option<String>,
    limit: usize,
    pages: usize,
}

impl<'a> Continuation<'a> {
    /// Browse continuations: the token rides in the body.
    pub fn browse(api: &'a dyn Browse, token: Option<String>) -> Self {
        Self { api, carrier: Carrier::Body, token, limit: usize::MAX, pages: MAX_PAGES }
    }

    /// Watch-panel continuations: the token rides in the query string.
    ///
    /// The crate appends its own "?alt=json" to whatever endpoint it is given,
    /// so the token goes in front of a throwaway parameter that swallows it.
    pub fn query(api: &'a dyn Browse, endpoint: &str, body: Value, token: Option<String>) -> Self {
        let carrier = Carrier::Query { endpoint: endpoint.to_owned(), body };
        Self { api, carrier, token, limit: usize::MAX, pages: MAX_PAGES }
    }

    /// Stop once this many rows have been added. Counts rows these pages add,
    /// not rows the caller already had.
    pub fn limit(mut self, rows: usize) -> Self {
        self.limit = rows;
        self
    }

    /// Stop after this many requests.
    pub fn pages(mut self, pages: usize) -> Self {
        self.pages = pages;
        self
    }

    /// Follow the tokens, parsing each page.
    pub async fn collect<T>(mut self, mut parse: impl FnMut(&[&Value]) -> Vec<T>) -> Paged<T> {
        let mut items = Vec::new();
        let mut requests = 0;
        while let Some(token) = self.token.take() {
            if items.len() >= self.limit || requests >= self.pages {
                break;
            }
            requests += 1;
            let response = match self.request(&token).await {
                Ok(response) => response,
                Err(error) => return Paged { items, error: Some(error) },
            };
            let (entries, next) = page(&response);
            let parsed = parse(&entries);
            if parsed.is_empty() {
                break;
            }
            items.extend(parsed);
            self.token = next;
        }
        Paged { items, error: None }
    }

    async fn request(&self, token: &str) -> Result<Value, NetError> {
        match &self.carrier {
            Carrier::Body => self.api.post("browse", json!({ "continuation": token })).await,
            Carrier::Query { endpoint, body } => {
                let endpoint = format!("{endpoint}?ctoken={token}&continuation={token}&_=");
                self.api.post(&endpoint, body.clone()).await
            }
        }
    }
}

/// Rows and the next token of one continuation response.
///
/// Covers both shapes: `appendContinuationItemsAction` for the 2024 responses
/// and `continuationContents` for the older ones. Token renderers are dropped
/// from the rows, since they are paging, not content.
fn page(response: &Value) -> (Vec<&Value>, Option<String>) {
    let mut entries = Vec::new();
    let mut token = None;
    for action in array_at(response, APPEND_ACTIONS) {
        take(array_at(action, "/appendContinuationItemsAction/continuationItems"), &mut entries, &mut token);
    }
    for key in LEGACY_KEYS {
        let Some(node) = response.pointer(&format!("/continuationContents/{key}")) else { continue };
        take(array_at(node, "/contents"), &mut entries, &mut token);
        take(array_at(node, "/items"), &mut entries, &mut token);
        token = token.or_else(|| legacy_token(node));
    }
    (entries, token)
}

/// Sort one batch of renderers into rows and the token that follows them.
fn take<'v>(raws: &'v [Value], entries: &mut Vec<&'v Value>, token: &mut Option<String>) {
    for raw in raws {
        match item_continuation_token(raw) {
            Some(next) => *token = Some(next),
            None => entries.push(raw),
        }
    }
}

/// The two legacy token spellings: playlists use one, radios the other.
fn legacy_token(node: &Value) -> Option<String> {
    next_continuation(node).or_else(|| owned_at(node, "/continuations/0/nextRadioContinuationData/continuation"))
}

#[cfg(test)]
/// Captured InnerTube responses on disk, keyed by endpoint and body.
///
/// The directory is not in git. `cargo test -- --ignored` records it against
/// the live service, and the offline tests skip while it is missing.
pub struct Fixtures {
    dir: PathBuf,
}

#[cfg(test)]
impl Fixtures {
    /// The fixture directory, or None when nothing has been captured.
    pub fn open() -> Option<Self> {
        let dir = Self::dir();
        dir.is_dir().then_some(Self { dir })
    }

    /// Where fixtures live: `$MIXTAPES_FIXTURES`, else `fixtures/innertube`.
    pub fn dir() -> PathBuf {
        match std::env::var_os("MIXTAPES_FIXTURES") {
            Some(dir) => PathBuf::from(dir),
            None => PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("fixtures/innertube"),
        }
    }

    /// File name for a request. The body is part of it, so the pages of a
    /// paged capture stay distinct and replay in the order they were recorded.
    fn key(endpoint: &str, body: &Value) -> String {
        let safe: String = endpoint.chars().map(|c| if c.is_ascii_alphanumeric() { c } else { '_' }).collect();
        let mut hash: u64 = 0xcbf2_9ce4_8422_2325;
        for byte in body.to_string().bytes() {
            hash ^= byte as u64;
            hash = hash.wrapping_mul(0x0000_0100_0000_01b3);
        }
        format!("{safe}-{hash:016x}.json")
    }
}

#[cfg(test)]
impl Browse for Fixtures {
    fn post<'a>(&'a self, endpoint: &'a str, body: Value) -> Response<'a> {
        let path = self.dir.join(Self::key(endpoint, &body));
        Box::pin(async move {
            let text = std::fs::read_to_string(&path).map_err(|_| NetError::Message(format!("no fixture for {endpoint} at {}", path.display())))?;
            Ok(serde_json::from_str(&text)?)
        })
    }
}

#[cfg(test)]
/// Writes every response passing through to the fixture directory.
///
/// Wrap the live client with this in a test marked `#[ignore]`, run it once,
/// and the offline tests have something to replay.
pub struct Recorder {
    inner: std::sync::Arc<dyn Browse>,
    dir: PathBuf,
}

#[cfg(test)]
impl Recorder {
    pub fn new(inner: std::sync::Arc<dyn Browse>) -> Self {
        Self { inner, dir: Fixtures::dir() }
    }
}

#[cfg(test)]
impl Browse for Recorder {
    fn post<'a>(&'a self, endpoint: &'a str, body: Value) -> Response<'a> {
        Box::pin(async move {
            let response = self.inner.post(endpoint, body.clone()).await?;
            let path = self.dir.join(Fixtures::key(endpoint, &body));
            if let Err(err) = std::fs::create_dir_all(&self.dir).and_then(|_| std::fs::write(&path, serde_json::to_string(&response).unwrap_or_default())) {
                tracing::warn!(%err, path = %path.display(), "fixture not written");
            }
            Ok(response)
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::Mutex;

    /// Pages handed out in order, then an error. The third adapter at the seam.
    struct Scripted {
        pages: Mutex<Vec<Value>>,
        requests: Mutex<Vec<String>>,
    }

    impl Scripted {
        fn new(pages: Vec<Value>) -> Self {
            Self { pages: Mutex::new(pages), requests: Mutex::new(Vec::new()) }
        }
    }

    impl Browse for Scripted {
        fn post<'a>(&'a self, endpoint: &'a str, body: Value) -> Response<'a> {
            self.requests.lock().unwrap().push(format!("{endpoint} {body}"));
            let next = self.pages.lock().unwrap().pop();
            Box::pin(async move {
                match next {
                    Some(page) => Ok(page),
                    None => Err(NetError::Message("out of pages".into())),
                }
            })
        }
    }

    fn row(id: &str) -> Value {
        json!({ "musicResponsiveListItemRenderer": { "id": id } })
    }

    fn token_row(token: &str) -> Value {
        json!({ "continuationItemRenderer": { "continuationEndpoint": { "continuationCommand": { "token": token } } } })
    }

    fn modern(rows: Vec<Value>) -> Value {
        json!({ "onResponseReceivedActions": [{ "appendContinuationItemsAction": { "continuationItems": rows } }] })
    }

    fn legacy(key: &str, rows: Vec<Value>, token: Option<&str>) -> Value {
        let mut node = json!({ "contents": rows });
        if let Some(token) = token {
            node["continuations"] = json!([{ "nextContinuationData": { "continuation": token } }]);
        }
        json!({ "continuationContents": { key: node } })
    }

    fn ids(entries: &[&Value]) -> Vec<String> {
        entries.iter().filter_map(|e| e.pointer("/musicResponsiveListItemRenderer/id")).map(|v| v.as_str().unwrap_or_default().to_owned()).collect()
    }

    #[tokio::test]
    async fn modern_pages_follow_the_trailing_token() {
        let api = Scripted::new(vec![modern(vec![row("c")]), modern(vec![row("a"), row("b"), token_row("t2")])]);
        let paged = Continuation::browse(&api, Some("t1".into())).collect(ids).await;
        assert_eq!(paged.items, ["a", "b", "c"]);
        assert!(paged.error.is_none());
        let requests = api.requests.lock().unwrap().clone();
        assert_eq!(requests.len(), 2);
        assert!(requests[1].contains("t2"), "{requests:?}");
    }

    #[tokio::test]
    async fn legacy_pages_follow_the_node_token() {
        let api = Scripted::new(vec![legacy("gridContinuation", vec![row("b")], None), legacy("musicShelfContinuation", vec![row("a")], Some("t2"))]);
        let paged = Continuation::browse(&api, Some("t1".into())).collect(ids).await;
        assert_eq!(paged.items, ["a", "b"]);
    }

    #[tokio::test]
    async fn a_radio_token_pages_like_a_playlist_token() {
        let mut first = legacy("playlistPanelContinuation", vec![row("a")], None);
        first["continuationContents"]["playlistPanelContinuation"]["continuations"] = json!([{ "nextRadioContinuationData": { "continuation": "t2" } }]);
        let api = Scripted::new(vec![legacy("playlistPanelContinuation", vec![row("b")], None), first]);
        let paged = Continuation::query(&api, "next", json!({ "videoId": "x" }), Some("t1".into())).collect(ids).await;
        assert_eq!(paged.items, ["a", "b"]);
        assert!(api.requests.lock().unwrap()[0].starts_with("next?ctoken=t1&continuation=t1"));
    }

    #[tokio::test]
    async fn a_failed_page_keeps_what_came_before_it() {
        let api = Scripted::new(vec![modern(vec![row("a"), token_row("t2")])]);
        let paged = Continuation::browse(&api, Some("t1".into())).collect(ids).await;
        assert_eq!(paged.items, ["a"]);
        assert!(paged.error.is_some());
        assert!(paged.strict().is_err());
    }

    #[tokio::test]
    async fn the_limit_counts_rows_these_pages_add() {
        let pages = vec![modern(vec![row("c"), token_row("t3")]), modern(vec![row("a"), row("b"), token_row("t2")])];
        let paged = Continuation::browse(&Scripted::new(pages), Some("t1".into())).limit(2).collect(ids).await;
        assert_eq!(paged.items, ["a", "b"]);
    }

    #[tokio::test]
    async fn a_page_that_adds_nothing_ends_the_paging() {
        let api = Scripted::new(vec![modern(vec![row("b")]), modern(vec![token_row("t2")])]);
        let paged = Continuation::browse(&api, Some("t1".into())).collect(ids).await;
        assert!(paged.items.is_empty());
        assert_eq!(api.requests.lock().unwrap().len(), 1);
    }

    #[tokio::test]
    async fn the_page_cap_bounds_a_token_that_never_ends() {
        let forever = (0..10).map(|_| modern(vec![row("x"), token_row("t")])).collect();
        let api = Scripted::new(forever);
        let paged = Continuation::browse(&api, Some("t".into())).pages(3).collect(ids).await;
        assert_eq!(paged.items.len(), 3);
    }

    #[test]
    fn fixture_keys_separate_the_pages_of_one_capture() {
        let first = Fixtures::key("browse", &json!({ "browseId": "VL123" }));
        let second = Fixtures::key("browse", &json!({ "continuation": "t2" }));
        assert_ne!(first, second);
        assert_eq!(first, Fixtures::key("browse", &json!({ "browseId": "VL123" })));
        assert!(Fixtures::key("next?ctoken=t&_=", &json!({})).starts_with("next_ctoken"));
    }
}

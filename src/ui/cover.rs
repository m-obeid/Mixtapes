//! Cover art loader. Fetches over the network runtime, decodes on the GTK
//! thread, keeps a small per-process texture cache. Stale results are dropped
//! by comparing the URL at completion, so rapid track changes never race.
//! Art is drawn cover-fit inside a fixed square, the way AsyncPicture cropped
//! every thumbnail to a centered square.

use std::cell::RefCell;
use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::rc::Rc;

use gtk::{gdk, glib};

use crate::model::HttpAuth;
use crate::net::NetHandle;
use crate::ui::context::UiContext;
use crate::ui::high_res_url;

const PLACEHOLDER_ICON: &str = "audio-x-generic-symbolic";
/// How long a cover must stay wanted before it is fetched.
const LOAD_SETTLE: std::time::Duration = std::time::Duration::from_millis(90);
/// Cover downloads at once. More only queue behind each other on a phone link.
static NET_SLOTS: tokio::sync::Semaphore = tokio::sync::Semaphore::const_new(6);
const CACHE_LIMIT: usize = 64;
/// Thumbnail size on phones, and the largest base size the swap applies to.
const COMPACT_SIZE: i32 = 44;
const COMPACT_MAX_BASE: i32 = 80;

/// Where cover bytes are kept between runs. Set once at startup.
static DISK_DIR: std::sync::OnceLock<PathBuf> = std::sync::OnceLock::new();
/// Files kept in the disk cache. A cover at the size it is shown is about 10 KB.
const DISK_LIMIT: usize = 4000;

type TrackArtLookup = Box<dyn Fn(&str) -> Option<PathBuf> + Send + Sync>;
type DownloadedLookup = Box<dyn Fn(&str) -> bool + Send + Sync>;
/// Find the cover a downloaded track carries: whether there is a download, a
/// quick look for the copy kept beside the cache, and the slow path that
/// pulls it out of the file. Set once at startup.
static TRACK_ART: std::sync::OnceLock<(DownloadedLookup, TrackArtLookup, TrackArtLookup)> = std::sync::OnceLock::new();

/// Install the lookups for a downloaded track's own cover, by video id.
pub fn set_track_art_lookup(
    downloaded: impl Fn(&str) -> bool + Send + Sync + 'static,
    cached: impl Fn(&str) -> Option<PathBuf> + Send + Sync + 'static,
    extract: impl Fn(&str) -> Option<PathBuf> + Send + Sync + 'static,
) {
    let _ = TRACK_ART.set((Box::new(downloaded), Box::new(cached), Box::new(extract)));
}

/// Whether the track has a downloaded file, and so a cover of its own.
pub fn is_downloaded(video_id: &str) -> bool {
    !video_id.is_empty() && TRACK_ART.get().is_some_and(|(downloaded, _, _)| downloaded(video_id))
}

/// The cover kept with a downloaded track, when it is already on disk. Cheap
/// enough for the GTK thread.
pub fn cached_track_art(video_id: &str) -> Option<PathBuf> {
    if video_id.is_empty() {
        return None;
    }
    if !is_downloaded(video_id) {
        return None;
    }
    TRACK_ART.get().and_then(|(_, cached, _)| cached(video_id))
}

/// The cover kept with a downloaded track, extracted from the file the first
/// time. Offline it is the only copy of the art that is sure to be there.
pub async fn track_art(net: &NetHandle, video_id: &str) -> Option<PathBuf> {
    if !is_downloaded(video_id) {
        return None;
    }
    if let Some(path) = cached_track_art(video_id) {
        return Some(path);
    }
    let id = video_id.to_owned();
    net.spawn(async move { tokio::task::spawn_blocking(move || TRACK_ART.get().and_then(|(_, _, extract)| extract(&id))).await.ok().flatten() }).await.ok().flatten()
}

/// The address to draw for a track: its downloaded cover when there is one, else `url`.
pub async fn track_cover_address(net: &NetHandle, video_id: &str, url: &str) -> String {
    match track_art(net, video_id).await {
        Some(path) => path.to_string_lossy().into_owned(),
        None => url.to_owned(),
    }
}

/// Point the disk cache at `<cache>/covers` and trim it in the background.
pub fn init_disk_cache(cache_dir: &std::path::Path) {
    let dir = cache_dir.join("covers");
    if DISK_DIR.set(dir.clone()).is_err() {
        return;
    }
    std::thread::spawn(move || {
        let _ = std::fs::create_dir_all(&dir);
        let Ok(read) = std::fs::read_dir(&dir) else { return };
        let mut files: Vec<(std::time::SystemTime, PathBuf)> = read.filter_map(|e| e.ok()).filter_map(|e| Some((e.metadata().ok()?.modified().ok()?, e.path()))).collect();
        if files.len() > DISK_LIMIT {
            files.sort();
            for (_, path) in &files[..files.len() - DISK_LIMIT] {
                let _ = std::fs::remove_file(path);
            }
        }
    });
}

/// The file a cover is kept in: a hash of its address without the query and
/// of the size asked for. YouTube signs the query per request, so the same
/// picture comes back under a new one each time, and a key that kept it
/// would never match the address saved with the offline library.
fn disk_path(url: &str, target: Option<u32>) -> Option<PathBuf> {
    let dir = DISK_DIR.get()?;
    Some(dir.join(format!("{}-{}", address_hash(url), target.unwrap_or(0))))
}

/// The largest copy of `url` kept on disk at any size but `skip`.
/// Size 0 is the full picture, so it ranks first.
fn disk_path_any_size(url: &str, skip: Option<u32>) -> Option<PathBuf> {
    let dir = DISK_DIR.get()?;
    let prefix = format!("{}-", address_hash(url));
    std::fs::read_dir(dir)
        .ok()?
        .filter_map(|e| e.ok())
        .filter_map(|e| {
            let name = e.file_name().to_string_lossy().into_owned();
            let size: u32 = name.strip_prefix(&prefix)?.parse().ok()?;
            (Some(size) != skip).then(|| (if size == 0 { u32::MAX } else { size }, e.path()))
        })
        .max_by_key(|(size, _)| *size)
        .map(|(_, path)| path)
}

/// The largest copy of a remote cover the texture cache kept, for when the
/// network is gone.
pub fn disk_copy(url: &str) -> Option<PathBuf> {
    disk_path_any_size(url, None)
}

/// The cache file name for a cover address. YouTube re-signs `sqp` and `rs` on
/// every request, so those two go. The rest of the query stays: a mix cover is
/// `image/mixart?r=<payload>`, and without its query every mix is one picture.
fn address_hash(url: &str) -> String {
    use sha1::{Digest, Sha1};
    Sha1::digest(unsigned_address(url).as_bytes()).iter().map(|b| format!("{b:02x}")).collect()
}

fn unsigned_address(url: &str) -> String {
    let Some((path, query)) = url.split_once('?') else { return url.to_owned() };
    let kept: Vec<&str> = query.split('&').filter(|param| !param.starts_with("sqp=") && !param.starts_with("rs=")).collect();
    if kept.is_empty() { path.to_owned() } else { format!("{path}?{}", kept.join("&")) }
}

/// Through a sibling tmp file, so a partial write is never read back as a cover.
async fn write_disk(path: &std::path::Path, bytes: &[u8]) {
    let tmp = path.with_extension("tmp");
    let write = async {
        tokio::fs::write(&tmp, bytes).await?;
        tokio::fs::rename(&tmp, path).await
    };
    if let Err(err) = write.await {
        tracing::debug!(%err, ?path, "cover not cached on disk");
    }
}

thread_local! {
    /// Covers whose load failed, retried when the network returns.
    static FAILED: RefCell<Vec<std::rc::Weak<CoverImage>>> = const { RefCell::new(Vec::new()) };
    static TEXTURES: RefCell<HashMap<String, gdk::Texture>> = RefCell::new(HashMap::new());
    /// Keys in the order they went in, so the oldest goes first when full.
    static TEXTURE_ORDER: RefCell<std::collections::VecDeque<String>> = const { RefCell::new(std::collections::VecDeque::new()) };
}

/// Texture for a URL or local path, from cache, disk, or the network. Must run on the GTK thread.
/// `target` is the size the image will be drawn at, which decides how big a
/// copy is asked for. Scaling a 544 px cover down into a 56 px row is done in
/// sRGB, so it comes out darker and duller than the same picture fetched at
/// the size it is shown; it also costs fifteen times the bytes. None asks for
/// the largest, which is what the media controls want.
pub async fn load_texture(net: &NetHandle, url: &str, target: Option<u32>) -> Option<gdk::Texture> {
    if url.is_empty() {
        return None;
    }
    let key = cache_key(url, target);
    if let Some(texture) = TEXTURES.with(|c| c.borrow().get(&key).cloned()) {
        return Some(texture);
    }
    if let Some(path) = local_path(url) {
        // Read, decode and scale on the runtime like a remote cover. A downloaded
        // playlist is all local files, and doing this inline put every one of
        // them on the GTK thread: 245 samples of pixbuf scaling in one capture.
        let handle = net.spawn(async move {
            let bytes = tokio::fs::read(&path).await.map_err(|e| e.to_string())?;
            tokio::task::spawn_blocking(move || decode_bounded(bytes, target)).await.map_err(|e| e.to_string())?
        });
        return match handle.await {
            Ok(Ok(texture)) => {
                remember(&key, &texture);
                Some(texture)
            }
            Ok(Err(err)) => {
                tracing::debug!(%err, url, "texture decode failed");
                None
            }
            Err(_) => None,
        };
    }
    // The upscaled address first, then the original, then lower ytimg qualities.
    let candidates = fallback_chain(url, target);
    let http = net.client().http().clone();
    // Fetch and decode on the runtime: GdkTexture is thread-safe, and decoding
    // a cover on the GTK thread is what Python avoided with its worker pool.
    let disk = disk_path(url, target);
    let url_owned = url.to_owned();
    let handle = net.spawn(async move {
        // The disk copy first. It is what shows offline, and it spares a request online.
        if let Some(path) = &disk {
            if let Ok(bytes) = tokio::fs::read(path).await {
                if let Ok(Ok(texture)) = tokio::task::spawn_blocking(move || decode_bounded(bytes, target)).await {
                    return Ok(texture);
                }
                // A file that no longer decodes is refetched.
                let _ = tokio::fs::remove_file(path).await;
            }
        }
        let _slot = NET_SLOTS.acquire().await;
        let mut last_err = None;
        for candidate in candidates {
            match http.get(&candidate).send().await.and_then(|r| r.error_for_status()) {
                Ok(response) => match response.bytes().await {
                    Ok(bytes) => {
                        let bytes = bytes.to_vec();
                        let decoded = tokio::task::spawn_blocking(move || decode_for_disk(bytes, target)).await;
                        return match decoded {
                            Ok(Ok((texture, for_disk))) => {
                                if let Some(path) = &disk {
                                    write_disk(path, &for_disk).await;
                                }
                                Ok(texture)
                            }
                            Ok(Err(err)) => Err(anyhow::anyhow!("decode failed: {err}")),
                            Err(err) => Err(anyhow::anyhow!("decode task failed: {err}")),
                        };
                    }
                    Err(err) => last_err = Some(anyhow::Error::from(err)),
                },
                Err(err) => last_err = Some(anyhow::Error::from(err)),
            }
        }
        // Offline, a copy kept at another size beats a placeholder.
        if let Some(path) = disk_path_any_size(&url_owned, Some(target.unwrap_or(0))) {
            if let Ok(bytes) = tokio::fs::read(&path).await {
                if let Ok(Ok(texture)) = tokio::task::spawn_blocking(move || decode_bounded(bytes, target)).await {
                    return Ok(texture);
                }
            }
        }
        Err(last_err.expect("at least one candidate"))
    });
    match handle.await {
        Ok(Ok(texture)) => {
            remember(&key, &texture);
            Some(texture)
        }
        Ok(Err(err)) => {
            tracing::debug!(%err, url, "texture load failed");
            None
        }
        Err(_) => None,
    }
}

/// A texture drawn as a centered square crop, the crop AsyncPicture made.
/// Reports a square intrinsic size, so any image widget sizes it as a square.
mod square {
    use std::cell::RefCell;

    use gtk::{gdk, glib, graphene, prelude::*, subclass::prelude::*};

    mod imp {
        use super::*;

        #[derive(Default)]
        pub struct SquarePaintable {
            pub texture: RefCell<Option<gdk::Texture>>,
        }

        #[glib::object_subclass]
        impl ObjectSubclass for SquarePaintable {
            const NAME: &'static str = "MxSquarePaintable";
            type Type = super::SquarePaintable;
            type Interfaces = (gdk::Paintable,);
        }

        impl ObjectImpl for SquarePaintable {}

        impl PaintableImpl for SquarePaintable {
            fn flags(&self) -> gdk::PaintableFlags {
                gdk::PaintableFlags::STATIC_SIZE | gdk::PaintableFlags::STATIC_CONTENTS
            }

            fn intrinsic_width(&self) -> i32 {
                self.side()
            }

            fn intrinsic_height(&self) -> i32 {
                self.side()
            }

            fn intrinsic_aspect_ratio(&self) -> f64 {
                1.0
            }

            fn snapshot(&self, snapshot: &gdk::Snapshot, width: f64, height: f64) {
                let Some(texture) = self.texture.borrow().clone() else { return };
                let Some(snapshot) = snapshot.downcast_ref::<gtk::Snapshot>() else { return };
                let (tw, th) = (texture.width() as f64, texture.height() as f64);
                if tw <= 0.0 || th <= 0.0 {
                    return;
                }
                // Scale so the shorter side fills the box, then center the overflow.
                let scale = (width / tw).max(height / th);
                let (dw, dh) = (tw * scale, th * scale);
                snapshot.push_clip(&graphene::Rect::new(0.0, 0.0, width as f32, height as f32));
                snapshot.translate(&graphene::Point::new(((width - dw) / 2.0) as f32, ((height - dh) / 2.0) as f32));
                texture.snapshot(snapshot, dw, dh);
                snapshot.pop();
            }
        }

        impl SquarePaintable {
            fn side(&self) -> i32 {
                self.texture.borrow().as_ref().map(|t| t.width().min(t.height())).unwrap_or(1)
            }
        }
    }

    glib::wrapper! {
        pub struct SquarePaintable(ObjectSubclass<imp::SquarePaintable>) @implements gdk::Paintable;
    }

    impl SquarePaintable {
        pub fn new(texture: &gdk::Texture) -> Self {
            let paintable: Self = glib::Object::new();
            paintable.imp().texture.replace(Some(texture.clone()));
            paintable
        }
    }
}

pub use square::SquarePaintable;

/// Square art in a gtk::Image: a placeholder icon until the cropped art arrives.
pub struct CoverImage {
    image: gtk::Image,
    net: NetHandle,
    base_size: std::cell::Cell<i32>,
    current: RefCell<Option<String>>,
    /// Track whose cover was asked for, empty for a plain address.
    track: RefCell<String>,
}

impl CoverImage {
    pub fn new(net: NetHandle, size: i32) -> Rc<Self> {
        // Overflow hidden lets the CSS border-radius clip the art, as GtkPicture does by default.
        let image = gtk::Image::builder().pixel_size(size).icon_name(PLACEHOLDER_ICON).overflow(gtk::Overflow::Hidden).build();
        Rc::new(Self { image, net, base_size: std::cell::Cell::new(size), current: RefCell::new(None), track: RefCell::new(String::new()) })
    }

    /// A cover that follows the phone layout: thumbnail-sized art drops to 44 px, larger art stays.
    pub fn in_context(ctx: &Rc<UiContext>, size: i32) -> Rc<Self> {
        let cover = Self::new(ctx.net.clone(), size);
        let weak = Rc::downgrade(&cover);
        ctx.on_compact(move |compact| match weak.upgrade() {
            Some(cover) => {
                cover.set_compact(compact);
                true
            }
            None => false,
        });
        cover
    }

    /// The image widget: give it CSS classes and pack it.
    pub fn widget(&self) -> &gtk::Image {
        &self.image
    }

    /// Resize the square.
    pub fn set_size(&self, size: i32) {
        self.base_size.set(size);
        self.image.set_pixel_size(size);
    }

    /// Port of AsyncImage.set_compact: only thumbnail-sized art shrinks.
    pub fn set_compact(&self, compact: bool) {
        let base = self.base_size.get();
        if base > COMPACT_MAX_BASE {
            return;
        }
        let size = if compact { COMPACT_SIZE } else { base };
        if self.image.pixel_size() != size {
            self.image.set_pixel_size(size);
        }
    }

    /// Drop the art and forget its URL, so the next load of the same URL repaints.
    pub fn clear(&self) {
        self.current.replace(None);
        self.image.set_paintable(None::<&gdk::Paintable>);
        self.image.set_icon_name(Some(PLACEHOLDER_ICON));
    }

    /// Icon shown when there is no art, or while it loads.
    pub fn set_placeholder(&self, icon_name: &str) {
        if self.current.borrow().is_none() || self.image.paintable().is_none() {
            self.image.set_icon_name(Some(icon_name));
        }
    }

    /// The URL currently shown or loading.
    pub fn url(&self) -> Option<String> {
        self.current.borrow().clone()
    }

    /// Load again from the same address, for a file that changed underneath.
    pub fn reload(self: &Rc<Self>) {
        let Some(url) = self.current.replace(None) else { return };
        forget_texture(&url);
        self.load_address(&url);
    }

    /// Load a track's cover: the downloaded file's own art when there is
    /// one, so it shows offline, else `url`.
    pub fn load_track(self: &Rc<Self>, video_id: &str, url: &str) {
        self.track.replace(video_id.to_owned());
        if let Some(path) = cached_track_art(video_id) {
            self.load_address(&path.to_string_lossy());
            return;
        }
        self.load_address(url);
        if !is_downloaded(video_id) {
            return;
        }
        // Downloaded before the cover was kept: pull it out of the file, then swap.
        let (net, id) = (self.net.clone(), video_id.to_owned());
        let weak = Rc::downgrade(self);
        glib::spawn_future_local(async move {
            let Some(path) = track_art(&net, &id).await else { return };
            let Some(this) = weak.upgrade() else { return };
            if *this.track.borrow() == id {
                this.load_address(&path.to_string_lossy());
            }
        });
    }

    pub fn load(self: &Rc<Self>, url: &str) {
        self.track.replace(String::new());
        self.load_address(url);
    }

    fn load_address(self: &Rc<Self>, url: &str) {
        if url.is_empty() {
            self.current.replace(None);
            self.image.set_icon_name(Some(PLACEHOLDER_ICON));
            return;
        }
        if self.current.borrow().as_deref() == Some(url) {
            return;
        }
        let url = url.to_owned();
        self.current.replace(Some(url.clone()));
        if self.image.paintable().is_some() {
            self.image.set_icon_name(Some(PLACEHOLDER_ICON));
        }

        let net = self.net.clone();
        let size = self.base_size.get().max(1) as u32;
        if let Some(texture) = TEXTURES.with(|c| c.borrow().get(&cache_key(&url, Some(size))).cloned()) {
            self.image.set_paintable(Some(&SquarePaintable::new(&texture)));
            return;
        }
        let weak = Rc::downgrade(self);
        glib::spawn_future_local(async move {
            // A row dragged past by the scrollbar is rebound within a frame or two.
            // Waiting a moment keeps it from queueing a fetch nobody will see.
            glib::timeout_future(LOAD_SETTLE).await;
            if weak.upgrade().is_none_or(|this| this.current.borrow().as_deref() != Some(url.as_str())) {
                return;
            }
            let texture = load_texture(&net, &url, Some(size)).await;
            let Some(this) = weak.upgrade() else { return };
            if this.current.borrow().as_deref() != Some(url.as_str()) {
                return;
            }
            match texture {
                Some(texture) => this.image.set_paintable(Some(&SquarePaintable::new(&texture))),
                // Kept on a list, so the cover fills in when the network is back.
                None => FAILED.with(|f| {
                    let mut failed = f.borrow_mut();
                    failed.retain(|cover| cover.strong_count() > 0);
                    failed.push(Rc::downgrade(&this));
                }),
            }
        });
    }
}

/// Bytes of the first address in the fallback chain that answers. For
/// consumers outside the texture cache, such as the MPRIS art file.
pub async fn fetch_cover_bytes(http: &reqwest::Client, auth: Option<&HttpAuth>, url: &str, target: Option<u32>) -> Option<Vec<u8>> {
    if let Some(path) = local_path(url) {
        return tokio::fs::read(path).await.ok().filter(|b| !b.is_empty());
    }
    if crate::net::offline::sealed() {
        return None;
    }
    for candidate in fallback_chain(url, target) {
        let mut request = http.get(&candidate);
        // Private covers on YouTube's hosts need the session cookie.
        if let Some(auth) = auth.filter(|_| ["youtube.com", "ytimg.com", "googleusercontent.com", "ggpht.com"].iter().any(|d| candidate.contains(d))) {
            request = request.header("Cookie", &auth.cookie).header("User-Agent", &auth.user_agent);
        }
        match request.send().await.and_then(|r| r.error_for_status()) {
            Ok(response) => match response.bytes().await {
                Ok(bytes) if !bytes.is_empty() => return Some(bytes.to_vec()),
                _ => continue,
            },
            Err(_) => continue,
        }
    }
    // Offline: whatever copy the texture cache kept.
    let path = disk_path(url, target).filter(|p| p.is_file()).or_else(|| disk_path_any_size(url, None))?;
    tokio::fs::read(path).await.ok().filter(|b| !b.is_empty())
}

/// Addresses to try in order: the upscaled form, the original, then each lower ytimg quality.
fn fallback_chain(url: &str, target: Option<u32>) -> Vec<String> {
    const QUALITIES: [&str; 5] = ["maxresdefault", "sddefault", "hqdefault", "mqdefault", "default"];
    let mut out = Vec::new();
    let mut push = |candidate: String| {
        if !out.contains(&candidate) {
            out.push(candidate);
        }
    };
    let upscaled = high_res_url(url, target);
    push(upscaled.clone());
    push(url.to_owned());
    for base in [upscaled, url.to_owned()] {
        if base.contains("i.ytimg.com") {
            if let Some(pos) = QUALITIES.iter().position(|q| base.contains(q)) {
                for lower in &QUALITIES[pos + 1..] {
                    push(base.replace(QUALITIES[pos], lower));
                }
            }
        }
    }
    out
}

fn local_path(url: &str) -> Option<PathBuf> {
    if url.starts_with("file://") {
        return glib::filename_from_uri(url).ok().map(|(path, _)| path);
    }
    // An absolute path in the platform's own spelling, a drive letter on Windows.
    Path::new(url).is_absolute().then(|| PathBuf::from(url))
}

/// Drop a cached texture, for a file that has been written over.
///
/// The cache is keyed by address, and a mirrored cover keeps its path when its
/// picture changes, so without this the old image stays on screen.
/// Load again every cover that failed, once the network is back.
pub fn retry_failed() {
    let failed = FAILED.with(|f| std::mem::take(&mut *f.borrow_mut()));
    for cover in failed.iter().filter_map(std::rc::Weak::upgrade) {
        if let Some(url) = cover.current.replace(None) {
            cover.load_address(&url);
        }
    }
}

pub fn forget_texture(url: &str) {
    // The disk copies go too, at every size: an edited cover keeps its address.
    if let Some(dir) = DISK_DIR.get() {
        let prefix = format!("{}-", address_hash(url));
        if let Ok(read) = std::fs::read_dir(dir) {
            for entry in read.filter_map(|e| e.ok()).filter(|e| e.file_name().to_string_lossy().starts_with(&prefix)) {
                let _ = std::fs::remove_file(entry.path());
            }
        }
    }
    TEXTURES.with(|cache| cache.borrow_mut().retain(|key, _| key.split_once('\n').map_or(key.as_str(), |(u, _)| u) != url));
}

/// A texture already in memory, for a widget that must not show a frame of
/// stale art while an async load comes back with it.
pub fn cached_texture(url: &str, target: Option<u32>) -> Option<gdk::Texture> {
    TEXTURES.with(|c| c.borrow().get(&cache_key(url, target)).cloned())
}

fn cache_key(url: &str, target: Option<u32>) -> String {
    format!("{url}\n{}", target.unwrap_or(0))
}

/// Decode, then shrink to what the widget can show. Port of the scale and
/// centre crop AsyncImage did: cover-fit into twice the target, for HiDPI.
/// Without it a 56 px row pins a 1280x720 video thumbnail at 3.7 MB, and a
/// home feed of 450 covers held about 500 MB of pixels.
fn decode_bounded(bytes: Vec<u8>, target: Option<u32>) -> Result<gdk::Texture, String> {
    decode_for_disk(bytes, target).map(|(texture, _)| texture)
}

/// `decode_bounded`, plus the bytes worth keeping on disk: the shrunk copy
/// re-encoded when the source was bigger than needed, else the source as it
/// came. A 1280x720 thumbnail shown at 56 px is 150 KB as fetched and 6 KB shrunk.
fn decode_for_disk(bytes: Vec<u8>, target: Option<u32>) -> Result<(gdk::Texture, Vec<u8>), String> {
    use gtk::gdk_pixbuf::{InterpType, Pixbuf};
    let Some(target) = target else {
        let texture = gdk::Texture::from_bytes(&glib::Bytes::from(&bytes)).map_err(|e| e.to_string())?;
        return Ok((texture, bytes));
    };
    let stream = gtk::gio::MemoryInputStream::from_bytes(&glib::Bytes::from(&bytes));
    let pixbuf = Pixbuf::from_stream(&stream, gtk::gio::Cancellable::NONE).map_err(|e| e.to_string())?;
    let side = (target * 2) as i32;
    let (w, h) = (pixbuf.width(), pixbuf.height());
    if w <= side && h <= side {
        return Ok((gdk::Texture::for_pixbuf(&pixbuf), bytes));
    }
    let scale = (f64::from(side) / f64::from(w)).max(f64::from(side) / f64::from(h));
    let (new_w, new_h) = (((f64::from(w) * scale) as i32).max(1), ((f64::from(h) * scale) as i32).max(1));
    let scaled = pixbuf.scale_simple(new_w, new_h, InterpType::Bilinear).ok_or("scale failed")?;
    let (crop_w, crop_h) = (side.min(new_w), side.min(new_h));
    let cropped = scaled.new_subpixbuf((new_w - crop_w) / 2, (new_h - crop_h) / 2, crop_w, crop_h);
    // JPEG has no alpha, so a picture that uses it stays PNG. Light compression:
    // the default level cost more worker time than the decode it saves.
    let encoded = if cropped.has_alpha() { cropped.save_to_bufferv("png", &[("compression", "2")]) } else { cropped.save_to_bufferv("jpeg", &[("quality", "92")]) };
    Ok((gdk::Texture::for_pixbuf(&cropped), encoded.unwrap_or(bytes)))
}

fn remember(url: &str, texture: &gdk::Texture) {
    // The oldest goes, not everything: emptying the cache when full also threw
    // out the covers the player carousel had just preloaded.
    TEXTURES.with(|cache| {
        TEXTURE_ORDER.with(|order| {
            let (mut cache, mut order) = (cache.borrow_mut(), order.borrow_mut());
            while cache.len() >= CACHE_LIMIT {
                let Some(oldest) = order.pop_front() else {
                    cache.clear();
                    break;
                };
                cache.remove(&oldest);
            }
            if cache.insert(url.to_owned(), texture.clone()).is_none() {
                order.push_back(url.to_owned());
            }
        });
    });
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn mix_covers_keep_their_query_in_the_cache_key() {
        assert_eq!(unsigned_address("https://i.ytimg.com/vi/x/hqdefault.jpg?sqp=abc&rs=def"), "https://i.ytimg.com/vi/x/hqdefault.jpg");
        assert_eq!(unsigned_address("https://music.youtube.com/image/mixart?r=AAA"), "https://music.youtube.com/image/mixart?r=AAA");
        assert_ne!(address_hash("https://music.youtube.com/image/mixart?r=AAA"), address_hash("https://music.youtube.com/image/mixart?r=BBB"));
        assert_eq!(address_hash("https://i.ytimg.com/vi/x/hqdefault.jpg?sqp=1"), address_hash("https://i.ytimg.com/vi/x/hqdefault.jpg?sqp=2"));
    }
}

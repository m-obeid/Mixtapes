//! Cover art loader. Fetches over the network runtime, decodes on the GTK
//! thread, keeps a small per-process texture cache. Stale results are dropped
//! by comparing the URL at completion, so rapid track changes never race.
//! Art is drawn cover-fit inside a fixed square, the way AsyncPicture cropped
//! every thumbnail to a centered square.

use std::cell::RefCell;
use std::collections::HashMap;
use std::path::PathBuf;
use std::rc::Rc;

use gtk::{gdk, glib};

use crate::model::HttpAuth;
use crate::net::NetHandle;
use crate::ui::context::UiContext;
use crate::ui::high_res_url;

const PLACEHOLDER_ICON: &str = "audio-x-generic-symbolic";
const CACHE_LIMIT: usize = 64;
/// Thumbnail size on phones, and the largest base size the swap applies to.
const COMPACT_SIZE: i32 = 44;
const COMPACT_MAX_BASE: i32 = 80;

thread_local! {
    static TEXTURES: RefCell<HashMap<String, gdk::Texture>> = RefCell::new(HashMap::new());
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
        let decoded = std::fs::read(&path).map_err(|e| e.to_string()).and_then(|bytes| decode_bounded(bytes, target));
        return match decoded {
            Ok(texture) => {
                remember(&key, &texture);
                Some(texture)
            }
            Err(err) => {
                tracing::debug!(%err, path = %path.display(), "texture decode failed");
                None
            }
        };
    }
    // The upscaled address first, then the original, then lower ytimg qualities.
    let candidates = fallback_chain(url, target);
    let http = net.client().http().clone();
    // Fetch and decode on the runtime: GdkTexture is thread-safe, and decoding
    // a cover on the GTK thread is what Python avoided with its worker pool.
    let handle = net.spawn(async move {
        let mut last_err = None;
        for candidate in candidates {
            match http.get(&candidate).send().await.and_then(|r| r.error_for_status()) {
                Ok(response) => match response.bytes().await {
                    Ok(bytes) => {
                        let decoded = tokio::task::spawn_blocking(move || decode_bounded(bytes.to_vec(), target)).await;
                        return match decoded {
                            Ok(Ok(texture)) => Ok(texture),
                            Ok(Err(err)) => Err(anyhow::anyhow!("decode failed: {err}")),
                            Err(err) => Err(anyhow::anyhow!("decode task failed: {err}")),
                        };
                    }
                    Err(err) => last_err = Some(anyhow::Error::from(err)),
                },
                Err(err) => last_err = Some(anyhow::Error::from(err)),
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
}

impl CoverImage {
    pub fn new(net: NetHandle, size: i32) -> Rc<Self> {
        // Overflow hidden lets the CSS border-radius clip the art, as GtkPicture does by default.
        let image = gtk::Image::builder().pixel_size(size).icon_name(PLACEHOLDER_ICON).overflow(gtk::Overflow::Hidden).build();
        Rc::new(Self { image, net, base_size: std::cell::Cell::new(size), current: RefCell::new(None) })
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
        self.load(&url);
    }

    pub fn load(self: &Rc<Self>, url: &str) {
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
        let weak = Rc::downgrade(self);
        glib::spawn_future_local(async move {
            let texture = load_texture(&net, &url, Some(size)).await;
            let Some(this) = weak.upgrade() else { return };
            if this.current.borrow().as_deref() != Some(url.as_str()) {
                return;
            }
            if let Some(texture) = texture {
                this.image.set_paintable(Some(&SquarePaintable::new(&texture)));
            }
        });
    }
}

/// Bytes of the first address in the fallback chain that answers. For
/// consumers outside the texture cache, such as the MPRIS art file.
pub async fn fetch_cover_bytes(http: &reqwest::Client, auth: Option<&HttpAuth>, url: &str, target: Option<u32>) -> Option<Vec<u8>> {
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
    None
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
    if url.starts_with('/') {
        return Some(PathBuf::from(url));
    }
    None
}

/// Drop a cached texture, for a file that has been written over.
///
/// The cache is keyed by address, and a mirrored cover keeps its path when its
/// picture changes, so without this the old image stays on screen.
pub fn forget_texture(url: &str) {
    // Every size of it: the key carries the target after the address.
    TEXTURES.with(|cache| cache.borrow_mut().retain(|key, _| key.split_once('\n').map_or(key.as_str(), |(u, _)| u) != url));
}

fn cache_key(url: &str, target: Option<u32>) -> String {
    format!("{url}\n{}", target.unwrap_or(0))
}

/// Decode, then shrink to what the widget can show. Port of the scale and
/// centre crop AsyncImage did: cover-fit into twice the target, for HiDPI.
/// Without it a 56 px row pins a 1280x720 video thumbnail at 3.7 MB, and a
/// home feed of 450 covers held about 500 MB of pixels.
fn decode_bounded(bytes: Vec<u8>, target: Option<u32>) -> Result<gdk::Texture, String> {
    use gtk::gdk_pixbuf::{InterpType, Pixbuf};
    let bytes = glib::Bytes::from_owned(bytes);
    let Some(target) = target else {
        return gdk::Texture::from_bytes(&bytes).map_err(|e| e.to_string());
    };
    let stream = gtk::gio::MemoryInputStream::from_bytes(&bytes);
    let pixbuf = Pixbuf::from_stream(&stream, gtk::gio::Cancellable::NONE).map_err(|e| e.to_string())?;
    let side = (target * 2) as i32;
    let (w, h) = (pixbuf.width(), pixbuf.height());
    if w <= side && h <= side {
        return Ok(gdk::Texture::for_pixbuf(&pixbuf));
    }
    let scale = (f64::from(side) / f64::from(w)).max(f64::from(side) / f64::from(h));
    let (new_w, new_h) = (((f64::from(w) * scale) as i32).max(1), ((f64::from(h) * scale) as i32).max(1));
    let scaled = pixbuf.scale_simple(new_w, new_h, InterpType::Bilinear).ok_or("scale failed")?;
    let (crop_w, crop_h) = (side.min(new_w), side.min(new_h));
    let cropped = scaled.new_subpixbuf((new_w - crop_w) / 2, (new_h - crop_h) / 2, crop_w, crop_h);
    Ok(gdk::Texture::for_pixbuf(&cropped))
}

fn remember(url: &str, texture: &gdk::Texture) {
    TEXTURES.with(|cache| {
        let mut cache = cache.borrow_mut();
        if cache.len() >= CACHE_LIMIT {
            cache.clear();
        }
        cache.insert(url.to_owned(), texture.clone());
    });
}

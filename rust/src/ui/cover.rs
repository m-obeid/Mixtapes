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
pub async fn load_texture(net: &NetHandle, url: &str) -> Option<gdk::Texture> {
    if url.is_empty() {
        return None;
    }
    if let Some(texture) = TEXTURES.with(|c| c.borrow().get(url).cloned()) {
        return Some(texture);
    }
    if let Some(path) = local_path(url) {
        return match gdk::Texture::from_filename(&path) {
            Ok(texture) => {
                remember(url, &texture);
                Some(texture)
            }
            Err(err) => {
                tracing::debug!(%err, path = %path.display(), "texture decode failed");
                None
            }
        };
    }
    // The upscaled address first, then the original, then lower ytimg qualities.
    let candidates = fallback_chain(url);
    let http = net.client().http().clone();
    // Fetch and decode on the runtime: GdkTexture is thread-safe, and decoding
    // a cover on the GTK thread is what Python avoided with its worker pool.
    let handle = net.spawn(async move {
        let mut last_err = None;
        for candidate in candidates {
            match http.get(&candidate).send().await.and_then(|r| r.error_for_status()) {
                Ok(response) => match response.bytes().await {
                    Ok(bytes) => {
                        let decoded = tokio::task::spawn_blocking(move || gdk::Texture::from_bytes(&glib::Bytes::from_owned(bytes.to_vec()))).await;
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
            remember(url, &texture);
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
        let weak = Rc::downgrade(self);
        glib::spawn_future_local(async move {
            let texture = load_texture(&net, &url).await;
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
pub async fn fetch_cover_bytes(http: &reqwest::Client, auth: Option<&HttpAuth>, url: &str) -> Option<Vec<u8>> {
    for candidate in fallback_chain(url) {
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
fn fallback_chain(url: &str) -> Vec<String> {
    const QUALITIES: [&str; 5] = ["maxresdefault", "sddefault", "hqdefault", "mqdefault", "default"];
    let mut out = Vec::new();
    let mut push = |candidate: String| {
        if !out.contains(&candidate) {
            out.push(candidate);
        }
    };
    let upscaled = high_res_url(url, None);
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

fn remember(url: &str, texture: &gdk::Texture) {
    TEXTURES.with(|cache| {
        let mut cache = cache.borrow_mut();
        if cache.len() >= CACHE_LIMIT {
            cache.clear();
        }
        cache.insert(url.to_owned(), texture.clone());
    });
}

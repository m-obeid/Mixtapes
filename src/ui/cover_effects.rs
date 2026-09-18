//! Port of ui/cover_effects.py: a heavily blurred copy of the cover for the
//! Amberol-style background, and the accent color extracted from it. Both run
//! on the network runtime, with the image work on blocking threads, and both
//! are cached so a repeated lookup for the same cover is free.
//! The disk layout is the Python app's: raw cover bytes under
//! <cache>/thumbs/<sha1 of url>, blurred PNGs under <cache>/covers_blurred.
#![allow(dead_code)]

mod pil;

use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::sync::{LazyLock, Mutex};
use std::time::Duration;

use image::RgbImage;
use sha1::{Digest, Sha1};
use tokio::sync::{Semaphore, watch};

use crate::ui::color_utils::{self, Rgb};
use pil::Filter;

const MAX_BLUR_CACHE_ENTRIES: usize = 48;
const MAX_COLOR_CACHE_ENTRIES: usize = 128;

/// PIL GaussianBlur radius, and the side of the square PNG it is applied to.
const BLUR_RADIUS: u32 = 42;
const BLUR_OUTPUT_SIZE: u32 = 720;

// Accent selection, in OkLCh. Below MIN_ACCENT_CHROMA a color reads as gray.
// Accents look best near IDEAL_ACCENT_LIGHTNESS, falling off over ACCENT_LIGHTNESS_SPREAD.
const MIN_ACCENT_CHROMA: f64 = 0.045;
const IDEAL_ACCENT_LIGHTNESS: f64 = 0.62;
const ACCENT_LIGHTNESS_SPREAD: f64 = 0.32;
/// Monochrome covers get the full treatment, in grays. Featureless ones get nothing.
/// This is the OkLCh lightness spread across the cover: solid fills measure 0,
/// the lowest real cover in 300 measured 0.03. Only colorless covers reach this test.
const MIN_COVER_DETAIL: f64 = 0.05;

// Blurred-background normalization. A fixed tint left the album art deciding how legible the chrome was.
// Dark: median luminance to 0.025, highlights capped at 0.12.
// Light: blend toward white until the 2nd-percentile luminance hits 0.35.
const BLUR_DARK_MEDIAN: f64 = 0.025;
const BLUR_DARK_HIGHLIGHT_CAP: f64 = 0.12;
const BLUR_LIGHT_FLOOR: f64 = 0.35;
const BLUR_MAX_GAIN: f64 = 3.0;

/// A blurred, normalized cover on disk.
#[derive(Clone, Debug, PartialEq)]
pub struct BlurredCover {
    pub path: PathBuf,
    /// (typical, worst for text) relative luminance the image landed on.
    /// Callers derive the colors going on top from it.
    pub backdrop: (f64, f64),
}

/// A small least-recently-used map, OrderedDict with move_to_end and popitem(last=False).
struct Lru<K, V> {
    limit: usize,
    // Oldest first.
    entries: Vec<(K, V)>,
}

impl<K: PartialEq, V: Clone> Lru<K, V> {
    const fn new(limit: usize) -> Self {
        Self { limit, entries: Vec::new() }
    }

    fn get(&mut self, key: &K) -> Option<V> {
        let at = self.entries.iter().position(|(k, _)| k == key)?;
        let entry = self.entries.remove(at);
        let value = entry.1.clone();
        self.entries.push(entry);
        Some(value)
    }

    fn put(&mut self, key: K, value: V) {
        self.entries.retain(|(k, _)| *k != key);
        self.entries.push((key, value));
        while self.entries.len() > self.limit {
            self.entries.remove(0);
        }
    }
}

/// None records a cover the pipeline declined, so it is not decoded again.
static BLUR_CACHE: Mutex<Lru<(String, bool), Option<BlurredCover>>> = Mutex::new(Lru::new(MAX_BLUR_CACHE_ENTRIES));
/// None records "no usable accent".
static COLOR_CACHE: Mutex<Lru<String, Option<Rgb>>> = Mutex::new(Lru::new(MAX_COLOR_CACHE_ENTRIES));

/// One fetch per cover. On a track change the blur and the accent both want the
/// same bytes. The second caller waits for the first, then reads the disk cache.
static INFLIGHT: LazyLock<Mutex<HashMap<String, watch::Receiver<bool>>>> = LazyLock::new(Default::default);
/// Python ran these on a pool of two threads. A 720 px blur holds several MB while it runs.
static EFFECT_SLOTS: Semaphore = Semaphore::const_new(2);

const FETCH_TIMEOUT: Duration = Duration::from_secs(10);
/// Cap the wait so a stuck leader does not hang its followers.
const FOLLOWER_TIMEOUT: Duration = Duration::from_secs(20);

fn thumb_cache_key(url: &str) -> String {
    Sha1::digest(url.as_bytes()).iter().map(|byte| format!("{byte:02x}")).collect()
}

fn thumb_cache_path(cache_dir: &Path, url: &str) -> PathBuf {
    cache_dir.join("thumbs").join(thumb_cache_key(url))
}

fn blur_cache_path(cache_dir: &Path, url: &str, dark: bool) -> PathBuf {
    let scheme = if dark { "dark" } else { "light" };
    cache_dir.join("covers_blurred").join(format!("{}_b{BLUR_RADIUS}_s{BLUR_OUTPUT_SIZE}_{scheme}.png", thumb_cache_key(url)))
}

async fn read_thumb_cache(cache_dir: &Path, url: &str) -> Option<Vec<u8>> {
    tokio::fs::read(thumb_cache_path(cache_dir, url)).await.ok().filter(|bytes| !bytes.is_empty())
}

/// Write to a sibling tmp file then rename, so a partial write is never read as a cover.
async fn write_thumb_cache(cache_dir: &Path, url: &str, bytes: &[u8]) {
    let path = thumb_cache_path(cache_dir, url);
    let tmp = path.with_extension("tmp");
    let write = async {
        tokio::fs::create_dir_all(cache_dir.join("thumbs")).await?;
        tokio::fs::write(&tmp, bytes).await?;
        tokio::fs::rename(&tmp, &path).await
    };
    if let Err(err) = write.await {
        tracing::debug!(%err, ?path, "thumb cache not written");
    }
}

/// A YouTube video thumbnail, then its lower-resolution variants. Anything else is tried as is.
/// maxres and sd only exist for high-resolution uploads, hq, mq and default always do.
/// Nothing above the requested quality is tried: that only finds something missing more often.
fn yt_thumb_fallback_urls(url: &str) -> Vec<String> {
    const FALLBACKS: [&str; 5] = ["maxresdefault", "sddefault", "hqdefault", "mqdefault", "default"];
    static YT_THUMB_RE: LazyLock<regex::Regex> = LazyLock::new(|| regex::Regex::new(r"^(https?://i\.ytimg\.com/vi/[^/]+/)([^/.?]+)(\.[A-Za-z]+)(\?.*)?$").unwrap());
    let mut out = vec![url.to_owned()];
    let Some(caps) = YT_THUMB_RE.captures(url) else { return out };
    let (prefix, variant, ext) = (&caps[1], &caps[2], &caps[3]);
    let query = caps.get(4).map_or("", |m| m.as_str());
    let start = FALLBACKS.iter().position(|f| *f == variant).map_or(0, |at| at + 1);
    for fallback in &FALLBACKS[start..] {
        if *fallback != variant {
            out.push(format!("{prefix}{fallback}{ext}{query}"));
        }
    }
    out
}

/// The path behind a file:// cover or a bare absolute path.
/// The ?m=<mtime> cache-buster on a local playlist cover comes off first.
fn local_cover_path(url: &str) -> Option<&str> {
    if let Some(path) = url.strip_prefix("file://") {
        return Some(path.rfind('?').map_or(path, |q| &path[..q]));
    }
    url.starts_with('/').then_some(url)
}

/// Clears the in-flight entry and wakes the followers, also when the fetch is aborted.
struct Leader {
    url: String,
    done: watch::Sender<bool>,
}

impl Drop for Leader {
    fn drop(&mut self) {
        INFLIGHT.lock().unwrap().remove(&self.url);
        let _ = self.done.send(true);
    }
}

/// Cached or downloaded bytes for `url`. Port of _ensure_image_bytes.
async fn ensure_image_bytes(http: &reqwest::Client, cache_dir: &Path, url: &str) -> Option<Vec<u8>> {
    if url.is_empty() {
        return None;
    }
    // Local covers skip the thumb cache: the bytes are already on disk.
    if let Some(path) = local_cover_path(url) {
        return match tokio::fs::read(path).await {
            Ok(bytes) => Some(bytes),
            Err(err) => {
                tracing::debug!(%err, path, "local cover read failed");
                None
            }
        };
    }
    if let Some(bytes) = read_thumb_cache(cache_dir, url).await {
        return Some(bytes);
    }

    let follow = {
        let mut inflight = INFLIGHT.lock().unwrap();
        match inflight.get(url) {
            Some(done) => Ok(done.clone()),
            None => {
                let (done, waiting) = watch::channel(false);
                inflight.insert(url.to_owned(), waiting);
                Err(Leader { url: url.to_owned(), done })
            }
        }
    };
    let _leader = match follow {
        Ok(mut done) => {
            let _ = tokio::time::timeout(FOLLOWER_TIMEOUT, done.wait_for(|finished| *finished)).await;
            return read_thumb_cache(cache_dir, url).await;
        }
        Err(leader) => leader,
    };

    for candidate in yt_thumb_fallback_urls(url) {
        let response = match http.get(&candidate).header("User-Agent", "Mozilla/5.0").timeout(FETCH_TIMEOUT).send().await {
            Ok(response) => response,
            Err(err) => {
                tracing::debug!(%err, url, "cover fetch failed");
                return None;
            }
        };
        // Only a 404 walks to the next fallback. A different URL fixes nothing for a timeout or a DNS failure.
        if response.status() == reqwest::StatusCode::NOT_FOUND {
            continue;
        }
        let bytes = match response.error_for_status() {
            Ok(response) => response.bytes().await,
            Err(err) => Err(err),
        };
        return match bytes {
            Ok(bytes) => {
                // Cached under the requested URL, whichever fallback served the bytes.
                write_thumb_cache(cache_dir, url, &bytes).await;
                Some(bytes.to_vec())
            }
            Err(err) => {
                tracing::debug!(%err, url, "cover fetch failed");
                None
            }
        };
    }
    tracing::debug!(url, "cover fetch failed: every variant is missing");
    None
}

/// Run image work on a blocking thread, two at a time.
async fn run_effect<T: Send + 'static>(work: impl FnOnce() -> T + Send + 'static) -> Option<T> {
    let _slot = EFFECT_SLOTS.acquire().await.ok()?;
    tokio::task::spawn_blocking(work).await.ok()
}

fn decode(bytes: &[u8]) -> Option<RgbImage> {
    match image::load_from_memory(bytes) {
        Ok(img) => Some(img.to_rgb8()),
        Err(err) => {
            tracing::debug!(%err, "cover decode failed");
            None
        }
    }
}

/// Relative luminance per 8-bit pixel, through a table of the 256 linearized channel values.
fn pixel_luminances(img: &RgbImage) -> Vec<f64> {
    static LINEAR: LazyLock<[f64; 256]> = LazyLock::new(|| std::array::from_fn(|i| color_utils::srgb_to_linear(i as f64 / 255.0)));
    img.pixels().map(|p| 0.2126 * LINEAR[usize::from(p[0])] + 0.7152 * LINEAR[usize::from(p[1])] + 0.0722 * LINEAR[usize::from(p[2])]).collect()
}

/// Sorted relative luminances of a 48x48 sample.
fn blur_luminances(img: &RgbImage) -> Vec<f64> {
    let mut values = pixel_luminances(&pil::resize(img, 48, 48, Filter::Bicubic));
    values.sort_by(f64::total_cmp);
    values
}

fn percentile(values: &[f64], fraction: f64) -> f64 {
    values[(values.len() - 1).min((values.len() as f64 * fraction) as usize)]
}

/// Bring a blurred cover into the scheme's luminance band.
fn normalize_blur(img: &RgbImage, dark: bool) -> RgbImage {
    if dark {
        // Blending toward black is a scale in sRGB. One gain dims a bright cover and lifts a near-black one.
        let values = blur_luminances(img);
        let mut gain = (BLUR_DARK_MEDIAN / percentile(&values, 0.5).max(1e-5)).powf(1.0 / 2.4);
        gain = gain.clamp(0.05, BLUR_MAX_GAIN);
        let highlight = percentile(&values, 0.98) * gain.powf(2.4);
        if highlight > BLUR_DARK_HIGHLIGHT_CAP {
            gain *= (BLUR_DARK_HIGHLIGHT_CAP / highlight).powf(1.0 / 2.4);
        }
        return pil::brightness(img, gain);
    }
    // Light: lift toward white until the darkest areas, which set worst-case text contrast, clear the floor.
    // The search runs on a 48x48 proxy, the full image is blended once.
    let proxy = pil::resize(img, 48, 48, Filter::Bicubic);
    let (mut lo, mut hi) = (0.0, 1.0);
    for _ in 0..12 {
        let mid = (lo + hi) / 2.0;
        if percentile(&blur_luminances(&pil::blend_toward_white(&proxy, mid)), 0.02) < BLUR_LIGHT_FLOOR {
            lo = mid;
        } else {
            hi = mid;
        }
    }
    pil::blend_toward_white(img, hi)
}

/// The blurred, normalized image and its backdrop luminances. None for a featureless cover.
/// One verdict for both effects: a cover that keeps its backdrop while its accent
/// falls back to the system accent mixes two unrelated colors.
fn render_blur(img: &RgbImage, dark: bool) -> Option<(RgbImage, (f64, f64))> {
    pick_accent(img)?;
    let (w, h) = img.dimensions();
    let side = w.min(h);
    let square = image::imageops::crop_imm(img, (w - side) / 2, (h - side) / 2, side, side).to_image();
    let resized = pil::resize(&square, BLUR_OUTPUT_SIZE, BLUR_OUTPUT_SIZE, Filter::Lanczos);
    let blurred = pil::gaussian_blur(&resized, BLUR_RADIUS as f32);
    let normalized = normalize_blur(&pil::saturation(&blurred, 1.25), dark);
    let values = blur_luminances(&normalized);
    // Typical brightness, plus the end worst for text.
    let backdrop = (percentile(&values, 0.5), percentile(&values, if dark { 0.98 } else { 0.02 }));
    Some((normalized, backdrop))
}

fn save_png(img: &RgbImage, path: &Path) -> anyhow::Result<()> {
    use image::ImageEncoder;
    use image::codecs::png::{CompressionType, FilterType, PngEncoder};

    if let Some(dir) = path.parent() {
        std::fs::create_dir_all(dir)?;
    }
    let tmp = PathBuf::from(format!("{}.tmp", path.display()));
    let file = std::io::BufWriter::new(std::fs::File::create(&tmp)?);
    PngEncoder::new_with_quality(file, CompressionType::Best, FilterType::Adaptive).write_image(img.as_raw(), img.width(), img.height(), image::ExtendedColorType::Rgb8)?;
    std::fs::rename(&tmp, path)?;
    Ok(())
}

/// Blur `url`, normalized into the luminance band of the dark or light scheme. See normalize_blur.
/// None on failure or for a cover the accent pipeline declined. Cached per (url, dark).
/// `cache_dir` is Paths::cache_dir. Runs on the network runtime: call through NetHandle::spawn.
pub async fn get_blurred_cover(http: reqwest::Client, cache_dir: PathBuf, url: String, dark: bool) -> Option<BlurredCover> {
    if url.is_empty() {
        return None;
    }
    let key = (url.clone(), dark);
    match BLUR_CACHE.lock().unwrap().get(&key) {
        Some(None) => return None,
        Some(Some(cover)) if cover.path.exists() => return Some(cover),
        _ => {}
    }

    let bytes = ensure_image_bytes(&http, &cache_dir, &url).await?;
    let path = blur_cache_path(&cache_dir, &url, dark);
    // Some(None) is a declined cover, None a failure. Only the first is remembered.
    let rendered = run_effect(move || {
        let img = decode(&bytes)?;
        let Some((blurred, backdrop)) = render_blur(&img, dark) else { return Some(None) };
        match save_png(&blurred, &path) {
            Ok(()) => Some(Some(BlurredCover { path, backdrop })),
            Err(err) => {
                tracing::debug!(%err, ?path, "blurred cover not saved");
                None
            }
        }
    })
    .await??;
    BLUR_CACHE.lock().unwrap().put(key, rendered.clone());
    rendered
}

/// Sorted OkLCh lightnesses of a coarse sample of the cover.
fn cover_lightnesses(img: &RgbImage) -> Vec<f64> {
    let small = pil::resize(img, 32, 32, Filter::Bicubic);
    let mut values: Vec<f64> = small.pixels().map(|p| color_utils::rgb_to_oklch((f64::from(p[0]) / 255.0, f64::from(p[1]) / 255.0, f64::from(p[2]) / 255.0)).0).collect();
    values.sort_by(f64::total_cmp);
    values
}

/// Spread between the cover's dark and light ends, in OkLCh lightness.
pub fn cover_detail(img: &RgbImage) -> f64 {
    let values = cover_lightnesses(img);
    if values.is_empty() {
        return 0.0;
    }
    let n = values.len() as f64;
    values[(n * 0.95) as usize] - values[(n * 0.05) as usize]
}

/// The cover's accent, or None when the cover is featureless. See get_dominant_color.
pub fn pick_accent(img: &RgbImage) -> Option<Rgb> {
    // 128 px and 32 bins. A coarse palette spends every bin on shades of white on a mostly-white cover.
    let img = pil::thumbnail(img, 128, 128, Filter::Lanczos);
    let counts = pil::quantize_median_cut(&img, 32);
    let total = f64::from(counts.iter().map(|(count, _)| count).sum::<u32>().max(1));

    let mut best = None;
    let mut best_score = 0.0;
    for (count, entry) in counts {
        let rgb = (f64::from(entry[0]) / 255.0, f64::from(entry[1]) / 255.0, f64::from(entry[2]) / 255.0);
        let (lightness, chroma, _) = color_utils::rgb_to_oklch(rgb);
        // Near-black and near-white are not accents, and neither is gray, however much of the cover it is.
        if !(0.12..=0.95).contains(&lightness) || chroma < MIN_ACCENT_CHROMA {
            continue;
        }
        let share = f64::from(count) / total;
        let lightness_weight = (-((lightness - IDEAL_ACCENT_LIGHTNESS) / ACCENT_LIGHTNESS_SPREAD).powf(2.0)).exp();
        // Chroma is capped so one neon speck cannot outrank the region that defines the cover.
        let score = chroma.min(0.18) * share.powf(0.3) * lightness_weight;
        if score > best_score {
            best_score = score;
            best = Some(rgb);
        }
    }
    if best.is_some() {
        return best;
    }

    // No chromatic bin. A monochrome cover still gets a neutral, a featureless one gets nothing.
    // The median lightness, not the most common: white line art on black is mostly black.
    if cover_detail(&img) < MIN_COVER_DETAIL {
        return None;
    }
    let values = cover_lightnesses(&img);
    let median = *values.get(values.len() / 2)?;
    Some(color_utils::oklch_to_rgb(median.clamp(0.35, 0.85), 0.0, 0.0))
}

/// The accent color of the cover at `url`, or None for a featureless cover or a failure. Cached per URL.
/// Scores palette bins in OkLCh on chroma, lightness near the middle, and share of the cover damped by share ** 0.3.
/// Monochrome covers return a neutral gray.
/// `cache_dir` is Paths::cache_dir. Runs on the network runtime: call through NetHandle::spawn.
pub async fn get_dominant_color(http: reqwest::Client, cache_dir: PathBuf, url: String) -> Option<Rgb> {
    if url.is_empty() {
        return None;
    }
    if let Some(cached) = COLOR_CACHE.lock().unwrap().get(&url) {
        return cached;
    }
    let bytes = ensure_image_bytes(&http, &cache_dir, &url).await?;
    let accent = run_effect(move || decode(&bytes).map(|img| pick_accent(&img))).await??;
    COLOR_CACHE.lock().unwrap().put(url, accent);
    accent
}

#[cfg(test)]
mod tests {
    use super::pil::fixtures::*;
    use super::*;

    // Expected values are what cover_effects.py and Pillow 12.3 return for the same synthetic covers. See tools/cover_effects_ref.py (in git history, removed with the Python app).
    const EPS: f64 = 1e-6;

    fn close(got: Rgb, want: Rgb) -> bool {
        (got.0 - want.0).abs() < EPS && (got.1 - want.1).abs() < EPS && (got.2 - want.2).abs() < EPS
    }

    fn write_png(dir: &Path, name: &str, img: &RgbImage) -> PathBuf {
        let path = dir.join(name);
        img.save_with_format(&path, image::ImageFormat::Png).expect("fixture written");
        path
    }

    #[test]
    fn pick_accent_matches_python_on_colorful_covers() {
        let cases = [
            (noisy(517, 389), (0.7372549019607844, 0.20392156862745098, 0.49411764705882355)),
            (smooth(800, 800), (0.8705882352941177, 0.17647058823529413, 0.4745098039215686)),
            (smooth(97, 131), (0.8705882352941177, 0.17647058823529413, 0.3764705882352941)),
        ];
        for (img, want) in cases {
            let got = pick_accent(&img).expect("an accent");
            assert!(close(got, want), "{:?}: {got:?}", img.dimensions());
        }
    }

    #[test]
    fn a_monochrome_cover_gets_a_neutral_and_a_featureless_one_nothing() {
        let got = pick_accent(&gray_ramp(300, 200)).expect("a neutral");
        assert!(close(got, (0.5137254855563773, 0.5137254855563773, 0.5137254855563773)), "{got:?}");
        // A dark ramp has its median lifted to the 0.35 lightness floor.
        let got = pick_accent(&pil::brightness(&gray_ramp(300, 200), 0.3)).expect("a neutral");
        assert!(close(got, (0.22901253948049455, 0.22901253948049455, 0.22901253948049455)), "{got:?}");
        assert_eq!(pick_accent(&flat(300, 300)), None);
    }

    #[test]
    fn cover_detail_matches_python() {
        let cases = [(noisy(517, 389), 0.0905598360957165), (smooth(800, 800), 0.5375265895137042), (gray_ramp(300, 200), 0.8145763783037056), (flat(300, 300), 0.0)];
        for (img, want) in cases {
            assert!((cover_detail(&img) - want).abs() < EPS, "{:?}: {}", img.dimensions(), cover_detail(&img));
        }
    }

    #[test]
    fn the_blur_pipeline_matches_python_in_both_schemes() {
        let cases = [
            ("smooth_517x389_pipeline_dark", smooth(517, 389), true, 0x1c3fb7e37c8fb591, (0.033512976542387135, 0.09977978651119439)),
            ("smooth_517x389_pipeline_light", smooth(517, 389), false, 0x42101503de24d4b7, (0.575936954261214, 0.34972863662377873)),
            ("noisy_200x150_pipeline_dark", noisy(200, 150), true, 0xdea76270f197b9c3, (0.033984987994412776, 0.04120893015650058)),
            ("noisy_200x150_pipeline_light", noisy(200, 150), false, 0x6e39aa7aa3860d11, (0.3904790666032587, 0.35043538707229643)),
        ];
        for (name, img, dark, hash, want) in cases {
            let (blurred, backdrop) = render_blur(&img, dark).expect("a backdrop");
            assert_pillow(name, &blurred, (720, 720), hash);
            assert!((backdrop.0 - want.0).abs() < EPS && (backdrop.1 - want.1).abs() < EPS, "{name}: {backdrop:?}");
        }
    }

    #[test]
    fn normalization_lands_in_the_band_of_each_scheme() {
        let (_, dark) = render_blur(&smooth(517, 389), true).expect("a backdrop");
        assert!(dark.1 <= BLUR_DARK_HIGHLIGHT_CAP + 0.01, "{dark:?}");
        let (_, light) = render_blur(&smooth(517, 389), false).expect("a backdrop");
        assert!(light.1 >= BLUR_LIGHT_FLOOR - 0.01, "{light:?}");
        assert!(render_blur(&flat(300, 300), true).is_none(), "no accent means no blur");
    }

    #[test]
    fn percentile_indexes_like_python() {
        let ten: Vec<f64> = (1..=10).map(f64::from).collect();
        assert_eq!(percentile(&ten, 0.5), 6.0);
        assert_eq!(percentile(&[1.0, 2.0, 3.0], 0.98), 3.0);
        assert_eq!(percentile(&[1.0, 2.0, 3.0], 1.0), 3.0);
        let sample: Vec<f64> = (0..2304).map(f64::from).collect();
        assert_eq!((percentile(&sample, 0.98), percentile(&sample, 0.02)), (2257.0, 46.0));
    }

    #[test]
    fn thumbnail_fallbacks_walk_down_from_the_requested_quality() {
        let base = "https://i.ytimg.com/vi/abc123";
        assert_eq!(
            yt_thumb_fallback_urls(&format!("{base}/maxresdefault.jpg")),
            ["maxresdefault", "sddefault", "hqdefault", "mqdefault", "default"].map(|q| format!("{base}/{q}.jpg"))
        );
        // The signing query rides along, and nothing above the request is tried.
        assert_eq!(yt_thumb_fallback_urls(&format!("{base}/hqdefault.jpg?sqp=x&rs=y")), ["hqdefault", "mqdefault", "default"].map(|q| format!("{base}/{q}.jpg?sqp=x&rs=y")));
        // An unknown variant walks the whole list.
        assert_eq!(yt_thumb_fallback_urls(&format!("{base}/hq720.jpg")).len(), 6);
        assert_eq!(yt_thumb_fallback_urls(&format!("{base}/default.jpg")), [format!("{base}/default.jpg")]);
        for other in ["https://lh3.googleusercontent.com/abc=w544-h544-l90-rj", "https://i.ytimg.com/vi_webp/abc123/sddefault.webp"] {
            assert_eq!(yt_thumb_fallback_urls(other), [other]);
        }
    }

    #[test]
    fn cache_paths_are_the_python_apps() {
        let url = "https://lh3.googleusercontent.com/abc=w544-h544-l90-rj";
        let cache = Path::new("/cache/muse");
        assert_eq!(thumb_cache_path(cache, url), Path::new("/cache/muse/thumbs/82b272759bc0929447bb3682c86ff15b4744d450"));
        assert_eq!(blur_cache_path(cache, url, true), Path::new("/cache/muse/covers_blurred/82b272759bc0929447bb3682c86ff15b4744d450_b42_s720_dark.png"));
        assert_eq!(blur_cache_path(cache, url, false), Path::new("/cache/muse/covers_blurred/82b272759bc0929447bb3682c86ff15b4744d450_b42_s720_light.png"));
    }

    #[test]
    fn local_covers_lose_their_cache_buster() {
        assert_eq!(local_cover_path("file:///music/Playlists/Mix.jpg?m=1712"), Some("/music/Playlists/Mix.jpg"));
        assert_eq!(local_cover_path("/music/Playlists/Mix.jpg"), Some("/music/Playlists/Mix.jpg"));
        assert_eq!(local_cover_path("https://i.ytimg.com/vi/abc/default.jpg"), None);
    }

    #[test]
    fn the_lru_drops_the_entry_used_longest_ago() {
        let mut lru = Lru::new(2);
        lru.put("a", 1);
        lru.put("b", 2);
        assert_eq!(lru.get(&"a"), Some(1));
        lru.put("c", 3);
        assert_eq!((lru.get(&"a"), lru.get(&"b"), lru.get(&"c")), (Some(1), None, Some(3)));
        lru.put("a", 10);
        assert_eq!((lru.get(&"a"), lru.entries.len()), (Some(10), 2));
    }

    #[tokio::test]
    async fn a_local_cover_is_blurred_to_the_python_cache_path_and_remembered() {
        let dir = tempfile::tempdir().unwrap();
        let cache = dir.path().join("cache");
        let source = write_png(dir.path(), "cover.png", &smooth(517, 389));
        let url = format!("file://{}?m=1", source.display());

        let cover = get_blurred_cover(reqwest::Client::new(), cache.clone(), url.clone(), true).await.expect("a blurred cover");
        assert_eq!(cover.path, blur_cache_path(&cache, &url, true));
        assert!((cover.backdrop.0 - 0.033512976542387135).abs() < EPS && (cover.backdrop.1 - 0.09977978651119439).abs() < EPS, "{:?}", cover.backdrop);
        let saved = image::open(&cover.path).expect("a PNG").to_rgb8();
        assert_pillow("smooth_517x389_pipeline_dark", &saved, (720, 720), 0x1c3fb7e37c8fb591);
        assert!(!PathBuf::from(format!("{}.tmp", cover.path.display())).exists());

        // The memory cache answers without the source.
        std::fs::remove_file(&source).unwrap();
        assert_eq!(get_blurred_cover(reqwest::Client::new(), cache.clone(), url.clone(), true).await, Some(cover.clone()));
        // A PNG that went missing is not handed out.
        std::fs::remove_file(&cover.path).unwrap();
        assert_eq!(get_blurred_cover(reqwest::Client::new(), cache, url, true).await, None);
    }

    #[tokio::test]
    async fn a_featureless_cover_is_declined_once_and_remembered() {
        let dir = tempfile::tempdir().unwrap();
        let source = write_png(dir.path(), "flat.png", &flat(200, 200));
        let url = source.display().to_string();
        assert_eq!(get_blurred_cover(reqwest::Client::new(), dir.path().to_owned(), url.clone(), false).await, None);
        assert_eq!(BLUR_CACHE.lock().unwrap().get(&(url.clone(), false)), Some(None));
        assert_eq!(get_dominant_color(reqwest::Client::new(), dir.path().to_owned(), url.clone()).await, None);
        assert_eq!(COLOR_CACHE.lock().unwrap().get(&url), Some(None));
    }

    #[tokio::test]
    async fn a_cover_in_the_thumb_cache_needs_no_network() {
        let dir = tempfile::tempdir().unwrap();
        let url = "https://covers.invalid/smooth-800.png";
        let source = write_png(dir.path(), "cover.png", &smooth(800, 800));
        write_thumb_cache(dir.path(), url, &std::fs::read(source).unwrap()).await;
        assert!(thumb_cache_path(dir.path(), url).is_file());

        let accent = get_dominant_color(reqwest::Client::new(), dir.path().to_owned(), url.to_owned()).await.expect("an accent");
        assert!(close(accent, (0.8705882352941177, 0.17647058823529413, 0.4745098039215686)), "{accent:?}");
        assert_eq!(COLOR_CACHE.lock().unwrap().get(&url.to_owned()), Some(Some(accent)));
    }

    #[tokio::test]
    async fn a_failed_read_is_not_remembered() {
        let url = "/nowhere/mixtapes-missing-cover.png".to_owned();
        assert_eq!(get_dominant_color(reqwest::Client::new(), PathBuf::from("/nowhere"), url.clone()).await, None);
        assert_eq!(COLOR_CACHE.lock().unwrap().get(&url), None);
        assert_eq!(get_blurred_cover(reqwest::Client::new(), PathBuf::from("/nowhere"), String::new(), true).await, None);
    }

    /// Fetches a thumbnail whose maxres variant is a 404, so the chain has to walk down.
    /// `cargo test -- --ignored live_fallback --nocapture`
    #[tokio::test]
    #[ignore]
    async fn live_fallback() {
        let dir = tempfile::tempdir().unwrap();
        let url = "https://i.ytimg.com/vi/jNQXAC9IVRw/maxresdefault.jpg".to_owned();
        let http = reqwest::Client::new();
        let (accent, cover) = tokio::join!(get_dominant_color(http.clone(), dir.path().to_owned(), url.clone()), get_blurred_cover(http, dir.path().to_owned(), url.clone(), true));
        println!("accent={accent:?} cover={cover:?}");
        assert!(thumb_cache_path(dir.path(), &url).is_file(), "bytes are cached under the requested address");
        assert!(cover.expect("a blurred cover").path.is_file());
        assert!(INFLIGHT.lock().unwrap().is_empty());
    }

    /// Runs both effects on real covers and prints what to hold against `tools/cover_effects_ref.py <files>`.
    /// `MIXTAPES_COVERS=/a.jpg:/b.jpg cargo test -- --ignored compare_covers --nocapture`
    #[test]
    #[ignore]
    fn compare_covers() {
        let covers = std::env::var_os("MIXTAPES_COVERS").expect("MIXTAPES_COVERS lists image files");
        for path in std::env::split_paths(&covers) {
            let img = decode(&std::fs::read(&path).unwrap()).expect("a decodable cover");
            let accent = pick_accent(&img).map(color_utils::to_css);
            println!("{} accent={accent:?} detail={:.6}", path.display(), cover_detail(&img));
            for dark in [true, false] {
                let scheme = if dark { "dark" } else { "light" };
                let Some((blurred, backdrop)) = render_blur(&img, dark) else { continue };
                let values = pixel_luminances(&blurred);
                let mean = values.iter().sum::<f64>() / values.len() as f64;
                println!("{} {scheme} mean_luminance={mean:.6} backdrop=({:.6}, {:.6})", path.display(), backdrop.0, backdrop.1);
                save_png(&blurred, &PathBuf::from(format!("{}.rust_{scheme}.png", path.display()))).unwrap();
            }
        }
    }
}

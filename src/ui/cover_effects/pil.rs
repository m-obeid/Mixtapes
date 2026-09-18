//! The Pillow operations cover_effects.py leans on, ported from libImaging so
//! the Rust app lands on the same pixels: Resample.c, Reduce.c, BoxBlur.c,
//! Blend.c and the median cut of Quant.c. Integer widths, float widths and
//! rounding follow the C, which is why f32 and wrapping u32 show up here.

use std::collections::HashMap;

use image::RgbImage;

const BANDS: usize = 3;

#[derive(Clone, Copy, Debug, PartialEq)]
pub enum Filter {
    /// Pillow's default for Image.resize.
    Bicubic,
    Lanczos,
}

impl Filter {
    fn support(self) -> f64 {
        match self {
            Filter::Bicubic => 2.0,
            Filter::Lanczos => 3.0,
        }
    }

    fn weight(self, x: f64) -> f64 {
        match self {
            Filter::Bicubic => {
                const A: f64 = -0.5;
                let x = x.abs();
                if x < 1.0 {
                    ((A + 2.0) * x - (A + 3.0)) * x * x + 1.0
                } else if x < 2.0 {
                    (((x - 5.0) * x + 8.0) * x - 4.0) * A
                } else {
                    0.0
                }
            }
            Filter::Lanczos => {
                let sinc = |x: f64| if x == 0.0 { 1.0 } else { (x * std::f64::consts::PI).sin() / (x * std::f64::consts::PI) };
                if (-3.0..3.0).contains(&x) { sinc(x) * sinc(x / 3.0) } else { 0.0 }
            }
        }
    }
}

fn new_image(width: usize, height: usize, data: Vec<u8>) -> RgbImage {
    RgbImage::from_raw(width as u32, height as u32, data).expect("buffer matches its size")
}

/// Image.resize((width, height), filter) over the whole image.
pub fn resize(img: &RgbImage, width: u32, height: u32, filter: Filter) -> RgbImage {
    if img.dimensions() == (width, height) {
        return img.clone();
    }
    resample(img, width, height, filter, [0.0, 0.0, img.width() as f32, img.height() as f32])
}

/// Image.thumbnail((max_width, max_height), filter) with Pillow's default reducing_gap of 2.0:
/// an integer box reduce first, then the filter over what is left.
pub fn thumbnail(img: &RgbImage, max_width: u32, max_height: u32, filter: Filter) -> RgbImage {
    const REDUCING_GAP: f64 = 2.0;
    let Some((width, height)) = thumbnail_size(img.width(), img.height(), max_width, max_height) else { return img.clone() };
    if img.dimensions() == (width, height) {
        return img.clone();
    }
    let factor_x = ((f64::from(img.width()) / f64::from(width) / REDUCING_GAP) as u32).max(1);
    let factor_y = ((f64::from(img.height()) / f64::from(height) / REDUCING_GAP) as u32).max(1);
    if factor_x == 1 && factor_y == 1 {
        return resize(img, width, height, filter);
    }
    // The source box in reduced coordinates is fractional when the factor does not divide the size.
    let area = [0.0, 0.0, (f64::from(img.width()) / f64::from(factor_x)) as f32, (f64::from(img.height()) / f64::from(factor_y)) as f32];
    let reduced = reduce(img, factor_x, factor_y);
    if reduced.dimensions() == (width, height) && area[2] == width as f32 && area[3] == height as f32 {
        return reduced;
    }
    resample(&reduced, width, height, filter, area)
}

/// The size Image.thumbnail settles on, None when the image already fits.
fn thumbnail_size(width: u32, height: u32, max_width: u32, max_height: u32) -> Option<(u32, u32)> {
    if max_width >= width && max_height >= height {
        return None;
    }
    // min(floor, ceil, key=...) keeps the floor on a tie.
    let round_aspect = |number: f64, key: &dyn Fn(f64) -> f64| {
        let (floor, ceil) = (number.floor(), number.ceil());
        let pick = if key(ceil) < key(floor) { ceil } else { floor };
        pick.max(1.0) as u32
    };
    let (x, y) = (f64::from(max_width), f64::from(max_height));
    let aspect = f64::from(width) / f64::from(height);
    if x / y >= aspect {
        Some((round_aspect(y * aspect, &|n| (aspect - n / y).abs()), max_height))
    } else {
        Some((max_width, round_aspect(x / aspect, &|n| if n == 0.0 { 0.0 } else { (aspect - x / n).abs() })))
    }
}

/// Fixed-point weights for one axis: per output pixel, the first source pixel and its weights.
struct Coeffs {
    ksize: usize,
    bounds: Vec<(usize, usize)>,
    weights: Vec<i64>,
}

const PRECISION_BITS: u32 = 32 - 8 - 2;

fn precompute_coeffs(in_size: usize, in0: f32, in1: f32, out_size: usize, filter: Filter) -> Coeffs {
    let scale = f64::from(in1 - in0) / out_size as f64;
    let filterscale = scale.max(1.0);
    let support = filter.support() * filterscale;
    let ksize = support.ceil() as usize * 2 + 1;
    let inv_filterscale = 1.0 / filterscale;
    let mut bounds = Vec::with_capacity(out_size);
    let mut weights = vec![0i64; out_size * ksize];
    let mut row = vec![0f64; ksize];
    for xx in 0..out_size {
        let center = f64::from(in0) + (xx as f64 + 0.5) * scale;
        let xmin = ((center - support + 0.5) as i64).max(0);
        let xmax = ((center + support + 0.5) as i64).min(in_size as i64);
        let count = (xmax - xmin).max(0) as usize;
        let mut total = 0.0;
        for (x, w) in row.iter_mut().enumerate().take(count) {
            *w = filter.weight((x as f64 + xmin as f64 - center + 0.5) * inv_filterscale);
            total += *w;
        }
        for (x, w) in row.iter().enumerate().take(count) {
            let w = if total != 0.0 { w / total } else { *w };
            // Rounded away from zero into 22 fractional bits, as normalize_coeffs_8bpc does.
            let scaled = w * f64::from(1u32 << PRECISION_BITS);
            weights[xx * ksize + x] = if w < 0.0 { (scaled - 0.5) as i64 } else { (scaled + 0.5) as i64 };
        }
        bounds.push((xmin as usize, count));
    }
    Coeffs { ksize, bounds, weights }
}

fn clip8(sum: i64) -> u8 {
    (sum >> PRECISION_BITS).clamp(0, 255) as u8
}

/// ImagingResample: a horizontal pass, then a vertical one, each rounding to 8 bits.
/// `area` is the source box (x0, y0, x1, y1), which Pillow carries as C floats.
fn resample(img: &RgbImage, width: u32, height: u32, filter: Filter, area: [f32; 4]) -> RgbImage {
    let (in_w, in_h) = (img.width() as usize, img.height() as usize);
    let (out_w, out_h) = (width as usize, height as usize);
    let src = img.as_raw();
    let horiz = precompute_coeffs(in_w, area[0], area[2], out_w, filter);
    let vert = precompute_coeffs(in_h, area[1], area[3], out_h, filter);

    let mut temp = vec![0u8; out_w * in_h * BANDS];
    for y in 0..in_h {
        let line = &src[y * in_w * BANDS..(y + 1) * in_w * BANDS];
        for (xx, &(xmin, count)) in horiz.bounds.iter().enumerate() {
            let k = &horiz.weights[xx * horiz.ksize..xx * horiz.ksize + count];
            for band in 0..BANDS {
                let mut sum = 1i64 << (PRECISION_BITS - 1);
                for (x, &w) in k.iter().enumerate() {
                    sum += i64::from(line[(xmin + x) * BANDS + band]) * w;
                }
                temp[(y * out_w + xx) * BANDS + band] = clip8(sum);
            }
        }
    }

    let mut out = vec![0u8; out_w * out_h * BANDS];
    for (yy, &(ymin, count)) in vert.bounds.iter().enumerate() {
        let k = &vert.weights[yy * vert.ksize..yy * vert.ksize + count];
        for i in 0..out_w * BANDS {
            let mut sum = 1i64 << (PRECISION_BITS - 1);
            for (y, &w) in k.iter().enumerate() {
                sum += i64::from(temp[(ymin + y) * out_w * BANDS + i]) * w;
            }
            out[yy * out_w * BANDS + i] = clip8(sum);
        }
    }
    new_image(out_w, out_h, out)
}

/// division_UINT32(divider, 8): the reciprocal Reduce.c multiplies by, computed in a C float.
fn reduce_multiplier(cells: u32) -> u32 {
    (4_294_967_296.0f32 / (256 * cells) as f32) as u32
}

/// Image.reduce((factor_x, factor_y)): box averages, with partial boxes on the far edges.
fn reduce(img: &RgbImage, factor_x: u32, factor_y: u32) -> RgbImage {
    let (in_w, in_h) = (img.width() as usize, img.height() as usize);
    let (fx, fy) = (factor_x as usize, factor_y as usize);
    let (out_w, out_h) = (in_w.div_ceil(fx), in_h.div_ceil(fy));
    let src = img.as_raw();
    let mut out = vec![0u8; out_w * out_h * BANDS];
    for y in 0..out_h {
        let rows = y * fy..((y + 1) * fy).min(in_h);
        for x in 0..out_w {
            let cols = x * fx..((x + 1) * fx).min(in_w);
            let cells = (rows.len() * cols.len()) as u32;
            let (multiplier, amend) = (reduce_multiplier(cells), cells / 2);
            for band in 0..BANDS {
                let mut sum = amend;
                for yy in rows.clone() {
                    for xx in cols.clone() {
                        sum += u32::from(src[(yy * in_w + xx) * BANDS + band]);
                    }
                }
                out[(y * out_w + x) * BANDS + band] = (sum.wrapping_mul(multiplier) >> 24) as u8;
            }
        }
    }
    new_image(out_w, out_h, out)
}

/// _gaussian_blur_radius: the box radius whose three passes have the asked standard deviation.
fn gaussian_box_radius(radius: f32, passes: u8) -> f32 {
    let sigma2 = radius * radius / f32::from(passes);
    let length = (12.0 * f64::from(sigma2) + 1.0).sqrt() as f32;
    let whole = ((f64::from(length) - 1.0) / 2.0).floor() as f32;
    let mut fraction = (2.0 * whole + 1.0) * (whole * (whole + 1.0) - 3.0 * sigma2);
    fraction /= 6.0 * (sigma2 - (whole + 1.0) * (whole + 1.0));
    whole + fraction
}

/// ImagingLineBoxBlur8 on one channel of one line. Edges extend the border pixel.
fn box_blur_line(out: &mut [u8], line: &[u8], radius: usize, ww: u32, fw: u32) {
    let size = line.len();
    let lastx = size - 1;
    let edge_a = (radius + 1).min(size);
    let edge_b = size.saturating_sub(radius + 1);
    let px = |i: usize| u32::from(line[i]);
    let mut acc = px(0).wrapping_mul(radius as u32 + 1);
    for x in 0..edge_a - 1 {
        acc = acc.wrapping_add(px(x));
    }
    acc = acc.wrapping_add(px(lastx).wrapping_mul((radius + 1 - edge_a) as u32));

    let mut step = |x: usize, subtract: usize, add: usize, far_left: usize, far_right: usize| {
        acc = acc.wrapping_add(px(add)).wrapping_sub(px(subtract));
        let bulk = acc.wrapping_mul(ww).wrapping_add((px(far_left) + px(far_right)).wrapping_mul(fw));
        out[x] = (bulk.wrapping_add(1 << 23) >> 24) as u8;
    };
    if edge_a <= edge_b {
        for x in 0..edge_a {
            step(x, 0, x + radius, 0, x + radius + 1);
        }
        for x in edge_a..edge_b {
            step(x, x - radius - 1, x + radius, x - radius - 1, x + radius + 1);
        }
        for x in edge_b..=lastx {
            step(x, x - radius - 1, lastx, x - radius - 1, lastx);
        }
    } else {
        for x in 0..edge_b {
            step(x, 0, x + radius, 0, x + radius + 1);
        }
        for x in edge_b..edge_a {
            step(x, 0, lastx, 0, lastx);
        }
        for x in edge_a..=lastx {
            step(x, x - radius - 1, lastx, x - radius - 1, lastx);
        }
    }
}

/// `passes` box blurs along lines of `len` pixels. `at` maps (line, position) to a pixel index.
fn box_blur_axis(data: &mut [u8], lines: usize, len: usize, float_radius: f32, passes: u8, at: impl Fn(usize, usize) -> usize) {
    let radius = float_radius as usize;
    let ww = (16_777_216.0f32 / (float_radius * 2.0 + 1.0)) as u32;
    let fw = (1u32 << 24).wrapping_sub((radius as u32 * 2 + 1).wrapping_mul(ww)) / 2;
    let mut line = vec![0u8; len];
    let mut blurred = vec![0u8; len];
    for index in 0..lines {
        for band in 0..BANDS {
            for (pos, value) in line.iter_mut().enumerate() {
                *value = data[at(index, pos) * BANDS + band];
            }
            for _ in 0..passes {
                box_blur_line(&mut blurred, &line, radius, ww, fw);
                std::mem::swap(&mut line, &mut blurred);
            }
            for (pos, value) in line.iter().enumerate() {
                data[at(index, pos) * BANDS + band] = *value;
            }
        }
    }
}

/// ImageFilter.GaussianBlur(radius): three box blurs per axis, rows first.
pub fn gaussian_blur(img: &RgbImage, radius: f32) -> RgbImage {
    const PASSES: u8 = 3;
    let (width, height) = (img.width() as usize, img.height() as usize);
    let mut data = img.as_raw().clone();
    let box_radius = gaussian_box_radius(radius, PASSES);
    if width == 0 || height == 0 || box_radius == 0.0 {
        return img.clone();
    }
    box_blur_axis(&mut data, height, width, box_radius, PASSES, |row, x| row * width + x);
    box_blur_axis(&mut data, width, height, box_radius, PASSES, |column, y| y * width + column);
    new_image(width, height, data)
}

/// One channel of ImagingBlend. C float math, truncated rather than rounded.
fn blend_channel(from: u8, to: u8, alpha: f32) -> u8 {
    let value = f32::from(from) + alpha * (f32::from(to) - f32::from(from));
    if (0.0..=1.0).contains(&alpha) {
        value as u8
    } else if value <= 0.0 {
        0
    } else if value >= 255.0 {
        255
    } else {
        value as u8
    }
}

fn map_pixels(img: &RgbImage, f: impl Fn([u8; 3]) -> [u8; 3]) -> RgbImage {
    let mut out = img.clone();
    for pixel in out.pixels_mut() {
        pixel.0 = f(pixel.0);
    }
    out
}

/// ImageEnhance.Brightness(img).enhance(factor): a blend up from black.
pub fn brightness(img: &RgbImage, factor: f64) -> RgbImage {
    let alpha = factor as f32;
    map_pixels(img, |p| p.map(|c| blend_channel(0, c, alpha)))
}

/// ImageEnhance.Color(img).enhance(factor): a blend away from the grayscale copy.
pub fn saturation(img: &RgbImage, factor: f64) -> RgbImage {
    let alpha = factor as f32;
    map_pixels(img, |p| {
        // The ITU-R 601 luma of convert("L").
        let luma = ((u32::from(p[0]) * 19595 + u32::from(p[1]) * 38470 + u32::from(p[2]) * 7471 + 0x8000) >> 16) as u8;
        p.map(|c| blend_channel(luma, c, alpha))
    })
}

/// Image.blend(img, white, alpha).
pub fn blend_toward_white(img: &RgbImage, alpha: f64) -> RgbImage {
    let alpha = alpha as f32;
    map_pixels(img, |p| p.map(|c| blend_channel(c, 255, alpha)))
}

/// One distinct color of the image and how many pixels carry it.
struct Swatch {
    rgb: [u8; 3],
    count: u32,
}

/// A median cut box: its swatches, listed once per axis in descending order of that channel.
struct CutBox {
    lists: [Vec<usize>; 3],
    pixel_count: u32,
    children: Option<(usize, usize)>,
}

impl CutBox {
    fn extent(&self, swatches: &[Swatch], axis: usize) -> i32 {
        let list = &self.lists[axis];
        i32::from(swatches[list[0]].rgb[axis]) - i32::from(swatches[*list.last().expect("box is not empty")].rgb[axis])
    }

    fn is_single_color(&self, swatches: &[Swatch]) -> bool {
        (0..3).all(|axis| self.extent(swatches, axis) == 0)
    }
}

/// QuantHeap.c: a binary max-heap on pixel count. Ported as is, since its
/// handling of equal counts decides which box is cut next.
struct BoxHeap {
    slots: Vec<usize>,
}

impl BoxHeap {
    fn new() -> Self {
        // Slot 0 is unused, the C heap is one-based.
        Self { slots: vec![usize::MAX] }
    }

    fn cmp(boxes: &[CutBox], a: usize, b: usize) -> i64 {
        i64::from(boxes[a].pixel_count) - i64::from(boxes[b].pixel_count)
    }

    fn add(&mut self, boxes: &[CutBox], value: usize) {
        self.slots.push(value);
        let mut k = self.slots.len() - 1;
        while k != 1 {
            if Self::cmp(boxes, value, self.slots[k / 2]) <= 0 {
                break;
            }
            self.slots[k] = self.slots[k / 2];
            k /= 2;
        }
        self.slots[k] = value;
    }

    fn remove(&mut self, boxes: &[CutBox]) -> Option<usize> {
        if self.slots.len() <= 1 {
            return None;
        }
        let top = self.slots[1];
        let value = self.slots.pop().expect("heap is not empty");
        let count = self.slots.len() - 1;
        if count == 0 {
            return Some(top);
        }
        let mut k = 1;
        while k * 2 <= count {
            let mut l = k * 2;
            if l < count && Self::cmp(boxes, self.slots[l], self.slots[l + 1]) < 0 {
                l += 1;
            }
            if Self::cmp(boxes, value, self.slots[l]) > 0 {
                break;
            }
            self.slots[k] = self.slots[l];
            k = l;
        }
        self.slots[k] = value;
        Some(top)
    }
}

/// Quant.c split(): cut along the axis with the widest weighted extent, at the
/// channel value where the running pixel count passes half.
fn split(boxes: &mut Vec<CutBox>, swatches: &[Swatch], flags: &mut [bool], node: usize) -> (usize, usize) {
    const AXIS_WEIGHTS: [i32; 3] = [77, 150, 29];
    // The first axis wins a tie.
    let (mut axis, mut best) = (0, i32::MIN);
    for (candidate, weight) in AXIS_WEIGHTS.iter().enumerate() {
        let spread = boxes[node].extent(swatches, candidate) * weight;
        if best < spread {
            best = spread;
            axis = candidate;
        }
    }

    let list = &boxes[node].lists[axis];
    let pixel_count = boxes[node].pixel_count;
    let (mut left, mut at) = (0u32, 0);
    while at < list.len() {
        left += swatches[list[at]].count;
        flags[list[at]] = false;
        at += 1;
        if left * 2 > pixel_count {
            break;
        }
    }
    // A channel value is never divided between the halves.
    if at < list.len() {
        let value = swatches[list[at - 1]].rgb[axis];
        while at < list.len() && swatches[list[at]].rgb[axis] == value {
            flags[list[at]] = false;
            at += 1;
        }
    }
    for &swatch in &list[at..] {
        flags[swatch] = true;
    }
    if at == list.len() {
        // Nothing fell right: hand it the lowest channel value.
        let value = swatches[*list.last().expect("box is not empty")].rgb[axis];
        for &swatch in list.iter().rev().take_while(|&&s| swatches[s].rgb[axis] == value) {
            flags[swatch] = true;
        }
    }

    let mut halves = [CutBox { lists: Default::default(), pixel_count: 0, children: None }, CutBox { lists: Default::default(), pixel_count: 0, children: None }];
    for i in 0..3 {
        for &swatch in &boxes[node].lists[i] {
            let half = &mut halves[usize::from(flags[swatch])];
            half.lists[i].push(swatch);
            if i == 0 {
                half.pixel_count += swatches[swatch].count;
            }
        }
    }
    let [low, high] = halves;
    let ids = (boxes.len(), boxes.len() + 1);
    boxes.push(low);
    boxes.push(high);
    boxes[node].children = Some(ids);
    boxes[node].lists = Default::default();
    ids
}

/// Leaves in the order annotate_hash_table numbers them: depth first, left before right.
fn collect_leaves(boxes: &[CutBox], node: usize, out: &mut Vec<usize>) {
    match boxes[node].children {
        Some((left, right)) => {
            collect_leaves(boxes, left, out);
            collect_leaves(boxes, right, out);
        }
        None if !boxes[node].lists[0].is_empty() => out.push(node),
        None => {}
    }
}

/// img.quantize(colors, method=MEDIANCUT) followed by getcolors():
/// each used palette entry with its pixel count, in palette order.
pub fn quantize_median_cut(img: &RgbImage, colors: usize) -> Vec<(u32, [u8; 3])> {
    let mut histogram: HashMap<[u8; 3], u32> = HashMap::new();
    for pixel in img.pixels() {
        *histogram.entry(pixel.0).or_default() += 1;
    }
    if histogram.is_empty() {
        return Vec::new();
    }
    let mut swatches: Vec<Swatch> = histogram.into_iter().map(|(rgb, count)| Swatch { rgb, count }).collect();
    // Hash order varies per run. Where equal channel values sit in a list never changes a cut, this only keeps runs alike.
    swatches.sort_by_key(|s| s.rgb);

    let sorted_by = |axis: usize| {
        let mut list: Vec<usize> = (0..swatches.len()).collect();
        list.sort_by(|&a, &b| swatches[b].rgb[axis].cmp(&swatches[a].rgb[axis]));
        list
    };
    let pixel_total = img.width() * img.height();
    let mut boxes = vec![CutBox { lists: [sorted_by(0), sorted_by(1), sorted_by(2)], pixel_count: pixel_total, children: None }];
    let mut heap = BoxHeap::new();
    heap.add(&boxes, 0);
    let mut flags = vec![false; swatches.len()];
    'cuts: for _ in 1..colors {
        // Boxes of one color cannot be cut. They leave the heap and stay leaves.
        let node = loop {
            match heap.remove(&boxes) {
                Some(node) if boxes[node].is_single_color(&swatches) => continue,
                Some(node) => break node,
                None => break 'cuts,
            }
        };
        let (left, right) = split(&mut boxes, &swatches, &mut flags, node);
        heap.add(&boxes, left);
        heap.add(&boxes, right);
    }

    let mut leaves = Vec::new();
    collect_leaves(&boxes, 0, &mut leaves);
    let mut owner = vec![0usize; swatches.len()];
    let palette: Vec<[u8; 3]> = leaves
        .iter()
        .enumerate()
        .map(|(entry, &leaf)| {
            let (mut sums, mut count) = ([0u64; 3], 0u64);
            for &swatch in &boxes[leaf].lists[0] {
                owner[swatch] = entry;
                let Swatch { rgb, count: n } = swatches[swatch];
                for band in 0..3 {
                    sums[band] += u64::from(rgb[band]) * u64::from(n);
                }
                count += u64::from(n);
            }
            sums.map(|sum| (0.5 + sum as f64 / count as f64) as u8)
        })
        .collect();

    // Pixels then move to the nearest entry. Their own box wins a tie, then the entry nearest to it.
    let dist = |a: [u8; 3], b: [u8; 3]| (0..3).map(|i| (i32::from(a[i]) - i32::from(b[i])).pow(2)).sum::<i32>();
    let order: Vec<Vec<usize>> = (0..palette.len())
        .map(|entry| {
            let mut others: Vec<usize> = (0..palette.len()).collect();
            others.sort_by_key(|&other| (dist(palette[entry], palette[other]), other));
            others
        })
        .collect();
    let mut counts = vec![0u32; palette.len()];
    for (index, swatch) in swatches.iter().enumerate() {
        let mut best = owner[index];
        let mut best_dist = dist(palette[best], swatch.rgb);
        for &candidate in &order[owner[index]] {
            let d = dist(palette[candidate], swatch.rgb);
            if d < best_dist {
                best_dist = d;
                best = candidate;
            }
        }
        counts[best] += swatch.count;
    }
    palette.into_iter().zip(counts).filter(|&(_, count)| count > 0).map(|(rgb, count)| (count, rgb)).collect()
}

/// Synthetic covers, built by the same formulas as tools/cover_effects_ref.py (in git history, removed with the Python app), which asks Pillow for the expected values.
#[cfg(test)]
pub mod fixtures {
    use image::RgbImage;

    /// High-frequency color noise.
    pub fn noisy(w: u32, h: u32) -> RgbImage {
        RgbImage::from_fn(w, h, |x, y| image::Rgb([((x * 7 + y * 3) % 256) as u8, ((x * x / 8 + y * 5) % 256) as u8, ((x * y / 4) % 256) as u8]))
    }

    /// Red and green ramps under a blue checkerboard.
    pub fn smooth(w: u32, h: u32) -> RgbImage {
        RgbImage::from_fn(w, h, |x, y| image::Rgb([(x * 255 / (w - 1)) as u8, (y * 255 / (h - 1)) as u8, (((x / 16 + y / 16) % 2) * 200 + 20) as u8]))
    }

    /// A gray ramp: no chromatic bin, plenty of detail.
    pub fn gray_ramp(w: u32, h: u32) -> RgbImage {
        RgbImage::from_fn(w, h, |x, _| image::Rgb([(x * 255 / (w - 1)) as u8; 3]))
    }

    pub fn flat(w: u32, h: u32) -> RgbImage {
        RgbImage::from_pixel(w, h, image::Rgb([120, 120, 120]))
    }

    /// FNV-1a over the raw bytes, enough to pin a whole image to Pillow's output.
    pub fn fnv(img: &RgbImage) -> u64 {
        img.as_raw().iter().fold(0xcbf2_9ce4_8422_2325, |h, &b| (h ^ u64::from(b)).wrapping_mul(0x0100_0000_01b3))
    }

    #[track_caller]
    pub fn assert_pillow(name: &str, img: &RgbImage, size: (u32, u32), hash: u64) {
        assert_eq!(img.dimensions(), size, "{name}");
        assert!(fnv(img) == hash, "{name}: differs from Pillow");
    }
}

#[cfg(test)]
mod tests {
    use super::fixtures::*;
    use super::*;

    #[test]
    fn resize_matches_pillow_byte_for_byte() {
        assert_pillow("noisy_200x150_bicubic48", &resize(&noisy(200, 150), 48, 48, Filter::Bicubic), (48, 48), 0x14f598a3ab033e19);
        assert_pillow("smooth_517x389_bicubic32", &resize(&smooth(517, 389), 32, 32, Filter::Bicubic), (32, 32), 0xa6adbb029bff783f);
        assert_pillow("smooth_100x100_lanczos720", &resize(&smooth(100, 100), 720, 720, Filter::Lanczos), (720, 720), 0x3d8836d876eedce2);
    }

    #[test]
    fn thumbnail_matches_pillow_with_and_without_the_box_reduce() {
        // 517x389 reduces by 2 with a ragged edge, 800x800 by 3 into a fractional box, 1280x720 by 5.
        assert_pillow("noisy_517x389_thumb", &thumbnail(&noisy(517, 389), 128, 128, Filter::Lanczos), (128, 96), 0x5fefc2858a22b6ab);
        assert_pillow("smooth_800x800_thumb", &thumbnail(&smooth(800, 800), 128, 128, Filter::Lanczos), (128, 128), 0x7c0636c05e7a8f7a);
        assert_pillow("noisy_1280x720_thumb", &thumbnail(&noisy(1280, 720), 128, 128, Filter::Lanczos), (128, 72), 0x5c67b350c1eee7c0);
        assert_pillow("smooth_97x131_thumb", &thumbnail(&smooth(97, 131), 128, 128, Filter::Lanczos), (95, 128), 0x8e4a7058bfc8c90f);
        // Already small enough: untouched.
        assert_pillow("smooth_100x60_thumb", &thumbnail(&smooth(100, 60), 128, 128, Filter::Lanczos), (100, 60), 0x269594c5678cd181);
    }

    #[test]
    fn gaussian_blur_matches_pillow_even_when_the_radius_passes_the_image() {
        assert_pillow("smooth_300x300_blur42", &gaussian_blur(&smooth(300, 300), 42.0), (300, 300), 0x71f5f8a83c5523e6);
        assert_pillow("noisy_64x40_blur42", &gaussian_blur(&noisy(64, 40), 42.0), (64, 40), 0x27ac4e960f67ada2);
        assert_pillow("noisy_200x150_blur5", &gaussian_blur(&noisy(200, 150), 5.0), (200, 150), 0x238072d8b823967c);
    }

    #[test]
    fn point_operations_match_pillow() {
        let img = noisy(200, 150);
        assert_pillow("noisy_200x150_color125", &saturation(&img, 1.25), (200, 150), 0x084c3fd2d72aa1aa);
        assert_pillow("noisy_200x150_bright04", &brightness(&img, 0.4), (200, 150), 0xe633998afa7ad56a);
        assert_pillow("noisy_200x150_bright17", &brightness(&img, 1.7), (200, 150), 0xb132e2245578a4c7);
        assert_pillow("noisy_200x150_white03", &blend_toward_white(&img, 0.3), (200, 150), 0x364bb54e495307d4);
    }

    #[test]
    fn median_cut_matches_pillow_palette_counts_and_order() {
        let noisy_want: [(u32, [u8; 3]); 32] = [(248, [212, 243, 122]), (486, [214, 212, 134]), (379, [162, 236, 111]), (473, [165, 205, 111]), (237, [213, 177, 118]), (847, [155, 176, 139]), (412, [215, 144, 99]), (256, [150, 144, 118]), (349, [93, 237, 115]), (241, [92, 204, 124]), (447, [33, 238, 135]), (476, [29, 204, 112]), (291, [92, 174, 127]), (290, [100, 142, 115]), (275, [29, 171, 118]), (485, [29, 141, 126]), (447, [224, 108, 100]), (296, [232, 73, 131]), (351, [146, 114, 106]), (300, [149, 81, 106]), (551, [233, 47, 137]), (359, [226, 17, 129]), (248, [163, 44, 111]), (356, [176, 13, 110]), (540, [96, 109, 98]), (501, [95, 81, 99]), (314, [25, 113, 119]), (278, [24, 84, 116]), (318, [93, 51, 107]), (420, [43, 50, 104]), (316, [88, 18, 114]), (501, [36, 16, 131])];
        assert_eq!(quantize_median_cut(&noisy(128, 96), 32), noisy_want);
        let smooth_want: [(u32, [u8; 3]); 32] = [(574, [222, 238, 117]), (573, [222, 205, 123]), (528, [157, 238, 120]), (485, [157, 205, 120]), (539, [222, 172, 118]), (558, [222, 140, 123]), (527, [157, 172, 120]), (457, [157, 140, 120]), (527, [93, 238, 120]), (543, [30, 238, 123]), (484, [93, 205, 120]), (544, [30, 205, 117]), (521, [93, 172, 120]), (458, [93, 140, 120]), (507, [30, 172, 122]), (532, [30, 140, 117]), (524, [222, 108, 118]), (544, [222, 76, 123]), (526, [157, 108, 120]), (453, [157, 76, 120]), (588, [222, 45, 117]), (500, [222, 14, 123]), (480, [157, 45, 120]), (464, [157, 14, 120]), (519, [93, 108, 120]), (451, [93, 76, 120]), (494, [30, 108, 122]), (519, [30, 76, 117]), (473, [93, 45, 120]), (462, [93, 14, 120]), (557, [30, 45, 123]), (473, [30, 14, 117])];
        assert_eq!(quantize_median_cut(&smooth(128, 128), 32), smooth_want);
    }

    #[test]
    fn median_cut_stops_at_the_colors_a_cover_has() {
        // One color cannot be cut, and two colors make two boxes however many are asked for.
        assert_eq!(quantize_median_cut(&flat(40, 30), 32), [(1200, [120, 120, 120])]);
        let mut two = flat(40, 30);
        two.put_pixel(0, 0, image::Rgb([10, 200, 30]));
        assert_eq!(quantize_median_cut(&two, 32), [(1, [10, 200, 30]), (1199, [120, 120, 120])]);
    }
}

//! Port of ui/color_utils.py: perceptual color math for cover-derived colors.
//! OkLCh changes a color, since its lightness is perceptual and hue and chroma
//! survive the edit. WCAG relative luminance checks a color.
//! The float operations mirror Python's one for one, so both apps land on the
//! same numbers for the same cover.
#![allow(dead_code)]

/// An (r, g, b) color, channels in 0..1.
pub type Rgb = (f64, f64, f64);

/// WCAG 2.x targets. High-contrast mode uses AAA.
pub const WCAG_AA: f64 = 4.5;
pub const WCAG_AAA: f64 = 7.0;

const BLACK: Rgb = (0.0, 0.0, 0.0);
const WHITE: Rgb = (1.0, 1.0, 1.0);

fn unit(c: f64) -> f64 {
    c.clamp(0.0, 1.0)
}

pub(crate) fn srgb_to_linear(c: f64) -> f64 {
    if c <= 0.04045 { c / 12.92 } else { ((c + 0.055) / 1.055).powf(2.4) }
}

fn linear_to_srgb(c: f64) -> f64 {
    if c <= 0.0031308 { c * 12.92 } else { 1.055 * c.powf(1.0 / 2.4) - 0.055 }
}

/// WCAG relative luminance. Channels linearize first.
pub fn relative_luminance(rgb: Rgb) -> f64 {
    let (r, g, b) = (srgb_to_linear(unit(rgb.0)), srgb_to_linear(unit(rgb.1)), srgb_to_linear(unit(rgb.2)));
    0.2126 * r + 0.7152 * g + 0.0722 * b
}

/// WCAG contrast ratio between two opaque colors, 1.0 to 21.0.
pub fn contrast_ratio(a: Rgb, b: Rgb) -> f64 {
    let (la, lb) = (relative_luminance(a), relative_luminance(b));
    (la.max(lb) + 0.05) / (la.min(lb) + 0.05)
}

// Bjorn Ottosson's Oklab, the space behind CSS oklch() and color-mix(in oklab).
fn rgb_to_oklab(rgb: Rgb) -> (f64, f64, f64) {
    let (r, g, b) = (srgb_to_linear(unit(rgb.0)), srgb_to_linear(unit(rgb.1)), srgb_to_linear(unit(rgb.2)));
    let l = 0.4122214708 * r + 0.5363325363 * g + 0.0514459929 * b;
    let m = 0.2119034982 * r + 0.6806995451 * g + 0.1073969566 * b;
    let s = 0.0883024619 * r + 0.2817188376 * g + 0.6299787005 * b;
    // powf, not cbrt, is what Python's ** (1 / 3) computes.
    let root = |v: f64| v.abs().powf(1.0 / 3.0).copysign(v);
    let (l, m, s) = (root(l), root(m), root(s));
    (
        0.2104542553 * l + 0.7936177850 * m - 0.0040720468 * s,
        1.9779984951 * l - 2.4285922050 * m + 0.4505937099 * s,
        0.0259040371 * l + 0.7827717662 * m - 0.8086757660 * s,
    )
}

fn oklab_to_rgb(lab: (f64, f64, f64)) -> Rgb {
    let (ll, aa, bb) = lab;
    let l = (ll + 0.3963377774 * aa + 0.2158037573 * bb).powf(3.0);
    let m = (ll - 0.1055613458 * aa - 0.0638541728 * bb).powf(3.0);
    let s = (ll - 0.0894841775 * aa - 1.2914855480 * bb).powf(3.0);
    (
        linear_to_srgb(4.0767416621 * l - 3.3077115913 * m + 0.2309699292 * s),
        linear_to_srgb(-1.2684380046 * l + 2.6097574011 * m - 0.3413193965 * s),
        linear_to_srgb(-0.0041960863 * l - 0.7034186147 * m + 1.7076147010 * s),
    )
}

/// (r, g, b) to (L, C, h): lightness 0..1, chroma about 0..0.4, hue in radians.
pub fn rgb_to_oklch(rgb: Rgb) -> (f64, f64, f64) {
    let (ll, aa, bb) = rgb_to_oklab(rgb);
    (ll, aa.hypot(bb), bb.atan2(aa))
}

fn in_gamut(rgb: Rgb) -> bool {
    const TOLERANCE: f64 = 1e-4;
    [rgb.0, rgb.1, rgb.2].iter().all(|c| (-TOLERANCE..=1.0 + TOLERANCE).contains(c))
}

fn clipped(rgb: Rgb) -> Rgb {
    (unit(rgb.0), unit(rgb.1), unit(rgb.2))
}

/// (L, C, h) to an in-gamut (r, g, b).
/// Walks chroma down until the color fits. Clipping channels would shift the hue.
pub fn oklch_to_rgb(lightness: f64, chroma: f64, hue: f64) -> Rgb {
    let lightness = unit(lightness);
    let at = |c: f64| oklab_to_rgb((lightness, c * hue.cos(), c * hue.sin()));
    let (mut lo, mut hi) = (0.0, chroma.max(0.0));
    let candidate = at(hi);
    if in_gamut(candidate) {
        return clipped(candidate);
    }
    for _ in 0..24 {
        let mid = (lo + hi) / 2.0;
        if in_gamut(at(mid)) {
            lo = mid;
        } else {
            hi = mid;
        }
    }
    clipped(at(lo))
}

/// Label color for something drawn on `background`. Python's default minimum is 3.0.
/// White while white clears `minimum`, black below. libadwaita does the same:
/// white on a mid blue, even though black scores higher.
pub fn best_foreground(background: Rgb, minimum: f64) -> Rgb {
    if contrast_ratio(WHITE, background) >= minimum {
        return WHITE;
    }
    if contrast_ratio(BLACK, background) >= contrast_ratio(WHITE, background) { BLACK } else { WHITE }
}

/// Twenty halvings of an OkLCh lightness range, returning the bound that passes `ok`.
/// `passes_low` says which end of the range passes: the dark one, or the light one.
fn bisect_lightness(mut lo: f64, mut hi: f64, passes_low: bool, ok: impl Fn(f64) -> bool) -> f64 {
    for _ in 0..20 {
        let mid = (lo + hi) / 2.0;
        if ok(mid) == passes_low {
            lo = mid;
        } else {
            hi = mid;
        }
    }
    if passes_low { lo } else { hi }
}

/// Move `color` far enough to clear `target` on `background`. Python's default target is WCAG_AA.
/// Only OkLCh lightness moves, away from the background, by the smallest step
/// reaching the target. Hue survives. Black or white when neither reaches it.
pub fn ensure_contrast(color: Rgb, background: Rgb, target: f64) -> Rgb {
    if contrast_ratio(color, background) >= target {
        return color;
    }
    let (lightness, chroma, hue) = rgb_to_oklch(color);
    // Darken on light surfaces, lighten on dark ones.
    let darken = relative_luminance(background) > relative_luminance(color);
    let (lo, hi) = if darken { (0.0, lightness) } else { (lightness, 1.0) };

    // Unreachable this way: the highest-contrast endpoint, by ratio, first one on a tie.
    let extreme = oklch_to_rgb(if darken { lo } else { hi }, chroma, hue);
    if contrast_ratio(extreme, background) < target {
        let mut best = extreme;
        for candidate in [BLACK, WHITE] {
            if contrast_ratio(candidate, background) > contrast_ratio(best, background) {
                best = candidate;
            }
        }
        return best;
    }
    let found = bisect_lightness(lo, hi, darken, |mid| contrast_ratio(oklch_to_rgb(mid, chroma, hue), background) >= target);
    oklch_to_rgb(found, chroma, hue)
}

/// Clamp a color's OkLCh lightness, preserving hue and chroma. Python's defaults are 0.0 and 1.0.
pub fn clamp_lightness(color: Rgb, minimum: f64, maximum: f64) -> Rgb {
    let (lightness, chroma, hue) = rgb_to_oklch(color);
    let clamped = maximum.min(minimum.max(lightness));
    if clamped == lightness {
        return color;
    }
    oklch_to_rgb(clamped, chroma, hue)
}

/// Neutral gray with the given WCAG relative luminance.
pub fn gray(luminance: f64) -> Rgb {
    let channel = linear_to_srgb(unit(luminance));
    (channel, channel, channel)
}

/// Overlay standing `target` contrast from `base` once composited at `alpha`.
/// Takes `hue_source`'s hue and chroma. `lighter` sets the direction:
/// highlights lift, chrome panels recede. Python's default is lighter.
pub fn overlay_for_contrast(base: Rgb, hue_source: Rgb, alpha: f64, target: f64, lighter: bool) -> Rgb {
    let (_, chroma, hue) = rgb_to_oklch(hue_source);
    let base_lightness = rgb_to_oklch(base).0;
    let (lo, hi) = if lighter { (base_lightness, 1.0) } else { (0.0, base_lightness) };
    // Far from the base passes here, so the passing end is the light one when lifting.
    let found = bisect_lightness(lo, hi, !lighter, |mid| contrast_ratio(mix(base, oklch_to_rgb(mid, chroma, hue), alpha), base) >= target);
    oklch_to_rgb(found, chroma, hue)
}

/// Overlay that still stands `target` contrast from `foreground` once composited on `base` at `alpha`.
/// The counterpart to overlay_for_contrast: sized by what is drawn on top, not by what sits behind.
/// Moves only lightness, away from `foreground`. Returns `hue_source` untouched when it already clears.
pub fn overlay_clear_of(base: Rgb, hue_source: Rgb, alpha: f64, foreground: Rgb, target: f64) -> Rgb {
    if contrast_ratio(mix(base, hue_source, alpha), foreground) >= target {
        return hue_source;
    }
    let (lightness, chroma, hue) = rgb_to_oklch(hue_source);
    let darken = relative_luminance(foreground) > relative_luminance(mix(base, hue_source, alpha));
    let clears = |l: f64| contrast_ratio(mix(base, oklch_to_rgb(l, chroma, hue), alpha), foreground) >= target;

    // Unreachable this way: the endpoint is the closest this hue gets.
    let extreme = if darken { 0.0 } else { 1.0 };
    if !clears(extreme) {
        return oklch_to_rgb(extreme, chroma, hue);
    }
    let (lo, hi) = if darken { (0.0, lightness) } else { (lightness, 1.0) };
    oklch_to_rgb(bisect_lightness(lo, hi, darken, clears), chroma, hue)
}

/// Composite `other` over `base` at `amount` alpha, in sRGB. Matches GTK's mix() and alpha().
pub fn mix(base: Rgb, other: Rgb, amount: f64) -> Rgb {
    let blend = |b: f64, o: f64| b * (1.0 - amount) + o * amount;
    (blend(base.0, other.0), blend(base.1, other.1), blend(base.2, other.2))
}

/// Format as `rgb(r, g, b)` for a GTK stylesheet.
pub fn to_css(rgb: Rgb) -> String {
    // Python's round() sends ties to even.
    let byte = |c: f64| (unit(c) * 255.0).round_ties_even() as u8;
    format!("rgb({}, {}, {})", byte(rgb.0), byte(rgb.1), byte(rgb.2))
}

/// Parse `#rrggbb`. None where Python raised.
pub fn from_hex(value: &str) -> Option<Rgb> {
    let value = value.trim_start_matches('#');
    let channel = |i: usize| value.get(i..i + 2).and_then(|pair| u8::from_str_radix(pair, 16).ok()).map(|v| f64::from(v) / 255.0);
    Some((channel(0)?, channel(2)?, channel(4)?))
}

#[cfg(test)]
mod tests {
    use super::*;

    // Every expected value below is what ui/color_utils.py returns for the same input.
    const RED: Rgb = (0.9, 0.1, 0.1);
    const YELLOW: Rgb = (0.95, 0.9, 0.2);
    const BLUE: Rgb = (0.1, 0.2, 0.8);
    const TEAL: Rgb = (0.2, 0.6, 0.55);
    const MID: Rgb = (0.5, 0.5, 0.5);
    const PINK: Rgb = (0.97, 0.72, 0.8);
    const NAVY: Rgb = (0.05, 0.05, 0.2);
    const EPS: f64 = 1e-6;

    fn close(got: Rgb, want: Rgb) -> bool {
        (got.0 - want.0).abs() < EPS && (got.1 - want.1).abs() < EPS && (got.2 - want.2).abs() < EPS
    }

    #[test]
    fn relative_luminance_matches_python() {
        let cases = [
        (RED, 0.17529582558316012),
        (YELLOW, 0.7547625830489877),
        (BLUE, 0.06940371563406952),
        (TEAL, 0.2538710691785679),
        (MID, 0.21404114048223255),
        (WHITE, 1.0),
        (BLACK, 0.0),
        (PINK, 0.5831250943591004),
        (NAVY, 0.006041928818311644),
        ];
        for (color, want) in cases {
            assert!((relative_luminance(color) - want).abs() < EPS, "{color:?}");
        }
    }

    #[test]
    fn contrast_ratio_matches_python() {
        let cases = [
        (RED, WHITE, 4.660539081370724),
        (NAVY, YELLOW, 14.360008658125135),
        (BLACK, WHITE, 21.0),
        (MID, MID, 1.0),
        ];
        for (a, b, want) in cases {
            assert!((contrast_ratio(a, b) - want).abs() < EPS, "{a:?} on {b:?}");
            assert!((contrast_ratio(b, a) - want).abs() < EPS, "ratio is symmetric");
        }
    }

    #[test]
    fn rgb_to_oklch_matches_python() {
        let cases = [
        (RED, (0.587456331346919, 0.23028199153488915, 0.4932681360668313)),
        (YELLOW, (0.9058112851600363, 0.1805509433963884, 1.8343998480238872)),
        (BLUE, (0.43105407310223576, 0.2316963100668275, -1.6321860198704015)),
        (TEAL, (0.6214365630564689, 0.09441146691187872, -3.082778199140394)),
        (MID, (0.5981807266228486, 2.2296585815985204e-08, 1.5686244884415987)),
        (WHITE, (0.9999999934735462, 3.727399553519285e-08, 1.5686244931074056)),
        (BLACK, (0.0, 0.0, 0.0)),
        (PINK, (0.8454625858872306, 0.07787493374194861, -0.03587930829621164)),
        (NAVY, (0.1881668979744383, 0.07338017529555463, -1.4537423740848614)),
        ];
        for (color, want) in cases {
            let got = rgb_to_oklch(color);
            // Hue is noise on a neutral, where chroma is about 1e-8.
            let hue_ok = want.1 < 1e-6 || (got.2 - want.2).abs() < EPS;
            assert!((got.0 - want.0).abs() < EPS && (got.1 - want.1).abs() < EPS && hue_ok, "{color:?}: {got:?}");
        }
    }

    #[test]
    fn oklch_to_rgb_matches_python_in_and_out_of_gamut() {
        let cases = [
        ((0.62, 0.1, 1.0), (0.7034847936847355, 0.46245645385810924, 0.2758592534975928)),
        ((0.7, 0.4, 2.5), (0.0, 0.752130469698673, 0.10394891048126215)),
        ((0.2, 0.3, -2.0), (0.0, 0.09204826825426676, 0.1698141795714898)),
        ((1.3, 0.05, 0.3), (1.0, 0.9999658544972002, 0.9999657983346938)),
        ((0.5, 0.0, 0.0), (0.3885728590463344, 0.3885728590463344, 0.3885728590463344)),
        ((0.9, 0.37, 1.9), (0.9161247769737902, 0.9051165451892963, 0.0)),
        ((0.5, -0.2, 1.0), (0.3885728590463344, 0.3885728590463344, 0.3885728590463344)),
        ];
        for ((l, c, h), want) in cases {
            let got = oklch_to_rgb(l, c, h);
            assert!(close(got, want), "({l}, {c}, {h}): {got:?}");
        }
    }

    #[test]
    fn oklch_round_trips_an_in_gamut_color() {
        for color in [RED, YELLOW, BLUE, TEAL, PINK, NAVY] {
            let (l, c, h) = rgb_to_oklch(color);
            assert!(close(oklch_to_rgb(l, c, h), color), "{color:?}");
        }
    }

    #[test]
    fn best_foreground_matches_python() {
        let cases = [
        (RED, 3.0, (1.0, 1.0, 1.0)),
        (RED, 4.5, (1.0, 1.0, 1.0)),
        (YELLOW, 3.0, (0.0, 0.0, 0.0)),
        (YELLOW, 4.5, (0.0, 0.0, 0.0)),
        (BLUE, 3.0, (1.0, 1.0, 1.0)),
        (BLUE, 4.5, (1.0, 1.0, 1.0)),
        (TEAL, 3.0, (1.0, 1.0, 1.0)),
        (TEAL, 4.5, (0.0, 0.0, 0.0)),
        (MID, 3.0, (1.0, 1.0, 1.0)),
        (MID, 4.5, (0.0, 0.0, 0.0)),
        (PINK, 3.0, (0.0, 0.0, 0.0)),
        (PINK, 4.5, (0.0, 0.0, 0.0)),
        ];
        for (background, minimum, want) in cases {
            assert_eq!(best_foreground(background, minimum), want, "{background:?} at {minimum}");
        }
    }

    #[test]
    fn ensure_contrast_matches_python() {
        let cases = [
        (YELLOW, WHITE, 4.5, (0.5052506567909558, 0.4745753703645373, 0.0)),
        (BLUE, BLACK, 4.5, (0.22648609500064748, 0.39860059310709395, 0.9972618987827228)),
        (TEAL, WHITE, 7.0, (0.0, 0.3948509058290511, 0.35639892482571206)),
        (RED, BLACK, 7.0, (1.0, 0.37178171334814164, 0.3167901853562488)),
        (MID, MID, 4.5, (0.0, 0.0, 0.0)),
        (PINK, WHITE, 3.0, (0.7531725289268723, 0.5179488420693457, 0.5955222173157669)),
        (NAVY, BLACK, 7.0, (0.5407130255807715, 0.5737088713851731, 0.7662303190715767)),
        (YELLOW, BLACK, 4.5, (0.95, 0.9, 0.2)),
        (BLUE, NAVY, 21.0, (1.0, 1.0, 1.0)),
        (WHITE, WHITE, 4.5, (0.0, 0.0, 0.0)),
        ];
        for (color, background, target, want) in cases {
            let got = ensure_contrast(color, background, target);
            assert!(close(got, want), "{color:?} on {background:?} at {target}: {got:?}");
        }
    }

    #[test]
    fn ensure_contrast_reaches_its_target_when_the_hue_allows() {
        let got = ensure_contrast(YELLOW, WHITE, WCAG_AA);
        assert!(contrast_ratio(got, WHITE) >= WCAG_AA);
        let got = ensure_contrast(TEAL, WHITE, WCAG_AAA);
        assert!(contrast_ratio(got, WHITE) >= WCAG_AAA);
    }

    #[test]
    fn clamp_lightness_matches_python() {
        let cases = [
        (YELLOW, 0.0, 0.6, (0.5488022739021544, 0.5157429108109715, 0.0)),
        (NAVY, 0.35, 1.0, (0.19198945185931984, 0.21193564541786258, 0.3773033291745566)),
        (TEAL, 0.2, 0.9, (0.2, 0.6, 0.55)),
        (PINK, 0.3, 0.5, (0.529344223714702, 0.3129754885735917, 0.3882791658474063)),
        ];
        for (color, minimum, maximum, want) in cases {
            let got = clamp_lightness(color, minimum, maximum);
            assert!(close(got, want), "{color:?} in {minimum}..{maximum}: {got:?}");
        }
    }

    #[test]
    fn gray_matches_python() {
        let cases = [
        (0.0, 0.0),
        (0.002, 0.025840000000000002),
        (0.025, 0.17184408698667786),
        (0.35, 0.6262096812245096),
        (1.0, 0.9999999999999999),
        (1.5, 0.9999999999999999),
        ];
        for (luminance, want) in cases {
            assert!(close(gray(luminance), (want, want, want)), "{luminance}");
        }
    }

    #[test]
    fn overlay_for_contrast_matches_python() {
        let cases = [
        (NAVY, RED, 0.2, 1.3, true, (1.0, 0.4061058498754389, 0.3490460158701902)),
        (WHITE, BLUE, 0.15, 1.2, false, (0.2774489738110963, 0.45301978161220596, 1.0)),
        (MID, TEAL, 0.5, 1.5, true, (0.43914500141459545, 0.8187013202831475, 0.7635196926111291)),
        (MID, TEAL, 0.5, 1.5, false, (0.0, 0.34033448330217253, 0.3065416536275113)),
        (BLACK, PINK, 0.1, 1.1, true, (0.7858848368539187, 0.548243103893267, 0.6261839658947482)),
        ];
        for (base, hue_source, alpha, target, lighter, want) in cases {
            let got = overlay_for_contrast(base, hue_source, alpha, target, lighter);
            assert!(close(got, want), "{hue_source:?} over {base:?}: {got:?}");
        }
    }

    #[test]
    fn overlay_clear_of_matches_python() {
        let cases = [
        (NAVY, YELLOW, 0.9, WHITE, 4.5, (0.5551703120984077, 0.5217623505299241, 0.0)),
        (WHITE, PINK, 0.8, BLACK, 4.5, (0.97, 0.72, 0.8)),
        (NAVY, BLUE, 0.5, WHITE, 3.0, (0.1, 0.2, 0.8)),
        (MID, TEAL, 0.6, WHITE, 4.5, (0.06630401623177976, 0.5151717716583494, 0.46766136166558786)),
        (MID, MID, 0.1, WHITE, 15.0, (0.0, 1.0339571620833351e-22, 0.0)),
        (WHITE, YELLOW, 0.9, WHITE, 3.0, (0.5862201852715103, 0.5511124777732289, 0.0)),
        (BLACK, NAVY, 0.7, BLACK, 4.5, (0.6062815288619, 0.6406517100645778, 0.8369938709000566)),
        ];
        for (base, hue_source, alpha, foreground, target, want) in cases {
            let got = overlay_clear_of(base, hue_source, alpha, foreground, target);
            assert!(close(got, want), "{hue_source:?} over {base:?} under {foreground:?}: {got:?}");
        }
    }

    #[test]
    fn mix_matches_python() {
        assert!(close(mix(NAVY, YELLOW, 0.3), (0.31999999999999995, 0.305, 0.19999999999999998)));
        assert_eq!(mix(NAVY, YELLOW, 0.0), NAVY);
        assert_eq!(mix(NAVY, YELLOW, 1.0), YELLOW);
    }

    #[test]
    fn to_css_clamps_and_rounds_ties_to_even_like_python() {
        assert_eq!(to_css((0.5, 0.1, 0.0098039)), "rgb(128, 26, 2)");
        assert_eq!(to_css((2.5 / 255.0, -0.2, 1.2)), "rgb(2, 0, 255)");
        assert_eq!(to_css((0.5 / 255.0, 1.5 / 255.0, 3.5 / 255.0)), "rgb(0, 2, 4)");
    }

    #[test]
    fn from_hex_parses_with_or_without_the_hash() {
        assert!(close(from_hex("#3584e4").unwrap(), (0.20784313725490197, 0.5176470588235295, 0.8941176470588236)));
        assert!(close(from_hex("ff00a0").unwrap(), (1.0, 0.0, 0.6274509803921569)));
        assert_eq!(from_hex("#12"), None);
        assert_eq!(from_hex("#zzzzzz"), None);
    }
}

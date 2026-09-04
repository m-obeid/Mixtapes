"""Perceptual color math for cover-derived colors.

OkLCh changes a color. Its lightness is perceptual, so hue and chroma
survive the edit. WCAG relative luminance checks a color.

The old code clamped in HLS and thresholded gamma-encoded channels. HLS
lightness is not perceptual, so one clamp left yellows unreadable on
white and blues unreadable on black.

Colors are (r, g, b) tuples of floats in 0..1.
"""

import math


# WCAG 2.x targets. High-contrast mode uses AAA.
WCAG_AA = 4.5
WCAG_AAA = 7.0


# ─── sRGB ⇄ linear ─────────────────────────────────────────────────────────


def _srgb_to_linear(c):
    if c <= 0.04045:
        return c / 12.92
    return ((c + 0.055) / 1.055) ** 2.4


def _linear_to_srgb(c):
    if c <= 0.0031308:
        return c * 12.92
    return 1.055 * (c ** (1 / 2.4)) - 0.055


def relative_luminance(rgb):
    """WCAG relative luminance of an (r, g, b) tuple. Channels linearize
    first, unlike the old `0.2126*r + ...` on gamma-encoded channels."""
    r, g, b = (_srgb_to_linear(min(1.0, max(0.0, c))) for c in rgb)
    return 0.2126 * r + 0.7152 * g + 0.0722 * b


def contrast_ratio(a, b):
    """WCAG contrast ratio between two opaque colors, 1.0 … 21.0."""
    la, lb = relative_luminance(a), relative_luminance(b)
    hi, lo = max(la, lb), min(la, lb)
    return (hi + 0.05) / (lo + 0.05)


# ─── OkLab / OkLCh ─────────────────────────────────────────────────────────
# Björn Ottosson's Oklab (https://bottosson.github.io/posts/oklab/).
# Same space as CSS `oklch()` and `color-mix(in oklab, ...)`.


def _rgb_to_oklab(rgb):
    r, g, b = (_srgb_to_linear(min(1.0, max(0.0, c))) for c in rgb)
    l = 0.4122214708 * r + 0.5363325363 * g + 0.0514459929 * b
    m = 0.2119034982 * r + 0.6806995451 * g + 0.1073969566 * b
    s = 0.0883024619 * r + 0.2817188376 * g + 0.6299787005 * b
    l_, m_, s_ = (math.copysign(abs(v) ** (1 / 3), v) for v in (l, m, s))
    return (
        0.2104542553 * l_ + 0.7936177850 * m_ - 0.0040720468 * s_,
        1.9779984951 * l_ - 2.4285922050 * m_ + 0.4505937099 * s_,
        0.0259040371 * l_ + 0.7827717662 * m_ - 0.8086757660 * s_,
    )


def _oklab_to_rgb(lab):
    ll, aa, bb = lab
    l_ = ll + 0.3963377774 * aa + 0.2158037573 * bb
    m_ = ll - 0.1055613458 * aa - 0.0638541728 * bb
    s_ = ll - 0.0894841775 * aa - 1.2914855480 * bb
    l, m, s = l_ ** 3, m_ ** 3, s_ ** 3
    return (
        _linear_to_srgb(4.0767416621 * l - 3.3077115913 * m + 0.2309699292 * s),
        _linear_to_srgb(-1.2684380046 * l + 2.6097574011 * m - 0.3413193965 * s),
        _linear_to_srgb(-0.0041960863 * l - 0.7034186147 * m + 1.7076147010 * s),
    )


def rgb_to_oklch(rgb):
    """(r, g, b) → (L, C, h): lightness 0..1, chroma ~0..0.4, hue in radians."""
    ll, aa, bb = _rgb_to_oklab(rgb)
    return ll, math.hypot(aa, bb), math.atan2(bb, aa)


def oklch_to_rgb(lightness, chroma, hue):
    """(L, C, h) to an in-gamut (r, g, b).

    Walks chroma down until the color fits. Clipping channels would
    shift the hue.
    """
    lightness = min(1.0, max(0.0, lightness))
    lo, hi = 0.0, max(0.0, chroma)
    candidate = _oklab_to_rgb(
        (lightness, hi * math.cos(hue), hi * math.sin(hue))
    )
    if _in_gamut(candidate):
        return tuple(min(1.0, max(0.0, c)) for c in candidate)
    for _ in range(24):
        mid = (lo + hi) / 2
        candidate = _oklab_to_rgb(
            (lightness, mid * math.cos(hue), mid * math.sin(hue))
        )
        if _in_gamut(candidate):
            lo = mid
        else:
            hi = mid
    candidate = _oklab_to_rgb(
        (lightness, lo * math.cos(hue), lo * math.sin(hue))
    )
    return tuple(min(1.0, max(0.0, c)) for c in candidate)


def _in_gamut(rgb, tolerance=1e-4):
    return all(-tolerance <= c <= 1.0 + tolerance for c in rgb)


# ─── Contrast-aware adjustments ────────────────────────────────────────────


def best_foreground(background, minimum=3.0):
    """Label color for something drawn on `background`.

    White while white clears `minimum`, black below. libadwaita does the
    same: white on a mid blue, even though black scores higher.
    """
    black, white = (0.0, 0.0, 0.0), (1.0, 1.0, 1.0)
    if contrast_ratio(white, background) >= minimum:
        return white
    if contrast_ratio(black, background) >= contrast_ratio(white, background):
        return black
    return white


def ensure_contrast(color, background, target=WCAG_AA):
    """Move `color` far enough to clear `target` on `background`.

    Only OkLCh lightness moves, away from the background, by the
    smallest step reaching the target. Hue survives. Returns black or
    white when neither reaches it.
    """
    if contrast_ratio(color, background) >= target:
        return color

    lightness, chroma, hue = rgb_to_oklch(color)
    # Darken on light surfaces, lighten on dark ones.
    darken = relative_luminance(background) > relative_luminance(color)
    lo, hi = (0.0, lightness) if darken else (lightness, 1.0)

    # Unreachable this way: return the highest-contrast endpoint. Uses
    # ratios directly, not best_foreground(), which prefers white.
    extreme = oklch_to_rgb(lo if darken else hi, chroma, hue)
    if contrast_ratio(extreme, background) < target:
        candidates = [extreme, (0.0, 0.0, 0.0), (1.0, 1.0, 1.0)]
        return max(candidates, key=lambda c: contrast_ratio(c, background))

    # Keep the bound clearing `target` for the least adjusted result.
    for _ in range(20):
        mid = (lo + hi) / 2
        candidate = oklch_to_rgb(mid, chroma, hue)
        ok = contrast_ratio(candidate, background) >= target
        if darken:
            lo, hi = (mid, hi) if ok else (lo, mid)
        else:
            lo, hi = (lo, mid) if ok else (mid, hi)
    return oklch_to_rgb(lo if darken else hi, chroma, hue)


def clamp_lightness(color, minimum=0.0, maximum=1.0):
    """Clamp a color's OkLCh lightness, preserving hue and chroma."""
    lightness, chroma, hue = rgb_to_oklch(color)
    clamped = min(maximum, max(minimum, lightness))
    if clamped == lightness:
        return color
    return oklch_to_rgb(clamped, chroma, hue)


def gray(luminance):
    """Neutral gray with the given WCAG relative luminance."""
    channel = _linear_to_srgb(min(1.0, max(0.0, luminance)))
    return (channel, channel, channel)


def overlay_for_contrast(base, hue_source, alpha, target, lighter=True):
    """Overlay standing `target` contrast from `base` once composited
    at `alpha`. Takes `hue_source`'s hue and chroma.

    Picks the overlay by the separation wanted, not by a guessed opacity.
    `lighter` sets the direction: highlights lift, chrome panels recede.
    """
    _lightness, chroma, hue = rgb_to_oklch(hue_source)
    base_lightness = rgb_to_oklch(base)[0]
    lo, hi = (base_lightness, 1.0) if lighter else (0.0, base_lightness)
    for _ in range(20):
        mid = (lo + hi) / 2
        composite = mix(base, oklch_to_rgb(mid, chroma, hue), alpha)
        reached = contrast_ratio(composite, base) >= target
        if lighter:
            hi, lo = (mid, lo) if reached else (hi, mid)
        else:
            lo, hi = (mid, hi) if reached else (lo, mid)
    return oklch_to_rgb(hi if lighter else lo, chroma, hue)


def mix(base, other, amount):
    """Composite `other` over `base` at `amount` alpha, in sRGB.

    Matches GTK's `mix()` and `alpha()`.
    """
    return tuple(
        b * (1.0 - amount) + o * amount for b, o in zip(base, other)
    )


def to_css(rgb):
    """Format as `rgb(r, g, b)` for a GTK stylesheet."""
    r, g, b = (int(round(min(1.0, max(0.0, c)) * 255)) for c in rgb)
    return f"rgb({r}, {g}, {b})"


def from_hex(value):
    """Parse `#rrggbb` into an (r, g, b) tuple of floats."""
    value = value.lstrip("#")
    return tuple(int(value[i:i + 2], 16) / 255.0 for i in (0, 2, 4))

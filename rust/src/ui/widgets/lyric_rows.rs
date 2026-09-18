//! The rows of the lyrics column. Port of LyricRow and InterludeRow in
//! ui/widgets/lyrics_view.py, plus the pure rules that feed them.
//!
//! A row always renders through Pango markup so active and inactive states
//! share one layout. While a line is sung its label is hidden and the row
//! paints the text itself: sung words in the active color, the word under the
//! cursor split at the sweep position, long words riding a small wave.

use std::cell::{Cell, RefCell};
use std::sync::LazyLock;

use gtk::prelude::*;
use gtk::subclass::prelude::*;
use gtk::{gdk, glib, graphene, gsk, pango};
use regex::Regex;

use crate::lyrics::model::{LyricLine, LyricPart};

const ALPHA_ACTIVE: f64 = 1.00;
const ALPHA_FUTURE_WORD: f64 = 0.32;

/// Per-frame fade speed, as a fraction of the remaining gap.
const LERP_SPEED: f64 = 0.18;
const LERP_SPEED_EFFECTS: f64 = 0.34;

/// How long a word takes to reach full brightness, as a share of how long it is held.
const WORD_RAMP_FRACTION: f64 = 0.55;
const WORD_RAMP_MIN_MS: f64 = 90.0;
const WORD_RAMP_MAX_MS: f64 = 420.0;
/// Synthesized timings are an estimate, so their words ramp faster than real ones.
const SWEEP_RAMP_FRACTION: f64 = 0.3;

const BLUR_START_DISTANCE: i32 = 0;
const BLUR_PER_LINE: f64 = 0.65;
const BLUR_MAX: f64 = 3.5;
const EFFECT_LERP: f64 = 0.16;

/// A span longer than this is followed by an instrumental, not sung throughout.
const SWEEP_MAX_MS: i64 = 12_000;
/// Below this a sweep reads as a flash.
const SWEEP_MIN_MS: i64 = 400;

/// A word held at least this long glows and waves.
const MIN_GLOW_MS: f64 = 600.0;
const MAX_GLOW_MS: f64 = 1200.0;

/// An instrumental stretch shorter than this is not worth marking.
const INTERLUDE_MIN_S: f64 = 5.0;
/// End the marker just before the vocal returns.
const INTERLUDE_END_EARLY_S: f64 = 0.25;
const INTERLUDE_DOTS: usize = 3;
const DOT_RADIUS: f64 = 3.0;
const DOT_SPACING: f64 = 11.0;
const DOT_FOCUS_SWELL: f64 = 0.55;
const DOT_WAVE_SWELL: f64 = 0.14;
const DOT_WAVE_PERIOD: f64 = 2.2;
const DOT_MAX_SWELL: f64 = 1.0 + DOT_FOCUS_SWELL + DOT_WAVE_SWELL;

/// The effects level a row was built for.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Effects {
    Off,
    Subtle,
    Full,
}

impl Effects {
    pub fn from_pref(value: &str) -> Self {
        match value {
            "off" => Effects::Off,
            "full" => Effects::Full,
            _ => Effects::Subtle,
        }
    }
}

/// A word with its timing in milliseconds and its place in the line's UTF-8 text.
#[derive(Clone, Debug, PartialEq)]
pub struct Part {
    pub start_ms: i64,
    pub end_ms: i64,
    pub text: String,
    pub space_after: bool,
    pub byte_start: i32,
    pub byte_end: i32,
}

// -- pure rules ---------------------------------------------------------------

// Scripts a romanization helps with. Greek is left out: it shows up as stylized Latin.
static NON_LATIN: LazyLock<Regex> = LazyLock::new(|| {
    Regex::new("[\u{0400}-\u{04FF}\u{0590}-\u{05FF}\u{0600}-\u{06FF}\u{0E00}-\u{0E7F}\u{3040}-\u{30FF}\u{3400}-\u{4DBF}\u{4E00}-\u{9FFF}\u{AC00}-\u{D7AF}]").expect("static regex")
});

pub fn is_non_latin(text: &str) -> bool {
    !text.is_empty() && NON_LATIN.is_match(text)
}

/// The second line's text for one lyric line, and its word timing when it is a background vocal.
pub fn second_line_for<'a>(line: &'a LyricLine, mode: &str) -> (Option<&'a str>, Option<&'a [LyricPart]>) {
    let clean = |value: &'a Option<String>| value.as_deref().map(str::trim).filter(|t| !t.is_empty());
    let roman = clean(&line.romanization);
    let translation = clean(&line.translation);
    let bg_text = clean(&line.bg_text);
    match mode {
        "off" => (None, None),
        "romanization" => (roman, None),
        "translation" => (translation, None),
        "background" => (bg_text, bg_text.map(|_| line.bg.as_slice())),
        // auto: a romanization where the script needs one, background vocals otherwise.
        _ => match (roman, bg_text) {
            (Some(roman), _) if is_non_latin(&line.text) => (Some(roman), None),
            (_, Some(bg)) => (Some(bg), Some(line.bg.as_slice())),
            _ => (None, None),
        },
    }
}

/// Scripts written without spaces between words.
fn is_cjk_char(ch: char) -> bool {
    matches!(ch as u32, 0x1100..=0x11FF | 0x3040..=0x30FF | 0x3130..=0x318F | 0x3400..=0x4DBF | 0x4E00..=0x9FFF | 0xAC00..=0xD7AF | 0xF900..=0xFAFF)
}

/// The chunks a sweep advances over: words, or single characters for CJK.
fn sweep_tokens(text: &str) -> Vec<(String, bool)> {
    let mut tokens: Vec<(String, bool)> = Vec::new();
    let mut buf = String::new();
    for ch in text.chars() {
        if ch.is_whitespace() {
            if !buf.is_empty() {
                tokens.push((std::mem::take(&mut buf), false));
            }
            if let Some(last) = tokens.last_mut() {
                last.1 = true;
            }
        } else if is_cjk_char(ch) {
            if !buf.is_empty() {
                tokens.push((std::mem::take(&mut buf), false));
            }
            tokens.push((ch.to_string(), false));
        } else {
            buf.push(ch);
        }
    }
    if !buf.is_empty() {
        tokens.push((buf, false));
    }
    tokens
}

/// Fake per-word timing for a line-synced lyric: the line's span divided
/// across its tokens by length. Empty when there is nothing to sweep across.
pub fn synthesize_parts(text: &str, start_ms: i64, end_ms: Option<i64>) -> Vec<Part> {
    let Some(end_ms) = end_ms else { return Vec::new() };
    let span = end_ms - start_ms;
    if span < SWEEP_MIN_MS {
        return Vec::new();
    }
    let span = span.min(SWEEP_MAX_MS) as f64;
    let tokens = sweep_tokens(text);
    if tokens.len() < 2 {
        return Vec::new();
    }
    let total = tokens.iter().map(|(t, _)| t.chars().count()).sum::<usize>().max(1) as f64;
    let mut cursor = start_ms as f64;
    tokens
        .into_iter()
        .map(|(text, space_after)| {
            let share = span * (text.chars().count() as f64 / total);
            let part = Part { start_ms: cursor as i64, end_ms: (cursor + share) as i64, text, space_after, byte_start: -1, byte_end: -1 };
            cursor += share;
            part
        })
        .collect()
}

/// Seconds to millisecond ints, dropping anything without a start time.
pub fn normalize_parts(raw: &[LyricPart]) -> Vec<Part> {
    raw.iter()
        .filter(|p| !p.text.is_empty())
        .filter_map(|p| {
            let start = p.start?;
            Some(Part {
                start_ms: (start * 1000.0) as i64,
                end_ms: (p.end.unwrap_or(start) * 1000.0) as i64,
                text: p.text.clone(),
                space_after: p.space_after,
                byte_start: -1,
                byte_end: -1,
            })
        })
        .collect()
}

/// Find each part in the line's text, in order, so Pango indices can address it.
fn assign_byte_offsets(text: &str, parts: &mut [Part]) {
    if text.is_empty() {
        return;
    }
    let mut offset = 0usize;
    for part in parts {
        match text.get(offset..).and_then(|rest| rest.find(&part.text)) {
            Some(found) => {
                part.byte_start = (offset + found) as i32;
                part.byte_end = part.byte_start + part.text.len() as i32;
                offset = part.byte_end as usize;
            }
            None => {
                part.byte_start = -1;
                part.byte_end = -1;
            }
        }
    }
}

/// The instrumental stretches in a line list, as (start, end) seconds.
/// Providers either stamp an empty line at the top of a break or leave a hole
/// after a line's own end. Both read as a gap between two lines with words.
pub fn find_interludes(lines: &[LyricLine]) -> Vec<(f64, f64)> {
    let timed: Vec<(usize, &LyricLine, f64)> = lines.iter().enumerate().filter_map(|(i, l)| l.start.map(|s| (i, l, s))).collect();
    let sung: Vec<&(usize, &LyricLine, f64)> = timed.iter().filter(|(_, l, _)| !l.text.trim().is_empty()).collect();
    let Some(first) = sung.first() else { return Vec::new() };

    let mut out = Vec::new();
    // A long lead-in before the first word is an interlude too.
    if first.2 >= INTERLUDE_MIN_S {
        out.push((0.0, first.2 - INTERLUDE_END_EARLY_S));
    }
    for pair in sung.windows(2) {
        let (idx_a, a, _) = pair[0];
        let gap_end = pair[1].2;
        // Prefer the line's own end, else an empty marker line after it.
        let gap_start = a.end.or_else(|| timed.iter().find(|(j, l, _)| j > idx_a && l.text.trim().is_empty()).map(|(_, _, s)| *s));
        let Some(gap_start) = gap_start else { continue };
        if gap_end - gap_start >= INTERLUDE_MIN_S {
            out.push((gap_start, gap_end - INTERLUDE_END_EARLY_S));
        }
    }
    out
}

fn word_alpha_for(cursor_ms: i64, part: &Part, effects: Effects, swept: bool) -> f64 {
    if cursor_ms < part.start_ms {
        return 0.0;
    }
    if effects == Effects::Off {
        return 1.0;
    }
    let held = (part.end_ms - part.start_ms).max(0) as f64;
    let fraction = if swept { SWEEP_RAMP_FRACTION } else { WORD_RAMP_FRACTION };
    let ramp = (held * fraction).clamp(WORD_RAMP_MIN_MS, WORD_RAMP_MAX_MS);
    ((cursor_ms - part.start_ms) as f64 / ramp).min(1.0)
}

/// Where the sweep stands inside a laid-out line.
struct Sweep {
    /// Index of the word under the cursor, if one is.
    active: Option<usize>,
    /// Sweep position, relative to the layout.
    x: f64,
    /// End of the last finished word: (x, y, height).
    sung: (f64, f64, f64),
    /// The active word's box: (x, y, width, height).
    active_rect: Option<(f64, f64, f64, f64)>,
    glow: f64,
    is_long: bool,
    wave_amp: f64,
}

fn sweep_state(cursor_ms: f64, parts: &[Part], layout: &pango::Layout) -> Sweep {
    let scale = f64::from(pango::SCALE);
    let mut sweep = Sweep { active: None, x: 0.0, sung: (0.0, 0.0, 0.0), active_rect: None, glow: 0.0, is_long: false, wave_amp: 0.0 };
    for (i, p) in parts.iter().enumerate() {
        if p.byte_start < 0 {
            continue;
        }
        let pos_start = layout.index_to_pos(p.byte_start);
        let pos_end = layout.index_to_pos(p.byte_end);
        let x_start = f64::from(pos_start.x()) / scale;
        let y_start = f64::from(pos_start.y()) / scale;
        let height = f64::from(pos_start.height()) / scale;
        let mut x_end = f64::from(pos_end.x()) / scale;
        if x_end <= x_start {
            x_end = x_start + f64::from(pos_start.width()) / scale * p.text.chars().count() as f64;
        }
        if cursor_ms >= p.end_ms as f64 {
            sweep.sung = (x_end, y_start, height);
        } else if cursor_ms >= p.start_ms as f64 {
            let duration = ((p.end_ms - p.start_ms) as f64).max(1.0);
            let progress = (cursor_ms - p.start_ms as f64) / duration;
            sweep.active = Some(i);
            sweep.x = x_start + (x_end - x_start) * progress;
            sweep.active_rect = Some((x_start, y_start, x_end - x_start, height));
            if duration >= MIN_GLOW_MS {
                let weight = ((duration - MIN_GLOW_MS) / (MAX_GLOW_MS - MIN_GLOW_MS)).min(1.0);
                sweep.is_long = true;
                sweep.glow = (progress * std::f64::consts::PI).sin() * (2.0 * weight);
                sweep.wave_amp = 2.0 + 4.0 * weight;
            }
            return sweep;
        } else {
            break;
        }
    }
    sweep
}

fn lerp_color(a: &gdk::RGBA, b: &gdk::RGBA, t: f64) -> gdk::RGBA {
    let t = t as f32;
    let mix = |x: f32, y: f32| (x + (y - x) * t).clamp(0.0, 1.0);
    gdk::RGBA::new(mix(a.red(), b.red()), mix(a.green(), b.green()), mix(a.blue(), b.blue()), mix(a.alpha(), b.alpha()))
}

fn color_to_markup(color: &gdk::RGBA, text: &str) -> String {
    let channel = |v: f32| (v * 255.0).round().clamp(0.0, 255.0) as u8;
    let alpha = (f64::from(color.alpha()) * 65535.0).round().clamp(1.0, 65535.0) as u32;
    format!("<span color='#{:02x}{:02x}{:02x}' fgalpha='{alpha}'>{}</span>", channel(color.red()), channel(color.green()), channel(color.blue()), glib::markup_escape_text(text))
}

/// The label's resting, active and glow colors, as the stylesheet resolves them.
#[allow(deprecated)]
fn css_colors(label: &gtk::Label) -> (gdk::RGBA, gdk::RGBA, gdk::RGBA) {
    let ctx = label.style_context();
    let resting = ctx.color();
    let with = |class: &str| {
        ctx.save();
        ctx.add_class(class);
        let color = ctx.color();
        ctx.restore();
        color
    };
    (resting, with("active"), with("glow"))
}

fn now_ms() -> f64 {
    glib::monotonic_time() as f64 / 1000.0
}

// -- LyricRow -----------------------------------------------------------------

struct RowState {
    text: String,
    parts: Vec<Part>,
    swept: bool,
    sub_text: String,
    sub_parts: Vec<Part>,
    label: gtk::Label,
    sub_label: Option<gtk::Label>,
    effects: Effects,
    opposite_voice: bool,
    can_scale: bool,
    can_glow: bool,
    can_blur: bool,
    active_scale: f64,
    lerp: f64,
    cursor_ms: i64,
    internal_cursor_ms: f64,
    wants_turn_off: bool,
    scale: f64,
    scale_target: f64,
    blur: f64,
    blur_target: f64,
    distance: i32,
    word_alphas: Vec<f64>,
    word_targets: Vec<f64>,
    sub_alphas: Vec<f64>,
    sub_targets: Vec<f64>,
    dirty: bool,
    last_tick: Option<f64>,
    last_colors: Option<[i32; 6]>,
}

impl RowState {
    fn recompute_effect_targets(&mut self) {
        let active = self.cursor_ms >= 0;
        self.scale_target = if active && self.can_scale { self.active_scale } else { 1.0 };
        self.blur_target = if !self.can_blur || active || self.distance < BLUR_START_DISTANCE { 0.0 } else { (f64::from(self.distance - BLUR_START_DISTANCE + 1) * BLUR_PER_LINE).min(BLUR_MAX) };
    }

    fn recompute_targets(&mut self) {
        let active = self.cursor_ms >= 0;
        let (cursor, effects, swept) = (self.cursor_ms, self.effects, self.swept);
        let fill = |parts: &[Part], targets: &mut [f64]| {
            if parts.is_empty() {
                targets[0] = if active { 1.0 } else { 0.0 };
            } else {
                for (target, part) in targets.iter_mut().zip(parts) {
                    *target = if active { word_alpha_for(cursor, part, effects, swept) } else { 0.0 };
                }
            }
        };
        fill(&self.parts, &mut self.word_targets);
        if self.sub_label.is_some() {
            fill(&self.sub_parts, &mut self.sub_targets);
        }
        self.dirty = true;
    }

    /// Step every alpha toward its target. True when one moved.
    fn step_alphas(&mut self) -> bool {
        let lerp = self.lerp;
        let sung = self.cursor_ms >= 0 && self.effects != Effects::Off;
        let internal = self.internal_cursor_ms;
        let step = |parts: &[Part], alphas: &mut [f64], targets: &[f64]| {
            let mut changed = false;
            for i in 0..alphas.len() {
                // While a line is sung a word lights up once the cursor has passed it.
                let target = match (sung, parts.get(i)) {
                    (true, Some(p)) => f64::from(u8::from(internal > p.end_ms as f64)),
                    (true, None) => 1.0,
                    (false, _) => targets[i],
                };
                if (alphas[i] - target).abs() > 0.005 {
                    alphas[i] += (target - alphas[i]) * lerp;
                    changed = true;
                }
            }
            changed
        };
        let mut changed = step(&self.parts, &mut self.word_alphas, &self.word_targets);
        if self.sub_label.is_some() {
            changed |= step(&self.sub_parts, &mut self.sub_alphas, &self.sub_targets);
        }
        changed
    }

    fn markup_for(&self, label: &gtk::Label, text: &str, parts: &[Part], alphas: &[f64]) -> String {
        let (resting, active, _) = css_colors(label);
        if self.effects == Effects::Off {
            return color_to_markup(if self.cursor_ms < 0 { &resting } else { &active }, text);
        }
        if parts.is_empty() {
            return color_to_markup(&lerp_color(&resting, &active, alphas[0].clamp(0.0, 1.0)), text);
        }
        let mut out = String::new();
        for (i, part) in parts.iter().enumerate() {
            out.push_str(&color_to_markup(&lerp_color(&resting, &active, alphas[i].clamp(0.0, 1.0)), &part.text));
            if part.space_after && i + 1 < parts.len() {
                out.push(' ');
            }
        }
        out
    }

    fn render_markup(&self) {
        self.label.set_markup(&self.markup_for(&self.label, &self.text, &self.parts, &self.word_alphas));
        if let Some(sub) = &self.sub_label {
            sub.set_markup(&self.markup_for(sub, &self.sub_text, &self.sub_parts, &self.sub_alphas));
        }
    }

    fn end_bound(&self) -> f64 {
        let last = |parts: &[Part]| parts.last().map_or(0, |p| p.end_ms);
        last(&self.parts).max(last(&self.sub_parts)) as f64
    }
}

mod imp {
    use super::*;

    #[derive(Default)]
    pub struct LyricRow {
        pub(super) state: RefCell<Option<RowState>>,
        pub line_idx: Cell<i32>,
        pub start_ms: Cell<i64>,
        pub is_static: Cell<bool>,
        pub paused: Cell<bool>,
    }

    #[glib::object_subclass]
    impl ObjectSubclass for LyricRow {
        const NAME: &'static str = "MixtapesLyricRow";
        type Type = super::LyricRow;
        type ParentType = gtk::ListBoxRow;
    }

    impl ObjectImpl for LyricRow {}
    impl ListBoxRowImpl for LyricRow {}

    impl WidgetImpl for LyricRow {
        fn snapshot(&self, snapshot: &gtk::Snapshot) {
            let guard = self.state.borrow();
            let Some(s) = guard.as_ref() else { return self.parent_snapshot(snapshot) };
            let scaling = (s.scale - 1.0).abs() > 0.002;
            let blurring = s.blur > 0.02;
            let active = s.internal_cursor_ms >= 0.0 && s.effects != Effects::Off;

            // The row paints a sung line itself, so its label steps aside.
            s.label.set_opacity(if active && !s.parts.is_empty() { 0.0 } else { 1.0 });
            if let Some(sub) = &s.sub_label {
                sub.set_opacity(if active && !s.sub_parts.is_empty() { 0.0 } else { 1.0 });
            }

            if blurring {
                snapshot.push_blur(s.blur);
            }
            if scaling {
                let row = self.obj();
                let height = row.height() as f32;
                let anchor_x = if s.opposite_voice { row.width() as f32 } else { 0.0 };
                snapshot.save();
                snapshot.translate(&graphene::Point::new(anchor_x, height / 2.0));
                snapshot.scale(s.scale as f32, s.scale as f32);
                snapshot.translate(&graphene::Point::new(-anchor_x, -height / 2.0));
            }
            self.parent_snapshot(snapshot);
            if active {
                if !s.parts.is_empty() {
                    self.render_layer(snapshot, s, &s.label, &s.text, &s.parts);
                }
                if let Some(sub) = s.sub_label.as_ref().filter(|_| !s.sub_parts.is_empty()) {
                    self.render_layer(snapshot, s, sub, &s.sub_text, &s.sub_parts);
                }
            }
            if scaling {
                snapshot.restore();
            }
            if blurring {
                snapshot.pop();
            }
        }
    }

    impl LyricRow {
        /// Paint one label's text split at the sweep position.
        #[allow(deprecated)]
        fn render_layer(&self, snapshot: &gtk::Snapshot, s: &RowState, label: &gtk::Label, text: &str, parts: &[Part]) {
            let row = self.obj();
            let layout = label.layout();
            let alloc = label.allocation();
            let row_w = f64::from(row.width());
            let (base_x, base_y) = label.compute_point(&*row, &graphene::Point::new(0.0, 0.0)).map(|p| (f64::from(p.x()), f64::from(p.y()))).unwrap_or((f64::from(alloc.x()), f64::from(alloc.y())));
            let (lx, ly) = label.layout_offsets();
            let x_offset = base_x + f64::from(lx);
            let y_offset = base_y + f64::from(ly);

            let (c_in, c_act, c_glow) = css_colors(label);
            let sweep = sweep_state(s.internal_cursor_ms, parts, &layout);
            let context = label.pango_context();
            let new_layout = || {
                let l = pango::Layout::new(&context);
                l.set_text(text);
                l.set_font_description(layout.font_description().as_ref());
                l.set_width(layout.width());
                l.set_alignment(layout.alignment());
                l.set_wrap(layout.wrap());
                l
            };
            let base_layout = new_layout();

            let clip_top = f64::from(alloc.y()) - 60.0;
            let clip_bottom = f64::from(alloc.height()) + 120.0;
            let origin = graphene::Point::new(x_offset as f32, y_offset as f32);
            let draw_base = |x: f64, y: f64, w: f64, h: f64, color: &gdk::RGBA| {
                if w <= 0.0 || h <= 0.0 {
                    return;
                }
                snapshot.push_clip(&graphene::Rect::new(x as f32, y as f32, w as f32, h as f32));
                snapshot.save();
                snapshot.translate(&origin);
                snapshot.append_layout(&base_layout, color);
                snapshot.restore();
                snapshot.pop();
            };

            let (Some(active_idx), Some((rx, ry, aw, ah))) = (sweep.active, sweep.active_rect) else {
                if s.internal_cursor_ms < parts[0].start_ms as f64 {
                    draw_base(0.0, clip_top, row_w, clip_bottom - clip_top, &c_in);
                } else {
                    let sx = x_offset + sweep.sung.0;
                    let sy = y_offset + sweep.sung.1;
                    let sh = sweep.sung.2;
                    draw_base(0.0, clip_top, row_w, sy - clip_top, &c_act);
                    draw_base(0.0, sy, sx, sh, &c_act);
                    draw_base(sx, sy, row_w - sx, sh, &c_in);
                    draw_base(0.0, sy + sh, row_w, clip_bottom - (sy + sh), &c_in);
                }
                return;
            };

            let ax = x_offset + rx;
            let ay = y_offset + ry;
            let sweep_abs_x = x_offset + sweep.x;
            draw_base(0.0, clip_top, row_w, ay - clip_top, &c_act);
            draw_base(0.0, ay + ah, row_w, clip_bottom - (ay + ah), &c_in);
            draw_base(0.0, ay, ax, ah, &c_act);
            draw_base(ax + aw, ay, row_w - (ax + aw), ah, &c_in);

            let part = &parts[active_idx];
            let full_len = text.len() as u32;
            let b_start = part.byte_start.max(0) as u32;
            let b_end = if part.byte_end >= 0 { part.byte_end as u32 } else { full_len };
            let duration = ((part.end_ms - part.start_ms) as f64).max(1.0);
            let progress = (s.internal_cursor_ms - part.start_ms as f64) / duration;
            let n_chars = part.text.chars().count().max(1) as f64;

            // The whole line laid out again with everything but the active word invisible.
            let word_only = |color: &gdk::RGBA| {
                let l = new_layout();
                let attrs = pango::AttrList::new();
                let to16 = |v: f32| (f64::from(v) * 65535.0).round().clamp(0.0, 65535.0) as u16;
                let insert = |mut attr: pango::Attribute, start: u32, end: u32| {
                    attr.set_start_index(start);
                    attr.set_end_index(end);
                    attrs.insert(attr);
                };
                insert(pango::AttrColor::new_foreground(to16(color.red()), to16(color.green()), to16(color.blue())).upcast(), b_start, b_end);
                insert(pango::AttrInt::new_foreground_alpha(to16(color.alpha())).upcast(), b_start, b_end);
                if b_start > 0 {
                    insert(pango::AttrInt::new_foreground_alpha(0).upcast(), 0, b_start);
                }
                if b_end < full_len {
                    insert(pango::AttrInt::new_foreground_alpha(0).upcast(), b_end, full_len);
                }
                l.set_attributes(Some(&attrs));
                l
            };
            let layout_in = word_only(&c_in);
            let layout_act = word_only(&c_act);
            let layout_glow = (s.can_glow && sweep.glow > 0.0).then(|| word_only(&c_glow));
            let transparent = gdk::RGBA::new(0.0, 0.0, 0.0, 0.0);
            let scale = f64::from(pango::SCALE);

            // Character by character, so a long word can ride the wave. `sung`
            // picks the side of the sweep position this pass paints.
            let draw_word = |colored: &pango::Layout, sung: bool| {
                let mut cb = b_start as i32;
                for (char_idx, ch) in part.text.chars().enumerate() {
                    let char_len = ch.len_utf8() as i32;
                    let pos = layout.index_to_pos(cb);
                    cb += char_len;
                    let cx = f64::from(pos.x()) / scale;
                    let cw = f64::from(pos.width()) / scale;
                    if cw <= 0.0 {
                        continue;
                    }
                    let wave = if sweep.is_long {
                        let t = (progress * 1.5 - (char_idx as f64 / n_chars) * 0.5).clamp(0.0, 1.0);
                        (t * std::f64::consts::PI).sin() * sweep.wave_amp
                    } else {
                        0.0
                    };
                    let char_x = x_offset + cx;
                    let (clip_x, clip_w) = if sung {
                        if char_x >= sweep_abs_x {
                            continue;
                        }
                        (char_x, cw.min(sweep_abs_x - char_x))
                    } else {
                        if char_x + cw <= sweep_abs_x {
                            continue;
                        }
                        let cut = (sweep_abs_x - char_x).max(0.0);
                        (char_x + cut, cw - cut)
                    };
                    if clip_w <= 0.0 {
                        continue;
                    }
                    snapshot.push_clip(&graphene::Rect::new(clip_x as f32, clip_top as f32, clip_w as f32, (clip_bottom - clip_top) as f32));
                    snapshot.save();
                    snapshot.translate(&graphene::Point::new(x_offset as f32, (y_offset - wave) as f32));
                    snapshot.append_layout(colored, &transparent);
                    snapshot.restore();
                    snapshot.pop();
                }
            };

            draw_word(&layout_in, false);
            draw_word(&layout_act, true);
            if let Some(glow) = layout_glow {
                snapshot.save();
                snapshot.push_blur(10.0 * sweep.glow);
                draw_word(&glow, true);
                snapshot.pop();
                snapshot.push_blur(6.0 * sweep.glow);
                draw_word(&layout_in, true);
                snapshot.pop();
                snapshot.restore();
            }
        }
    }
}

glib::wrapper! {
    pub struct LyricRow(ObjectSubclass<imp::LyricRow>) @extends gtk::ListBoxRow, gtk::Widget, @implements gtk::Accessible, gtk::Actionable, gtk::Buildable, gtk::ConstraintTarget;
}

/// How a row is built, from the view's display prefs.
#[derive(Clone, Debug)]
pub struct RowOptions {
    pub second_line_mode: String,
    pub effects: Effects,
    pub sweep: bool,
    pub active_scale: f64,
}

impl LyricRow {
    pub fn new(line: &LyricLine, line_idx: usize, sweep_end_ms: Option<i64>, options: &RowOptions) -> Self {
        let row: Self = glib::Object::new();
        let imp = row.imp();
        let is_static = line.start.is_none();
        let start_ms = (line.start.unwrap_or(0.0) * 1000.0) as i64;
        imp.line_idx.set(line_idx as i32);
        imp.start_ms.set(start_ms);
        imp.is_static.set(is_static);

        let effects = options.effects;
        let sweep = options.sweep && !is_static;
        let mut parts = normalize_parts(&line.parts);
        let mut swept = false;
        if parts.is_empty() && sweep {
            parts = synthesize_parts(&line.text, start_ms, sweep_end_ms);
            swept = !parts.is_empty();
        }
        assign_byte_offsets(&line.text, &mut parts);

        let opposite_voice = line.opposite_voice();
        let (align, justify, xalign) = if opposite_voice { (gtk::Align::End, gtk::Justification::Right, 1.0) } else { (gtk::Align::Start, gtk::Justification::Left, 0.0) };
        let make_label = |class: &str| gtk::Label::builder().wrap(true).wrap_mode(pango::WrapMode::WordChar).justify(justify).halign(align).valign(gtk::Align::Center).xalign(xalign).css_classes([class]).build();

        let content = gtk::Box::new(gtk::Orientation::Vertical, 2);
        row.set_child(Some(&content));
        let label = make_label("lyrics-line-label");
        content.append(&label);

        let mode = options.second_line_mode.as_str();
        let (sub_text, sub_raw) = second_line_for(line, mode);
        let sub_text = sub_text.unwrap_or_default().to_owned();
        let has_bg = line.bg_text.as_deref().is_some_and(|t| !t.is_empty());
        let is_bg = mode == "background" || (mode == "auto" && !line.romanization.as_deref().is_some_and(|r| !r.is_empty()) && has_bg);
        let mut sub_parts = if is_bg { sub_raw.map(normalize_parts).unwrap_or_default() } else { Vec::new() };
        if is_bg && sub_parts.is_empty() && !sub_text.is_empty() && sweep {
            sub_parts = synthesize_parts(&sub_text, start_ms, sweep_end_ms);
        }
        assign_byte_offsets(&sub_text, &mut sub_parts);
        let sub_label = (!sub_text.is_empty()).then(|| {
            let sub = make_label("lyrics-line-sub");
            content.append(&sub);
            sub
        });

        row.add_css_class("lyrics-line");
        row.add_css_class(if opposite_voice { "opposite-voice" } else { "lead-voice" });
        row.set_selectable(true);
        row.set_activatable(true);
        row.set_can_focus(false);
        row.set_focusable(false);

        let timed_effect = |wanted: bool| wanted && !is_static;
        let base_lerp = if effects == Effects::Off { LERP_SPEED } else { LERP_SPEED_EFFECTS };
        let n = parts.len().max(1);
        let n_sub = sub_parts.len().max(1);
        let mut state = RowState {
            text: line.text.clone(),
            parts,
            swept,
            sub_text,
            sub_parts,
            label,
            sub_label,
            effects,
            opposite_voice,
            can_scale: timed_effect(effects != Effects::Off),
            can_glow: timed_effect(effects == Effects::Full),
            can_blur: timed_effect(effects == Effects::Full),
            active_scale: options.active_scale,
            lerp: base_lerp * 0.6,
            cursor_ms: -1,
            internal_cursor_ms: -1.0,
            wants_turn_off: false,
            scale: 1.0,
            scale_target: 1.0,
            blur: 0.0,
            blur_target: 0.0,
            distance: 99,
            word_alphas: vec![0.0; n],
            word_targets: vec![0.0; n],
            sub_alphas: vec![0.0; n_sub],
            sub_targets: vec![0.0; n_sub],
            dirty: true,
            last_tick: None,
            last_colors: None,
        };
        state.recompute_targets();
        imp.state.replace(Some(state));
        row.add_tick_callback(|row, _| {
            row.on_tick();
            glib::ControlFlow::Continue
        });
        row
    }

    pub fn line_idx(&self) -> i32 {
        self.imp().line_idx.get()
    }

    pub fn start_ms(&self) -> i64 {
        self.imp().start_ms.get()
    }

    pub fn is_static(&self) -> bool {
        self.imp().is_static.get()
    }

    pub fn set_paused(&self, paused: bool) {
        self.imp().paused.set(paused);
    }

    pub fn reset_state(&self) {
        self.imp().paused.set(false);
        if let Some(s) = self.imp().state.borrow_mut().as_mut() {
            s.cursor_ms = -1;
            s.internal_cursor_ms = -1.0;
            s.wants_turn_off = false;
            s.scale = 1.0;
            s.scale_target = 1.0;
            s.blur = 0.0;
            s.blur_target = 0.0;
            s.distance = 99;
            for list in [&mut s.word_alphas, &mut s.word_targets, &mut s.sub_alphas, &mut s.sub_targets] {
                list.fill(0.0);
            }
            s.label.set_opacity(1.0);
            if let Some(sub) = &s.sub_label {
                sub.set_opacity(1.0);
            }
            s.last_tick = Some(now_ms());
            s.dirty = false;
            s.render_markup();
        }
        self.queue_draw();
    }

    /// The play-head inside this line in ms, or -1 when the line is not the active one.
    pub fn set_cursor_ms(&self, ms: i64) {
        let mut guard = self.imp().state.borrow_mut();
        let Some(s) = guard.as_mut() else { return };
        if ms == s.cursor_ms && ms != -1 {
            return;
        }
        if ms == -1 {
            // Let the sweep run to the end of the line before it fades.
            s.wants_turn_off = true;
        } else {
            s.wants_turn_off = false;
            if s.cursor_ms < 0 || (s.cursor_ms - ms).abs() > 1000 {
                s.internal_cursor_ms = ms as f64;
            }
            s.cursor_ms = ms;
        }
        s.recompute_effect_targets();
        s.recompute_targets();
    }

    pub fn set_distance(&self, distance: i32) {
        if let Some(s) = self.imp().state.borrow_mut().as_mut() {
            if s.distance != distance {
                s.distance = distance;
                s.recompute_effect_targets();
            }
        }
    }

    fn on_tick(&self) {
        let paused = self.imp().paused.get();
        let start_ms = self.start_ms() as f64;
        let mut guard = self.imp().state.borrow_mut();
        let Some(s) = guard.as_mut() else { return };
        let now = now_ms();
        let delta = now - s.last_tick.unwrap_or(now);
        s.last_tick = Some(now);
        let mut changed = false;

        // A theme or accent change moves the colors the markup was baked with.
        let (c_in, c_act, _) = css_colors(&s.label);
        let key = [c_in.red(), c_in.green(), c_in.blue(), c_act.red(), c_act.green(), c_act.blue()].map(|v| (v * 100.0).round() as i32);
        if s.last_colors != Some(key) {
            s.last_colors = Some(key);
            s.dirty = true;
            changed = true;
        }

        if s.wants_turn_off {
            if !paused {
                let end_bound = s.end_bound();
                if delta > 250.0 {
                    s.internal_cursor_ms = end_bound + 1.0;
                }
                if s.internal_cursor_ms > end_bound || s.internal_cursor_ms < start_ms {
                    s.cursor_ms = -1;
                    s.internal_cursor_ms = -1.0;
                    s.wants_turn_off = false;
                    s.recompute_effect_targets();
                    s.recompute_targets();
                } else if delta < 100.0 {
                    s.internal_cursor_ms += delta;
                }
                changed = true;
            }
        } else if s.cursor_ms >= 0 && !paused {
            // The player ticks a few times a second. The frame clock fills in between.
            if delta > 250.0 {
                s.internal_cursor_ms = s.cursor_ms as f64;
            } else if s.internal_cursor_ms < s.cursor_ms as f64 {
                s.internal_cursor_ms += delta;
            }
            changed = true;
        }

        if s.step_alphas() || s.dirty {
            s.dirty = false;
            s.render_markup();
        }
        let ease = EFFECT_LERP * 0.6;
        if (s.scale - s.scale_target).abs() > 0.001 {
            s.scale += (s.scale_target - s.scale) * ease;
            changed = true;
        }
        if (s.blur - s.blur_target).abs() > 0.001 {
            s.blur += (s.blur_target - s.blur) * ease;
            changed = true;
        }
        let active = s.cursor_ms >= 0;
        drop(guard);
        if changed || active {
            self.queue_draw();
        }
    }
}

// -- InterludeRow ---------------------------------------------------------------

struct InterludeState {
    dots: gtk::Box,
    effects: Effects,
    lerp: f64,
    alphas: [f64; INTERLUDE_DOTS],
    targets: [f64; INTERLUDE_DOTS],
    swell: [f64; INTERLUDE_DOTS],
    cursor_ms: i64,
    t0: std::time::Instant,
}

mod interlude_imp {
    use super::*;

    #[derive(Default)]
    pub struct InterludeRow {
        pub(super) state: RefCell<Option<InterludeState>>,
        pub start_ms: Cell<i64>,
        pub end_ms: Cell<i64>,
    }

    #[glib::object_subclass]
    impl ObjectSubclass for InterludeRow {
        const NAME: &'static str = "MixtapesLyricInterludeRow";
        type Type = super::InterludeRow;
        type ParentType = gtk::ListBoxRow;
    }

    impl ObjectImpl for InterludeRow {}
    impl ListBoxRowImpl for InterludeRow {}

    impl WidgetImpl for InterludeRow {
        #[allow(deprecated)]
        fn snapshot(&self, snapshot: &gtk::Snapshot) {
            self.parent_snapshot(snapshot);
            let guard = self.state.borrow();
            let Some(s) = guard.as_ref() else { return };
            let alloc = s.dots.allocation();
            if alloc.width() <= 0 {
                return;
            }
            let color = s.dots.color();
            let cy = f64::from(alloc.y()) + f64::from(alloc.height()) / 2.0;
            for i in 0..INTERLUDE_DOTS {
                let radius = DOT_RADIUS * (1.0 + s.swell[i]);
                let cx = f64::from(alloc.x()) + DOT_RADIUS + i as f64 * DOT_SPACING;
                let dot = gdk::RGBA::new(color.red(), color.green(), color.blue(), color.alpha() * s.alphas[i].clamp(0.0, 1.0) as f32);
                let rect = graphene::Rect::new((cx - radius) as f32, (cy - radius) as f32, (radius * 2.0) as f32, (radius * 2.0) as f32);
                snapshot.push_rounded_clip(&gsk::RoundedRect::from_rect(rect, radius as f32));
                snapshot.append_color(&dot, &rect);
                snapshot.pop();
            }
        }
    }
}

glib::wrapper! {
    /// The music-only marker for an instrumental stretch: three dots that fill
    /// in turn across the gap, so the row doubles as a countdown to the next line.
    pub struct InterludeRow(ObjectSubclass<interlude_imp::InterludeRow>) @extends gtk::ListBoxRow, gtk::Widget, @implements gtk::Accessible, gtk::Actionable, gtk::Buildable, gtk::ConstraintTarget;
}

/// How far through its own slice of the gap dot `i` is.
fn dot_fill(progress: f64, i: usize) -> f64 {
    ((progress - i as f64 / INTERLUDE_DOTS as f64) * INTERLUDE_DOTS as f64).clamp(0.0, 1.0)
}

impl InterludeRow {
    pub fn new(start_s: f64, end_s: f64, effects: Effects) -> Self {
        let row: Self = glib::Object::new();
        let imp = row.imp();
        imp.start_ms.set((start_s * 1000.0) as i64);
        imp.end_ms.set((end_s * 1000.0) as i64);

        // An empty box claims the row's space. The dots are painted over it.
        let dots = gtk::Box::builder().halign(gtk::Align::Start).valign(gtk::Align::Center).css_classes(["lyrics-interlude"]).build();
        dots.set_size_request((DOT_SPACING * (INTERLUDE_DOTS - 1) as f64 + DOT_RADIUS * 2.0) as i32, (DOT_RADIUS * 2.0 * DOT_MAX_SWELL) as i32);
        row.set_child(Some(&dots));
        row.add_css_class("lyrics-line");
        row.set_selectable(true);
        row.set_activatable(true);
        row.set_can_focus(false);
        row.set_focusable(false);

        imp.state.replace(Some(InterludeState {
            dots,
            effects,
            lerp: if effects == Effects::Off { LERP_SPEED } else { LERP_SPEED_EFFECTS },
            alphas: [ALPHA_FUTURE_WORD; INTERLUDE_DOTS],
            targets: [ALPHA_FUTURE_WORD; INTERLUDE_DOTS],
            swell: [0.0; INTERLUDE_DOTS],
            cursor_ms: -1,
            t0: std::time::Instant::now(),
        }));
        row.add_tick_callback(|row, _| {
            row.on_tick();
            glib::ControlFlow::Continue
        });
        row
    }

    pub fn start_ms(&self) -> i64 {
        self.imp().start_ms.get()
    }

    pub fn end_ms(&self) -> i64 {
        self.imp().end_ms.get()
    }

    pub fn reset_state(&self) {
        if let Some(s) = self.imp().state.borrow_mut().as_mut() {
            s.cursor_ms = -1;
            s.alphas = [ALPHA_FUTURE_WORD; INTERLUDE_DOTS];
            s.targets = [ALPHA_FUTURE_WORD; INTERLUDE_DOTS];
            s.swell = [0.0; INTERLUDE_DOTS];
            s.t0 = std::time::Instant::now();
        }
        self.queue_draw();
    }

    fn progress(&self, cursor_ms: i64) -> Option<f64> {
        if cursor_ms < 0 {
            return None;
        }
        let span = (self.end_ms() - self.start_ms()).max(1) as f64;
        Some(((cursor_ms - self.start_ms()) as f64 / span).clamp(0.0, 1.0))
    }

    pub fn set_cursor_ms(&self, ms: i64) {
        let progress = self.progress(ms);
        if let Some(s) = self.imp().state.borrow_mut().as_mut() {
            if s.cursor_ms == ms {
                return;
            }
            s.cursor_ms = ms;
            for (i, target) in s.targets.iter_mut().enumerate() {
                *target = progress.map_or(ALPHA_FUTURE_WORD, |p| ALPHA_FUTURE_WORD + (ALPHA_ACTIVE - ALPHA_FUTURE_WORD) * dot_fill(p, i));
            }
        }
    }

    fn on_tick(&self) {
        let mut guard = self.imp().state.borrow_mut();
        let Some(s) = guard.as_mut() else { return };
        let mut changed = false;
        for i in 0..INTERLUDE_DOTS {
            if (s.alphas[i] - s.targets[i]).abs() > 0.002 {
                s.alphas[i] += (s.targets[i] - s.alphas[i]) * s.lerp;
                changed = true;
            }
        }
        match self.progress(s.cursor_ms).filter(|_| s.effects != Effects::Off) {
            // A dot swells as its slice fills, over a slow wave that keeps a long break moving.
            Some(progress) => {
                let elapsed = s.t0.elapsed().as_secs_f64();
                for i in 0..INTERLUDE_DOTS {
                    let filled = dot_fill(progress, i);
                    let focus = if filled > 0.0 && filled < 1.0 { 1.0 - (2.0 * filled - 1.0).abs() } else { 0.0 };
                    let wave = (elapsed * (2.0 * std::f64::consts::PI / DOT_WAVE_PERIOD) - i as f64 * 0.8).sin();
                    s.swell[i] = DOT_FOCUS_SWELL * focus + DOT_WAVE_SWELL * wave;
                }
                changed = true;
            }
            None if s.swell.iter().any(|v| *v != 0.0) => {
                s.swell = [0.0; INTERLUDE_DOTS];
                changed = true;
            }
            None => {}
        }
        drop(guard);
        if changed {
            self.queue_draw();
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn line(start: Option<f64>, text: &str) -> LyricLine {
        LyricLine::new(start, text)
    }

    #[test]
    fn a_sweep_splits_words_and_cjk_characters() {
        assert_eq!(sweep_tokens("hello big world"), vec![("hello".to_owned(), true), ("big".to_owned(), true), ("world".to_owned(), false)]);
        let tokens = sweep_tokens("夜に ok");
        assert_eq!(tokens, vec![("夜".to_owned(), false), ("に".to_owned(), true), ("ok".to_owned(), false)]);
    }

    #[test]
    fn synthetic_timing_divides_the_span_by_length() {
        let parts = synthesize_parts("aa bbbb", 1000, Some(4000));
        assert_eq!(parts.len(), 2);
        assert_eq!((parts[0].start_ms, parts[0].end_ms), (1000, 2000));
        assert_eq!((parts[1].start_ms, parts[1].end_ms), (2000, 4000));
        assert!(parts[0].space_after && !parts[1].space_after);
    }

    #[test]
    fn nothing_to_sweep_means_no_parts() {
        assert!(synthesize_parts("one", 0, Some(5000)).is_empty(), "a single token has nothing to advance across");
        assert!(synthesize_parts("a b", 0, Some(300)).is_empty(), "too short to read");
        assert!(synthesize_parts("a b", 0, None).is_empty());
        let capped = synthesize_parts("a b", 0, Some(60_000));
        assert_eq!(capped.last().unwrap().end_ms, 12_000);
    }

    #[test]
    fn byte_offsets_follow_repeated_and_multibyte_words() {
        let mut parts = normalize_parts(&[
            LyricPart { start: Some(0.0), end: Some(1.0), text: "la".into(), space_after: true },
            LyricPart { start: Some(1.0), end: None, text: "la".into(), space_after: true },
            LyricPart { start: None, end: None, text: "skipped".into(), space_after: true },
            LyricPart { start: Some(2.0), end: Some(3.0), text: "夜".into(), space_after: false },
        ]);
        assert_eq!(parts.len(), 3);
        assert_eq!(parts[1].end_ms, 1000, "a missing end falls back to the start");
        assign_byte_offsets("la la 夜", &mut parts);
        assert_eq!((parts[0].byte_start, parts[0].byte_end), (0, 2));
        assert_eq!((parts[1].byte_start, parts[1].byte_end), (3, 5));
        assert_eq!((parts[2].byte_start, parts[2].byte_end), (6, 9));
    }

    #[test]
    fn interludes_come_from_line_ends_marker_lines_and_the_lead_in() {
        let mut a = line(Some(10.0), "first");
        a.end = Some(12.0);
        let lines = vec![a, line(Some(20.0), "second"), line(Some(22.0), ""), line(Some(30.0), "third"), line(Some(32.0), "fourth")];
        assert_eq!(find_interludes(&lines), vec![(0.0, 9.75), (12.0, 19.75), (22.0, 29.75)]);
        assert!(find_interludes(&[line(None, "plain")]).is_empty());
    }

    #[test]
    fn the_second_line_follows_the_mode() {
        let mut l = line(Some(0.0), "夜に駆ける");
        l.romanization = Some("yoru ni kakeru".into());
        l.translation = Some("racing into the night".into());
        l.bg_text = Some("ooh".into());
        assert_eq!(second_line_for(&l, "off").0, None);
        assert_eq!(second_line_for(&l, "auto").0, Some("yoru ni kakeru"));
        assert_eq!(second_line_for(&l, "translation").0, Some("racing into the night"));
        assert!(second_line_for(&l, "background").1.is_some());
        l.text = "latin text".into();
        assert_eq!(second_line_for(&l, "auto").0, Some("ooh"), "a romanization of latin text adds nothing");
    }

    #[test]
    fn a_word_ramps_over_part_of_its_own_length() {
        let part = Part { start_ms: 1000, end_ms: 2000, text: "word".into(), space_after: true, byte_start: 0, byte_end: 4 };
        assert_eq!(word_alpha_for(500, &part, Effects::Subtle, false), 0.0);
        assert_eq!(word_alpha_for(1000, &part, Effects::Off, false), 1.0);
        // Held 1000 ms: the ramp is 420 ms (the cap), so 210 ms in is half.
        assert!((word_alpha_for(1210, &part, Effects::Subtle, false) - 0.5).abs() < 1e-9);
        // Swept: 300 ms ramp.
        assert!((word_alpha_for(1150, &part, Effects::Subtle, true) - 0.5).abs() < 1e-9);
    }
}

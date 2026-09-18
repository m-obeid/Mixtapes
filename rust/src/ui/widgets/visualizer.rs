//! Port of ui/widgets/visualizer.py: CAVA-style bars fed by the spectrum
//! frames the audio thread posts. Log-scaled bins, treble boost, contrast
//! gamma, auto-sensitivity, Monstercat smoothing, gravity fall-off.

use std::cell::{Cell, RefCell};
use std::rc::Rc;

use gtk::{glib, prelude::*};

use crate::player::Player;
use crate::ui::context::UiContext;

const THRESHOLD_DB: f32 = -80.0;
const GRAVITY: f64 = 0.0028;
const MONSTERCAT_DEFAULT: f64 = 1.8;
const BARS_DEFAULT: usize = 56;
const UPPER_FRACTION: f64 = 0.6;
const HIGH_FREQ_BOOST: f64 = 1.8;
const CONTRAST_GAMMA: f64 = 2.5;
const AUTO_GAIN_DECAY: f64 = 0.997;
const AUTO_GAIN_TARGET: f64 = 0.92;
const AUTO_GAIN_MAX: f64 = 3.5;
const AUTO_GAIN_FLOOR: f64 = 0.05;
const IDLE_ALPHA: f64 = 0.08;
const ACTIVE_ALPHA_MIN: f64 = 0.15;
const ACTIVE_ALPHA_MAX: f64 = 0.6;

pub struct Visualizer {
    area: gtk::DrawingArea,
    player: Rc<Player>,
    bars: Cell<usize>,
    smoothing: Cell<f64>,
    levels: RefCell<Vec<f64>>,
    velocities: RefCell<Vec<f64>>,
    bins: RefCell<Vec<(usize, usize)>>,
    weights: RefCell<Vec<f64>>,
    raw_bands: Cell<usize>,
    recent_max: Cell<f64>,
    active: Cell<bool>,
    tick: RefCell<Option<gtk::TickCallbackId>>,
}

impl Visualizer {
    pub fn new(ctx: &Rc<UiContext>, height: i32) -> Rc<Self> {
        let prefs = ctx.paths.read_prefs();
        let bars = prefs.get("visualizer_bars").and_then(|v| v.as_u64()).map(|n| n.clamp(8, 100) as usize).unwrap_or(BARS_DEFAULT);
        let smoothing = prefs.get("visualizer_smoothing").and_then(|v| v.as_f64()).unwrap_or(MONSTERCAT_DEFAULT).max(1.05);
        let enabled = prefs.get("visualizer_enabled").and_then(|v| v.as_bool()).unwrap_or(true);

        let area = gtk::DrawingArea::builder().content_height(height).visible(enabled).build();
        let this = Rc::new(Self {
            area,
            player: ctx.player.clone(),
            bars: Cell::new(bars),
            smoothing: Cell::new(smoothing),
            levels: RefCell::new(Vec::new()),
            velocities: RefCell::new(Vec::new()),
            bins: RefCell::new(Vec::new()),
            weights: RefCell::new(Vec::new()),
            raw_bands: Cell::new(0),
            recent_max: Cell::new(0.0),
            active: Cell::new(false),
            tick: RefCell::new(None),
        });
        let weak = Rc::downgrade(&this);
        this.area.set_draw_func(move |area, cr, w, h| {
            if let Some(v) = weak.upgrade() {
                v.draw(area, cr, w, h);
            }
        });
        let weak = Rc::downgrade(&this);
        this.area.connect_map(move |_| {
            if let Some(v) = weak.upgrade() {
                v.sync_tick();
            }
        });
        let weak = Rc::downgrade(&this);
        this.area.connect_unmap(move |_| {
            if let Some(v) = weak.upgrade() {
                v.stop_tick();
            }
        });
        this
    }

    pub fn widget(&self) -> &gtk::DrawingArea {
        &self.area
    }

    /// Port of set_bar_count. The bins are rebuilt on the next frame.
    pub fn set_bar_count(&self, n: usize) {
        let n = n.clamp(8, 100);
        if n == self.bars.get() {
            return;
        }
        self.bars.set(n);
        self.levels.borrow_mut().clear();
        self.velocities.borrow_mut().clear();
        self.area.queue_draw();
    }

    pub fn set_smoothing(&self, intensity: f64) {
        self.smoothing.set(intensity.max(1.05));
    }

    /// Bars animate only while playing.
    pub fn set_active(self: &Rc<Self>, active: bool) {
        self.active.set(active);
        self.sync_tick();
    }

    fn sync_tick(self: &Rc<Self>) {
        if !self.active.get() || !self.area.is_mapped() {
            self.stop_tick();
            return;
        }
        if self.tick.borrow().is_some() {
            return;
        }
        let weak = Rc::downgrade(self);
        let id = self.area.add_tick_callback(move |_, _| match weak.upgrade() {
            Some(v) => {
                v.on_tick();
                glib::ControlFlow::Continue
            }
            None => glib::ControlFlow::Break,
        });
        self.tick.replace(Some(id));
    }

    fn stop_tick(&self) {
        if let Some(id) = self.tick.borrow_mut().take() {
            id.remove();
        }
    }

    fn recompute_bins(&self, raw_n: usize) {
        let n_display = self.bars.get();
        let lo_min = 1usize;
        let hi_max = (lo_min + n_display).max((raw_n as f64 * UPPER_FRACTION) as usize);
        let ratio = hi_max as f64 / lo_min as f64;
        let mut bins = Vec::with_capacity(n_display);
        let mut weights = Vec::with_capacity(n_display);
        let mut prev = lo_min;
        for i in 0..n_display {
            let mut hi = (lo_min as f64 * ratio.powf((i + 1) as f64 / n_display as f64)) as usize;
            hi = hi.max(prev + 1).min(hi_max);
            bins.push((prev, hi));
            weights.push(1.0 + HIGH_FREQ_BOOST * (i as f64 / (n_display.max(2) - 1) as f64));
            prev = hi;
        }
        self.bins.replace(bins);
        self.weights.replace(weights);
        self.raw_bands.set(raw_n);
    }

    fn reduce(&self, raw: &[f32]) -> Vec<f64> {
        if raw.len() != self.raw_bands.get() || self.bins.borrow().len() != self.bars.get() {
            self.recompute_bins(raw.len());
        }
        let bins = self.bins.borrow();
        let weights = self.weights.borrow();
        bins.iter()
            .enumerate()
            .map(|(idx, &(lo, hi))| {
                if hi <= lo || lo >= raw.len() {
                    return 0.0;
                }
                let chunk = &raw[lo..hi.min(raw.len())];
                let peak = chunk.iter().copied().fold(f32::MIN, f32::max);
                if peak <= THRESHOLD_DB {
                    return 0.0;
                }
                let norm = ((peak - THRESHOLD_DB) / -THRESHOLD_DB) as f64;
                (norm.powf(CONTRAST_GAMMA) * weights[idx]).max(0.0)
            })
            .collect()
    }

    fn smooth(&self, bars: &[f64]) -> Vec<f64> {
        let n = bars.len();
        let intensity = self.smoothing.get();
        let mut out = bars.to_vec();
        for i in 0..n {
            let peak = bars[i];
            if peak < 0.01 {
                continue;
            }
            let mut damped = peak;
            for k in 1..n {
                damped /= intensity;
                if damped < 0.01 {
                    break;
                }
                if i >= k && damped > out[i - k] {
                    out[i - k] = damped;
                }
                if i + k < n && damped > out[i + k] {
                    out[i + k] = damped;
                }
            }
        }
        out
    }

    fn ingest(&self, magnitudes: &[f32]) {
        if magnitudes.is_empty() {
            return;
        }
        let mut bars = self.reduce(magnitudes);
        let frame_peak = bars.iter().copied().fold(0.0, f64::max);
        let recent = frame_peak.max(self.recent_max.get() * AUTO_GAIN_DECAY);
        self.recent_max.set(recent);
        if recent > AUTO_GAIN_FLOOR {
            let gain = (AUTO_GAIN_TARGET / recent).min(AUTO_GAIN_MAX);
            for b in &mut bars {
                *b = (*b * gain).min(1.0);
            }
        } else {
            for b in &mut bars {
                *b = b.min(1.0);
            }
        }
        let bars = self.smooth(&bars);
        let mut levels = self.levels.borrow_mut();
        let mut velocities = self.velocities.borrow_mut();
        if levels.len() != bars.len() {
            *levels = vec![0.0; bars.len()];
            *velocities = vec![0.0; bars.len()];
        }
        for (i, h) in bars.iter().enumerate() {
            if *h > levels[i] {
                levels[i] = *h;
                velocities[i] = 0.0;
            }
        }
    }

    fn on_tick(&self) {
        if let Some(bands) = self.player.pull_visualizer_bands() {
            self.ingest(&bands);
        }
        {
            let mut levels = self.levels.borrow_mut();
            let mut velocities = self.velocities.borrow_mut();
            for i in 0..levels.len() {
                if levels[i] > 0.0 {
                    velocities[i] += GRAVITY;
                    levels[i] -= velocities[i];
                    if levels[i] <= 0.0 {
                        levels[i] = 0.0;
                        velocities[i] = 0.0;
                    }
                }
            }
        }
        self.area.queue_draw();
    }

    fn draw(&self, _area: &gtk::DrawingArea, cr: &gtk::cairo::Context, width: i32, height: i32) {
        if width <= 0 || height <= 0 {
            return;
        }
        let n = self.bars.get();
        let levels = self.levels.borrow();
        let (r, g, b) = bar_color(_area);
        let gap = 2.0;
        let bar_w = ((width as f64 - gap * (n as f64 - 1.0)) / n as f64).max(1.0);
        let min_h = 3.0;
        for i in 0..n {
            let level = levels.get(i).copied().unwrap_or(0.0);
            let h = (level * height as f64).max(min_h);
            let x = i as f64 * (bar_w + gap);
            let y = height as f64 - h;
            let alpha = if level > 0.0 { ACTIVE_ALPHA_MIN + (ACTIVE_ALPHA_MAX - ACTIVE_ALPHA_MIN) * level.min(1.0).sqrt() } else { IDLE_ALPHA };
            cr.set_source_rgba(r, g, b, alpha);
            rounded_rect(cr, x, y, bar_w, h, (bar_w / 2.0).min(3.0));
            let _ = cr.fill();
        }
    }
}

fn rounded_rect(cr: &gtk::cairo::Context, x: f64, y: f64, w: f64, h: f64, radius: f64) {
    let radius = radius.min(w / 2.0).min(h / 2.0).max(0.0);
    if radius <= 0.5 {
        cr.rectangle(x, y, w, h);
        return;
    }
    use std::f64::consts::PI;
    cr.new_sub_path();
    cr.arc(x + w - radius, y + radius, radius, -PI / 2.0, 0.0);
    cr.arc(x + w - radius, y + h - radius, radius, 0.0, PI / 2.0);
    cr.arc(x + radius, y + h - radius, radius, PI / 2.0, PI);
    cr.arc(x + radius, y + radius, radius, PI, 3.0 * PI / 2.0);
    cr.close_path();
}

/// @visualizer_bar when the window has derived one, else the plain accent.
/// The derived value keeps the tallest bar clear of the labels drawn over it.
#[allow(deprecated)]
fn bar_color(area: &gtk::DrawingArea) -> (f64, f64, f64) {
    let ctx = area.style_context();
    ["visualizer_bar", "accent_color"]
        .iter()
        .find_map(|name| ctx.lookup_color(name))
        .map(|c| (f64::from(c.red()), f64::from(c.green()), f64::from(c.blue())))
        .unwrap_or((0.42, 0.34, 0.85))
}

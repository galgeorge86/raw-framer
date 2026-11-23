//! Viewer assists: composition guides, focus peaking, false color and histogram.
//!
//! All assists are viewer-only: they never change decoded pixels, thumbnails or exports.
//! Guides and the histogram are egui overlays; peaking and false color run in viewer.wgsl.

use crate::theme::{self, *};
use crate::widgets::{self, *};
use egui::{Align2, Color32, CornerRadius, Pos2, Rect, RichText, Shape, Stroke, Ui, Vec2};
use rf_color::{CubeLut, ViewSettings};
use rf_core::{DecodedFrame, Pixels};
use serde::{Deserialize, Serialize};
use std::f32::consts::{FRAC_PI_2, PI};

// ------------------------------------------------------------------ model

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub enum AssistKind {
    Guides,
    Peaking,
    FalseColor,
    Histogram,
}

impl AssistKind {
    pub const ALL: [AssistKind; 4] = [Self::Guides, Self::Peaking, Self::FalseColor, Self::Histogram];
    pub fn icon(self) -> &'static str {
        match self {
            Self::Guides => "\u{EC8C}",     // grid-nine
            Self::Peaking => "\u{EBB6}",    // scan
            Self::FalseColor => "\u{E598}", // rainbow
            Self::Histogram => "\u{E150}",  // chart-bar
        }
    }
    pub fn label(self) -> &'static str {
        match self {
            Self::Guides => "Guides",
            Self::Peaking => "Focus peaking",
            Self::FalseColor => "False color",
            Self::Histogram => "Histogram",
        }
    }
    pub fn short(self) -> &'static str {
        match self {
            Self::Guides => "Guides",
            Self::Peaking => "Peaking",
            Self::FalseColor => "False color",
            Self::Histogram => "Histogram",
        }
    }
    pub fn shortcut(self) -> &'static str {
        match self {
            Self::Guides => "Alt+G",
            Self::Peaking => "Alt+P",
            Self::FalseColor => "Alt+C",
            Self::Histogram => "Alt+H",
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Serialize, Deserialize, Default)]
pub enum FrameAspect {
    #[default]
    None,
    A239,
    A235,
    A200,
    A185,
    A169,
    A43,
    A32,
    A54,
    A45,
    A11,
    A916,
    Custom,
}

impl FrameAspect {
    pub const ALL: [FrameAspect; 13] = [
        Self::None,
        Self::A239,
        Self::A235,
        Self::A200,
        Self::A185,
        Self::A169,
        Self::A43,
        Self::A32,
        Self::A54,
        Self::A45,
        Self::A11,
        Self::A916,
        Self::Custom,
    ];
    pub fn label(self) -> &'static str {
        match self {
            Self::None => "Full sensor",
            Self::A239 => "2.39:1",
            Self::A235 => "2.35:1",
            Self::A200 => "2.00:1",
            Self::A185 => "1.85:1",
            Self::A169 => "16:9",
            Self::A43 => "4:3",
            Self::A32 => "3:2",
            Self::A54 => "5:4",
            Self::A45 => "4:5 portrait",
            Self::A11 => "1:1",
            Self::A916 => "9:16 vertical",
            Self::Custom => "Custom",
        }
    }
    fn ratio(self, g: &Guides) -> Option<f32> {
        Some(match self {
            Self::None => return None,
            Self::A239 => 2.39,
            Self::A235 => 2.35,
            Self::A200 => 2.0,
            Self::A185 => 1.85,
            Self::A169 => 16.0 / 9.0,
            Self::A43 => 4.0 / 3.0,
            Self::A32 => 1.5,
            Self::A54 => 1.25,
            Self::A45 => 0.8,
            Self::A11 => 1.0,
            Self::A916 => 9.0 / 16.0,
            Self::Custom => (g.custom_w / g.custom_h.max(0.01)).max(0.05),
        })
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Serialize, Deserialize)]
#[serde(default)]
pub struct Guides {
    pub frame: FrameAspect,
    pub custom_w: f32,
    pub custom_h: f32,
    /// Darkening outside the frame guide, 0..1.
    pub mask: f32,
    pub safe: bool,
    pub safe_pct: f32,
    pub thirds: bool,
    pub golden: bool,
    pub spiral: bool,
    /// 0 = top-left, 1 = top-right, 2 = bottom-left, 3 = bottom-right (where the spiral winds in).
    pub spiral_corner: u8,
    pub diagonals: bool,
    pub triangles: bool,
    pub armature: bool,
    pub cross: bool,
    pub dot: bool,
    pub grid: bool,
    pub grid_cols: u8,
    pub grid_rows: u8,
    pub color: [u8; 3],
    pub opacity: f32,
    pub thickness: f32,
}

impl Default for Guides {
    fn default() -> Self {
        Self {
            frame: FrameAspect::None,
            custom_w: 2.0,
            custom_h: 1.0,
            mask: 0.5,
            safe: false,
            safe_pct: 90.0,
            thirds: true,
            golden: false,
            spiral: false,
            spiral_corner: 3,
            diagonals: false,
            triangles: false,
            armature: false,
            cross: false,
            dot: false,
            grid: false,
            grid_cols: 4,
            grid_rows: 4,
            color: [0xd6, 0xd6, 0xd4],
            opacity: 0.6,
            thickness: 1.0,
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, Default)]
pub enum PeakMode {
    /// Colored lines over in-focus edges (camera "colored lines").
    #[default]
    Lines,
    /// In-focus areas sharpened on screen (camera "peaking").
    Peaking,
}

#[derive(Debug, Clone, Copy, PartialEq, Serialize, Deserialize)]
#[serde(default)]
pub struct Peaking {
    pub mode: PeakMode,
    pub color: [u8; 3],
    /// Sensitivity 0..1 (higher shows more edges).
    pub level: f32,
    /// Dim the image under the lines so the color reads clearly.
    pub dim: bool,
}

impl Default for Peaking {
    fn default() -> Self {
        Self { mode: PeakMode::Lines, color: [0xff, 0x24, 0x18], level: 0.5, dim: false }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, Default)]
pub enum FcMeasure {
    /// What you see (after CST / LUT).
    #[default]
    Display,
    /// The camera log signal (Blackmagic Film Gen 5).
    Log,
}

#[derive(Debug, Clone, Copy, PartialEq, Serialize, Deserialize)]
#[serde(default)]
pub struct FalseColor {
    pub measure: FcMeasure,
    pub legend: bool,
}

impl Default for FalseColor {
    fn default() -> Self {
        Self { measure: FcMeasure::Display, legend: true }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, Default)]
pub enum HistMode {
    Luma,
    #[default]
    Rgb,
    Parade,
}

#[derive(Debug, Clone, Copy, PartialEq, Serialize, Deserialize, Default)]
#[serde(default)]
pub struct HistogramCfg {
    pub mode: HistMode,
}

#[derive(Debug, Clone, Copy, PartialEq, Serialize, Deserialize, Default)]
#[serde(default)]
pub struct Assists {
    pub guides_on: bool,
    pub peaking_on: bool,
    pub false_color_on: bool,
    pub histogram_on: bool,
    pub guides: Guides,
    pub peaking: Peaking,
    pub false_color: FalseColor,
    pub histogram: HistogramCfg,
}

impl Assists {
    pub fn is_on(&self, k: AssistKind) -> bool {
        match k {
            AssistKind::Guides => self.guides_on,
            AssistKind::Peaking => self.peaking_on,
            AssistKind::FalseColor => self.false_color_on,
            AssistKind::Histogram => self.histogram_on,
        }
    }
    pub fn toggle(&mut self, k: AssistKind) {
        let v = match k {
            AssistKind::Guides => &mut self.guides_on,
            AssistKind::Peaking => &mut self.peaking_on,
            AssistKind::FalseColor => &mut self.false_color_on,
            AssistKind::Histogram => &mut self.histogram_on,
        };
        *v = !*v;
    }
    pub fn all_off(&mut self) {
        self.guides_on = false;
        self.peaking_on = false;
        self.false_color_on = false;
        self.histogram_on = false;
    }
    pub fn active_labels(&self) -> Vec<&'static str> {
        AssistKind::ALL.iter().filter(|k| self.is_on(**k)).map(|k| k.short()).collect()
    }

    /// Shader parameters for peaking / false color.
    pub fn gpu(&self) -> AssistGpu {
        let c = self.peaking.color;
        // Level -> edge threshold on log-encoded luma (Sobel magnitude at native pixel spacing).
        // 0 % -> 0.20, 50 % -> 0.12, 100 % -> 0.04 (edges are thinned in the shader, so a low
        // threshold marks detail without blobs).
        let threshold = 0.20 - 0.16 * self.peaking.level.clamp(0.0, 1.0);
        AssistGpu {
            peak: [c[0] as f32 / 255.0, c[1] as f32 / 255.0, c[2] as f32 / 255.0, threshold],
            modes: [
                if !self.peaking_on {
                    0
                } else if self.peaking.mode == PeakMode::Lines {
                    1
                } else {
                    2
                },
                if !self.false_color_on {
                    0
                } else if self.false_color.measure == FcMeasure::Display {
                    1
                } else {
                    2
                },
                (self.peaking_on && self.peaking.dim) as u32,
                0,
            ],
        }
    }
}

/// Assist state as the shader sees it.
#[derive(Debug, Clone, Copy, Default, PartialEq)]
pub struct AssistGpu {
    /// rgb + edge threshold
    pub peak: [f32; 4],
    /// peaking mode (0 off, 1 lines, 2 sharpen), false color (0 off, 1 display, 2 log), dim, -
    pub modes: [u32; 4],
}

// ------------------------------------------------------------------ false color scale

/// Blackmagic camera false color bands (IRE %), measured from the Pocket Cinema Camera
/// 4K/6K manual's chart: BDL purple, NBDL blue, 18% middle grey green, MG+1 pink,
/// 80% near-white-clipping yellow, 95% white-clipping red. Grey elsewhere.
pub const FALSE_COLOR_BANDS: [(f32, f32, [u8; 3], &str); 6] = [
    (0.0, 6.0, [119, 22, 119], "BDL"),
    (8.0, 15.0, [31, 2, 220], "NBDL"),
    (33.0, 43.0, [90, 193, 64], "18% MG"),
    (50.0, 59.0, [241, 180, 189], "MG+1"),
    (84.0, 93.0, [252, 249, 25], "80% WC"),
    (93.0, 1000.0, [226, 12, 2], "95% WC"),
];

fn smoothstep(e0: f32, e1: f32, x: f32) -> f32 {
    let t = ((x - e0) / (e1 - e0)).clamp(0.0, 1.0);
    t * t * (3.0 - 2.0 * t)
}

/// CPU reference of the shader's false color (used for the legend and tests).
pub fn false_color(luma: f32) -> [f32; 3] {
    let ire = luma * 100.0;
    let mut c = [luma; 3];
    for (lo, hi, col, _) in FALSE_COLOR_BANDS {
        let w = smoothstep(lo - 1.0, lo + 1.0, ire) * (1.0 - smoothstep(hi - 1.0, hi + 1.0, ire));
        for k in 0..3 {
            c[k] += (col[k] as f32 / 255.0 - c[k]) * w;
        }
    }
    c
}

// ------------------------------------------------------------------ histogram

#[derive(Clone)]
pub struct Histogram {
    /// r, g, b, luma; 128 bins each, normalised to the tallest non-edge bin.
    pub bins: [[f32; 128]; 4],
    /// Fraction of samples clipped at 0 / at 1 per channel (r, g, b).
    pub clip_lo: [f32; 3],
    pub clip_hi: [f32; 3],
}

/// Histogram of the displayed image (after CST / LUT), from a sampled grid of pixels.
pub fn compute_histogram(frame: &DecodedFrame, view: ViewSettings, lut: Option<&CubeLut>) -> Histogram {
    use rayon::prelude::*;
    let p = rf_color::ViewPipeline::new(view, &frame.encoding, lut);
    let (w, h) = (frame.width as usize, frame.height as usize);
    let step = (((w * h) as f32 / 160_000.0).sqrt().ceil() as usize).max(1);
    let rows: Vec<usize> = (0..h).step_by(step).collect();
    let counts = rows
        .par_iter()
        .map(|&y| {
            let mut c = [[0u32; 128]; 4];
            let mut lo = [0u32; 3];
            let mut hi = [0u32; 3];
            let mut n = 0u32;
            for x in (0..w).step_by(step) {
                let i = y * w + x;
                let v = match &frame.pixels {
                    Pixels::RgbaF16(d) => [d[i * 4].to_f32(), d[i * 4 + 1].to_f32(), d[i * 4 + 2].to_f32()],
                    Pixels::RgbF32(d) => [d[i * 3], d[i * 3 + 1], d[i * 3 + 2]],
                };
                let o = p.apply(v);
                let l = 0.2126 * o[0] + 0.7152 * o[1] + 0.0722 * o[2];
                for (k, val) in [o[0], o[1], o[2], l].into_iter().enumerate() {
                    c[k][((val * 127.0).round() as usize).min(127)] += 1;
                }
                for k in 0..3 {
                    if o[k] <= 0.002 {
                        lo[k] += 1;
                    }
                    if o[k] >= 0.998 {
                        hi[k] += 1;
                    }
                }
                n += 1;
            }
            (c, lo, hi, n)
        })
        .reduce(
            || ([[0u32; 128]; 4], [0u32; 3], [0u32; 3], 0u32),
            |mut a, b| {
                for k in 0..4 {
                    for j in 0..128 {
                        a.0[k][j] += b.0[k][j];
                    }
                }
                for k in 0..3 {
                    a.1[k] += b.1[k];
                    a.2[k] += b.2[k];
                }
                a.3 += b.3;
                a
            },
        );
    let (c, lo, hi, n) = counts;
    let mut bins = [[0f32; 128]; 4];
    for k in 0..4 {
        // Normalise to the tallest interior bin so a clipped spike doesn't flatten the rest.
        let peak = c[k][1..127].iter().copied().max().unwrap_or(1).max(1) as f32;
        for j in 0..128 {
            bins[k][j] = (c[k][j] as f32 / peak).min(1.0);
        }
    }
    let n = n.max(1) as f32;
    Histogram {
        bins,
        clip_lo: [lo[0] as f32 / n, lo[1] as f32 / n, lo[2] as f32 / n],
        clip_hi: [hi[0] as f32 / n, hi[1] as f32 / n, hi[2] as f32 / n],
    }
}

const HIST_RGB: [Color32; 3] = [
    Color32::from_rgb(0xe8, 0x4a, 0x3c),
    Color32::from_rgb(0x52, 0xc4, 0x5a),
    Color32::from_rgb(0x4a, 0x8c, 0xf0),
];

/// One mesh of pixel-snapped columns (no overlaps, so no seams between bins).
fn bins_shape(painter: &egui::Painter, rect: Rect, bins: &[f32; 128], fill: Color32) -> Shape {
    let ppp = painter.pixels_per_point();
    let snap = |x: f32| (x * ppp).round() / ppp;
    let mut mesh = egui::Mesh::default();
    let bw = rect.width() / 128.0;
    for (j, v) in bins.iter().enumerate() {
        if *v <= 0.0 {
            continue;
        }
        let (x0, x1) = (snap(rect.left() + j as f32 * bw), snap(rect.left() + (j + 1) as f32 * bw));
        if x1 <= x0 {
            continue;
        }
        let top = snap(rect.bottom() - v * rect.height());
        mesh.add_colored_rect(Rect::from_min_max(Pos2::new(x0, top), Pos2::new(x1, rect.bottom())), fill);
    }
    Shape::mesh(mesh)
}

/// Additive colour (premultiplied, alpha 0): overlapping channels sum toward white.
fn additive(c: Color32, k: f32) -> Color32 {
    Color32::from_rgba_premultiplied((c.r() as f32 * k) as u8, (c.g() as f32 * k) as u8, (c.b() as f32 * k) as u8, 0)
}

/// Draw the histogram panel (BMPCC style) into `rect`.
pub fn draw_histogram(painter: &egui::Painter, rect: Rect, h: &Histogram, mode: HistMode) {
    painter.rect_filled(rect, CornerRadius::same(6), Color32::from_black_alpha(150));
    let inner = rect.shrink2(Vec2::new(8.0, 7.0));
    // quarter gridlines
    for q in 1..4 {
        let x = inner.left() + inner.width() * q as f32 / 4.0;
        painter.vline(x, inner.y_range(), Stroke::new(1.0, Color32::from_white_alpha(14)));
    }
    match mode {
        HistMode::Luma => {
            painter.add(bins_shape(painter, inner, &h.bins[3], Color32::from_white_alpha(170)));
        }
        HistMode::Rgb => {
            for k in 0..3 {
                painter.add(bins_shape(painter, inner, &h.bins[k], additive(HIST_RGB[k], 0.75)));
            }
        }
        HistMode::Parade => {
            let w = (inner.width() - 8.0) / 3.0;
            for k in 0..3 {
                let r = Rect::from_min_size(Pos2::new(inner.left() + k as f32 * (w + 4.0), inner.top()), Vec2::new(w, inner.height()));
                painter.add(bins_shape(painter, r, &h.bins[k], HIST_RGB[k].gamma_multiply(0.85)));
            }
        }
    }
    // Clipping indicators: shadows on the left edge, highlights on the right (lit > 0.1%).
    for k in 0..3 {
        let y = rect.top() + 9.0 + k as f32 * 7.0;
        for (frac, x) in [(h.clip_lo[k], rect.left() + 4.0), (h.clip_hi[k], rect.right() - 4.0)] {
            let lit = frac > 0.001;
            painter.circle_filled(Pos2::new(x, y), 2.0, if lit { HIST_RGB[k] } else { Color32::from_white_alpha(20) });
        }
    }
}

// ------------------------------------------------------------------ guides

/// Largest rect of `aspect` (w/h) centred in `r`.
fn fit_aspect(r: Rect, aspect: f32) -> Rect {
    let ra = r.width() / r.height().max(1.0);
    if aspect > ra {
        Rect::from_center_size(r.center(), Vec2::new(r.width(), r.width() / aspect))
    } else {
        Rect::from_center_size(r.center(), Vec2::new(r.height() * aspect, r.height()))
    }
}

fn line(painter: &egui::Painter, a: Pos2, b: Pos2, s: Stroke) {
    painter.line_segment([a, b], s);
}

/// Where the ray from `p` along `d` leaves rect `r` (p must be inside or on the border).
fn ray_exit(r: Rect, p: Pos2, d: Vec2) -> Pos2 {
    let mut t = f32::INFINITY;
    if d.x > 1e-6 {
        t = t.min((r.right() - p.x) / d.x);
    } else if d.x < -1e-6 {
        t = t.min((r.left() - p.x) / d.x);
    }
    if d.y > 1e-6 {
        t = t.min((r.bottom() - p.y) / d.y);
    } else if d.y < -1e-6 {
        t = t.min((r.top() - p.y) / d.y);
    }
    p + d * t
}

/// Draw the enabled guides over the image rect `img` (screen space).
pub fn draw_guides(painter: &egui::Painter, img: Rect, g: &Guides) {
    let col = Color32::from_rgb(g.color[0], g.color[1], g.color[2]).gamma_multiply(g.opacity.clamp(0.05, 1.0));
    let s = Stroke::new(g.thickness, col);
    let frame = match g.frame.ratio(g) {
        Some(a) => fit_aspect(img, a),
        None => img,
    };
    // Mask outside the frame guide (camera "guide opacity").
    if g.frame != FrameAspect::None && g.mask > 0.0 {
        let m = Color32::from_black_alpha((g.mask.clamp(0.0, 1.0) * 255.0) as u8);
        for r in [
            Rect::from_min_max(img.min, Pos2::new(img.right(), frame.top())),
            Rect::from_min_max(Pos2::new(img.left(), frame.bottom()), img.max),
            Rect::from_min_max(Pos2::new(img.left(), frame.top()), Pos2::new(frame.left(), frame.bottom())),
            Rect::from_min_max(Pos2::new(frame.right(), frame.top()), Pos2::new(img.right(), frame.bottom())),
        ] {
            if r.width() > 0.5 && r.height() > 0.5 {
                painter.rect_filled(r, 0.0, m);
            }
        }
        painter.rect_stroke(frame, 0.0, s, egui::StrokeKind::Inside);
    }
    let f = frame;
    let at = |u: f32, v: f32| Pos2::new(f.left() + u * f.width(), f.top() + v * f.height());
    if g.safe {
        let k = (g.safe_pct / 100.0).clamp(0.1, 1.0);
        let sr = Rect::from_center_size(f.center(), f.size() * k);
        painter.rect_stroke(sr, 0.0, Stroke::new(g.thickness, col.gamma_multiply(0.9)), egui::StrokeKind::Inside);
    }
    let fractions = |fr: &[f32]| {
        for &t in fr {
            line(painter, at(t, 0.0), at(t, 1.0), s);
            line(painter, at(0.0, t), at(1.0, t), s);
        }
    };
    if g.thirds {
        fractions(&[1.0 / 3.0, 2.0 / 3.0]);
    }
    if g.golden {
        fractions(&[0.381_966, 0.618_034]);
    }
    if g.grid {
        let (c, r) = (g.grid_cols.max(1) as f32, g.grid_rows.max(1) as f32);
        for i in 1..g.grid_cols.max(1) {
            line(painter, at(i as f32 / c, 0.0), at(i as f32 / c, 1.0), s);
        }
        for i in 1..g.grid_rows.max(1) {
            line(painter, at(0.0, i as f32 / r), at(1.0, i as f32 / r), s);
        }
    }
    if g.cross {
        let c = f.center();
        let arm = f.height().min(f.width()) * 0.05;
        line(painter, c - Vec2::new(arm, 0.0), c + Vec2::new(arm, 0.0), s);
        line(painter, c - Vec2::new(0.0, arm), c + Vec2::new(0.0, arm), s);
    }
    if g.dot {
        painter.circle_filled(f.center(), (g.thickness * 2.0).max(2.5), col);
    }
    let (tl, tr, bl, br) = (f.left_top(), f.right_top(), f.left_bottom(), f.right_bottom());
    if g.diagonals {
        // Diagonal method: 45° lines from each corner.
        let (w, h) = (f.width(), f.height());
        let d = w.min(h);
        if w >= h {
            line(painter, tl, tl + Vec2::new(d, d), s);
            line(painter, tr, tr + Vec2::new(-d, d), s);
            line(painter, bl, bl + Vec2::new(d, -d), s);
            line(painter, br, br + Vec2::new(-d, -d), s);
        } else {
            line(painter, tl, tl + Vec2::new(d, d), s);
            line(painter, tr, tr + Vec2::new(-d, d), s);
            line(painter, bl, bl + Vec2::new(d, -d), s);
            line(painter, br, br + Vec2::new(-d, -d), s);
        }
    }
    let mirror = g.spiral_corner;
    if g.triangles {
        // Golden triangles: one diagonal + perpendiculars from the other two corners.
        let (a, b, c1, c2) = if mirror & 1 == 0 { (tl, br, tr, bl) } else { (tr, bl, tl, br) };
        line(painter, a, b, s);
        let dir = (b - a).normalized();
        for c in [c1, c2] {
            let foot = a + dir * (c - a).dot(dir);
            line(painter, c, foot, s);
        }
    }
    if g.armature {
        // Harmonic armature: 2 diagonals, 4 reciprocals, corners to opposite-side midpoints.
        line(painter, tl, br, s);
        line(painter, tr, bl, s);
        for (c, other_a, other_b) in [(tl, tr, bl), (br, tr, bl), (tr, tl, br), (bl, tl, br)] {
            let d = other_b - other_a;
            let perp = Vec2::new(d.y, -d.x).normalized();
            let dirv = if (f.center() - c).dot(perp) > 0.0 { perp } else { -perp };
            line(painter, c, ray_exit(f, c, dirv), s);
        }
        let mids = [at(0.5, 0.0), at(1.0, 0.5), at(0.5, 1.0), at(0.0, 0.5)];
        for (c, m1, m2) in [(tl, mids[1], mids[2]), (tr, mids[2], mids[3]), (br, mids[3], mids[0]), (bl, mids[0], mids[1])] {
            line(painter, c, m1, Stroke::new(g.thickness, col.gamma_multiply(0.6)));
            line(painter, c, m2, Stroke::new(g.thickness, col.gamma_multiply(0.6)));
        }
    }
    if g.spiral {
        draw_spiral(painter, f, mirror, s);
    }
}

/// Golden (Fibonacci) spiral with its square subdivisions, stretched to the frame.
/// `corner` mirrors it so the spiral winds into the chosen corner.
fn draw_spiral(painter: &egui::Painter, f: Rect, corner: u8, s: Stroke) {
    const PHI: f32 = 1.618_034;
    // Work in a φ × 1 rectangle; the default construction winds toward the bottom-right.
    let map = |p: Pos2| -> Pos2 {
        let mut u = p.x / PHI;
        let mut v = p.y;
        if corner & 1 == 0 {
            u = 1.0 - u; // wind toward the left
        }
        if corner & 2 == 0 {
            v = 1.0 - v; // wind toward the top
        }
        Pos2::new(f.left() + u * f.width(), f.top() + v * f.height())
    };
    let (mut x, mut y, mut w, mut h) = (0.0f32, 0.0f32, PHI, 1.0f32);
    let sub = Stroke::new(s.width, s.color.gamma_multiply(0.45));
    for i in 0..10 {
        let (c, r, a0, a1, divider): (Pos2, f32, f32, f32, [Pos2; 2]);
        match i % 4 {
            0 => {
                r = h;
                c = Pos2::new(x + h, y + h);
                a0 = PI;
                a1 = 1.5 * PI;
                divider = [Pos2::new(x + h, y), Pos2::new(x + h, y + h)];
                x += h;
                w -= h;
            }
            1 => {
                r = w;
                c = Pos2::new(x, y + w);
                a0 = 1.5 * PI;
                a1 = 2.0 * PI;
                divider = [Pos2::new(x, y + w), Pos2::new(x + w, y + w)];
                y += w;
                h -= w;
            }
            2 => {
                r = h;
                c = Pos2::new(x + w - h, y);
                a0 = 0.0;
                a1 = FRAC_PI_2;
                divider = [Pos2::new(x + w - h, y), Pos2::new(x + w - h, y + h)];
                w -= h;
            }
            _ => {
                r = w;
                c = Pos2::new(x + w, y + h - w);
                a0 = FRAC_PI_2;
                a1 = PI;
                divider = [Pos2::new(x, y + h - w), Pos2::new(x + w, y + h - w)];
                h -= w;
            }
        }
        if r <= 1e-4 {
            break;
        }
        painter.line_segment([map(divider[0]), map(divider[1])], sub);
        let n = 24;
        let pts: Vec<Pos2> = (0..=n)
            .map(|k| {
                let a = a0 + (a1 - a0) * k as f32 / n as f32;
                map(Pos2::new(c.x + r * a.cos(), c.y + r * a.sin()))
            })
            .collect();
        painter.add(Shape::line(pts, s));
    }
}

/// Vertical false color legend (camera style) in the bottom-left corner of the picture,
/// labelled with the same IRE ranges as the options popover.
pub fn draw_false_color_legend(painter: &egui::Painter, img: Rect) {
    let h = (img.height() * 0.32).clamp(110.0, 200.0);
    let bar = Rect::from_min_size(Pos2::new(img.left() + S3 + 6.0, img.bottom() - S3 - 8.0 - h), Vec2::new(10.0, h));
    let bg = Rect::from_min_max(bar.min - Vec2::new(6.0, 8.0), Pos2::new(bar.right() + 50.0, bar.bottom() + 8.0));
    painter.rect_filled(bg, CornerRadius::same(6), Color32::from_black_alpha(150));
    let steps = 100;
    for i in 0..steps {
        let l = 1.0 - i as f32 / steps as f32;
        let c = false_color(l);
        let to8 = |v: f32| (v.clamp(0.0, 1.0) * 255.0) as u8;
        let r = Rect::from_min_size(Pos2::new(bar.left(), bar.top() + i as f32 * h / steps as f32), Vec2::new(bar.width(), h / steps as f32 + 0.6));
        painter.rect_filled(r, 0.0, Color32::from_rgb(to8(c[0]), to8(c[1]), to8(c[2])));
    }
    for (lo, hi, _, _) in FALSE_COLOR_BANDS {
        let mid = (lo + hi.min(100.0)) / 2.0 / 100.0;
        let y = bar.bottom() - mid * h;
        let label = if hi > 100.0 { format!("{lo:.0}+") } else { format!("{lo:.0}–{hi:.0}") };
        painter.text(Pos2::new(bar.right() + 6.0, y), Align2::LEFT_CENTER, label, theme::mono(10.5), INK_DIM);
    }
}

// ------------------------------------------------------------------ option popovers

fn swatches(ui: &mut Ui, color: &mut [u8; 3], presets: &[([u8; 3], &str)]) {
    ui.horizontal(|ui| {
        ui.spacing_mut().item_spacing.x = 4.0;
        for (c, name) in presets {
            let (r, resp) = ui.allocate_exact_size(Vec2::splat(20.0), egui::Sense::click());
            let on = color == c;
            ui.painter().circle_filled(r.center(), 7.0, Color32::from_rgb(c[0], c[1], c[2]));
            ui.painter().circle_stroke(r.center(), 9.0, Stroke::new(if on { 1.5 } else { 1.0 }, if on { INK } else { CONTROL_HI }));
            if resp.on_hover_text(*name).clicked() {
                *color = *c;
            }
        }
        // Custom colour slot: same circle as the presets, with an eyedropper until a
        // custom colour is chosen; opens a picker popup.
        let custom = !presets.iter().any(|(c, _)| c == color);
        let (r, resp) = ui.allocate_exact_size(Vec2::splat(20.0), egui::Sense::click());
        if custom {
            ui.painter().circle_filled(r.center(), 7.0, Color32::from_rgb(color[0], color[1], color[2]));
            ui.painter().circle_stroke(r.center(), 9.0, Stroke::new(1.5, INK));
        } else {
            ui.painter().circle_filled(r.center(), 9.0, if resp.hovered() { CONTROL } else { HOVER });
            ui.painter().text(r.center(), Align2::CENTER_CENTER, "\u{E568}", theme::ui(11.0), if resp.hovered() { INK } else { SECONDARY });
        }
        let resp = resp.on_hover_text("Any color");
        egui::Popup::from_toggle_button_response(&resp)
            .close_behavior(egui::PopupCloseBehavior::CloseOnClickOutside)
            .show(|ui| {
                let mut c = Color32::from_rgb(color[0], color[1], color[2]);
                if egui::widgets::color_picker::color_picker_color32(ui, &mut c, egui::widgets::color_picker::Alpha::Opaque) {
                    *color = [c.r(), c.g(), c.b()];
                }
            });
    });
}

/// Fixed-size toggle chip for the 3×3 composition grid.
fn toggle_chip(ui: &mut Ui, on: &mut bool, label: &str, tip: &str) {
    let font = theme::ui(12.0);
    let (r, resp) = ui.allocate_exact_size(Vec2::new(96.0, 26.0), egui::Sense::click());
    let fill = if *on {
        CONTROL
    } else if resp.hovered() {
        CONTROL
    } else {
        HOVER
    };
    ui.painter().rect_filled(r, CornerRadius::same(RADIUS), fill);
    ui.painter().text(r.center(), Align2::CENTER_CENTER, label, font, if *on { INK } else { SECONDARY });
    if *on {
        ui.painter().rect_filled(Rect::from_center_size(Pos2::new(r.center().x, r.bottom() - 3.0), Vec2::new(10.0, 2.0)), 1.0, INK);
    }
    if resp.on_hover_text(tip).clicked() {
        *on = !*on;
    }
}

pub fn guides_options(ui: &mut Ui, g: &mut Guides) {
    row(ui, "Frame", |ui| {
        egui::ComboBox::from_id_salt("frame_guide").selected_text(g.frame.label()).width(150.0).icon(widgets::combo_caret).show_ui(ui, |ui| {
            for a in FrameAspect::ALL {
                ui.selectable_value(&mut g.frame, a, a.label());
            }
        });
    });
    if g.frame == FrameAspect::Custom {
        row(ui, "Custom ratio", |ui| {
            ui.add(egui::DragValue::new(&mut g.custom_h).range(0.1..=50.0).speed(0.01).max_decimals(2));
            ui.label(RichText::new(":").color(MUTED));
            ui.add(egui::DragValue::new(&mut g.custom_w).range(0.1..=50.0).speed(0.01).max_decimals(2));
        });
    }
    if g.frame != FrameAspect::None {
        row_ltr(ui, "Outside frame", 170.0, |ui| {
            let fmt = |x: f32| format!("{:.0}%", x * 100.0);
            Slider { value: &mut g.mask, range: 0.0..=1.0, default: 0.5, step: 0.25, log: false, text: &fmt }.show(ui, 170.0);
        });
    }
    row(ui, "Safe area", |ui| {
        if g.safe {
            ui.add(egui::DragValue::new(&mut g.safe_pct).range(50.0..=100.0).speed(0.5).suffix("%"));
        }
        ui.checkbox(&mut g.safe, "");
    });
    ui.add_space(S2);
    ui.label(RichText::new("COMPOSITION").font(theme::strong(10.5)).color(MUTED).extra_letter_spacing(0.8));
    ui.add_space(S1);
    egui::Grid::new("composition").num_columns(3).spacing(Vec2::new(6.0, 6.0)).show(ui, |ui| {
        toggle_chip(ui, &mut g.thirds, "Thirds", "Rule of thirds");
        toggle_chip(ui, &mut g.golden, "Golden ratio", "Phi grid (0.382 / 0.618)");
        toggle_chip(ui, &mut g.spiral, "Spiral", "Golden (Fibonacci) spiral");
        ui.end_row();
        toggle_chip(ui, &mut g.diagonals, "Diagonals", "Diagonal method: 45° lines from each corner");
        toggle_chip(ui, &mut g.triangles, "Triangles", "Golden triangles");
        toggle_chip(ui, &mut g.armature, "Armature", "Harmonic armature (dynamic symmetry)");
        ui.end_row();
        toggle_chip(ui, &mut g.cross, "Center cross", "Center crosshair");
        toggle_chip(ui, &mut g.dot, "Center dot", "Center dot");
        toggle_chip(ui, &mut g.grid, "Grid", "Custom rows × columns grid");
        ui.end_row();
    });
    if g.spiral || g.triangles {
        ui.add_space(S1);
        row(ui, "Orientation", |ui| {
            segmented(ui, &mut g.spiral_corner, &[(0, "\u{E090}"), (1, "\u{E092}"), (2, "\u{E040}"), (3, "\u{E042}")]);
        });
    }
    if g.grid {
        row(ui, "Grid", |ui| {
            ui.add(egui::DragValue::new(&mut g.grid_rows).range(1..=24).suffix(" rows"));
            ui.add(egui::DragValue::new(&mut g.grid_cols).range(1..=24).suffix(" cols"));
        });
    }
    ui.add_space(S2);
    row_ltr(ui, "Line color", 176.0, |ui| {
        swatches(ui, &mut g.color, &[([0xd6, 0xd6, 0xd4], "Grey"), ([0xff, 0xff, 0xff], "White"), ([0x10, 0x10, 0x10], "Black"), ([0xff, 0xd4, 0x20], "Yellow"), ([0x40, 0xd8, 0xe8], "Cyan")]);
    });
    row_ltr(ui, "Opacity", 170.0, |ui| {
        let fmt = |x: f32| format!("{:.0}%", x * 100.0);
        Slider { value: &mut g.opacity, range: 0.1..=1.0, default: 0.6, step: 0.05, log: false, text: &fmt }.show(ui, 170.0);
    });
    row(ui, "Thickness", |ui| {
        let mut t = (g.thickness * 2.0).round() as u8;
        segmented(ui, &mut t, &[(2, "1"), (3, "1.5"), (4, "2")]);
        g.thickness = t as f32 / 2.0;
    });
}

pub fn peaking_options(ui: &mut Ui, p: &mut Peaking) {
    row(ui, "Style", |ui| {
        segmented(ui, &mut p.mode, &[(PeakMode::Lines, "Colored lines"), (PeakMode::Peaking, "Peaking")]);
    });
    if p.mode == PeakMode::Lines {
        row_ltr(ui, "Color", 176.0, |ui| {
            swatches(ui, &mut p.color, &[([0xff, 0x24, 0x18], "Red"), ([0x30, 0xe0, 0x40], "Green"), ([0x2a, 0x6c, 0xff], "Blue"), ([0xff, 0xff, 0xff], "White"), ([0x00, 0x00, 0x00], "Black")]);
        });
    }
    row_ltr(ui, "Sensitivity", 170.0, |ui| {
        let fmt = |x: f32| format!("{:.0}%", x * 100.0);
        Slider { value: &mut p.level, range: 0.0..=1.0, default: 0.5, step: 0.01, log: false, text: &fmt }.show(ui, 170.0);
    });
    if p.mode == PeakMode::Lines {
        row(ui, "Dim image", |ui| {
            ui.checkbox(&mut p.dim, "");
        });
    }
    ui.add_space(S1);
    ui.label(RichText::new("Higher sensitivity marks finer detail. Faces want more; foliage and brick want less.").color(MUTED).font(theme::ui(11.5)));
}

pub fn false_color_options(ui: &mut Ui, f: &mut FalseColor) {
    row(ui, "Measure", |ui| {
        segmented(ui, &mut f.measure, &[(FcMeasure::Display, "Display"), (FcMeasure::Log, "Camera log")]);
    });
    row(ui, "Show legend", |ui| {
        ui.checkbox(&mut f.legend, "");
    });
    ui.add_space(S2);
    for (lo, hi, c, label) in FALSE_COLOR_BANDS.iter().rev() {
        ui.horizontal(|ui| {
            let (r, _) = ui.allocate_exact_size(Vec2::new(14.0, 14.0), egui::Sense::hover());
            ui.painter().rect_filled(r, CornerRadius::same(3), Color32::from_rgb(c[0], c[1], c[2]));
            let range = if *hi > 100.0 { format!("≥ {lo:.0}%") } else { format!("{lo:.0}–{hi:.0}%") };
            let meaning = match *label {
                "95% WC" => "White clipping",
                "80% WC" => "Near white clipping",
                "MG+1" => "One stop over middle grey",
                "18% MG" => "Middle grey",
                "NBDL" => "Near black detail loss",
                _ => "Black detail loss",
            };
            ui.label(RichText::new(meaning).color(INK_DIM).font(theme::ui(12.0)));
            ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                ui.label(RichText::new(range).font(theme::mono(11.0)).color(MUTED));
            });
        });
    }
}

pub fn histogram_options(ui: &mut Ui, h: &mut HistogramCfg) {
    row(ui, "Mode", |ui| {
        segmented(ui, &mut h.mode, &[(HistMode::Luma, "Luma"), (HistMode::Rgb, "RGB"), (HistMode::Parade, "Parade")]);
    });
    ui.add_space(S1);
    ui.label(RichText::new("Dots light up when more than 0.1% of a channel clips: left for shadows, right for highlights.").color(MUTED).font(theme::ui(11.5)));
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn false_color_bands() {
        let g = false_color(0.38); // ~18% grey on a Rec.709 display
        assert!(g[1] > g[0] && g[1] > g[2], "middle grey is green: {g:?}");
        let r = false_color(0.99);
        assert!(r[0] > 0.8 && r[1] < 0.2, "clipping is red: {r:?}");
        let p = false_color(0.02);
        assert!(p[0] > 0.4 && p[2] > 0.4 && p[1] < 0.2, "black clip is purple: {p:?}");
        let grey = false_color(0.25);
        assert!((grey[0] - grey[1]).abs() < 1e-3 && (grey[1] - grey[2]).abs() < 1e-3, "between bands stays grey");
    }
}

//! Viewer transform (CPU reference implementation). `app/src/viewer.wgsl` mirrors this
//! exactly; see .claude/COLOR_PIPELINE.md §3.

use crate::matrix::{rgb_to_rgb, Mat3, Primaries};
use crate::transfer::{film_gen5_decode, film_gen5_encode, gamma_encode, srgb_encode};
use crate::{primaries_of, CubeLut};
use rf_core::{ColorEncoding, DecodedFrame, Pixels, Transfer};
use serde::{Deserialize, Serialize};

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, Default)]
pub enum OutputMode {
    /// CST to Rec.709 primaries, Gamma 2.4 (Resolve CST equivalent).
    #[default]
    Rec709G24,
    /// CST to sRGB primaries + sRGB curve.
    Srgb,
    /// No transform: show the decoded signal (log) as-is.
    AsDecoded,
}

impl OutputMode {
    pub const ALL: [OutputMode; 3] = [Self::Rec709G24, Self::Srgb, Self::AsDecoded];
    pub fn label(self) -> &'static str {
        match self {
            Self::Rec709G24 => "Rec.709 · Gamma 2.4",
            Self::Srgb => "sRGB",
            Self::AsDecoded => "Log (as decoded)",
        }
    }
    pub fn id(self) -> u32 {
        match self {
            Self::Rec709G24 => 0,
            Self::Srgb => 1,
            Self::AsDecoded => 2,
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, Default)]
pub enum ToneMap {
    #[default]
    None,
    /// Hue-preserving highlight rolloff above 0.8 (approximation, not DaVinci's operator).
    Rolloff,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, Default)]
pub enum LutPlacement {
    /// LUT receives the BMD Film Gen 5 log signal (replaces the CST).
    #[default]
    OnLog,
    /// LUT receives the display-encoded output of the CST.
    AfterCst,
}

#[derive(Debug, Clone, Copy, PartialEq, Serialize, Deserialize)]
pub struct ViewSettings {
    pub output: OutputMode,
    pub tonemap: ToneMap,
    /// Viewer-only exposure trim in stops.
    pub exposure_ev: f32,
    pub lut_enabled: bool,
    pub lut_placement: LutPlacement,
    /// 0..1
    pub lut_mix: f32,
}

impl Default for ViewSettings {
    fn default() -> Self {
        Self {
            output: OutputMode::Rec709G24,
            tonemap: ToneMap::None,
            exposure_ev: 0.0,
            lut_enabled: true,
            lut_placement: LutPlacement::OnLog,
            lut_mix: 1.0,
        }
    }
}

/// Input stage derived from a frame's [`ColorEncoding`].
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum InputKind {
    Linear = 0,
    FilmGen5 = 1,
    /// Display-referred or unknown: no CST possible, pass through.
    Passthrough = 2,
}

pub fn input_kind(enc: &ColorEncoding) -> InputKind {
    match enc.transfer {
        Transfer::Linear => InputKind::Linear,
        Transfer::BmdFilmGen5 => InputKind::FilmGen5,
        _ => InputKind::Passthrough,
    }
}

/// Gamut matrix from the frame's gamut to the output display primaries.
pub fn display_matrix(enc: &ColorEncoding, output: OutputMode) -> Mat3 {
    let dst = match output {
        OutputMode::Rec709G24 | OutputMode::Srgb => Primaries::REC709,
        OutputMode::AsDecoded => return Mat3::IDENTITY,
    };
    match primaries_of(&enc.gamut) {
        Some(src) => rgb_to_rgb(&src, &dst),
        None => Mat3::IDENTITY,
    }
}

#[inline]
pub fn rolloff(rgb: [f32; 3]) -> [f32; 3] {
    const K: f32 = 0.8;
    let m = rgb[0].max(rgb[1]).max(rgb[2]);
    if m <= K {
        return rgb;
    }
    let y = K + (1.0 - K) * (1.0 - (-(m - K) / (1.0 - K)).exp());
    let s = y / m;
    [rgb[0] * s, rgb[1] * s, rgb[2] * s]
}

/// Precomputed per-frame view pipeline.
pub struct ViewPipeline<'a> {
    pub settings: ViewSettings,
    pub kind: InputKind,
    pub matrix: Mat3,
    pub lut: Option<&'a CubeLut>,
}

impl<'a> ViewPipeline<'a> {
    pub fn new(settings: ViewSettings, enc: &ColorEncoding, lut: Option<&'a CubeLut>) -> Self {
        Self {
            settings,
            kind: input_kind(enc),
            matrix: display_matrix(enc, settings.output),
            lut: if settings.lut_enabled { lut } else { None },
        }
    }

    fn mix_lut(&self, v: [f32; 3], lut: &CubeLut) -> [f32; 3] {
        let o = lut.apply(v);
        let t = self.settings.lut_mix.clamp(0.0, 1.0);
        [v[0] + (o[0] - v[0]) * t, v[1] + (o[1] - v[1]) * t, v[2] + (o[2] - v[2]) * t]
    }

    /// One pixel, decoded signal in -> display-encoded value out (0..1, clamped).
    pub fn apply(&self, v: [f32; 3]) -> [f32; 3] {
        let s = &self.settings;
        let gain = 2f32.powf(s.exposure_ev);
        let linearize = |c: f32| match self.kind {
            InputKind::Linear => c,
            InputKind::FilmGen5 => film_gen5_decode(c),
            InputKind::Passthrough => c,
        };
        let out = if let (Some(lut), LutPlacement::OnLog) = (self.lut, s.lut_placement) {
            // Log signal for the LUT: Film Gen 5 (re-encoded if input is linear or exposure is trimmed).
            let log = match self.kind {
                InputKind::Passthrough => v,
                _ if gain == 1.0 && self.kind == InputKind::FilmGen5 => v,
                _ => v.map(|c| film_gen5_encode(linearize(c) * gain)),
            };
            self.mix_lut(log, lut)
        } else {
            let mut o = if self.kind == InputKind::Passthrough {
                v
            } else if s.output == OutputMode::AsDecoded {
                v.map(|c| film_gen5_encode(linearize(c) * gain))
            } else {
                let l = v.map(|c| linearize(c) * gain);
                let mut d = self.matrix.apply_f32(l);
                if s.tonemap == ToneMap::Rolloff {
                    d = rolloff(d);
                }
                match s.output {
                    OutputMode::Srgb => d.map(srgb_encode),
                    _ => d.map(|c| gamma_encode(c, 2.4)),
                }
            };
            if let (Some(lut), LutPlacement::AfterCst) = (self.lut, s.lut_placement) {
                o = self.mix_lut(o.map(|c| c.clamp(0.0, 1.0)), lut);
            }
            o
        };
        out.map(|c| if c.is_finite() { c.clamp(0.0, 1.0) } else { 0.0 })
    }
}

/// Render a frame through the view pipeline, nearest-sampled to no wider than `max_w`,
/// converting each display value (0..1) with `quantize`.
fn render<T: Copy + Default + Send>(
    frame: &DecodedFrame,
    settings: ViewSettings,
    lut: Option<&CubeLut>,
    max_w: u32,
    quantize: impl Fn(f32) -> T + Sync,
) -> (u32, u32, Vec<T>) {
    use rayon::prelude::*;
    let step = frame.width.div_ceil(max_w.max(1)).max(1);
    let w = frame.width / step;
    let h = frame.height / step;
    let p = ViewPipeline::new(settings, &frame.encoding, lut);
    let mut out = vec![T::default(); (w * h * 3) as usize];
    out.par_chunks_mut((w * 3) as usize).enumerate().for_each(|(y, row)| {
        for x in 0..w as usize {
            let sx = x * step as usize;
            let sy = y * step as usize;
            let i = sy * frame.width as usize + sx;
            let v = match &frame.pixels {
                Pixels::RgbaF16(d) => [d[i * 4].to_f32(), d[i * 4 + 1].to_f32(), d[i * 4 + 2].to_f32()],
                Pixels::RgbF32(d) => [d[i * 3], d[i * 3 + 1], d[i * 3 + 2]],
            };
            let o = p.apply(v);
            for c in 0..3 {
                row[x * 3 + c] = quantize(o[c]);
            }
        }
    });
    (w, h, out)
}

/// Render a frame through the view pipeline into an 8-bit RGB image no wider than `max_w`.
/// Used for thumbnails (saved-frame list, DNG preview IFD) and graded JPEG exports.
pub fn render_rgb8(frame: &DecodedFrame, settings: ViewSettings, lut: Option<&CubeLut>, max_w: u32) -> (u32, u32, Vec<u8>) {
    render(frame, settings, lut, max_w, |v| (v * 255.0 + 0.5) as u8)
}

/// Full-resolution 16-bit render through the view pipeline (graded TIFF exports).
pub fn render_rgb16(frame: &DecodedFrame, settings: ViewSettings, lut: Option<&CubeLut>) -> (u32, u32, Vec<u16>) {
    render(frame, settings, lut, frame.width, |v| (v * 65535.0 + 0.5) as u16)
}

#[cfg(test)]
mod tests {
    use super::*;
    use rf_core::Gamut;

    fn enc(t: Transfer) -> ColorEncoding {
        ColorEncoding { transfer: t, gamut: Gamut::BmdWideGamut }
    }

    #[test]
    fn grey_maps_to_expected_rec709() {
        // 18% grey in Film Gen5 -> Rec.709 gamma 2.4 = 0.18^(1/2.4) ≈ 0.4895
        let p = ViewPipeline::new(ViewSettings::default(), &enc(Transfer::BmdFilmGen5), None);
        let o = p.apply([0.383_561_6; 3]);
        for c in o {
            assert!((c - 0.18f32.powf(1.0 / 2.4)).abs() < 1e-3, "{o:?}");
        }
    }

    #[test]
    fn linear_and_log_inputs_agree() {
        let a = ViewPipeline::new(ViewSettings::default(), &enc(Transfer::BmdFilmGen5), None);
        let b = ViewPipeline::new(ViewSettings::default(), &enc(Transfer::Linear), None);
        let lin = [0.05f32, 0.3, 0.6];
        let oa = a.apply(lin.map(film_gen5_encode));
        let ob = b.apply(lin);
        for c in 0..3 {
            assert!((oa[c] - ob[c]).abs() < 1e-4);
        }
    }

    #[test]
    fn rolloff_monotonic_and_bounded() {
        let mut last = 0.0;
        for i in 0..1000 {
            let x = i as f32 / 100.0;
            let y = rolloff([x, x, x])[0];
            assert!(y >= last - 1e-6 && y <= 1.0);
            last = y;
        }
    }
}

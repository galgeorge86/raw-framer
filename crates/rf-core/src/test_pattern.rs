//! Synthetic clip used for development, tests and running without any decoder SDK.
//! Pixels are generated as scene-linear Blackmagic Wide Gamut and then encoded as
//! Blackmagic Film Gen 5 (or left linear when the "Linear" gamma is requested).

use crate::*;

pub const TEST_GAMMA_FILM: &str = "Blackmagic Design Film";
pub const TEST_GAMMA_LINEAR: &str = "Linear";
pub const TEST_GAMUT: &str = "Blackmagic Design";

pub struct TestPatternSource {
    info: ClipInfo,
    caps: DecodeCapabilities,
    meta: Vec<(String, String)>,
}

impl TestPatternSource {
    pub fn new(path: PathBuf) -> Self {
        let info = ClipInfo {
            path,
            format_name: "Test pattern".into(),
            width: 1920,
            height: 1080,
            fps: 24.0,
            frame_count: 240,
            camera_make: "Raw Framer".into(),
            camera_model: "Test Pattern Generator".into(),
            as_shot: AsShot {
                color_science_gen: Some(5),
                gamma: Some(TEST_GAMMA_FILM.into()),
                gamut: Some(TEST_GAMUT.into()),
                iso: Some(800),
                kelvin: Some(5600),
                tint: Some(0),
                exposure: Some(0.0),
                highlight_recovery: Some(true),
                gamut_compression: Some(false),
                post3dlut_mode: None,
            },
            start_timecode: Some("01:00:00:00".into()),
        };
        let caps = DecodeCapabilities {
            color_science_gens: vec![5],
            gammas: vec![TEST_GAMMA_FILM.into(), TEST_GAMMA_LINEAR.into()],
            gamuts: vec![TEST_GAMUT.into()],
            isos: vec![400, 800, 1600, 3200],
            kelvin_range: Some((2500, 10000)),
            tint_range: Some((-50, 50)),
            exposure_range: Some((-5.0, 5.0)),
            post3dlut_modes: vec![],
        };
        let meta = vec![
            ("camera_type".into(), "Test Pattern".into()),
            ("note".into(), "Synthetic scene-linear BMD WG content".into()),
        ];
        Self { info, caps, meta }
    }

    fn scene_linear(&self, x: f32, y: f32, frame: u64, color: &DecodeColor) -> [f32; 3] {
        // Exposure: ISO and exposure act as simple gains, WB as a crude R/B gain.
        let iso = color.iso.unwrap_or(800) as f32;
        let mut gain = iso / 800.0 * 2f32.powf(color.exposure().unwrap_or(0.0));
        let k = color.kelvin.unwrap_or(5600) as f32;
        let wb_r = (k / 5600.0).powf(0.6);
        let wb_b = (5600.0 / k).powf(0.6);
        let t = frame as f32 / self.info.frame_count as f32;
        let mut rgb;
        if y < 0.15 {
            // Exposure ramp: -8 .. +8 stops around 18% grey, in 1-stop steps.
            let stop = (x * 16.0).floor() - 8.0;
            let v = 0.18 * 2f32.powf(stop);
            rgb = [v, v, v];
        } else if y < 0.75 {
            // 6 x 4 patch chart.
            let col = ((x * 6.0).floor() as usize).min(5);
            let row = (((y - 0.15) / 0.6 * 4.0).floor() as usize).min(3);
            const PATCHES: [[f32; 3]; 24] = [
                [0.11, 0.08, 0.06], [0.38, 0.29, 0.23], [0.13, 0.18, 0.27], [0.10, 0.14, 0.07],
                [0.24, 0.22, 0.38], [0.20, 0.47, 0.40], [0.60, 0.27, 0.05], [0.08, 0.10, 0.36],
                [0.48, 0.11, 0.12], [0.10, 0.05, 0.14], [0.36, 0.48, 0.06], [0.68, 0.40, 0.03],
                [0.04, 0.05, 0.27], [0.10, 0.28, 0.07], [0.38, 0.03, 0.04], [0.79, 0.59, 0.01],
                [0.42, 0.08, 0.28], [0.00, 0.22, 0.38], [0.88, 0.88, 0.88], [0.58, 0.58, 0.58],
                [0.36, 0.36, 0.36], [0.19, 0.19, 0.19], [0.09, 0.09, 0.09], [0.03, 0.03, 0.03],
            ];
            rgb = PATCHES[row * 6 + col];
        } else {
            // Smooth hue sweep with a moving specular highlight bar (tests highlight headroom).
            let h = x * 6.0;
            let c = |o: f32| ((h + o).rem_euclid(6.0) - 3.0).abs().clamp(0.0, 3.0) / 3.0;
            rgb = [c(0.0) * 0.5, c(2.0) * 0.5, c(4.0) * 0.5];
            let bar = (x - t).abs() < 0.01;
            if bar {
                rgb = [40.0, 40.0, 40.0];
            }
        }
        // Frame counter: 8 binary blocks top-right.
        if y < 0.04 && x > 0.8 {
            let bit = ((x - 0.8) / 0.2 * 8.0).floor() as u32;
            let on = (frame >> (7 - bit.min(7))) & 1 == 1;
            let v = if on { 2.0 } else { 0.01 };
            rgb = [v, v, v];
        }
        gain = gain.max(0.0);
        [rgb[0] * gain * wb_r, rgb[1] * gain, rgb[2] * gain * wb_b]
    }
}

fn film_gen5_encode(x: f32) -> f32 {
    const A: f32 = 0.086_928_76;
    const B: f32 = 0.005_494_072;
    const C: f32 = 0.530_013_3;
    const D: f32 = 8.283_606;
    const E: f32 = 0.092_465_75;
    if x < 0.005 {
        D * x + E
    } else {
        A * (x + B).ln() + C
    }
}

impl MediaSource for TestPatternSource {
    fn info(&self) -> &ClipInfo {
        &self.info
    }
    fn capabilities(&self) -> &DecodeCapabilities {
        &self.caps
    }
    fn metadata(&self) -> &[(String, String)] {
        &self.meta
    }
    fn shooting_info(&self, _frame: u64) -> ShootingInfo {
        ShootingInfo {
            exposure_time_s: Some(1.0 / 48.0),
            shutter_angle: Some(180.0),
            f_number: Some(2.8),
            focal_length_mm: Some(35.0),
            lens_model: Some("Test Lens 35mm".into()),
            iso: Some(800),
            date_time: Some("2026:01:01 12:00:00".into()),
            camera_serial: None,
            reel: Some("A001".into()),
        }
    }
    fn timecode(&self, frame: u64) -> Option<String> {
        let fps = 24u64;
        let total = 3600 * fps + frame;
        let f = total % fps;
        let s = (total / fps) % 60;
        let m = (total / fps / 60) % 60;
        let h = total / fps / 3600;
        Some(format!("{h:02}:{m:02}:{s:02}:{f:02}"))
    }
    fn linear_gamma_name(&self) -> Option<String> {
        Some(TEST_GAMMA_LINEAR.into())
    }

    fn decode(&self, req: &DecodeRequest) -> Result<DecodedFrame> {
        if req.frame >= self.info.frame_count {
            return Err(Error::FrameOutOfRange(req.frame));
        }
        let d = req.scale.divisor();
        let w = self.info.width / d;
        let h = self.info.height / d;
        let linear = req.color.gamma.as_deref() == Some(TEST_GAMMA_LINEAR);
        let enc = |v: f32| if linear { v.max(0.0) } else { film_gen5_encode(v) };
        let px = |i: usize| {
            let x = (i as u32 % w) as f32 / w as f32;
            let y = (i as u32 / w) as f32 / h as f32;
            let c = self.scene_linear(x, y, req.frame, &req.color);
            [enc(c[0]), enc(c[1]), enc(c[2])]
        };
        let n = (w * h) as usize;
        let pixels = match req.format {
            PixelFormat::RgbaF16 => {
                let mut v = Vec::with_capacity(n * 4);
                for i in 0..n {
                    let c = px(i);
                    v.extend([c[0], c[1], c[2], 1.0].map(half::f16::from_f32));
                }
                Pixels::RgbaF16(v)
            }
            PixelFormat::RgbF32 => {
                let mut v = Vec::with_capacity(n * 3);
                for i in 0..n {
                    v.extend(px(i));
                }
                Pixels::RgbF32(v)
            }
        };
        Ok(DecodedFrame {
            frame: req.frame,
            width: w,
            height: h,
            pixels,
            encoding: ColorEncoding {
                transfer: if linear { Transfer::Linear } else { Transfer::BmdFilmGen5 },
                gamut: Gamut::BmdWideGamut,
            },
        })
    }
}

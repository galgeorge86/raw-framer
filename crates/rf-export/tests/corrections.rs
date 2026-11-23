//! M8 end-to-end checks on a real clip: Camera RAW corrections reach the exported DNG
//! pixels and metadata, editable white balance reproduces the baked result, and corrections
//! survive a round trip through the clip's `.sidecar`.
//!
//! Uses `RF_TEST_CLIP` (or the author's sample clip); skipped when it isn't present.

use rf_color::Mat3;
use rf_core::*;
use rf_export::*;
use std::path::{Path, PathBuf};

const DEFAULT_CLIP: &str = r"D:\Filmmaking\Before Japan\A002_05302106_C014.braw";
const FRAME: u64 = 40;

fn clip() -> Option<PathBuf> {
    let p = PathBuf::from(std::env::var("RF_TEST_CLIP").unwrap_or_else(|_| DEFAULT_CLIP.into()));
    if p.exists() && rf_braw::sdk_available() && rf_braw::find_runtime().is_some() {
        Some(p)
    } else {
        eprintln!("skipped: test clip / Blackmagic RAW runtime not available");
        None
    }
}

fn tmp(name: &str) -> PathBuf {
    let d = PathBuf::from(env!("CARGO_TARGET_TMPDIR")).join("rf-corrections");
    std::fs::create_dir_all(&d).unwrap();
    d.join(name)
}

fn corrected() -> DecodeColor {
    let mut c = DecodeColor { iso: Some(800), kelvin: Some(3800), tint: Some(8), ..Default::default() };
    c.set_exposure(Some(0.7));
    c
}

fn settings(wb: WhiteBalanceMode) -> ExportSettings {
    ExportSettings { sample: Sample::Float32, preview: PreviewSize::None, white_balance: wb, overwrite: true, ..Default::default() }
}

fn export(src: &dyn MediaSource, color: &DecodeColor, s: &ExportSettings, name: &str) -> (rf_dng::read::DngRead, ExportReport) {
    let path = tmp(name);
    let r = export_frame(src, FRAME, color, s, None, &path).expect("export");
    let d = rf_dng::read::read(&std::fs::read(&path).unwrap()).expect("read back");
    (d, r)
}

fn mat(v: &[f64]) -> Mat3 {
    Mat3([[v[0], v[1], v[2]], [v[3], v[4], v[5]], [v[6], v[7], v[8]]])
}

#[test]
fn corrections_reach_dng_pixels_and_metadata() {
    let Some(p) = clip() else { return };
    let src = rf_braw::BrawSource::open(&p).unwrap();
    let color = corrected();
    let (dng, _) = export(&src, &color, &settings(WhiteBalanceMode::Baked), "corrected.dng");

    // Metadata
    assert_eq!(dng.exif_iso(), Some(800), "Exif ISO must be the corrected ISO");
    let desc = dng.ifd0.get(&270).map(|t| t.text()).unwrap_or_default();
    assert!(desc.contains("WB 3800K") && desc.contains("ISO 800") && desc.contains("tint 8") && desc.contains("+0.70"), "{desc}");
    assert_eq!(dng.reals(50728), vec![1.0, 1.0, 1.0], "baked WB says neutral");

    // Pixels: stored × 2^BaselineExposure == a direct SDK decode with the same settings.
    let be = dng.reals(50730)[0];
    let (lin, _) = linear_decode_color(&src, &color).unwrap();
    let direct = src.decode(&DecodeRequest { frame: FRAME, scale: ResolutionScale::Full, format: PixelFormat::RgbF32, color: lin }).unwrap();
    let Pixels::RgbF32(d) = &direct.pixels else { panic!() };
    assert_eq!((dng.width, dng.height), (direct.width, direct.height));
    let k = 2f64.powf(be) as f32;
    let (mut worst, mut n) = (0f32, 0usize);
    for (s, v) in dng.pixels.iter().zip(d) {
        if *s >= 0.999 || *v <= 0.0 {
            continue; // clipped at the normalisation peak / negative noise floored to 0
        }
        worst = worst.max((s * k - v).abs() / v.max(1e-3));
        n += 1;
    }
    println!("compared {n} samples, worst relative error {worst:e}");
    assert!(n > dng.pixels.len() / 2);
    assert!(worst < 1e-5, "DNG pixels differ from the SDK decode: {worst}");

    // And the corrections really changed the image (vs as shot).
    let (plain, _) = export(&src, &DecodeColor::default(), &settings(WhiteBalanceMode::Baked), "as_shot.dng");
    let mean = |v: &[f32], k: f64| v.iter().map(|x| *x as f64).sum::<f64>() / v.len() as f64 * 2f64.powf(k);
    let (a, b) = (mean(&plain.pixels, plain.reals(50730)[0]), mean(&dng.pixels, be));
    assert!(b > a * 2.0, "ISO 800 + 0.7 EV should be much brighter than ISO 200: {a} vs {b}");
}

#[test]
fn editable_white_balance_matches_baked() {
    let Some(p) = clip() else { return };
    let src = rf_braw::BrawSource::open(&p).unwrap();
    let mut color = DecodeColor::default();
    color.kelvin = Some(3800);
    color.tint = Some(8);
    let (baked, _) = export(&src, &color, &settings(WhiteBalanceMode::Baked), "wb_baked.dng");
    let (meta, rep) = export(&src, &color, &settings(WhiteBalanceMode::Metadata), "wb_meta.dng");
    let fit = rep.wb.expect("metadata mode reports its fit");
    println!("gains {:?} neutral {:?} model error {:.2} %", fit.gains, fit.neutral, fit.residual * 100.0);
    let neutral = meta.reals(50728);
    assert!(neutral.iter().all(|v| *v > 0.0 && *v <= 1.0) && neutral.iter().any(|v| *v < 0.9), "{neutral:?}");

    // What a DNG reader does: XYZ(D50) = ForwardMatrix × diag(1/AsShotNeutral) × raw × 2^BE.
    let xyz = |d: &rf_dng::read::DngRead| -> Vec<[f64; 3]> {
        let fm = mat(&d.reals(50964));
        let n = d.reals(50728);
        let k = 2f64.powf(d.reals(50730)[0]);
        d.pixels.chunks(3).step_by(97).map(|p| fm.apply([p[0] as f64 * k / n[0], p[1] as f64 * k / n[1], p[2] as f64 * k / n[2]])).collect()
    };
    let (xb, xm) = (xyz(&baked), xyz(&meta));
    let (mut dy, mut dxy, mut count) = (Vec::new(), Vec::new(), 0);
    for (b, m) in xb.iter().zip(&xm) {
        let (sb, sm) = (b[0] + b[1] + b[2], m[0] + m[1] + m[2]);
        if b[1] < 0.01 || b[1] > 0.7 * 2f64.powf(baked.reals(50730)[0]) {
            continue;
        }
        dy.push((m[1] - b[1]).abs() / b[1]);
        dxy.push(((b[0] / sb - m[0] / sm).powi(2) + (b[1] / sb - m[1] / sm).powi(2)).sqrt());
        count += 1;
    }
    let med = |v: &mut Vec<f64>| {
        v.sort_by(|a, b| a.partial_cmp(b).unwrap());
        v[v.len() / 2]
    };
    let (my, mxy) = (med(&mut dy), med(&mut dxy));
    println!("{count} pixels: median luminance difference {:.2} %, median chromaticity difference {mxy:.4}", my * 100.0);

    // The Kelvin a raw converter derives for "As Shot": xy of ColorMatrix1⁻¹ × neutral.
    let cm = mat(&meta.reals(50721)).inverse();
    let w = cm.apply([neutral[0], neutral[1], neutral[2]]);
    let (x, y) = (w[0] / (w[0] + w[1] + w[2]), w[1] / (w[0] + w[1] + w[2]));
    let nn = (x - 0.3320) / (0.1858 - y);
    let cct = 449.0 * nn.powi(3) + 3525.0 * nn.powi(2) + 6823.3 * nn + 5520.33; // McCamy
    println!("As Shot white point xy ({x:.4}, {y:.4}) ≈ {cct:.0} K (chosen 3800 K)");

    assert!(fit.residual < 0.05, "per-channel model doesn't describe the SDK's WB: {}", fit.residual);
    assert!(my < 0.03, "brightness differs from baked: {my}");
    assert!(mxy < 0.01, "color differs from baked: {mxy}");
    assert!((cct - 3800.0).abs() < 600.0, "As Shot temperature {cct:.0} K is far from 3800 K");
}

/// Hard link (same volume, no copy) or copy the clip into the test temp dir so the
/// `.sidecar` is written there, never next to the original footage.
fn scratch_clip(p: &Path) -> PathBuf {
    let dst = tmp("sidecar_clip.braw");
    let _ = std::fs::remove_file(&dst);
    let _ = std::fs::remove_file(dst.with_extension("sidecar"));
    if std::fs::hard_link(p, &dst).is_err() {
        std::fs::copy(p, &dst).unwrap();
    }
    dst
}

#[test]
fn sidecar_round_trip() {
    let Some(p) = clip() else { return };
    let path = scratch_clip(&p);
    let side = path.with_extension("sidecar");
    let original = rf_braw::BrawSource::open(&path).unwrap();
    let a0 = original.info().as_shot.clone();
    assert!(!original.sidecar_attached());
    let mut color = corrected();
    color.highlight_recovery = Some(true);
    original.write_sidecar(&color).unwrap();
    assert!(side.exists());

    // Reopened, the clip's "as shot" is now the correction; decoding with no overrides
    // gives the corrected picture.
    let reopened = rf_braw::BrawSource::open(&path).unwrap();
    assert!(reopened.sidecar_attached());
    let a = &reopened.info().as_shot;
    assert_eq!((a.iso, a.kelvin, a.tint, a.highlight_recovery), (Some(800), Some(3800), Some(8), Some(true)));
    assert!((a.exposure.unwrap() - 0.7).abs() < 1e-4);
    assert_eq!((a.gamma.clone(), a.gamut.clone(), a.color_science_gen), (a0.gamma.clone(), a0.gamut.clone(), a0.color_science_gen));
    let small = |s: &dyn MediaSource, c: &DecodeColor| match s.decode(&DecodeRequest { frame: 80, scale: ResolutionScale::Eighth, format: PixelFormat::RgbF32, color: c.clone() }).unwrap().pixels {
        Pixels::RgbF32(v) => v,
        _ => unreachable!(),
    };
    assert_eq!(small(&reopened, &DecodeColor::default()), small(&original, &color), "frame 80 must use the sidecar values too");

    // Revert: camera original values and no sidecar file left behind.
    reopened.clear_sidecar().unwrap();
    drop((original, reopened));
    let back = rf_braw::BrawSource::open(&path).unwrap();
    let b = &back.info().as_shot;
    assert_eq!((b.iso, b.kelvin, b.tint, b.exposure, b.highlight_recovery), (a0.iso, a0.kelvin, a0.tint, a0.exposure, a0.highlight_recovery));
    assert!(!side.exists(), "empty sidecar should be removed");
}

/// Uncompressed 16-bit RGB TIFF from the graded export -> samples.
fn read_tiff16(path: &Path) -> (u32, u32, Vec<u16>) {
    let b = std::fs::read(path).unwrap();
    let ifd = rf_dng::read::read_ifd0(&b).unwrap();
    let w = ifd.get(&256).unwrap().ints()[0];
    let h = ifd.get(&257).unwrap().ints()[0];
    assert_eq!(ifd.get(&259).unwrap().ints()[0], 1, "test reads uncompressed TIFFs");
    let mut px = Vec::new();
    for (o, l) in ifd.get(&273).unwrap().ints().iter().zip(ifd.get(&279).unwrap().ints()) {
        px.extend(b[*o as usize..(*o + l) as usize].chunks(2).map(|c| u16::from_le_bytes([c[0], c[1]])));
    }
    (w, h, px)
}

/// A non-trivial 17³ look: per-channel S-curve plus a little channel crosstalk.
fn test_lut() -> rf_color::CubeLut {
    let n = 17;
    let mut t = format!("TITLE \"test look\"\nLUT_3D_SIZE {n}\n");
    let s = |x: f64| x * x * (3.0 - 2.0 * x);
    for b in 0..n {
        for g in 0..n {
            for r in 0..n {
                let (r, g, b) = (r as f64 / 16.0, g as f64 / 16.0, b as f64 / 16.0);
                let (r2, g2, b2) = (s(0.9 * r + 0.1 * g), s(g), s(0.85 * b + 0.15 * r));
                t += &format!("{r2:.6} {g2:.6} {b2:.6}\n");
            }
        }
    }
    rf_color::CubeLut::parse(&t).unwrap()
}

#[test]
fn graded_copy_matches_viewer() {
    let Some(p) = clip() else { return };
    let src = rf_braw::BrawSource::open(&p).unwrap();
    let color = corrected();
    let lut = test_lut();
    // What the viewer does: decode with the clip's own gamma (log) and run the view pipeline.
    let viewer_frame = src.decode(&DecodeRequest { frame: FRAME, scale: ResolutionScale::Full, format: PixelFormat::RgbF32, color: color.clone() }).unwrap();
    for placement in [rf_color::LutPlacement::OnLog, rf_color::LutPlacement::AfterCst] {
        let view = rf_color::ViewSettings { lut_placement: placement, exposure_ev: 0.3, ..Default::default() };
        let s = ExportSettings { graded: GradedFormat::Tiff16, raw: false, compress: false, ..settings(WhiteBalanceMode::Baked) };
        let path = tmp(&format!("graded_{placement:?}.tif"));
        let r = export_frame(&src, FRAME, &color, &s, Some(Look { view, lut: Some(&lut), decode: Some(&color) }), &path).expect("graded export");
        assert_eq!(r.path, path);
        let (w, h, got) = read_tiff16(&path);
        let (vw, vh, want) = rf_color::view::render_rgb16(&viewer_frame, view, Some(&lut));
        assert_eq!((w, h), (vw, vh));
        let mut d: Vec<u32> = got.iter().zip(&want).map(|(a, b)| (*a as i32 - *b as i32).unsigned_abs()).collect();
        d.sort_unstable();
        let (med, p99, max) = (d[d.len() / 2], d[d.len() * 99 / 100], d[d.len() - 1]);
        eprintln!("{placement:?}: |graded - viewer| in 16-bit codes: median {med}, p99 {p99}, max {max}");
        // One 8-bit step is 257 codes: the graded copy must be indistinguishable from the viewer.
        assert!(med <= 16 && p99 <= 257, "{placement:?}: median {med}, p99 {p99}");
    }
}

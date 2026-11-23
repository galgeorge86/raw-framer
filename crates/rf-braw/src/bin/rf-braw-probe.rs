//! Prints what the Blackmagic RAW SDK reports for a clip and times a few decodes.
//! Usage: rf-braw-probe <clip.braw> [frame]

use rf_braw::BrawSource;
use rf_core::*;
use std::time::Instant;

fn main() {
    let args: Vec<String> = std::env::args().collect();
    let Some(path) = args.get(1) else {
        eprintln!("usage: rf-braw-probe <clip.braw> [frame]");
        std::process::exit(2);
    };
    let frame: u64 = args.get(2).and_then(|s| s.parse().ok()).unwrap_or(0);
    println!("runtime: {:?}", rf_braw::find_runtime());
    // --pipelines: list usable decode pipelines; --pipeline cpu|cuda|opencl: decode with one.
    if args.iter().any(|a| a == "--pipelines") {
        for p in rf_braw::pipelines() {
            println!("pipeline {}: {}", p.id, p.name);
        }
    }
    if let Some(id) = args.iter().position(|a| a == "--pipeline").and_then(|i| args.get(i + 1)) {
        rf_braw::set_pipeline(id);
    }
    let t = Instant::now();
    let src = match BrawSource::open(std::path::Path::new(path)) {
        Ok(s) => s,
        Err(e) => {
            eprintln!("open failed: {e}");
            std::process::exit(1);
        }
    };
    println!("opened in {:?}, SDK {}, sidecar attached: {}, pipeline: {}", t.elapsed(), src.sdk_version(), src.sidecar_attached(), rf_braw::active_pipeline());
    // --sidecar F:key=value[,F:key=value...] (F = frame or "clip"; value "-" clears), then save.
    if let Some(i) = args.iter().position(|a| a == "--sidecar") {
        for item in args.get(i + 1).expect("--sidecar <F:key=value,...>").split(',') {
            let (f, kv) = item.split_once(':').expect("F:key=value");
            let (k, v) = kv.split_once('=').expect("key=value");
            let frame = (f != "clip").then(|| f.parse::<u64>().expect("frame"));
            let r = src.set_metadata(frame, k, (v != "-").then_some(v));
            println!("set {f}:{k}={v} -> {r:?}");
        }
        println!("save -> {:?}", src.save_sidecar());
        return;
    }
    let i = src.info();
    println!("{} {} | {}x{} @ {} fps | {} frames | TC0 {:?}", i.camera_make, i.camera_model, i.width, i.height, i.fps, i.frame_count, i.start_timecode);
    println!("as shot: {:#?}", i.as_shot);
    println!("caps: {:#?}", src.capabilities());
    println!("linear gamma: {:?}", src.linear_gamma_name());
    match src.audio_info() {
        Some(a) => {
            let t = Instant::now();
            let s = src.read_audio(0, a.sample_rate as usize).unwrap_or_default();
            let rms = (s.iter().map(|v| v * v).sum::<f32>() / s.len().max(1) as f32).sqrt();
            let peak = s.iter().fold(0f32, |m, v| m.max(v.abs()));
            println!("audio: {a:?} | first 1s: {} samples in {:?}, rms {rms:.4}, peak {peak:.4}", s.len(), t.elapsed());
        }
        None => println!("audio: none"),
    }
    println!("--- clip metadata");
    for (k, v) in src.metadata() {
        println!("  {k} = {v}");
    }
    println!("--- frame {frame} metadata: {}", src.frame_metadata(frame).map(|v| v.to_string()).unwrap_or_default());
    println!("--- shooting info: {:#?}", src.shooting_info(frame));

    for (scale, fmt) in [(ResolutionScale::Quarter, PixelFormat::RgbaF16), (ResolutionScale::Half, PixelFormat::RgbaF16), (ResolutionScale::Full, PixelFormat::RgbF32)] {
        let req = DecodeRequest { frame, scale, format: fmt, color: DecodeColor::default() };
        let t = Instant::now();
        match src.decode(&req) {
            Ok(f) => {
                let (mn, mx, mean) = stats(&f);
                println!("decode {:?} {:?}: {}x{} in {:?} | enc {:?} | min {mn:.4} max {mx:.4} mean {mean:.4}", scale, fmt, f.width, f.height, t.elapsed(), f.encoding);
            }
            Err(e) => println!("decode {:?} failed: {e}", scale),
        }
    }
    // --render out.ppm [--lut file.cube]: CPU reference render of the viewer (1/4 res).
    if let Some(i) = args.iter().position(|a| a == "--render") {
        let out = args.get(i + 1).expect("--render <out.ppm>");
        let lut = args.iter().position(|a| a == "--lut").and_then(|j| args.get(j + 1)).map(|p| rf_color::CubeLut::load(std::path::Path::new(p)).expect("bad LUT"));
        let req = DecodeRequest { frame, scale: if args.iter().any(|a| a == "--full") { ResolutionScale::Full } else { ResolutionScale::Quarter }, format: PixelFormat::RgbaF16, color: DecodeColor::default() };
        let f = src.decode(&req).expect("decode");
        let view = rf_color::ViewSettings::default();
        let (w, h, rgb) = rf_color::view::render_rgb8(&f, view, lut.as_ref(), f.width);
        let zeros = rgb.chunks(3).filter(|p| p == &[0, 0, 0]).count();
        let mut data = format!("P6\n{w} {h}\n255\n").into_bytes();
        data.extend(rgb);
        std::fs::write(out, data).expect("write");
        println!("rendered {w}x{h} to {out}; pure-black pixels: {zeros}");
        return;
    }
    if let Some(i) = args.iter().position(|a| a == "--dng") {
        let out = std::path::PathBuf::from(args.get(i + 1).expect("--dng <path>"));
        let mut s = rf_export::ExportSettings::default();
        for a in &args {
            match a.as_str() {
                "--f32" => s.sample = rf_export::Sample::Float32,
                "--u16" => s.sample = rf_export::Sample::UInt16,
                "--cdng" => s.kind = rf_export::ExportKind::CinemaDng,
                "--raw" => s.compress = false,
                "--jpg" => s.graded = rf_export::GradedFormat::Jpeg,
                "--tif" => s.graded = rf_export::GradedFormat::Tiff16,
                "--graded-only" => s.raw = false,
                _ => {}
            }
        }
        // Graded copy: Rec.709 Gamma 2.4, plus --lut (applied on the log signal) when given.
        let lut = args.iter().position(|a| a == "--lut").and_then(|j| args.get(j + 1)).map(|p| rf_color::CubeLut::load(std::path::Path::new(p)).expect("bad LUT"));
        let look = rf_export::Look { view: rf_color::ViewSettings { lut_enabled: lut.is_some(), ..Default::default() }, lut: lut.as_ref(), decode: None };
        let out = if s.writes_raw() { out } else { out.with_extension(s.graded.extension().unwrap_or("dng")) };
        match rf_export::export_frame(&src, frame, &DecodeColor::default(), &s, Some(look), &out) {
            Ok(r) => println!(
                "written: {}{} | {}x{} | {:.1} MB | baseline exposure {:+.2} EV | {} ms",
                r.path.display(),
                r.graded.as_ref().filter(|g| **g != r.path).map(|g| format!(" + {}", g.display())).unwrap_or_default(),
                r.width,
                r.height,
                r.bytes as f64 / 1e6,
                r.baseline_exposure,
                r.millis
            ),
            Err(e) => println!("DNG export failed: {e}"),
        }
        return;
    }
    if let Some(lin) = src.linear_gamma_name() {
        let mut color = DecodeColor::default();
        color.gamma = Some(lin);
        let req = DecodeRequest { frame, scale: ResolutionScale::Quarter, format: PixelFormat::RgbF32, color };
        match src.decode(&req) {
            Ok(f) => {
                let (mn, mx, mean) = stats(&f);
                println!("linear decode: enc {:?} | min {mn:.5} max {mx:.4} mean {mean:.4}", f.encoding);
            }
            Err(e) => println!("linear decode failed: {e}"),
        }
    }
}

fn stats(f: &DecodedFrame) -> (f32, f32, f32) {
    let vals: Vec<f32> = match &f.pixels {
        Pixels::RgbaF16(v) => v.chunks(4).flat_map(|c| [c[0].to_f32(), c[1].to_f32(), c[2].to_f32()]).collect(),
        Pixels::RgbF32(v) => v.clone(),
    };
    let bad = vals.iter().filter(|v| !v.is_finite()).count();
    let neg = vals.iter().filter(|v| **v < 0.0).count();
    if bad > 0 || neg > 0 {
        println!("  non-finite samples: {bad}, negative samples: {neg}");
    }
    let mn = vals.iter().cloned().fold(f32::MAX, f32::min);
    let mx = vals.iter().cloned().fold(f32::MIN, f32::max);
    let mean = vals.iter().sum::<f32>() / vals.len() as f32;
    (mn, mx, mean)
}

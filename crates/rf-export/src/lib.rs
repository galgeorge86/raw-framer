//! Export one frame of any [`MediaSource`] as a Linear DNG / CinemaDNG.
//! Steps follow .claude/COLOR_PIPELINE.md §4 and .claude/DNG_EXPORT.md.

use rayon::prelude::*;
use rf_color::matrix::{bradford, D50};
use rf_color::{primaries_of, CubeLut, ViewSettings};
use rf_core::*;
use rf_dng::*;
use serde::{Deserialize, Serialize};
use std::path::{Path, PathBuf};

type Result<T, E = ExportError> = std::result::Result<T, E>;

#[derive(Debug, thiserror::Error)]
pub enum ExportError {
    #[error("decode: {0}")]
    Decode(#[from] rf_core::Error),
    #[error("dng: {0}")]
    Dng(#[from] DngError),
    #[error("{0}")]
    Unsupported(String),
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, Default)]
pub enum ExportKind {
    /// Single DNG with preview, deflate. For Lightroom / Photoshop.
    #[default]
    Dng,
    /// CinemaDNG frame (raw in IFD0, timecode/frame-rate tags, fixed normalisation).
    CinemaDng,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, Default)]
pub enum Sample {
    #[default]
    Float16,
    Float32,
    UInt16,
}

impl Sample {
    pub const ALL: [Sample; 3] = [Self::Float16, Self::Float32, Self::UInt16];
    pub fn label(self) -> &'static str {
        match self {
            Self::Float16 => "16-bit float",
            Self::Float32 => "32-bit float",
            Self::UInt16 => "16-bit integer",
        }
    }
    fn dng(self) -> SampleType {
        match self {
            Self::Float16 => SampleType::F16,
            Self::Float32 => SampleType::F32,
            Self::UInt16 => SampleType::U16,
        }
    }
}

/// Size of the JPEG preview embedded in DNGs (what Windows Photos & co. display).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, Default)]
pub enum PreviewSize {
    #[default]
    Full,
    Px2048,
    None,
}

impl PreviewSize {
    pub const ALL: [PreviewSize; 3] = [Self::Full, Self::Px2048, Self::None];
    pub fn label(self) -> &'static str {
        match self {
            Self::Full => "Full size",
            Self::Px2048 => "2048 px",
            Self::None => "None",
        }
    }
}

/// How the chosen white balance reaches the DNG.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, Default)]
pub enum WhiteBalanceMode {
    /// Pixels are decoded with the chosen WB (exactly what the viewer shows); the DNG says
    /// "neutral", so Lightroom's As Shot is a generic daylight value.
    #[default]
    Baked,
    /// Pixels are decoded at a fixed 6500 K reference and the chosen WB is written as
    /// `AsShotNeutral`, so Lightroom's As Shot shows (approximately) your Kelvin and its
    /// Temp/Tint sliders start from there.
    Metadata,
}

impl WhiteBalanceMode {
    pub fn label(self) -> &'static str {
        match self {
            Self::Baked => "Baked",
            Self::Metadata => "Metadata",
        }
    }
}

/// The reference white balance used by [`WhiteBalanceMode::Metadata`] (D65-ish, matching
/// `CalibrationIlluminant1 = D65`).
pub const WB_REFERENCE: (u32, i16) = (6500, 0);

/// A display-referred copy of the frame, rendered exactly as the viewer shows it (output
/// transform, LUT, viewer exposure), written next to the DNG or on its own.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, Default)]
pub enum GradedFormat {
    #[default]
    None,
    /// 8-bit JPEG, quality 95, 4:4:4.
    Jpeg,
    /// 16-bit RGB TIFF (Deflate when compression is on).
    Tiff16,
}

impl GradedFormat {
    pub const ALL: [GradedFormat; 3] = [Self::None, Self::Jpeg, Self::Tiff16];
    pub fn label(self) -> &'static str {
        match self {
            Self::None => "None",
            Self::Jpeg => "JPEG",
            Self::Tiff16 => "TIFF 16-bit",
        }
    }
    pub fn extension(self) -> Option<&'static str> {
        match self {
            Self::None => None,
            Self::Jpeg => Some("jpg"),
            Self::Tiff16 => Some("tif"),
        }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(default)]
pub struct ExportSettings {
    pub kind: ExportKind,
    pub sample: Sample,
    pub compress: bool,
    pub white_balance: WhiteBalanceMode,
    pub output_dir: PathBuf,
    /// Tokens: {clip} {frame} {tc} {date}
    pub filename_template: String,
    pub overwrite: bool,
    pub preview: PreviewSize,
    /// Graded (viewer-look) copy to write as well.
    pub graded: GradedFormat,
    /// Write the DNG. Only honoured when a graded copy is chosen (something must be written).
    pub raw: bool,
}

impl ExportSettings {
    /// Whether the DNG / CinemaDNG is written.
    pub fn writes_raw(&self) -> bool {
        self.raw || self.graded == GradedFormat::None
    }
}

impl Default for ExportSettings {
    fn default() -> Self {
        Self {
            kind: ExportKind::Dng,
            sample: Sample::Float16,
            compress: true,
            white_balance: WhiteBalanceMode::Baked,
            output_dir: PathBuf::new(),
            filename_template: "{clip}_{frame}".into(),
            overwrite: false,
            preview: PreviewSize::Full,
            graded: GradedFormat::None,
            raw: true,
        }
    }
}

/// Render the embedded previews: a 256 px RGB thumbnail and (optionally) a JPEG of the frame
/// through the viewer pipeline, so quick viewers show what Raw Framer showed.
fn build_previews(frame: &DecodedFrame, view: ViewSettings, lut: Option<&CubeLut>, size: PreviewSize, date_time: Option<String>) -> Previews {
    let (tw, th, trgb) = rf_color::view::render_rgb8(frame, view, lut, 256);
    let jpeg = match size {
        PreviewSize::None => None,
        PreviewSize::Full | PreviewSize::Px2048 => {
            let max_w = if size == PreviewSize::Full { frame.width } else { 2048 };
            let (w, h, rgb) = rf_color::view::render_rgb8(frame, view, lut, max_w);
            let mut data = Vec::with_capacity(rgb.len() / 6);
            let mut enc = jpeg_encoder::Encoder::new(&mut data, 92);
            enc.set_sampling_factor(jpeg_encoder::SamplingFactor::F_2_2);
            match enc.encode(&rgb, w as u16, h as u16, jpeg_encoder::ColorType::Rgb) {
                Ok(()) => Some(JpegPreview { width: w, height: h, data, subsampling: [2, 2] }),
                Err(e) => {
                    log::warn!("JPEG preview encoding failed: {e}");
                    None
                }
            }
        }
    };
    Previews { thumbnail: Some(Thumbnail { width: tw, height: th, rgb: trgb }), jpeg, date_time }
}

#[derive(Debug, Clone)]
pub struct ExportReport {
    pub path: PathBuf,
    pub bytes: u64,
    pub width: u32,
    pub height: u32,
    pub baseline_exposure: f64,
    pub millis: u128,
    /// Set for [`WhiteBalanceMode::Metadata`].
    pub wb: Option<WbFit>,
    /// The graded copy, when one was written.
    pub graded: Option<PathBuf>,
}

/// The chosen white balance expressed as per-channel gains on the reference decode.
#[derive(Debug, Clone, Copy)]
pub struct WbFit {
    /// chosen ≈ gains × reference (camera-gamut linear).
    pub gains: [f64; 3],
    /// `AsShotNeutral` written to the DNG (= 1/gains, largest component 1).
    pub neutral: [f64; 3],
    /// RMS error of the per-channel model relative to the signal (0.01 = 1 %).
    pub residual: f64,
}

/// Fit the per-channel gains that turn a decode at the reference WB into a decode at the
/// chosen WB (1/8 resolution is plenty: the relation doesn't depend on detail).
pub fn fit_wb(src: &dyn MediaSource, frame: u64, chosen: &DecodeColor, reference: &DecodeColor) -> Result<WbFit, ExportError> {
    let dec = |c: &DecodeColor| -> Result<Vec<f32>, ExportError> {
        let f = src.decode(&DecodeRequest { frame, scale: ResolutionScale::Eighth, format: PixelFormat::RgbF32, color: c.clone() })?;
        match f.pixels {
            Pixels::RgbF32(v) => Ok(v),
            _ => Err(ExportError::Unsupported("expected RGB f32".into())),
        }
    };
    let (u, r) = (dec(chosen)?, dec(reference)?);
    // Ignore near-black (noise) and anything near clipping in either decode.
    let peak = r.iter().chain(u.iter()).cloned().fold(0f32, f32::max).max(1e-6);
    let usable = |i: usize| (0..3).all(|c| r[i * 3 + c] > peak * 1e-3 && r[i * 3 + c] < peak * 0.8 && u[i * 3 + c] < peak * 0.8);
    let n = r.len() / 3;
    let (mut ur, mut rr) = ([0f64; 3], [0f64; 3]);
    for i in (0..n).filter(|&i| usable(i)) {
        for c in 0..3 {
            ur[c] += u[i * 3 + c] as f64 * r[i * 3 + c] as f64;
            rr[c] += (r[i * 3 + c] as f64).powi(2);
        }
    }
    if rr.iter().any(|v| *v <= 0.0) {
        return Err(ExportError::Unsupported("frame too dark or clipped to measure white balance".into()));
    }
    let gains = [ur[0] / rr[0], ur[1] / rr[1], ur[2] / rr[2]];
    let (mut err, mut sig) = (0f64, 0f64);
    for i in (0..n).filter(|&i| usable(i)) {
        for c in 0..3 {
            let e = u[i * 3 + c] as f64 - gains[c] * r[i * 3 + c] as f64;
            err += e * e;
            sig += (u[i * 3 + c] as f64).powi(2);
        }
    }
    let inv = gains.map(|g| 1.0 / g.max(1e-9));
    let m = inv.iter().cloned().fold(0f64, f64::max);
    Ok(WbFit { gains, neutral: inv.map(|v| v / m), residual: (err / sig.max(1e-12)).sqrt() })
}

/// Build the output path for a frame (without creating anything).
pub fn output_path(src: &dyn MediaSource, frame: u64, s: &ExportSettings) -> PathBuf {
    let info = src.info();
    let clip = info.file_stem();
    let tc = src.timecode(frame).unwrap_or_default().replace([':', ';'], "-");
    let date = src.shooting_info(frame).date_time.unwrap_or_default();
    let date = date.split(' ').next().unwrap_or("").replace(':', "-");
    let mut name = s
        .filename_template
        .replace("{clip}", &clip)
        .replace("{frame}", &format!("{frame:06}"))
        .replace("{tc}", &tc)
        .replace("{date}", &date);
    name.retain(|c| !matches!(c, '<' | '>' | ':' | '"' | '/' | '\\' | '|' | '?' | '*'));
    if name.trim().is_empty() {
        name = format!("{clip}_{frame:06}");
    }
    let (dir, ext) = match (s.writes_raw(), s.kind) {
        (true, ExportKind::Dng) => (s.output_dir.clone(), "dng"),
        (true, ExportKind::CinemaDng) => (s.output_dir.join(format!("{clip}_CDNG")), "dng"),
        (false, _) => (s.output_dir.clone(), s.graded.extension().unwrap_or("dng")),
    };
    let mut p = dir.join(format!("{name}.{ext}"));
    if !s.overwrite {
        let taken = |p: &Path| p.exists() || graded_path(p, s).is_some_and(|g| g.exists());
        let mut i = 1;
        while taken(&p) {
            p = dir.join(format!("{name}_{i}.{ext}"));
            i += 1;
        }
    }
    p
}

/// Where the graded copy goes for an export whose main file is `main` (from [`output_path`]):
/// beside the DNG (CinemaDNG: in the export folder, keeping the sequence folder clean), or
/// `main` itself when only the graded copy is written.
pub fn graded_path(main: &Path, s: &ExportSettings) -> Option<PathBuf> {
    let ext = s.graded.extension()?;
    if !s.writes_raw() {
        return Some(main.to_path_buf());
    }
    let p = main.with_extension(ext);
    Some(match s.kind {
        ExportKind::Dng => p,
        ExportKind::CinemaDng => s.output_dir.join(p.file_name()?),
    })
}

/// Decode settings for a linear, BMD-WG export decode based on the user's choices.
pub fn linear_decode_color(src: &dyn MediaSource, user: &DecodeColor) -> Result<(DecodeColor, bool), ExportError> {
    let caps = src.capabilities();
    let mut c = user.clone();
    // Camera gamut (keeps the widest data); fall back to whatever the user chose.
    if let Some(g) = caps.gamuts.iter().find(|g| rf_color::classify_gamut(g) == Gamut::BmdWideGamut) {
        c.gamut = Some(g.clone());
    }
    // Never bake a look LUT into raw data.
    if caps.post3dlut_modes.iter().any(|m| m == "Disabled") {
        c.post3dlut_mode = Some("Disabled".into());
    }
    if let Some(lin) = src.linear_gamma_name() {
        c.gamma = Some(lin);
        return Ok((c, false));
    }
    // Fallback: BMD Film Gen 5 and invert the published curve ourselves.
    let film = caps
        .gammas
        .iter()
        .find(|g| g.to_ascii_lowercase().contains("film") && !g.to_ascii_lowercase().contains("video"))
        .cloned()
        .ok_or_else(|| ExportError::Unsupported("decoder offers neither a linear nor a Film gamma".into()))?;
    if !caps.color_science_gens.contains(&5) && !caps.color_science_gens.is_empty() {
        return Err(ExportError::Unsupported("no linear gamma and no Gen 5 color science available".into()));
    }
    c.gamma = Some(film);
    if !caps.color_science_gens.is_empty() {
        c.color_science_gen = Some(5);
    }
    Ok((c, true))
}

fn fps_rational(fps: f32) -> (i32, i32) {
    let f = fps as f64;
    let ntsc = f * 1.001;
    if (ntsc - ntsc.round()).abs() < 0.01 && (f - f.round()).abs() > 0.01 {
        ((ntsc.round() as i32) * 1000, 1001)
    } else {
        ((f * 1000.0).round() as i32, 1000)
    }
}

fn describe(src: &dyn MediaSource, c: &DecodeColor) -> String {
    let a = &src.info().as_shot;
    let mut parts = vec![format!("Raw Framer linear export from {}", src.info().file_stem())];
    if let Some(k) = c.kelvin.or(a.kelvin) {
        parts.push(format!("WB {k}K"));
    }
    if let Some(t) = c.tint.or(a.tint) {
        parts.push(format!("tint {t}"));
    }
    if let Some(i) = c.iso.or(a.iso) {
        parts.push(format!("ISO {i}"));
    }
    if let Some(e) = c.exposure().or(a.exposure) {
        parts.push(format!("exposure {e:+.2}"));
    }
    if let Some(g) = c.color_science_gen.or(a.color_science_gen) {
        parts.push(format!("color science Gen {g}"));
    }
    parts.join(", ")
}

/// How the viewer shows a frame: output transform, LUT, and the decode settings the viewer
/// asks the SDK for (they can differ from the export decode, e.g. the clip's own log curve
/// feeding a LUT). Used for the embedded previews and the graded copy.
#[derive(Clone, Copy)]
pub struct Look<'a> {
    pub view: ViewSettings,
    pub lut: Option<&'a CubeLut>,
    /// `None`: the user's decode settings as they are.
    pub decode: Option<&'a DecodeColor>,
}

/// Whether the linear export decode, through the viewer pipeline, renders exactly what a
/// decode with `viewer` settings would (Film Gen 5 / linear in BMD Wide Gamut, no clip LUT).
fn linear_decode_is_viewer_equivalent(src: &dyn MediaSource, viewer: &DecodeColor) -> bool {
    let r = viewer.resolved(&src.info().as_shot);
    let lut_off = r.post3dlut_mode.as_deref().is_none_or(|m| m.eq_ignore_ascii_case("disabled"));
    let t = rf_color::classify_gamma(r.gamma.as_deref().unwrap_or(""), r.color_science_gen);
    let g = rf_color::classify_gamut(r.gamut.as_deref().unwrap_or(""));
    lut_off && matches!(t, Transfer::Linear | Transfer::BmdFilmGen5) && g == Gamut::BmdWideGamut
}

/// Render `f` through the viewer pipeline and write it as the graded JPEG / TIFF at `path`.
/// Tagged with the ICC profile of the viewer's output space. Returns the file size.
fn write_graded(
    src: &dyn MediaSource,
    frame: u64,
    f: &DecodedFrame,
    settings: &ExportSettings,
    view: ViewSettings,
    lut: Option<&CubeLut>,
    path: &Path,
) -> Result<u64, ExportError> {
    let icc = rf_color::icc::display_profile(view.output);
    let mut desc = format!("Raw Framer graded export from {}, frame {frame}: {}", src.info().file_stem(), view.output.label());
    if let Some(l) = lut.filter(|_| view.lut_enabled) {
        let name = if l.name.is_empty() { l.title.clone() } else { l.name.clone() };
        desc.push_str(&format!(", LUT {}", if name.is_empty() { "(unnamed)" } else { &name }));
        if view.lut_mix < 1.0 {
            desc.push_str(&format!(" at {:.0} %", view.lut_mix * 100.0));
        }
    }
    if view.exposure_ev != 0.0 {
        desc.push_str(&format!(", viewer exposure {:+.2}", view.exposure_ev));
    }
    if let Some(dir) = path.parent() {
        std::fs::create_dir_all(dir).map_err(DngError::Io)?;
    }
    let date_time = src.shooting_info(frame).date_time;
    let bytes = match settings.graded {
        GradedFormat::None => return Err(ExportError::Unsupported("no graded format chosen".into())),
        GradedFormat::Jpeg => {
            let (w, h, rgb) = rf_color::view::render_rgb8(f, view, lut, f.width);
            if w > u16::MAX as u32 || h > u16::MAX as u32 {
                return Err(ExportError::Unsupported(format!("{w}×{h} is too large for JPEG")));
            }
            let mut data = Vec::with_capacity(rgb.len() / 4);
            let mut enc = jpeg_encoder::Encoder::new(&mut data, 95);
            enc.set_sampling_factor(jpeg_encoder::SamplingFactor::F_1_1);
            if let Some(p) = &icc {
                enc.add_icc_profile(p).map_err(|e| ExportError::Unsupported(format!("JPEG: {e}")))?;
            }
            enc.encode(&rgb, w as u16, h as u16, jpeg_encoder::ColorType::Rgb).map_err(|e| ExportError::Unsupported(format!("JPEG: {e}")))?;
            data
        }
        GradedFormat::Tiff16 => {
            let (w, h, rgb) = rf_color::view::render_rgb16(f, view, lut);
            let meta = rf_dng::rgb::RgbTiffMeta {
                description: Some(desc),
                software: Some(format!("Raw Framer {}", env!("CARGO_PKG_VERSION"))),
                date_time,
                icc_profile: icc,
            };
            rf_dng::rgb::encode_rgb16_tiff(w, h, &rgb, &meta, settings.compress)?
        }
    };
    Ok(rf_dng::rgb::write_file_atomic(path, &bytes)?)
}

/// Decode `frame` at full resolution and write it as a DNG (and/or the graded copy).
/// `look` renders the embedded preview and the graded copy the way the viewer shows them.
pub fn export_frame(
    src: &dyn MediaSource,
    frame: u64,
    user_color: &DecodeColor,
    settings: &ExportSettings,
    look: Option<Look>,
    path: &Path,
) -> Result<ExportReport, ExportError> {
    let t0 = std::time::Instant::now();
    let raw = settings.writes_raw();
    let (mut color, invert_film) = linear_decode_color(src, user_color)?;
    let mut description = describe(src, &color);
    // Editable WB: decode at the reference and carry the chosen WB as AsShotNeutral. Needs
    // a WB control and linear output from the decoder (the gain fit assumes linear data).
    let wb_capable = src.capabilities().kelvin_range.is_some() && !invert_film;
    if raw && settings.white_balance == WhiteBalanceMode::Metadata && !wb_capable {
        log::warn!("editable white balance isn't available for this clip; baking it instead");
    }
    let wb = if raw && settings.white_balance == WhiteBalanceMode::Metadata && wb_capable {
        let mut reference = color.clone();
        reference.kelvin = Some(WB_REFERENCE.0);
        reference.tint = Some(WB_REFERENCE.1);
        let fit = fit_wb(src, frame, &color, &reference)?;
        log::info!("editable WB: gains {:?}, AsShotNeutral {:?}, model error {:.2} %", fit.gains, fit.neutral, fit.residual * 100.0);
        description.push_str(", white balance as DNG metadata");
        color = reference;
        Some(fit)
    } else {
        None
    };
    let req = DecodeRequest { frame, scale: ResolutionScale::Full, format: PixelFormat::RgbF32, color: color.clone() };
    let mut decoded = src.decode(&req)?;

    // Make sure we really have scene-linear data.
    match (&decoded.encoding.transfer, invert_film) {
        (Transfer::Linear, _) => {}
        (Transfer::BmdFilmGen5, true) => {
            if let Pixels::RgbF32(v) = &mut decoded.pixels {
                v.par_iter_mut().for_each(|x| *x = rf_color::transfer::film_gen5_decode_f64(*x as f64) as f32);
            }
            decoded.encoding.transfer = Transfer::Linear;
        }
        (t, _) => return Err(ExportError::Unsupported(format!("decoder returned non-linear data ({t:?})"))),
    }
    let prim = primaries_of(&decoded.encoding.gamut)
        .ok_or_else(|| ExportError::Unsupported(format!("unknown gamut {:?}", decoded.encoding.gamut)))?;

    // What the viewer showed: the chosen WB (Metadata mode decodes at the reference).
    let shown = wb.map(|fit| {
        let mut f = decoded.clone();
        if let Pixels::RgbF32(v) = &mut f.pixels {
            let g = fit.gains.map(|g| g as f32);
            v.par_chunks_mut(3).for_each(|p| {
                p[0] *= g[0];
                p[1] *= g[1];
                p[2] *= g[2];
            });
        }
        f
    });
    let (view, lut) = look.map_or((ViewSettings { lut_enabled: false, ..Default::default() }, None), |l| (l.view, l.lut));

    // Previews before normalisation (linear frame through the viewer pipeline).
    let previews = (raw && settings.kind == ExportKind::Dng).then(|| {
        // "YYYY:MM:DD HH:MM:SS" -> "YYYY-MM-DDTHH:MM:SS"
        let iso = src.shooting_info(frame).date_time.map(|d| {
            let (date, time) = d.split_once(' ').unwrap_or((&d, "00:00:00"));
            format!("{}T{time}", date.replace(':', "-"))
        });
        build_previews(shown.as_ref().unwrap_or(&decoded), view, lut, settings.preview, iso)
    });

    // Graded copy, rendered like the viewer. When the viewer decodes differently (the clip's
    // own log curve into a LUT, the clip LUT, a non-WG gamut), render from that same decode.
    let graded = match graded_path(path, settings) {
        None => None,
        Some(gp) => {
            let viewer_color = look.and_then(|l| l.decode).unwrap_or(user_color);
            let own;
            let f = if !linear_decode_is_viewer_equivalent(src, viewer_color) {
                log::info!("graded copy: decoding with the viewer's settings");
                own = src.decode(&DecodeRequest { frame, scale: ResolutionScale::Full, format: PixelFormat::RgbF32, color: viewer_color.clone() })?;
                &own
            } else {
                shown.as_ref().unwrap_or(&decoded)
            };
            let bytes = write_graded(src, frame, f, settings, view, lut, &gp)?;
            Some((gp, bytes))
        }
    };
    drop(shown);
    if !raw {
        let (gp, bytes) = graded.expect("graded-only export has a graded path");
        let report = ExportReport {
            path: gp.clone(),
            bytes,
            width: decoded.width,
            height: decoded.height,
            baseline_exposure: 0.0,
            millis: t0.elapsed().as_millis(),
            wb: None,
            graded: Some(gp),
        };
        log::info!("exported {} ({} MB, {} ms)", path.display(), bytes / 1_000_000, report.millis);
        return Ok(report);
    }

    let (width, height) = (decoded.width, decoded.height);
    let Pixels::RgbF32(mut data) = decoded.pixels else {
        return Err(ExportError::Unsupported("expected RGB f32 pixels".into()));
    };
    let norm = match settings.kind {
        ExportKind::Dng => Normalization::Auto,
        ExportKind::CinemaDng => Normalization::Fixed(rf_color::transfer::film_gen5::LINEAR_MAX as f32),
    };
    let mut baseline_exposure = normalize(&mut data, norm);
    // A DNG reader white-balances by dividing by AsShotNeutral (largest component 1), which
    // equals the chosen-WB decode divided by min(gains): compensate so brightness matches.
    if let Some(fit) = &wb {
        baseline_exposure += fit.gains.iter().cloned().fold(f64::MAX, f64::min).log2();
    }

    let to_xyz = prim.rgb_to_xyz();
    let color_matrix1 = to_xyz.inverse().flat();
    let forward_matrix1 = bradford(prim.white, D50).mul(&to_xyz).flat();

    let info = src.info();
    let shoot = src.shooting_info(frame);
    let cinema = (settings.kind == ExportKind::CinemaDng).then(|| CinemaInfo {
        frame_rate: fps_rational(info.fps),
        timecode: src.timecode(frame).as_deref().and_then(parse_timecode).map(|(h, m, s, f, d)| Timecode {
            hours: h,
            minutes: m,
            seconds: s,
            frames: f,
            drop_frame: d,
        }),
        reel_name: shoot.reel.clone(),
        camera_label: None,
    });
    let meta = DngMeta {
        make: info.camera_make.clone(),
        model: info.camera_model.clone(),
        unique_camera_model: format!("{} {} (Raw Framer Linear)", info.camera_make, info.camera_model).trim().to_string(),
        date_time: shoot.date_time.clone(),
        image_description: Some(description),
        original_raw_file_name: info.path.file_name().map(|s| s.to_string_lossy().into_owned()),
        camera_serial: shoot.camera_serial.clone(),
        color_matrix1,
        forward_matrix1: Some(forward_matrix1),
        calibration_illuminant1: 21,
        as_shot_neutral: wb.map(|f| f.neutral).unwrap_or([1.0, 1.0, 1.0]),
        baseline_exposure,
        profile_name: "Raw Framer Linear (camera gamut)".into(),
        exif: ExifInfo {
            exposure_time_s: shoot.exposure_time_s,
            f_number: shoot.f_number,
            iso: color.iso.or(info.as_shot.iso).or(shoot.iso),
            date_time_original: shoot.date_time.clone(),
            focal_length_mm: shoot.focal_length_mm,
            lens_model: shoot.lens_model.clone(),
            body_serial: shoot.camera_serial.clone(),
        },
        cinema,
        ..Default::default()
    };
    let opts = DngOptions {
        sample: settings.sample.dng(),
        compression: if settings.compress { Compression::Deflate } else { Compression::None },
        layout: match settings.kind {
            ExportKind::Dng => Layout::PreviewInIfd0,
            ExportKind::CinemaDng => Layout::RawInIfd0,
        },
        ..Default::default()
    };
    if let Some(dir) = path.parent() {
        std::fs::create_dir_all(dir).map_err(DngError::Io)?;
    }
    let img = LinearImage { width, height, data: &data };
    let mut bytes = write_dng_file(path, &img, previews.as_ref(), &meta, &opts)?;
    bytes += graded.as_ref().map_or(0, |g| g.1);
    let report = ExportReport {
        path: path.to_path_buf(),
        bytes,
        width,
        height,
        baseline_exposure,
        millis: t0.elapsed().as_millis(),
        wb,
        graded: graded.map(|g| g.0),
    };
    log::info!("exported {} ({} MB, {} ms)", path.display(), bytes / 1_000_000, report.millis);
    Ok(report)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn fps() {
        assert_eq!(fps_rational(23.976), (24000, 1001));
        assert_eq!(fps_rational(23.976028), (24000, 1001));
        assert_eq!(fps_rational(24.0), (24000, 1000));
        assert_eq!(fps_rational(29.97), (30000, 1001));
        assert_eq!(fps_rational(25.0), (25000, 1000));
    }

    #[test]
    fn export_test_pattern() {
        let dir = std::env::temp_dir().join(format!("rfexp_{}", std::process::id()));
        let src = TestPatternSource::new("pattern.rfpattern".into());
        let s = ExportSettings { output_dir: dir.clone(), ..Default::default() };
        let p = output_path(&src, 12, &s);
        assert!(p.ends_with("pattern_000012.dng"));
        let r = export_frame(&src, 12, &DecodeColor::default(), &s, None, &p).unwrap();
        assert_eq!((r.width, r.height), (1920, 1080));
        assert!(r.baseline_exposure > 0.0, "highlight bar at 40.0 should set the peak");
        // CinemaDNG goes to a sub-folder
        let s2 = ExportSettings { kind: ExportKind::CinemaDng, output_dir: dir.clone(), ..Default::default() };
        let p2 = output_path(&src, 3, &s2);
        assert!(p2.parent().unwrap().ends_with("pattern_CDNG"));
        export_frame(&src, 3, &DecodeColor::default(), &s2, None, &p2).unwrap();
        std::fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn graded_copies() {
        let dir = std::env::temp_dir().join(format!("rfgraded_{}", std::process::id()));
        let src = TestPatternSource::new("pattern.rfpattern".into());
        let view = ViewSettings { lut_enabled: false, ..Default::default() };
        // DNG + 16-bit TIFF beside it, tagged Rec.709 Gamma 2.4.
        let s = ExportSettings { output_dir: dir.clone(), graded: GradedFormat::Tiff16, ..Default::default() };
        let p = output_path(&src, 5, &s);
        let r = export_frame(&src, 5, &DecodeColor::default(), &s, Some(Look { view, lut: None, decode: None }), &p).unwrap();
        assert!(p.extension().unwrap() == "dng" && p.exists());
        let g = r.graded.clone().unwrap();
        assert_eq!(g, p.with_extension("tif"));
        let b = std::fs::read(&g).unwrap();
        let ifd = rf_dng::read::read_ifd0(&b).unwrap();
        assert_eq!(ifd.get(&258).unwrap().ints(), vec![16, 16, 16]);
        assert_eq!(&ifd.get(&34675).unwrap().raw[36..40], b"acsp");
        // The next export of the same frame doesn't collide with either file.
        let p_again = output_path(&src, 5, &s);
        assert_ne!(p_again, p);
        // JPEG only: no DNG at all.
        let s2 = ExportSettings { output_dir: dir.clone(), graded: GradedFormat::Jpeg, raw: false, ..Default::default() };
        let p2 = output_path(&src, 6, &s2);
        assert_eq!(p2.extension().unwrap(), "jpg");
        let r2 = export_frame(&src, 6, &DecodeColor::default(), &s2, Some(Look { view, lut: None, decode: None }), &p2).unwrap();
        assert_eq!(r2.path, p2);
        assert!(!p2.with_extension("dng").exists());
        let jpg = std::fs::read(&p2).unwrap();
        assert_eq!(&jpg[0..2], &[0xFF, 0xD8]);
        assert!(jpg.windows(12).any(|w| w == b"ICC_PROFILE\0"), "JPEG carries the ICC profile");
        // CinemaDNG keeps its sequence folder clean: the graded copy goes in the export folder.
        let s3 = ExportSettings { kind: ExportKind::CinemaDng, output_dir: dir.clone(), graded: GradedFormat::Jpeg, ..Default::default() };
        let p3 = output_path(&src, 7, &s3);
        let r3 = export_frame(&src, 7, &DecodeColor::default(), &s3, Some(Look { view, lut: None, decode: None }), &p3).unwrap();
        assert_eq!(r3.graded.unwrap().parent().unwrap(), dir.as_path());
        // "Graded only" with no graded format still writes the DNG.
        let s4 = ExportSettings { raw: false, ..Default::default() };
        assert!(s4.writes_raw());
        std::fs::remove_dir_all(&dir).ok();
    }
}

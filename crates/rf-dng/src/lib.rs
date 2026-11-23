//! Linear DNG / CinemaDNG writer. See .claude/DNG_EXPORT.md for the file layout and tag choices.
//!
//! Input is scene-linear RGB (f32, interleaved), already white balanced, in the camera
//! gamut described by `color_matrix1`. Use [`normalize`] first to fit it into [0, 1] and
//! get the matching `baseline_exposure`.

pub mod tiff;
pub mod read;
pub mod rgb;

use rayon::prelude::*;
use std::io::Write;
use std::path::Path;
use tiff::*;

#[derive(Debug, thiserror::Error)]
pub enum DngError {
    #[error("io: {0}")]
    Io(#[from] std::io::Error),
    #[error("invalid input: {0}")]
    Invalid(String),
    #[error("file too large for DNG (over 4 GB)")]
    TooLarge,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum SampleType {
    /// IEEE half float (DNG 1.4). Default: huge range, half the size of f32.
    #[default]
    F16,
    F32,
    /// 16-bit unsigned integer, for readers without float DNG support.
    U16,
}

impl SampleType {
    pub fn bits(self) -> u16 {
        match self {
            Self::F16 | Self::U16 => 16,
            Self::F32 => 32,
        }
    }
    pub fn is_float(self) -> bool {
        !matches!(self, Self::U16)
    }
    pub fn bytes(self) -> usize {
        self.bits() as usize / 8
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum Compression {
    None,
    /// Adobe Deflate (zlib) with the TIFF floating-point / horizontal predictor.
    #[default]
    Deflate,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum Layout {
    /// IFD0 = 8-bit RGB preview, full-res raw in a SubIFD (standard DNG).
    #[default]
    PreviewInIfd0,
    /// Full-res raw in IFD0 (CinemaDNG style).
    RawInIfd0,
}

#[derive(Debug, Clone)]
pub struct DngOptions {
    pub sample: SampleType,
    pub compression: Compression,
    pub layout: Layout,
    pub tile_size: u32,
    pub deflate_level: u32,
}

impl Default for DngOptions {
    fn default() -> Self {
        Self { sample: SampleType::F16, compression: Compression::Deflate, layout: Layout::PreviewInIfd0, tile_size: 256, deflate_level: 5 }
    }
}

#[derive(Debug, Clone)]
pub struct Thumbnail {
    pub width: u32,
    pub height: u32,
    pub rgb: Vec<u8>,
}

#[derive(Debug, Clone, Copy)]
pub struct Timecode {
    pub hours: u8,
    pub minutes: u8,
    pub seconds: u8,
    pub frames: u8,
    pub drop_frame: bool,
}

impl Timecode {
    /// SMPTE 12M packed BCD as used by the CinemaDNG TimeCodes tag.
    pub fn to_smpte_bytes(&self) -> [u8; 8] {
        let bcd = |v: u8| ((v / 10) << 4) | (v % 10);
        let mut f = bcd(self.frames) & 0x3F;
        if self.drop_frame {
            f |= 0x40;
        }
        [f, bcd(self.seconds) & 0x7F, bcd(self.minutes) & 0x7F, bcd(self.hours) & 0x3F, 0, 0, 0, 0]
    }
}

#[derive(Debug, Clone, Default)]
pub struct CinemaInfo {
    pub frame_rate: (i32, i32),
    pub timecode: Option<Timecode>,
    pub reel_name: Option<String>,
    pub camera_label: Option<String>,
}

#[derive(Debug, Clone, Default)]
pub struct ExifInfo {
    pub exposure_time_s: Option<f64>,
    pub f_number: Option<f64>,
    pub iso: Option<u32>,
    pub date_time_original: Option<String>,
    pub focal_length_mm: Option<f64>,
    pub lens_model: Option<String>,
    pub body_serial: Option<String>,
}

#[derive(Debug, Clone)]
pub struct DngMeta {
    pub make: String,
    pub model: String,
    pub unique_camera_model: String,
    pub software: String,
    /// "YYYY:MM:DD HH:MM:SS"
    pub date_time: Option<String>,
    pub image_description: Option<String>,
    pub original_raw_file_name: Option<String>,
    pub camera_serial: Option<String>,
    /// XYZ -> camera, row-major.
    pub color_matrix1: [f64; 9],
    /// White-balanced camera -> XYZ D50, row-major.
    pub forward_matrix1: Option<[f64; 9]>,
    /// EXIF LightSource code (21 = D65).
    pub calibration_illuminant1: u16,
    pub as_shot_neutral: [f64; 3],
    pub baseline_exposure: f64,
    pub profile_name: String,
    pub exif: ExifInfo,
    pub cinema: Option<CinemaInfo>,
}

impl Default for DngMeta {
    fn default() -> Self {
        Self {
            make: String::new(),
            model: String::new(),
            unique_camera_model: "Raw Framer Linear".into(),
            software: format!("Raw Framer {}", env!("CARGO_PKG_VERSION")),
            date_time: None,
            image_description: None,
            original_raw_file_name: None,
            camera_serial: None,
            color_matrix1: [1.0, 0.0, 0.0, 0.0, 1.0, 0.0, 0.0, 0.0, 1.0],
            forward_matrix1: None,
            calibration_illuminant1: 21,
            as_shot_neutral: [1.0, 1.0, 1.0],
            baseline_exposure: 0.0,
            profile_name: "Raw Framer Linear".into(),
            exif: ExifInfo::default(),
            cinema: None,
        }
    }
}

/// Scene-linear RGB image, interleaved, `width * height * 3` samples.
pub struct LinearImage<'a> {
    pub width: u32,
    pub height: u32,
    pub data: &'a [f32],
}

#[derive(Debug, Clone, Copy, PartialEq)]
pub enum Normalization {
    /// Scale by the frame's robust peak (99.9999th percentile).
    Auto,
    /// Scale by a fixed peak (e.g. 222.86, the top of BMD Film Gen 5), consistent across frames.
    Fixed(f32),
}

/// Clamp to [0, peak], divide by peak. Returns the DNG BaselineExposure (= log2(peak)).
/// Negative values / NaNs become 0. Peaks below 1.0 are not boosted.
pub fn normalize(data: &mut [f32], mode: Normalization) -> f64 {
    for v in data.iter_mut() {
        if !v.is_finite() || *v < 0.0 {
            *v = 0.0;
        }
    }
    let peak = match mode {
        Normalization::Fixed(p) => p.max(1e-6),
        Normalization::Auto => robust_peak(data).max(1.0),
    };
    let inv = 1.0 / peak;
    data.par_iter_mut().for_each(|v| *v = (*v * inv).min(1.0));
    (peak as f64).log2()
}

fn robust_peak(data: &[f32]) -> f32 {
    const LO: f32 = -24.0;
    const HI: f32 = 12.0;
    const BINS: usize = 36 * 64;
    let hist = data
        .par_chunks(1 << 16)
        .map(|c| {
            let mut h = vec![0u64; BINS];
            for &v in c {
                if v > 0.0 {
                    let t = ((v.log2() - LO) / (HI - LO) * BINS as f32).clamp(0.0, BINS as f32 - 1.0);
                    h[t as usize] += 1;
                }
            }
            h
        })
        .reduce(|| vec![0u64; BINS], |mut a, b| {
            a.iter_mut().zip(b).for_each(|(x, y)| *x += y);
            a
        });
    let allowed = (data.len() as f64 * 1e-6) as u64;
    let mut acc = 0u64;
    for i in (0..BINS).rev() {
        acc += hist[i];
        if acc > allowed {
            return 2f32.powf(LO + (i + 1) as f32 / BINS as f32 * (HI - LO));
        }
    }
    1.0
}

// ---------------------------------------------------------------- tile encoding

fn tile_samples(img: &LinearImage, tx: u32, ty: u32, ts: u32, sample: SampleType) -> Vec<u8> {
    let bps = sample.bytes();
    let mut out = vec![0u8; (ts * ts * 3) as usize * bps];
    for y in 0..ts {
        let iy = ty * ts + y;
        if iy >= img.height {
            break;
        }
        for x in 0..ts {
            let ix = tx * ts + x;
            if ix >= img.width {
                break;
            }
            let src = ((iy * img.width + ix) * 3) as usize;
            let dst = ((y * ts + x) * 3) as usize * bps;
            for c in 0..3 {
                let v = img.data[src + c];
                let o = dst + c * bps;
                match sample {
                    SampleType::F16 => out[o..o + 2].copy_from_slice(&half::f16::from_f32(v).to_le_bytes()),
                    SampleType::F32 => out[o..o + 4].copy_from_slice(&v.to_le_bytes()),
                    SampleType::U16 => {
                        let q = (v.clamp(0.0, 1.0) * 65535.0 + 0.5) as u16;
                        out[o..o + 2].copy_from_slice(&q.to_le_bytes())
                    }
                }
            }
        }
    }
    out
}

/// TIFF Tech Note 3 floating-point predictor, applied per row in place.
pub fn fp_predict_row(row: &mut [u8], bps: usize, spp: usize) {
    let wc = row.len() / bps;
    let tmp = row.to_vec();
    for count in 0..wc {
        for byte in 0..bps {
            // little-endian source; byte plane 0 = most significant byte
            row[(bps - byte - 1) * wc + count] = tmp[bps * count + byte];
        }
    }
    for j in (spp..row.len()).rev() {
        row[j] = row[j].wrapping_sub(row[j - spp]);
    }
}

pub fn fp_unpredict_row(row: &mut [u8], bps: usize, spp: usize) {
    for j in spp..row.len() {
        row[j] = row[j].wrapping_add(row[j - spp]);
    }
    let wc = row.len() / bps;
    let tmp = row.to_vec();
    for count in 0..wc {
        for byte in 0..bps {
            row[bps * count + byte] = tmp[(bps - byte - 1) * wc + count];
        }
    }
}

/// Horizontal differencing (Predictor = 2) on 16-bit LE samples, per row in place.
pub fn hdiff_u16_row(row: &mut [u8], spp: usize) {
    let n = row.len() / 2;
    for i in (spp..n).rev() {
        let a = u16::from_le_bytes([row[i * 2], row[i * 2 + 1]]);
        let b = u16::from_le_bytes([row[(i - spp) * 2], row[(i - spp) * 2 + 1]]);
        row[i * 2..i * 2 + 2].copy_from_slice(&a.wrapping_sub(b).to_le_bytes());
    }
}

pub fn hundiff_u16_row(row: &mut [u8], spp: usize) {
    let n = row.len() / 2;
    for i in spp..n {
        let a = u16::from_le_bytes([row[i * 2], row[i * 2 + 1]]);
        let b = u16::from_le_bytes([row[(i - spp) * 2], row[(i - spp) * 2 + 1]]);
        row[i * 2..i * 2 + 2].copy_from_slice(&a.wrapping_add(b).to_le_bytes());
    }
}

fn encode_tiles(img: &LinearImage, opts: &DngOptions) -> Vec<Vec<u8>> {
    let ts = opts.tile_size;
    let tiles_x = img.width.div_ceil(ts);
    let tiles_y = img.height.div_ceil(ts);
    let bps = opts.sample.bytes();
    (0..tiles_x * tiles_y)
        .into_par_iter()
        .map(|i| {
            let mut t = tile_samples(img, i % tiles_x, i / tiles_x, ts, opts.sample);
            if opts.compression == Compression::None {
                return t;
            }
            let row_len = ts as usize * 3 * bps;
            for row in t.chunks_mut(row_len) {
                if opts.sample.is_float() {
                    fp_predict_row(row, bps, 3);
                } else {
                    hdiff_u16_row(row, 3);
                }
            }
            let mut z = flate2::write::ZlibEncoder::new(Vec::new(), flate2::Compression::new(opts.deflate_level));
            z.write_all(&t).expect("in-memory write");
            z.finish().expect("in-memory write")
        })
        .collect()
}

// ---------------------------------------------------------------- tags

const T_NEW_SUBFILE: u16 = 254;
const T_WIDTH: u16 = 256;
const T_HEIGHT: u16 = 257;
const T_BPS: u16 = 258;
const T_COMPRESSION: u16 = 259;
const T_PHOTOMETRIC: u16 = 262;
const T_DESCRIPTION: u16 = 270;
const T_MAKE: u16 = 271;
const T_MODEL: u16 = 272;
const T_STRIP_OFFSETS: u16 = 273;
const T_ORIENTATION: u16 = 274;
const T_SPP: u16 = 277;
const T_ROWS_PER_STRIP: u16 = 278;
const T_STRIP_COUNTS: u16 = 279;
const T_PLANAR: u16 = 284;
const T_SOFTWARE: u16 = 305;
const T_DATETIME: u16 = 306;
const T_PREDICTOR: u16 = 317;
const T_TILE_W: u16 = 322;
const T_TILE_H: u16 = 323;
const T_TILE_OFFSETS: u16 = 324;
const T_TILE_COUNTS: u16 = 325;
const T_SUBIFDS: u16 = 330;
const T_SAMPLE_FORMAT: u16 = 339;
const T_EXIF_IFD: u16 = 34665;
const T_DNG_VERSION: u16 = 50706;
const T_DNG_BACKWARD: u16 = 50707;
const T_UNIQUE_MODEL: u16 = 50708;
const T_WHITE_LEVEL: u16 = 50717;
const T_CROP_ORIGIN: u16 = 50719;
const T_CROP_SIZE: u16 = 50720;
const T_COLOR_MATRIX1: u16 = 50721;
const T_AS_SHOT_NEUTRAL: u16 = 50728;
const T_BASELINE_EXPOSURE: u16 = 50730;
const T_BASELINE_NOISE: u16 = 50731;
const T_BASELINE_SHARPNESS: u16 = 50732;
const T_LINEAR_RESPONSE: u16 = 50734;
const T_CAMERA_SERIAL: u16 = 50735;
const T_CAL_ILLUMINANT1: u16 = 50778;
const T_ORIGINAL_RAW_NAME: u16 = 50827;
const T_PROFILE_NAME: u16 = 50936;
const T_FORWARD_MATRIX1: u16 = 50964;
const T_TIMECODES: u16 = 51043;
const T_FRAME_RATE: u16 = 51044;
const T_REEL_NAME: u16 = 51081;
const T_CAMERA_LABEL: u16 = 51105;
const T_DEFAULT_BLACK_RENDER: u16 = 51110;

const E_EXPOSURE_TIME: u16 = 33434;
const E_FNUMBER: u16 = 33437;
const E_ISO: u16 = 34855;
const E_EXIF_VERSION: u16 = 36864;
const E_DATETIME_ORIGINAL: u16 = 36867;
const E_FOCAL_LENGTH: u16 = 37386;
const E_BODY_SERIAL: u16 = 42033;
const E_LENS_MODEL: u16 = 42036;

fn add_dng_tags(ifd: &mut Ifd, meta: &DngMeta, opts: &DngOptions) {
    let modern = opts.sample.is_float() || opts.compression == Compression::Deflate;
    ifd.bytes(T_DNG_VERSION, &[1, 4, 0, 0]);
    ifd.bytes(T_DNG_BACKWARD, if modern { &[1, 4, 0, 0] } else { &[1, 1, 0, 0] });
    if !meta.make.is_empty() {
        ifd.ascii(T_MAKE, &meta.make);
    }
    if !meta.model.is_empty() {
        ifd.ascii(T_MODEL, &meta.model);
    }
    ifd.ascii(T_UNIQUE_MODEL, &meta.unique_camera_model);
    ifd.ascii(T_SOFTWARE, &meta.software);
    ifd.shorts(T_ORIENTATION, &[1]);
    if let Some(d) = &meta.date_time {
        ifd.ascii(T_DATETIME, d);
    }
    if let Some(d) = &meta.image_description {
        ifd.ascii(T_DESCRIPTION, d);
    }
    let m = |v: &[f64; 9]| v.map(|x| srat(x, 10000));
    ifd.srationals(T_COLOR_MATRIX1, &m(&meta.color_matrix1));
    if let Some(fm) = &meta.forward_matrix1 {
        ifd.srationals(T_FORWARD_MATRIX1, &m(fm));
    }
    ifd.shorts(T_CAL_ILLUMINANT1, &[meta.calibration_illuminant1]);
    ifd.rationals(T_AS_SHOT_NEUTRAL, &meta.as_shot_neutral.map(|v| ((v * 1_000_000.0).round() as u32, 1_000_000)));
    ifd.srationals(T_BASELINE_EXPOSURE, &[srat(meta.baseline_exposure, 10000)]);
    ifd.rationals(T_BASELINE_NOISE, &[(1, 1)]);
    ifd.rationals(T_BASELINE_SHARPNESS, &[(1, 1)]);
    ifd.rationals(T_LINEAR_RESPONSE, &[(1, 1)]);
    ifd.longs(T_DEFAULT_BLACK_RENDER, &[1]);
    ifd.ascii(T_PROFILE_NAME, &meta.profile_name);
    if let Some(s) = &meta.camera_serial {
        ifd.ascii(T_CAMERA_SERIAL, s);
    }
    if let Some(n) = &meta.original_raw_file_name {
        ifd.ascii(T_ORIGINAL_RAW_NAME, n);
    }
    if let Some(c) = &meta.cinema {
        ifd.srationals(T_FRAME_RATE, &[c.frame_rate]);
        if let Some(tc) = &c.timecode {
            ifd.bytes(T_TIMECODES, &tc.to_smpte_bytes());
        }
        if let Some(r) = &c.reel_name {
            ifd.ascii(T_REEL_NAME, r);
        }
        if let Some(l) = &c.camera_label {
            ifd.ascii(T_CAMERA_LABEL, l);
        }
    }
    ifd.longs(T_EXIF_IFD, &[0]); // patched after layout
}

fn add_raw_tags(ifd: &mut Ifd, img: &LinearImage, opts: &DngOptions, n_tiles: usize) {
    let b = opts.sample.bits();
    ifd.longs(T_NEW_SUBFILE, &[0]);
    ifd.longs(T_WIDTH, &[img.width]);
    ifd.longs(T_HEIGHT, &[img.height]);
    ifd.shorts(T_BPS, &[b, b, b]);
    ifd.shorts(T_COMPRESSION, &[if opts.compression == Compression::Deflate { 8 } else { 1 }]);
    if opts.compression == Compression::Deflate {
        ifd.shorts(T_PREDICTOR, &[if opts.sample.is_float() { 3 } else { 2 }]);
    }
    ifd.shorts(T_PHOTOMETRIC, &[34892]);
    ifd.shorts(T_SPP, &[3]);
    ifd.shorts(T_PLANAR, &[1]);
    let fmt = if opts.sample.is_float() { 3 } else { 1 };
    ifd.shorts(T_SAMPLE_FORMAT, &[fmt, fmt, fmt]);
    ifd.longs(T_TILE_W, &[opts.tile_size]);
    ifd.longs(T_TILE_H, &[opts.tile_size]);
    ifd.longs(T_TILE_OFFSETS, &vec![0; n_tiles]);
    ifd.longs(T_TILE_COUNTS, &vec![0; n_tiles]);
    if opts.sample == SampleType::U16 {
        ifd.longs(T_WHITE_LEVEL, &[65535, 65535, 65535]);
    }
    ifd.longs(T_CROP_ORIGIN, &[0, 0]);
    ifd.longs(T_CROP_SIZE, &[img.width, img.height]);
}

fn exif_ifd(e: &ExifInfo) -> Ifd {
    let mut ifd = Ifd::default();
    ifd.undefined(E_EXIF_VERSION, b"0230");
    if let Some(t) = e.exposure_time_s {
        ifd.rationals(E_EXPOSURE_TIME, &[urat(t)]);
    }
    if let Some(f) = e.f_number {
        ifd.rationals(E_FNUMBER, &[((f * 100.0).round() as u32, 100)]);
    }
    if let Some(iso) = e.iso {
        ifd.shorts(E_ISO, &[iso.min(65535) as u16]);
    }
    if let Some(d) = &e.date_time_original {
        ifd.ascii(E_DATETIME_ORIGINAL, d);
    }
    if let Some(f) = e.focal_length_mm {
        ifd.rationals(E_FOCAL_LENGTH, &[((f * 100.0).round() as u32, 100)]);
    }
    if let Some(l) = &e.lens_model {
        ifd.ascii(E_LENS_MODEL, l);
    }
    if let Some(s) = &e.body_serial {
        ifd.ascii(E_BODY_SERIAL, s);
    }
    ifd
}

/// A baseline JPEG (8-bit YCbCr, sRGB) preview, already encoded.
#[derive(Debug, Clone)]
pub struct JpegPreview {
    pub width: u32,
    pub height: u32,
    pub data: Vec<u8>,
    /// Chroma subsampling of the JPEG (e.g. [2, 2] for 4:2:0).
    pub subsampling: [u16; 2],
}

/// Preview images embedded next to the raw data.
#[derive(Debug, Clone, Default)]
pub struct Previews {
    /// Small RGB thumbnail in IFD0 (file icons in Explorer, Bridge, Lightroom import).
    pub thumbnail: Option<Thumbnail>,
    /// Large JPEG preview in its own SubIFD (what quick viewers like Windows Photos show).
    pub jpeg: Option<JpegPreview>,
    /// "YYYY-MM-DDTHH:MM:SS" for PreviewDateTime.
    pub date_time: Option<String>,
}

const T_YCBCR_SUBSAMPLING: u16 = 530;
const T_YCBCR_POSITIONING: u16 = 531;
const T_PREVIEW_APP_NAME: u16 = 50966;
const T_PREVIEW_APP_VERSION: u16 = 50967;
const T_PREVIEW_COLOR_SPACE: u16 = 50970;
const T_PREVIEW_DATE_TIME: u16 = 50971;

fn pad2(n: usize) -> u64 {
    ((n as u64) + 1) & !1
}

/// Encode a complete DNG into a writer.
///
/// Layout with previews: IFD0 = RGB thumbnail + DNG tags, SubIFDs = [raw, JPEG preview].
/// CinemaDNG (`Layout::RawInIfd0`) or no thumbnail: the raw image lives in IFD0.
pub fn write_dng<W: Write>(
    out: &mut W,
    img: &LinearImage,
    previews: Option<&Previews>,
    meta: &DngMeta,
    opts: &DngOptions,
) -> Result<(), DngError> {
    if img.width == 0 || img.height == 0 || img.data.len() != (img.width * img.height * 3) as usize {
        return Err(DngError::Invalid("image size/data mismatch".into()));
    }
    if opts.tile_size == 0 || opts.tile_size % 16 != 0 {
        return Err(DngError::Invalid("tile size must be a multiple of 16".into()));
    }
    let tiles = encode_tiles(img, opts);
    let previews = if opts.layout == Layout::PreviewInIfd0 { previews } else { None };
    let thumb = previews.and_then(|p| p.thumbnail.as_ref());
    let jpeg = previews.and_then(|p| p.jpeg.as_ref()).filter(|_| thumb.is_some());

    // Build IFDs with placeholder offsets.
    let mut ifd0 = Ifd::default();
    let mut raw = Ifd::default();
    let mut prev = Ifd::default();
    let raw_in_ifd0 = opts.layout == Layout::RawInIfd0 || thumb.is_none();
    add_dng_tags(&mut ifd0, meta, opts);
    if raw_in_ifd0 {
        add_raw_tags(&mut ifd0, img, opts, tiles.len());
    } else {
        let t = thumb.unwrap();
        ifd0.longs(T_NEW_SUBFILE, &[1]);
        ifd0.longs(T_WIDTH, &[t.width]);
        ifd0.longs(T_HEIGHT, &[t.height]);
        ifd0.shorts(T_BPS, &[8, 8, 8]);
        ifd0.shorts(T_COMPRESSION, &[1]);
        ifd0.shorts(T_PHOTOMETRIC, &[2]);
        ifd0.longs(T_STRIP_OFFSETS, &[0]);
        ifd0.shorts(T_SPP, &[3]);
        ifd0.longs(T_ROWS_PER_STRIP, &[t.height]);
        ifd0.longs(T_STRIP_COUNTS, &[t.rgb.len() as u32]);
        ifd0.shorts(T_PLANAR, &[1]);
        ifd0.longs(T_SUBIFDS, &if jpeg.is_some() { vec![0, 0] } else { vec![0] });
        add_raw_tags(&mut raw, img, opts, tiles.len());
        if let Some(j) = jpeg {
            prev.longs(T_NEW_SUBFILE, &[1]);
            prev.longs(T_WIDTH, &[j.width]);
            prev.longs(T_HEIGHT, &[j.height]);
            prev.shorts(T_BPS, &[8, 8, 8]);
            prev.shorts(T_COMPRESSION, &[7]); // baseline JPEG (with YCbCr + 8 bits)
            prev.shorts(T_PHOTOMETRIC, &[6]); // YCbCr
            prev.longs(T_STRIP_OFFSETS, &[0]);
            prev.shorts(T_SPP, &[3]);
            prev.longs(T_ROWS_PER_STRIP, &[j.height]);
            prev.longs(T_STRIP_COUNTS, &[j.data.len() as u32]);
            prev.shorts(T_PLANAR, &[1]);
            prev.shorts(T_YCBCR_SUBSAMPLING, &j.subsampling);
            prev.shorts(T_YCBCR_POSITIONING, &[1]);
            prev.ascii(T_PREVIEW_APP_NAME, "Raw Framer");
            prev.ascii(T_PREVIEW_APP_VERSION, env!("CARGO_PKG_VERSION"));
            prev.longs(T_PREVIEW_COLOR_SPACE, &[2]); // sRGB
            if let Some(d) = previews.and_then(|p| p.date_time.as_deref()) {
                prev.ascii(T_PREVIEW_DATE_TIME, d);
            }
        }
    }
    let exif = exif_ifd(&meta.exif);

    // Layout: header | IFD0 | raw IFD | preview IFD | EXIF IFD | thumbnail | JPEG | tiles
    let off_ifd0 = 8u32;
    let off_raw = off_ifd0 + ifd0.byte_size();
    let off_prev = off_raw + if raw_in_ifd0 { 0 } else { raw.byte_size() };
    let off_exif = off_prev + if jpeg.is_some() { prev.byte_size() } else { 0 };
    let off_thumb = off_exif + exif.byte_size();
    let off_jpeg = off_thumb as u64 + thumb.map(|t| pad2(t.rgb.len())).unwrap_or(0);
    let off_tiles = off_jpeg + jpeg.map(|j| pad2(j.data.len())).unwrap_or(0);
    let mut tile_offsets = Vec::with_capacity(tiles.len());
    let mut pos = off_tiles;
    for t in &tiles {
        tile_offsets.push(pos);
        pos += pad2(t.len());
    }
    if pos > u32::MAX as u64 {
        return Err(DngError::TooLarge);
    }
    let tile_offsets: Vec<u32> = tile_offsets.into_iter().map(|o| o as u32).collect();
    let tile_counts: Vec<u32> = tiles.iter().map(|t| t.len() as u32).collect();

    ifd0.longs(T_EXIF_IFD, &[off_exif]);
    let raw_ifd = if raw_in_ifd0 { &mut ifd0 } else { &mut raw };
    raw_ifd.longs(T_TILE_OFFSETS, &tile_offsets);
    raw_ifd.longs(T_TILE_COUNTS, &tile_counts);
    if !raw_in_ifd0 {
        ifd0.longs(T_STRIP_OFFSETS, &[off_thumb]);
        if jpeg.is_some() {
            ifd0.longs(T_SUBIFDS, &[off_raw, off_prev]);
            prev.longs(T_STRIP_OFFSETS, &[off_jpeg as u32]);
        } else {
            ifd0.longs(T_SUBIFDS, &[off_raw]);
        }
    }

    // Serialise.
    let blob = |out: &mut W, b: &[u8]| -> std::io::Result<()> {
        out.write_all(b)?;
        if b.len() % 2 == 1 {
            out.write_all(&[0])?;
        }
        Ok(())
    };
    out.write_all(b"II")?;
    out.write_all(&42u16.to_le_bytes())?;
    out.write_all(&off_ifd0.to_le_bytes())?;
    out.write_all(&ifd0.serialize(off_ifd0, 0))?;
    if !raw_in_ifd0 {
        out.write_all(&raw.serialize(off_raw, 0))?;
    }
    if jpeg.is_some() {
        out.write_all(&prev.serialize(off_prev, 0))?;
    }
    out.write_all(&exif.serialize(off_exif, 0))?;
    if let Some(t) = thumb {
        blob(out, &t.rgb)?;
    }
    if let Some(j) = jpeg {
        blob(out, &j.data)?;
    }
    for t in &tiles {
        blob(out, t)?;
    }
    out.flush()?;
    Ok(())
}

/// Write to `path` atomically (via `<path>.part` + rename).
pub fn write_dng_file(
    path: &Path,
    img: &LinearImage,
    previews: Option<&Previews>,
    meta: &DngMeta,
    opts: &DngOptions,
) -> Result<u64, DngError> {
    let mut part = path.as_os_str().to_owned();
    part.push(".part");
    let part = std::path::PathBuf::from(part);
    let res = (|| {
        let f = std::fs::File::create(&part)?;
        let mut w = std::io::BufWriter::with_capacity(8 << 20, f);
        write_dng(&mut w, img, previews, meta, opts)?;
        let f = w.into_inner().map_err(|e| e.into_error())?;
        f.sync_all()?;
        let len = f.metadata()?.len();
        drop(f);
        std::fs::rename(&part, path)?;
        Ok(len)
    })();
    if res.is_err() {
        let _ = std::fs::remove_file(&part);
    }
    res
}

#[cfg(test)]
mod tests;

//! Core, source-agnostic types for Raw Framer.
//!
//! Every camera-raw backend (BRAW today; R3D, ProRes RAW, ... later) implements
//! [`MediaSource`]. The viewer, player, saved-frame list and DNG exporter only ever
//! talk to this trait, so adding a format never touches them.

mod registry;
mod test_pattern;

pub use registry::{SourceOpener, SourceRegistry};
pub use test_pattern::TestPatternSource;

use serde::{Deserialize, Serialize};
use std::path::PathBuf;
use std::sync::Arc;

#[derive(Debug, thiserror::Error)]
pub enum Error {
    #[error("unsupported file type: {0}")]
    Unsupported(String),
    #[error("decoder backend unavailable: {0}")]
    BackendUnavailable(String),
    #[error("decode failed: {0}")]
    Decode(String),
    #[error("frame {0} out of range")]
    FrameOutOfRange(u64),
    #[error("io: {0}")]
    Io(#[from] std::io::Error),
}

pub type Result<T> = std::result::Result<T, Error>;

/// Decode resolution relative to the full sensor image.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize, Default)]
pub enum ResolutionScale {
    #[default]
    Full,
    Half,
    Quarter,
    Eighth,
}

impl ResolutionScale {
    pub const ALL: [ResolutionScale; 4] = [Self::Full, Self::Half, Self::Quarter, Self::Eighth];
    pub fn divisor(self) -> u32 {
        match self {
            Self::Full => 1,
            Self::Half => 2,
            Self::Quarter => 4,
            Self::Eighth => 8,
        }
    }
    pub fn label(self) -> &'static str {
        match self {
            Self::Full => "Full",
            Self::Half => "1/2",
            Self::Quarter => "1/4",
            Self::Eighth => "1/8",
        }
    }
}

/// Pixel layout requested from / returned by a backend.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub enum PixelFormat {
    /// Interleaved RGBA, IEEE half floats (preview, uploads straight to an Rgba16Float texture).
    RgbaF16,
    /// Interleaved RGB, 32-bit floats (export).
    RgbF32,
}

/// Decoded pixel storage.
#[derive(Clone)]
pub enum Pixels {
    RgbaF16(Vec<half::f16>),
    RgbF32(Vec<f32>),
}

impl Pixels {
    pub fn format(&self) -> PixelFormat {
        match self {
            Pixels::RgbaF16(_) => PixelFormat::RgbaF16,
            Pixels::RgbF32(_) => PixelFormat::RgbF32,
        }
    }
    pub fn byte_len(&self) -> usize {
        match self {
            Pixels::RgbaF16(v) => v.len() * 2,
            Pixels::RgbF32(v) => v.len() * 4,
        }
    }
}

/// Transfer function (OETF) of decoded pixels.
#[derive(Debug, Clone, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub enum Transfer {
    /// Scene-linear (18% grey = 0.18).
    Linear,
    /// Blackmagic Film Generation 5 log.
    BmdFilmGen5,
    /// Already a display encoding (Rec.709, Extended Video, ...). The viewer shows it as-is.
    Display(String),
    /// A log/other curve we have no formula for; shown as-is with a warning.
    Unknown(String),
}

/// Color primaries of decoded pixels.
#[derive(Debug, Clone, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub enum Gamut {
    BmdWideGamut,
    Rec709,
    Rec2020,
    P3D65,
    AcesAp0,
    Unknown(String),
}

/// What the pixels of a [`DecodedFrame`] actually are.
#[derive(Debug, Clone, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub struct ColorEncoding {
    pub transfer: Transfer,
    pub gamut: Gamut,
}

impl ColorEncoding {
    pub fn is_scene_referred(&self) -> bool {
        matches!(self.transfer, Transfer::Linear | Transfer::BmdFilmGen5)
    }
}

/// Raw decode ("Camera RAW panel") settings. `None` = use the clip's as-shot value.
#[derive(Debug, Clone, Default, PartialEq, Hash, Serialize, Deserialize)]
pub struct DecodeColor {
    pub color_science_gen: Option<u16>,
    pub gamma: Option<String>,
    pub gamut: Option<String>,
    pub iso: Option<u32>,
    pub kelvin: Option<u32>,
    pub tint: Option<i16>,
    /// Exposure in stops (f32 stored as bits so the struct stays `Hash`).
    pub exposure_bits: Option<u32>,
    pub highlight_recovery: Option<bool>,
    pub gamut_compression: Option<bool>,
    /// "Disabled", "Embedded", "Sidecar" ... `None` = as shot.
    pub post3dlut_mode: Option<String>,
}

impl DecodeColor {
    pub fn exposure(&self) -> Option<f32> {
        self.exposure_bits.map(f32::from_bits)
    }
    pub fn set_exposure(&mut self, ev: Option<f32>) {
        self.exposure_bits = ev.map(f32::to_bits);
    }
    /// Every `None` ("as shot") replaced by the clip's recorded value, so the settings no
    /// longer depend on what the clip currently reports as shot (e.g. after a sidecar save).
    pub fn resolved(&self, a: &AsShot) -> DecodeColor {
        let mut c = self.clone();
        c.color_science_gen = c.color_science_gen.or(a.color_science_gen);
        c.gamma = c.gamma.or_else(|| a.gamma.clone());
        c.gamut = c.gamut.or_else(|| a.gamut.clone());
        c.iso = c.iso.or(a.iso);
        c.kelvin = c.kelvin.or(a.kelvin);
        c.tint = c.tint.or(a.tint);
        if c.exposure_bits.is_none() {
            c.set_exposure(Some(a.exposure.unwrap_or(0.0)));
        }
        c.highlight_recovery = c.highlight_recovery.or(a.highlight_recovery);
        c.gamut_compression = c.gamut_compression.or(a.gamut_compression);
        c.post3dlut_mode = c.post3dlut_mode.or_else(|| a.post3dlut_mode.clone());
        c
    }

    pub fn settings_hash(&self) -> u64 {
        use std::hash::{Hash, Hasher};
        let mut h = std::collections::hash_map::DefaultHasher::new();
        self.hash(&mut h);
        h.finish()
    }
}

#[derive(Debug, Clone)]
pub struct DecodeRequest {
    pub frame: u64,
    pub scale: ResolutionScale,
    pub format: PixelFormat,
    pub color: DecodeColor,
}

#[derive(Clone)]
pub struct DecodedFrame {
    pub frame: u64,
    pub width: u32,
    pub height: u32,
    pub pixels: Pixels,
    pub encoding: ColorEncoding,
}

/// The clip's recorded ("as shot") look and exposure settings.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct AsShot {
    pub color_science_gen: Option<u16>,
    pub gamma: Option<String>,
    pub gamut: Option<String>,
    pub iso: Option<u32>,
    pub kelvin: Option<u32>,
    pub tint: Option<i16>,
    pub exposure: Option<f32>,
    pub highlight_recovery: Option<bool>,
    pub gamut_compression: Option<bool>,
    pub post3dlut_mode: Option<String>,
}

/// What a backend lets the user choose. Lists come from the decoder at run time.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct DecodeCapabilities {
    pub color_science_gens: Vec<u16>,
    pub gammas: Vec<String>,
    pub gamuts: Vec<String>,
    pub isos: Vec<u32>,
    pub kelvin_range: Option<(u32, u32)>,
    pub tint_range: Option<(i16, i16)>,
    pub exposure_range: Option<(f32, f32)>,
    pub post3dlut_modes: Vec<String>,
}

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct ClipInfo {
    pub path: PathBuf,
    pub format_name: String,
    pub width: u32,
    pub height: u32,
    pub fps: f32,
    pub frame_count: u64,
    pub camera_make: String,
    pub camera_model: String,
    pub as_shot: AsShot,
    /// Lens/exposure info useful for Exif: (key, value) pairs already humanised.
    pub start_timecode: Option<String>,
}

impl ClipInfo {
    pub fn file_stem(&self) -> String {
        self.path
            .file_stem()
            .map(|s| s.to_string_lossy().into_owned())
            .unwrap_or_else(|| "clip".into())
    }
}

/// Information the DNG writer wants to embed (all optional).
#[derive(Debug, Clone, Default)]
pub struct ShootingInfo {
    pub exposure_time_s: Option<f64>,
    pub shutter_angle: Option<f64>,
    pub f_number: Option<f64>,
    pub focal_length_mm: Option<f64>,
    pub lens_model: Option<String>,
    pub iso: Option<u32>,
    pub date_time: Option<String>,
    pub camera_serial: Option<String>,
    pub reel: Option<String>,
}

/// Clip audio format.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct AudioInfo {
    pub sample_rate: u32,
    pub channels: u16,
    /// Total sample frames.
    pub frames: u64,
}

/// A clip that can be decoded frame by frame. Implementations must be thread-safe:
/// several worker threads call [`MediaSource::decode`] concurrently.
pub trait MediaSource: Send + Sync {
    fn info(&self) -> &ClipInfo;
    fn capabilities(&self) -> &DecodeCapabilities;
    /// Clip-level metadata as display strings.
    fn metadata(&self) -> &[(String, String)];
    fn shooting_info(&self, frame: u64) -> ShootingInfo;
    fn timecode(&self, frame: u64) -> Option<String>;
    /// Name of a gamma option that yields scene-linear output, if the backend has one.
    fn linear_gamma_name(&self) -> Option<String>;
    /// Blocking decode. Safe to call from many threads at once.
    fn decode(&self, req: &DecodeRequest) -> Result<DecodedFrame>;
    /// The clip's audio, if it has any.
    fn audio_info(&self) -> Option<AudioInfo> {
        None
    }
    /// Read up to `max_frames` sample frames from `start` as interleaved f32 in [-1, 1].
    /// Returns fewer (or none) at the end of the clip.
    fn read_audio(&self, _start: u64, _max_frames: usize) -> Result<Vec<f32>> {
        Ok(Vec::new())
    }

    /// The format can store corrections next to the clip (Blackmagic `.sidecar`).
    fn supports_sidecar(&self) -> bool {
        false
    }
    /// A sidecar with corrections was found when the clip was opened (its values are what
    /// [`ClipInfo::as_shot`] reports).
    fn sidecar_attached(&self) -> bool {
        false
    }
    /// Store `color` (resolved against as-shot) in the clip's sidecar. Reopen the clip
    /// afterwards to see the new as-shot values.
    fn write_sidecar(&self, _color: &DecodeColor) -> Result<()> {
        Err(Error::Unsupported("this format has no sidecar".into()))
    }
    /// Remove Raw Framer's corrections from the sidecar (back to the camera original).
    fn clear_sidecar(&self) -> Result<()> {
        Err(Error::Unsupported("this format has no sidecar".into()))
    }
}

pub type SharedSource = Arc<dyn MediaSource>;

/// Parse "HH:MM:SS:FF" (or with ';' for drop-frame) into components.
pub fn parse_timecode(tc: &str) -> Option<(u8, u8, u8, u8, bool)> {
    let drop = tc.contains(';');
    let parts: Vec<&str> = tc.split([':', ';', '.']).collect();
    if parts.len() != 4 {
        return None;
    }
    let p = |i: usize| parts[i].trim().parse::<u8>().ok();
    Some((p(0)?, p(1)?, p(2)?, p(3)?, drop))
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn timecode_parse() {
        assert_eq!(parse_timecode("01:02:03:04"), Some((1, 2, 3, 4, false)));
        assert_eq!(parse_timecode("01:02:03;04"), Some((1, 2, 3, 4, true)));
        assert_eq!(parse_timecode("bad"), None);
    }
}

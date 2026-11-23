//! Safe Blackmagic RAW backend implementing [`rf_core::MediaSource`].

use braw_sys as sys;
use rayon::prelude::*;
use rf_core::*;
use serde_json::Value;
use std::ffi::{CStr, CString};
use std::os::windows::ffi::OsStrExt;
use std::path::{Path, PathBuf};

pub const EXTENSIONS: &[&str] = &["braw"];

/// Is BRAW support compiled in?
pub fn sdk_available() -> bool {
    sys::SDK_AVAILABLE
}

/// Folders searched for `BlackmagicRawAPI.dll`, in order.
pub fn runtime_search_paths() -> Vec<PathBuf> {
    let mut v = Vec::new();
    if let Ok(exe) = std::env::current_exe() {
        if let Some(d) = exe.parent() {
            v.push(d.join("BlackmagicRawAPI"));
            v.push(d.to_path_buf());
        }
    }
    if let Some(d) = std::env::var_os("BRAW_RUNTIME_DIR") {
        v.push(PathBuf::from(d));
    }
    let x86 = PathBuf::from(r"C:\Program Files (x86)\Blackmagic Design\Blackmagic RAW");
    v.push(x86.join(r"Blackmagic RAW SDK\Win\Libraries"));
    v.push(x86.join(r"Blackmagic RAW Player\BlackmagicRawAPI"));
    v.push(PathBuf::from(r"C:\Program Files\Blackmagic Design\DaVinci Resolve"));
    v
}

pub fn find_runtime() -> Option<PathBuf> {
    runtime_search_paths().into_iter().find(|p| p.join("BlackmagicRawAPI.dll").exists())
}

/// A decode pipeline the Blackmagic RAW SDK can use on this machine.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Pipeline {
    /// "cpu", "cuda" or "opencl".
    pub id: String,
    /// e.g. "OpenCL · AMD Radeon(TM) Graphics".
    pub name: String,
}

/// Pipelines with a usable device (CPU is always first).
pub fn pipelines() -> Vec<Pipeline> {
    let cpu = vec![Pipeline { id: "cpu".into(), name: "CPU".into() }];
    let Some(runtime) = find_runtime().filter(|_| sdk_available()) else { return cpu };
    let Ok(v) = take_json(|o| unsafe { sys::rf_braw_pipelines(wide(&runtime).as_ptr(), o) }) else { return cpu };
    let list: Vec<Pipeline> = v
        .as_array()
        .into_iter()
        .flatten()
        .filter_map(|p| Some(Pipeline { id: p.get("id")?.as_str()?.to_string(), name: p.get("name")?.as_str()?.to_string() }))
        .collect();
    if list.is_empty() {
        cpu
    } else {
        list
    }
}

/// Choose the pipeline for clips opened from now on ("cpu", "cuda", "opencl"). Reopen open
/// clips to move them over. A GPU pipeline that can't start falls back to CPU.
pub fn set_pipeline(id: &str) {
    let c = CString::new(id).unwrap_or_default();
    unsafe {
        sys::rf_braw_set_pipeline(c.as_ptr());
    }
}

/// What the current codec actually uses, e.g. "CPU" or "OpenCL · AMD Radeon(TM) Graphics".
pub fn active_pipeline() -> String {
    let mut p = std::ptr::null_mut();
    unsafe {
        if sys::rf_braw_active_pipeline(&mut p) != sys::RF_OK || p.is_null() {
            return "CPU".into();
        }
        let s = CStr::from_ptr(p).to_string_lossy().into_owned();
        sys::rf_free(p as *mut _);
        s
    }
}

fn last_error() -> String {
    unsafe {
        let p = sys::rf_braw_last_error();
        if p.is_null() {
            "unknown error".into()
        } else {
            CStr::from_ptr(p).to_string_lossy().into_owned()
        }
    }
}

fn wide(p: &Path) -> Vec<u16> {
    p.as_os_str().encode_wide().chain(Some(0)).collect()
}

fn take_json(f: impl FnOnce(*mut *mut std::os::raw::c_char) -> i32) -> Result<Value> {
    let mut ptr = std::ptr::null_mut();
    if f(&mut ptr) != sys::RF_OK || ptr.is_null() {
        return Err(Error::Decode(last_error()));
    }
    let s = unsafe { CStr::from_ptr(ptr).to_string_lossy().into_owned() };
    unsafe { sys::rf_free(ptr as *mut _) };
    serde_json::from_str(&s).map_err(|e| Error::Decode(format!("bad metadata JSON: {e}")))
}

struct Handle(*mut sys::rf_braw);
// SAFETY: the shim serialises job submission and the SDK codec is thread-safe.
unsafe impl Send for Handle {}
unsafe impl Sync for Handle {}
impl Drop for Handle {
    fn drop(&mut self) {
        unsafe { sys::rf_braw_close(self.0) }
    }
}

pub struct BrawSource {
    h: Handle,
    info: ClipInfo,
    caps: DecodeCapabilities,
    meta: Vec<(String, String)>,
    raw_meta: serde_json::Map<String, Value>,
    linear_gamma: Option<String>,
    sdk_version: String,
}

/// Any non-null leaf (a sidecar whose entries are all null or empty has no effect).
fn has_value(v: &Value) -> bool {
    match v {
        Value::Null => false,
        Value::Object(m) => m.values().any(has_value),
        Value::Array(a) => a.iter().any(has_value),
        _ => true,
    }
}

fn value_to_string(v: &Value) -> String {
    match v {
        Value::String(s) => s.clone(),
        Value::Number(n) => {
            if let Some(f) = n.as_f64() {
                if f.fract() == 0.0 && f.abs() < 1e15 {
                    format!("{}", f as i64)
                } else {
                    format!("{:.4}", f).trim_end_matches('0').trim_end_matches('.').to_string()
                }
            } else {
                n.to_string()
            }
        }
        Value::Array(a) if a.len() > 16 => format!("[{} values]", a.len()),
        Value::Array(a) => a.iter().map(value_to_string).collect::<Vec<_>>().join(", "),
        Value::Null => String::new(),
        other => other.to_string(),
    }
}

fn first_number(s: &str) -> Option<f64> {
    let t: String = s
        .chars()
        .skip_while(|c| !c.is_ascii_digit() && *c != '.')
        .take_while(|c| c.is_ascii_digit() || *c == '.')
        .collect();
    t.parse().ok()
}

impl BrawSource {
    pub fn open(path: &Path) -> Result<Self> {
        if !sdk_available() {
            return Err(Error::BackendUnavailable(last_error()));
        }
        let runtime = find_runtime().ok_or_else(|| {
            Error::BackendUnavailable(
                "Blackmagic RAW runtime (BlackmagicRawAPI.dll) not found. Install Blackmagic RAW or DaVinci Resolve, or set BRAW_RUNTIME_DIR."
                    .into(),
            )
        })?;
        let mut h = std::ptr::null_mut();
        let rc = unsafe { sys::rf_braw_open(wide(path).as_ptr(), wide(&runtime).as_ptr(), &mut h) };
        if rc != sys::RF_OK || h.is_null() {
            return Err(Error::Decode(format!("{}: {}", path.display(), last_error())));
        }
        let h = Handle(h);
        let mut bi = sys::rf_braw_info_t::default();
        if unsafe { sys::rf_braw_info(h.0, &mut bi) } != sys::RF_OK {
            return Err(Error::Decode(last_error()));
        }
        let clip = take_json(|p| unsafe { sys::rf_braw_clip_json(h.0, p) })?;
        let frame0 = take_json(|p| unsafe { sys::rf_braw_frame_json(h.0, 0, p) }).unwrap_or(Value::Null);

        let proc_ = &clip["processing"];
        let lists = &clip["lists"];
        let fproc = &frame0["processing"];
        let ranges = &frame0["ranges"];
        let strs = |v: &Value| -> Vec<String> {
            v.as_array().map(|a| a.iter().filter_map(|x| x.as_str().map(String::from)).collect()).unwrap_or_default()
        };
        let nums = |v: &Value| -> Vec<f64> { v.as_array().map(|a| a.iter().filter_map(|x| x.as_f64()).collect()).unwrap_or_default() };

        let gen = proc_["color_science_gen"].as_f64().map(|v| v as u16);
        let mut isos: Vec<u32> = nums(&frame0["isos"]).into_iter().map(|v| v as u32).collect();
        if isos.is_empty() {
            isos = nums(&lists["iso"]).into_iter().map(|v| v as u32).collect();
        }
        let range = |k: &str| -> Option<(f64, f64)> {
            let a = nums(&ranges[k]);
            (a.len() == 2).then(|| (a[0], a[1]))
        };

        let as_shot = AsShot {
            color_science_gen: gen,
            gamma: proc_["gamma"].as_str().map(String::from),
            gamut: proc_["gamut"].as_str().map(String::from),
            iso: fproc["iso"].as_f64().map(|v| v as u32),
            kelvin: fproc["kelvin"].as_f64().map(|v| v as u32),
            tint: fproc["tint"].as_f64().map(|v| v as i16),
            exposure: fproc["exposure"].as_f64().map(|v| v as f32),
            highlight_recovery: proc_["highlight_recovery"].as_f64().map(|v| v != 0.0),
            gamut_compression: proc_["gamut_compression"].as_f64().map(|v| v != 0.0),
            post3dlut_mode: proc_["post3dlut_mode"].as_str().map(String::from),
        };

        // Gammas across all generations (the list depends on the generation).
        let gens: Vec<u16> = nums(&lists["color_science_gen"]).into_iter().map(|v| v as u16).collect();
        let mut gammas = strs(&lists["gamma"]);
        for g in &gens {
            if let Ok(Value::Array(a)) = take_json(|p| unsafe { sys::rf_braw_gamma_list(h.0, *g as i32, p) }) {
                for v in a.iter().filter_map(|x| x.as_str()) {
                    if !gammas.iter().any(|x| x == v) {
                        gammas.push(v.to_string());
                    }
                }
            }
        }
        let linear_gamma = gammas.iter().find(|g| g.to_ascii_lowercase().contains("linear")).cloned();

        let caps = DecodeCapabilities {
            color_science_gens: gens,
            gammas,
            gamuts: strs(&lists["gamut"]),
            isos,
            kelvin_range: range("kelvin").map(|(a, b)| (a as u32, b as u32)),
            tint_range: range("tint").map(|(a, b)| (a as i16, b as i16)),
            exposure_range: range("exposure").map(|(a, b)| (a as f32, b as f32)),
            post3dlut_modes: strs(&lists["post3dlut_mode"]),
        };

        let raw_meta = clip["metadata"].as_object().cloned().unwrap_or_default();
        let mut meta: Vec<(String, String)> = raw_meta.iter().map(|(k, v)| (k.clone(), value_to_string(v))).collect();
        meta.sort();
        let camera_type = clip["camera_type"].as_str().unwrap_or("").to_string();
        let model = if camera_type.is_empty() {
            raw_meta.get("camera_type").map(value_to_string).unwrap_or_else(|| "Blackmagic camera".into())
        } else {
            camera_type
        };

        let info = ClipInfo {
            path: path.to_path_buf(),
            format_name: "Blackmagic RAW".into(),
            width: bi.width,
            height: bi.height,
            fps: bi.fps,
            frame_count: bi.frame_count,
            camera_make: "Blackmagic Design".into(),
            camera_model: model,
            as_shot,
            start_timecode: clip["timecode0"].as_str().map(String::from),
        };
        log::info!(
            "opened {} ({}x{} @ {} fps, {} frames, SDK {})",
            path.display(),
            info.width,
            info.height,
            info.fps,
            info.frame_count,
            clip["version"].as_str().unwrap_or("?")
        );
        Ok(Self {
            h,
            info,
            caps,
            meta,
            raw_meta,
            linear_gamma,
            sdk_version: clip["version"].as_str().unwrap_or("").to_string(),
        })
    }

    pub fn sdk_version(&self) -> &str {
        &self.sdk_version
    }

    /// Set clip (`frame = None`) or frame metadata. `value = None` clears the override
    /// (camera original). Kept in memory until [`save_sidecar`](Self::save_sidecar).
    pub fn set_metadata(&self, frame: Option<u64>, key: &str, value: Option<&str>) -> Result<()> {
        let k = std::ffi::CString::new(key).map_err(|_| Error::Decode("bad metadata key".into()))?;
        let v = value.map(|v| std::ffi::CString::new(v).unwrap_or_default());
        let vp = v.as_ref().map(|v| v.as_ptr()).unwrap_or(std::ptr::null());
        let f = frame.map(|f| f as i64).unwrap_or(-1);
        if unsafe { sys::rf_braw_set_metadata(self.h.0, f, k.as_ptr(), vp) } != sys::RF_OK {
            return Err(Error::Decode(last_error()));
        }
        Ok(())
    }

    /// Write all metadata set so far to the clip's `.sidecar` file.
    pub fn save_sidecar(&self) -> Result<()> {
        if unsafe { sys::rf_braw_save_sidecar(self.h.0) } != sys::RF_OK {
            return Err(Error::Decode(last_error()));
        }
        Ok(())
    }

    /// Drop unsaved metadata changes, re-reading the `.sidecar` file.
    pub fn reload_sidecar(&self) -> Result<()> {
        if unsafe { sys::rf_braw_reload_sidecar(self.h.0) } != sys::RF_OK {
            return Err(Error::Decode(last_error()));
        }
        Ok(())
    }

    /// A `.sidecar` file was found and parsed when the clip was opened.
    pub fn sidecar_attached(&self) -> bool {
        let mut a = 0;
        unsafe { sys::rf_braw_sidecar_attached(self.h.0, &mut a) == sys::RF_OK && a != 0 }
    }

    pub fn frame_metadata(&self, frame: u64) -> Result<Value> {
        take_json(|p| unsafe { sys::rf_braw_frame_json(self.h.0, frame, p) })
    }

    fn meta_str(&self, keys: &[&str]) -> Option<String> {
        keys.iter()
            .find_map(|k| self.raw_meta.get(*k))
            .map(value_to_string)
            .filter(|s| !s.trim().is_empty())
    }

    /// Effective colour encoding for a decode with these settings.
    pub fn encoding_for(&self, color: &DecodeColor) -> ColorEncoding {
        let gen = color.color_science_gen.or(self.info.as_shot.color_science_gen);
        let gamma = color.gamma.clone().or_else(|| self.info.as_shot.gamma.clone()).unwrap_or_default();
        let gamut = color.gamut.clone().or_else(|| self.info.as_shot.gamut.clone()).unwrap_or_default();
        ColorEncoding { transfer: rf_color::classify_gamma(&gamma, gen), gamut: rf_color::classify_gamut(&gamut) }
    }
}

impl MediaSource for BrawSource {
    fn info(&self) -> &ClipInfo {
        &self.info
    }
    fn capabilities(&self) -> &DecodeCapabilities {
        &self.caps
    }
    fn metadata(&self) -> &[(String, String)] {
        &self.meta
    }

    fn shooting_info(&self, frame: u64) -> ShootingInfo {
        // Per-frame metadata wins over clip metadata where present.
        let fm = self.frame_metadata(frame).ok();
        let fget = |keys: &[&str]| -> Option<String> {
            let m = fm.as_ref()?.get("metadata")?.as_object()?;
            keys.iter().find_map(|k| m.get(*k)).map(value_to_string).filter(|s| !s.trim().is_empty())
        };
        let get = |keys: &[&str]| fget(keys).or_else(|| self.meta_str(keys));
        let fps = self.info.fps as f64;
        let shutter = get(&["shutter_value", "shutter_angle", "shutter_speed"]);
        let (mut exposure_time_s, mut shutter_angle) = (None, None);
        if let Some(s) = &shutter {
            if s.contains('/') {
                let parts: Vec<&str> = s.split('/').collect();
                if let (Some(a), Some(b)) = (first_number(parts[0]), parts.get(1).and_then(|p| first_number(p))) {
                    if b > 0.0 {
                        exposure_time_s = Some(a / b);
                    }
                }
            } else if let Some(deg) = first_number(s) {
                shutter_angle = Some(deg);
                if fps > 0.0 {
                    exposure_time_s = Some(deg / 360.0 / fps);
                }
            }
        }
        let date = get(&["date_recorded"]).map(|d| d.replace('-', ":"));
        let time = get(&["time_recorded"]);
        ShootingInfo {
            exposure_time_s,
            shutter_angle,
            f_number: get(&["aperture"]).and_then(|s| first_number(&s)),
            focal_length_mm: get(&["focal_length"]).and_then(|s| first_number(&s)),
            lens_model: get(&["lens_type", "lens_name", "lens"]),
            iso: get(&["iso"]).and_then(|s| first_number(&s)).map(|v| v as u32),
            date_time: date.map(|d| match &time {
                Some(t) => format!("{d} {t}"),
                None => format!("{d} 00:00:00"),
            }),
            camera_serial: get(&["camera_serial_number", "serial_number"]),
            reel: get(&["reel_name", "camera_number", "reel"]),
        }
    }

    fn timecode(&self, frame: u64) -> Option<String> {
        self.frame_metadata(frame).ok()?.get("timecode")?.as_str().map(String::from)
    }

    fn linear_gamma_name(&self) -> Option<String> {
        self.linear_gamma.clone()
    }

    fn audio_info(&self) -> Option<AudioInfo> {
        let mut a = sys::rf_braw_audio_info_t::default();
        if unsafe { sys::rf_braw_audio_info(self.h.0, &mut a) } != sys::RF_OK {
            return None;
        }
        Some(AudioInfo { sample_rate: a.sample_rate, channels: a.channels as u16, frames: a.sample_count })
    }

    fn supports_sidecar(&self) -> bool {
        true
    }

    fn sidecar_attached(&self) -> bool {
        BrawSource::sidecar_attached(self)
    }

    /// Writes the same keys Resolve's "Update Sidecar" writes: look settings at clip level,
    /// ISO/WB/tint/exposure at the first frame's timecode (which the SDK applies to every
    /// frame from there on).
    fn write_sidecar(&self, color: &DecodeColor) -> Result<()> {
        let c = color.resolved(&self.info.as_shot);
        let set = |frame: Option<u64>, k: &str, v: Option<String>| -> Result<()> {
            match v {
                Some(v) => self.set_metadata(frame, k, Some(&v)),
                None => Ok(()),
            }
        };
        let flag = |b: Option<bool>| b.map(|b| if b { "1" } else { "0" }.to_string());
        let r = (|| -> Result<()> {
            set(None, "viewing_bmdgen", c.color_science_gen.map(|v| v.to_string()))?;
            set(None, "viewing_gamut", c.gamut.clone())?;
            set(None, "viewing_gamma", c.gamma.clone())?;
            set(None, "highlight_recovery", flag(c.highlight_recovery))?;
            set(None, "gamut_compression_enable", flag(c.gamut_compression))?;
            if color.post3dlut_mode.is_some() {
                set(None, "post_3dlut_mode", c.post3dlut_mode.clone())?;
            }
            set(Some(0), "iso", c.iso.map(|v| v.to_string()))?;
            set(Some(0), "white_balance_kelvin", c.kelvin.map(|v| v.to_string()))?;
            set(Some(0), "white_balance_tint", c.tint.map(|v| v.to_string()))?;
            set(Some(0), "exposure", c.exposure().map(|v| format!("{v:.6}")))?;
            self.save_sidecar()
        })();
        if r.is_err() {
            let _ = self.reload_sidecar(); // drop half-applied changes
        }
        r
    }

    fn clear_sidecar(&self) -> Result<()> {
        for k in ["viewing_bmdgen", "viewing_gamut", "viewing_gamma", "highlight_recovery", "gamut_compression_enable", "post_3dlut_mode"] {
            let _ = self.set_metadata(None, k, None);
        }
        for k in ["iso", "white_balance_kelvin", "white_balance_tint", "exposure"] {
            let _ = self.set_metadata(Some(0), k, None);
        }
        self.save_sidecar()?;
        // Nothing of ours (or anyone's) left: remove the file so the clip is pristine again.
        let side = self.info.path.with_extension("sidecar");
        if let Ok(text) = std::fs::read_to_string(&side) {
            if serde_json::from_str::<Value>(&text).is_ok_and(|v| !has_value(&v)) {
                let _ = std::fs::remove_file(&side);
            }
        }
        Ok(())
    }

    fn read_audio(&self, start: u64, max_frames: usize) -> Result<Vec<f32>> {
        let mut a = sys::rf_braw_audio_info_t::default();
        if unsafe { sys::rf_braw_audio_info(self.h.0, &mut a) } != sys::RF_OK {
            return Ok(Vec::new());
        }
        if start >= a.sample_count {
            return Ok(Vec::new());
        }
        let ch = a.channels.max(1) as usize;
        let bps = (a.bit_depth as usize).div_ceil(8).max(1);
        let frames = max_frames.min((a.sample_count - start) as usize);
        let mut buf = vec![0u8; frames * ch * bps];
        let mut got = 0u32;
        let rc = unsafe { sys::rf_braw_audio_read(self.h.0, start, frames as u32, buf.as_mut_ptr() as *mut _, buf.len() as u32, &mut got) };
        if rc != sys::RF_OK {
            return Err(Error::Decode(last_error()));
        }
        let n = got as usize * ch;
        let mut out = Vec::with_capacity(n);
        // Signed little-endian PCM (16 or 24 bit, occasionally 32) -> f32.
        for i in 0..n {
            let s = &buf[i * bps..i * bps + bps];
            let v = match bps {
                2 => i16::from_le_bytes([s[0], s[1]]) as f32 / 32768.0,
                3 => ((i32::from_le_bytes([0, s[0], s[1], s[2]])) >> 8) as f32 / 8_388_608.0,
                4 => i32::from_le_bytes([s[0], s[1], s[2], s[3]]) as f32 / 2_147_483_648.0,
                _ => (s[0] as f32 - 128.0) / 128.0,
            };
            out.push(v);
        }
        Ok(out)
    }

    fn decode(&self, req: &DecodeRequest) -> Result<DecodedFrame> {
        if req.frame >= self.info.frame_count {
            return Err(Error::FrameOutOfRange(req.frame));
        }
        let c = &req.color;
        let cs = |s: &Option<String>| s.as_ref().map(|v| CString::new(v.as_str()).unwrap_or_default());
        let gamma = cs(&c.gamma);
        let gamut = cs(&c.gamut);
        let lutm = cs(&c.post3dlut_mode);
        let ptr = |s: &Option<CString>| s.as_ref().map(|v| v.as_ptr()).unwrap_or(std::ptr::null());
        let params = sys::rf_braw_decode_t {
            frame: req.frame,
            scale: req.scale.divisor(),
            // The CPU pipeline only offers 32-bit float RGB among float formats; RGBA f16
            // for the viewer is produced from it below.
            format: sys::RF_FORMAT_RGB_F32,
            gamma: ptr(&gamma),
            gamut: ptr(&gamut),
            post3dlut_mode: ptr(&lutm),
            color_science_gen: c.color_science_gen.map(|v| v as i32).unwrap_or(-1),
            highlight_recovery: c.highlight_recovery.map(|v| v as i32).unwrap_or(-1),
            gamut_compression: c.gamut_compression.map(|v| v as i32).unwrap_or(-1),
            iso: c.iso.map(|v| v as i64).unwrap_or(-1),
            kelvin: c.kelvin.map(|v| v as i64).unwrap_or(-1),
            has_tint: c.tint.is_some() as i32,
            tint: c.tint.unwrap_or(0) as i32,
            has_exposure: c.exposure().is_some() as i32,
            exposure: c.exposure().unwrap_or(0.0),
        };
        let mut img = sys::rf_image_t::default();
        if unsafe { sys::rf_braw_decode(self.h.0, &params, &mut img) } != sys::RF_OK {
            return Err(Error::Decode(last_error()));
        }
        let n = (img.width as usize) * (img.height as usize);
        let bytes = unsafe { std::slice::from_raw_parts(img.data, img.size as usize) };
        let src = &bytes[..(n * 12).min(bytes.len())];
        let pixels = match req.format {
            PixelFormat::RgbaF16 => {
                let mut v = vec![half::f16::ONE; n * 4];
                v.par_chunks_mut(4).zip(src.par_chunks_exact(12)).for_each(|(d, s)| {
                    for c in 0..3 {
                        let f = f32::from_le_bytes([s[c * 4], s[c * 4 + 1], s[c * 4 + 2], s[c * 4 + 3]]);
                        d[c] = half::f16::from_f32(f);
                    }
                });
                Pixels::RgbaF16(v)
            }
            PixelFormat::RgbF32 => {
                let mut v = vec![0f32; n * 3];
                v.par_iter_mut().zip(src.par_chunks_exact(4)).for_each(|(d, s)| {
                    *d = f32::from_le_bytes([s[0], s[1], s[2], s[3]]);
                });
                Pixels::RgbF32(v)
            }
        };
        let (w, h) = (img.width, img.height);
        unsafe { sys::rf_free(img.data as *mut _) };
        Ok(DecodedFrame { frame: req.frame, width: w, height: h, pixels, encoding: self.encoding_for(c) })
    }
}

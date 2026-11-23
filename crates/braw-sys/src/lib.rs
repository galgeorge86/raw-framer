//! Raw FFI to the C shim in `shim/braw_shim.h`. Use the safe `rf-braw` crate instead.
#![allow(non_camel_case_types)]

use std::os::raw::{c_char, c_int, c_void};

pub const RF_OK: c_int = 0;
pub const RF_FORMAT_RGBA_F16: u32 = 0;
pub const RF_FORMAT_RGB_F32: u32 = 1;

#[repr(C)]
pub struct rf_braw {
    _private: [u8; 0],
}

#[repr(C)]
#[derive(Debug, Default, Clone, Copy)]
pub struct rf_braw_info_t {
    pub width: u32,
    pub height: u32,
    pub fps: f32,
    pub frame_count: u64,
}

#[repr(C)]
#[derive(Debug, Clone, Copy)]
pub struct rf_braw_decode_t {
    pub frame: u64,
    pub scale: u32,
    pub format: u32,
    pub gamma: *const c_char,
    pub gamut: *const c_char,
    pub post3dlut_mode: *const c_char,
    pub color_science_gen: i32,
    pub highlight_recovery: i32,
    pub gamut_compression: i32,
    pub iso: i64,
    pub kelvin: i64,
    pub has_tint: i32,
    pub tint: i32,
    pub has_exposure: i32,
    pub exposure: f32,
}

#[repr(C)]
#[derive(Debug)]
pub struct rf_image_t {
    pub width: u32,
    pub height: u32,
    pub format: u32,
    pub data: *mut u8,
    pub size: u64,
}

impl Default for rf_image_t {
    fn default() -> Self {
        Self { width: 0, height: 0, format: 0, data: std::ptr::null_mut(), size: 0 }
    }
}

#[repr(C)]
#[derive(Debug, Default, Clone, Copy)]
pub struct rf_braw_audio_info_t {
    pub sample_rate: u32,
    pub channels: u32,
    pub bit_depth: u32,
    pub sample_count: u64,
}

/// True when the crate was compiled against the Blackmagic RAW SDK.
pub const SDK_AVAILABLE: bool = cfg!(braw_sdk);

#[cfg(braw_sdk)]
extern "C" {
    pub fn rf_braw_open(clip_path: *const u16, runtime_dir: *const u16, out: *mut *mut rf_braw) -> c_int;
    pub fn rf_braw_close(b: *mut rf_braw);
    pub fn rf_braw_info(b: *mut rf_braw, out: *mut rf_braw_info_t) -> c_int;
    pub fn rf_braw_clip_json(b: *mut rf_braw, json_out: *mut *mut c_char) -> c_int;
    pub fn rf_braw_frame_json(b: *mut rf_braw, frame: u64, json_out: *mut *mut c_char) -> c_int;
    pub fn rf_braw_gamma_list(b: *mut rf_braw, gen: i32, json_out: *mut *mut c_char) -> c_int;
    pub fn rf_braw_decode(b: *mut rf_braw, params: *const rf_braw_decode_t, out: *mut rf_image_t) -> c_int;
    pub fn rf_braw_set_metadata(b: *mut rf_braw, frame: i64, key: *const c_char, value: *const c_char) -> c_int;
    pub fn rf_braw_save_sidecar(b: *mut rf_braw) -> c_int;
    pub fn rf_braw_reload_sidecar(b: *mut rf_braw) -> c_int;
    pub fn rf_braw_sidecar_attached(b: *mut rf_braw, attached: *mut c_int) -> c_int;
    pub fn rf_braw_pipelines(runtime_dir: *const u16, json_out: *mut *mut c_char) -> c_int;
    pub fn rf_braw_set_pipeline(id: *const c_char) -> c_int;
    pub fn rf_braw_active_pipeline(out: *mut *mut c_char) -> c_int;
    pub fn rf_free(p: *mut c_void);
    pub fn rf_braw_last_error() -> *const c_char;
    pub fn rf_braw_audio_info(b: *mut rf_braw, out: *mut rf_braw_audio_info_t) -> c_int;
    pub fn rf_braw_audio_read(b: *mut rf_braw, start: u64, max_frames: u32, buf: *mut c_void, buf_bytes: u32, frames_read: *mut u32) -> c_int;
}

#[cfg(not(braw_sdk))]
mod stubs {
    use super::*;
    const MSG: &[u8] = b"Raw Framer was built without the Blackmagic RAW SDK\0";
    pub unsafe extern "C" fn rf_braw_open(_: *const u16, _: *const u16, _: *mut *mut rf_braw) -> c_int {
        1
    }
    pub unsafe extern "C" fn rf_braw_close(_: *mut rf_braw) {}
    pub unsafe extern "C" fn rf_braw_info(_: *mut rf_braw, _: *mut rf_braw_info_t) -> c_int {
        1
    }
    pub unsafe extern "C" fn rf_braw_clip_json(_: *mut rf_braw, _: *mut *mut c_char) -> c_int {
        1
    }
    pub unsafe extern "C" fn rf_braw_frame_json(_: *mut rf_braw, _: u64, _: *mut *mut c_char) -> c_int {
        1
    }
    pub unsafe extern "C" fn rf_braw_gamma_list(_: *mut rf_braw, _: i32, _: *mut *mut c_char) -> c_int {
        1
    }
    pub unsafe extern "C" fn rf_braw_decode(_: *mut rf_braw, _: *const rf_braw_decode_t, _: *mut rf_image_t) -> c_int {
        1
    }
    pub unsafe extern "C" fn rf_braw_set_metadata(_: *mut rf_braw, _: i64, _: *const c_char, _: *const c_char) -> c_int {
        1
    }
    pub unsafe extern "C" fn rf_braw_save_sidecar(_: *mut rf_braw) -> c_int {
        1
    }
    pub unsafe extern "C" fn rf_braw_reload_sidecar(_: *mut rf_braw) -> c_int {
        1
    }
    pub unsafe extern "C" fn rf_braw_sidecar_attached(_: *mut rf_braw, _: *mut c_int) -> c_int {
        1
    }
    pub unsafe extern "C" fn rf_braw_pipelines(_: *const u16, _: *mut *mut c_char) -> c_int {
        1
    }
    pub unsafe extern "C" fn rf_braw_set_pipeline(_: *const c_char) -> c_int {
        1
    }
    pub unsafe extern "C" fn rf_braw_active_pipeline(_: *mut *mut c_char) -> c_int {
        1
    }
    pub unsafe extern "C" fn rf_free(_: *mut c_void) {}
    pub unsafe extern "C" fn rf_braw_audio_info(_: *mut rf_braw, _: *mut rf_braw_audio_info_t) -> c_int {
        1
    }
    pub unsafe extern "C" fn rf_braw_audio_read(_: *mut rf_braw, _: u64, _: u32, _: *mut c_void, _: u32, _: *mut u32) -> c_int {
        1
    }
    pub unsafe extern "C" fn rf_braw_last_error() -> *const c_char {
        MSG.as_ptr() as *const c_char
    }
}
#[cfg(not(braw_sdk))]
pub use stubs::*;

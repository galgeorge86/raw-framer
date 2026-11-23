//! Grabbed frames ("mounts" on the lightbox tray).
//!
//! A saved frame stays *linked* to its clip's Camera RAW settings: correcting ISO, WB,
//! tint or exposure after grabbing updates the frame (thumbnail and export). A frame can
//! instead keep its own settings (`own`), which then never change with the clip.

use rf_core::{AsShot, DecodeColor, SharedSource};
use std::path::PathBuf;

#[derive(Clone, PartialEq)]
pub enum Status {
    Ready,
    Queued,
    Exporting,
    /// Exported; `seen` flips once the user has looked at it (hover/click).
    Exported { path: PathBuf, seen: bool },
    Failed(String),
}

pub struct SavedFrame {
    pub id: u64,
    pub source: SharedSource,
    pub clip: String,
    pub frame: u64,
    pub timecode: String,
    /// Effective decode settings (synced from the clip while linked).
    pub color: DecodeColor,
    /// `Some` = this frame keeps its own settings instead of following the clip.
    pub own: Option<DecodeColor>,
    pub thumb: egui::TextureHandle,
    pub thumb_size: egui::Vec2,
    /// Fingerprint of the settings the thumbnail was rendered with.
    pub thumb_key: u64,
    /// Fingerprint of a thumbnail render currently in flight.
    pub thumb_pending: Option<u64>,
    pub status: Status,
    /// Settings fingerprint of the last successful export, and its file.
    pub exported: Option<(u64, PathBuf)>,
}

impl SavedFrame {
    pub fn short_clip(&self) -> String {
        // A002_05302106_C014 -> C014
        self.clip.rsplit('_').next().unwrap_or(&self.clip).to_string()
    }
}

/// Effective values (falling back to as-shot) and whether they differ from the camera.
pub fn summarize(color: &DecodeColor, a: &AsShot) -> (bool, String) {
    let iso = color.iso.or(a.iso);
    let k = color.kelvin.or(a.kelvin);
    let t = color.tint.or(a.tint);
    let ev = color.exposure().or(a.exposure).unwrap_or(0.0);
    let edited = iso != a.iso
        || k != a.kelvin
        || t != a.tint
        || (ev - a.exposure.unwrap_or(0.0)).abs() > 1e-4
        || (color.color_science_gen.is_some() && color.color_science_gen != a.color_science_gen)
        || (color.gamma.is_some() && color.gamma != a.gamma)
        || (color.gamut.is_some() && color.gamut != a.gamut)
        || (color.highlight_recovery.is_some() && color.highlight_recovery != a.highlight_recovery)
        || (color.gamut_compression.is_some() && color.gamut_compression != a.gamut_compression);
    let text = format!(
        "ISO {} · {} · tint {} · {:+.2} EV",
        iso.map(|v| v.to_string()).unwrap_or("–".into()),
        k.map(|v| format!("{v}K")).unwrap_or("–".into()),
        t.map(|v| format!("{v:+}")).unwrap_or("–".into()),
        ev
    );
    (edited, text)
}

//! The working session (open clips, playhead positions, grabbed frames and all
//! preferences), saved to `%APPDATA%\Raw Framer\session.json` within about a second of any
//! change so a crash or power cut loses nothing. Writes are atomic (temp file + rename,
//! previous copy kept as `session.bak`).

use crate::app::Prefs;
use rf_core::DecodeColor;
use serde::{Deserialize, Serialize};
use std::path::{Path, PathBuf};

pub const VERSION: u32 = 1;

#[derive(Serialize, Deserialize, Default)]
#[serde(default)]
pub struct Session {
    pub version: u32,
    pub prefs: Option<Prefs>,
    pub clips: Vec<SessionClip>,
    pub active: Option<usize>,
    pub frames: Vec<SessionFrame>,
    pub selected: Option<u64>,
    pub next_id: u64,
}

#[derive(Serialize, Deserialize, Clone, PartialEq, Debug)]
pub struct SessionClip {
    pub path: PathBuf,
    pub frame: u64,
}

#[derive(Serialize, Deserialize, Clone, PartialEq, Debug)]
#[serde(default)]
pub struct SessionFrame {
    pub id: u64,
    pub path: PathBuf,
    pub frame: u64,
    /// Effective settings when saved (used if the clip isn't open to link to).
    pub color: DecodeColor,
    /// The frame's own settings, when it doesn't follow its clip.
    pub own: Option<DecodeColor>,
    /// Last successful export: settings fingerprint and file.
    pub exported: Option<(u64, PathBuf)>,
    /// Shown as exported (and whether the user has seen it).
    pub exported_shown: Option<(PathBuf, bool)>,
    pub failed: Option<String>,
}

impl Default for SessionFrame {
    fn default() -> Self {
        Self { id: 0, path: PathBuf::new(), frame: 0, color: DecodeColor::default(), own: None, exported: None, exported_shown: None, failed: None }
    }
}

pub fn path() -> PathBuf {
    crate::safety::data_dir().join("session.json")
}

/// Load the session, falling back to the backup when the main file is damaged.
pub fn load() -> Option<Session> {
    load_from(&path())
}

pub fn load_from(p: &Path) -> Option<Session> {
    for candidate in [p.to_path_buf(), p.with_extension("bak")] {
        let Ok(bytes) = std::fs::read(&candidate) else { continue };
        match serde_json::from_slice::<Session>(&bytes) {
            Ok(s) => return Some(s),
            Err(e) => log::warn!("session: couldn't read {}: {e}", candidate.display()),
        }
    }
    None
}

pub fn save_bytes(p: &Path, bytes: &[u8]) -> std::io::Result<()> {
    crate::safety::write_atomic(p, bytes)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn round_trip_and_backup_fallback() {
        let dir = std::env::temp_dir().join(format!("rf-session-{}", std::process::id()));
        let p = dir.join("session.json");
        let mut s = Session { version: VERSION, next_id: 7, active: Some(0), selected: Some(3), ..Default::default() };
        s.clips.push(SessionClip { path: "D:/clips/A001_C014.braw".into(), frame: 120 });
        s.frames.push(SessionFrame {
            id: 3,
            path: "D:/clips/A001_C014.braw".into(),
            frame: 120,
            own: Some(DecodeColor { iso: Some(800), ..Default::default() }),
            exported: Some((42, "C:/out/A001_C014_000120.dng".into())),
            ..Default::default()
        });
        s.prefs = Some(Prefs::default());
        let bytes = serde_json::to_vec(&s).unwrap();
        save_bytes(&p, &bytes).unwrap();
        save_bytes(&p, &bytes).unwrap(); // second write leaves a good .bak
        let back = load_from(&p).unwrap();
        assert_eq!(back.clips, s.clips);
        assert_eq!(back.frames, s.frames);
        assert_eq!((back.active, back.selected, back.next_id), (Some(0), Some(3), 7));

        // A torn/corrupt main file falls back to the backup.
        std::fs::write(&p, b"{\"version\":1,\"clips\":[{\"pa").unwrap();
        let back = load_from(&p).unwrap();
        assert_eq!(back.frames.len(), 1);

        // Older/newer files with unknown or missing fields still load.
        std::fs::write(&p, b"{\"version\":1,\"clips\":[],\"future_field\":true}").unwrap();
        assert!(load_from(&p).is_some());
        let _ = std::fs::remove_dir_all(dir);
    }
}

//! Raw Framer main window: state, shortcuts and the Darkroom Lightbox layout
//! (viewer + transport + tray on the left, one 320px control column on the right).

use crate::export::{ExportEvent, ExportJob, Exporter};
use crate::player::{Key, Player};
use crate::assists::{self, AssistKind, Assists, Histogram};
use crate::audio::AudioOut;
use crate::saved_frames::{summarize, SavedFrame, Status};
use crate::session::{self, Session, SessionClip, SessionFrame};
use crate::theme::{self, icon, *};
use crate::thumbs::{ThumbJob, Thumbnailer};
use std::collections::HashMap;
use crate::viewer::ViewerCallback;
use crate::widgets::{self, *};
use eframe::egui_wgpu;
use egui::{Align, Align2, CornerRadius, Layout, Pos2, Rect, RichText, Sense, Stroke, Ui, Vec2};
use rf_color::{CubeLut, LutPlacement, OutputMode, ToneMap, ViewSettings};
use rf_core::*;
use rf_export::{ExportKind, ExportSettings, Sample};
use serde::{Deserialize, Serialize};
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::{Duration, Instant};

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum PreviewRes {
    Auto,
    Fixed(ResolutionScale),
}

#[derive(Serialize, Deserialize)]
#[serde(default)]
pub struct Prefs {
    pub view: ViewSettings,
    pub export: ExportSettings,
    pub preview: PreviewRes,
    /// Blackmagic RAW decode pipeline: "cpu", "cuda" or "opencl".
    pub decode_pipeline: String,
    pub full_when_paused: bool,
    pub last_dir: Option<PathBuf>,
    pub lut_path: Option<PathBuf>,
    /// Camera RAW corrections per clip (keyed by full path), restored when the clip reopens.
    pub clip_settings: HashMap<String, ClipSettings>,
    /// Viewer assists (guides, peaking, false color, histogram). Viewer-only.
    pub assists: Assists,
    /// Playback volume 0..1 and mute.
    pub volume: f32,
    pub muted: bool,
    /// Side column panel heights / collapsed state.
    pub dock: crate::dock::Dock,
}

#[derive(Clone, Default, PartialEq, Serialize, Deserialize)]
pub struct ClipSettings {
    pub custom: bool,
    pub color: DecodeColor,
}

impl Default for Prefs {
    fn default() -> Self {
        let pictures = std::env::var_os("USERPROFILE")
            .map(|p| PathBuf::from(p).join("Pictures").join("Raw Framer"))
            .unwrap_or_else(|| PathBuf::from("Raw Framer"));
        Self {
            view: ViewSettings::default(),
            export: ExportSettings { output_dir: pictures, ..Default::default() },
            preview: PreviewRes::Auto,
            decode_pipeline: "cpu".into(),
            full_when_paused: true,
            last_dir: None,
            lut_path: None,
            clip_settings: HashMap::new(),
            assists: Assists::default(),
            volume: 0.8,
            muted: false,
            dock: crate::dock::Dock::default(),
        }
    }
}

/// Decode settings for viewing a clip: when the CST is on and the clip's curve/gamut has
/// no formula here (e.g. Gen 4/6 Film), ask the SDK for linear BMD WG instead.
fn viewer_decode_color(source: &dyn MediaSource, mut c: DecodeColor, v: &ViewSettings, has_lut: bool) -> DecodeColor {
    let lut_on_log = v.lut_enabled && has_lut && v.lut_placement == LutPlacement::OnLog;
    if v.output == OutputMode::AsDecoded || lut_on_log {
        return c;
    }
    let a = &source.info().as_shot;
    let gen = c.color_science_gen.or(a.color_science_gen);
    let gamma = c.gamma.clone().or_else(|| a.gamma.clone()).unwrap_or_default();
    let gamut = c.gamut.clone().or_else(|| a.gamut.clone()).unwrap_or_default();
    let t = rf_color::classify_gamma(&gamma, gen);
    let g = rf_color::classify_gamut(&gamut);
    let unknown = matches!(t, Transfer::Unknown(_)) || (matches!(g, Gamut::Unknown(_)) && !matches!(t, Transfer::Display(_)));
    if unknown {
        if let Some(lin) = source.linear_gamma_name() {
            c.gamma = Some(lin);
            if let Some(wg) = source.capabilities().gamuts.iter().find(|n| rf_color::classify_gamut(n) == Gamut::BmdWideGamut) {
                c.gamut = Some(wg.clone());
            }
        }
    }
    c
}

/// Fingerprint of everything a thumbnail depends on.
fn thumb_key(color: &DecodeColor, view: &ViewSettings, lut: &Option<Arc<CubeLut>>) -> u64 {
    use std::hash::{Hash, Hasher};
    let mut h = std::collections::hash_map::DefaultHasher::new();
    color.hash(&mut h);
    format!("{view:?}").hash(&mut h);
    lut.as_ref().map(|l| Arc::as_ptr(l) as usize).hash(&mut h);
    h.finish()
}

pub struct Clip {
    pub source: SharedSource,
    pub name: String,
    pub player: Player,
    pub custom: bool,
    pub color: DecodeColor,
    /// Small poster of the first frame for the clip list.
    pub poster: Option<egui::TextureHandle>,
    pub has_audio: bool,
}

impl Clip {
    fn decode_color(&self) -> DecodeColor {
        if self.custom {
            self.color.clone()
        } else {
            DecodeColor::default()
        }
    }
}

#[derive(Clone, Copy, PartialEq)]
enum RawAction {
    SaveSidecar,
    RevertSidecar,
    CopyToAll,
}

#[derive(Clone, Copy, PartialEq)]
enum Zoom {
    Fit,
    /// Image pixels per physical screen pixel.
    Scale(f32),
}

struct Toast {
    text: String,
    error: bool,
    born: Instant,
    reveal: Option<PathBuf>,
}

pub struct RawFramerApp {
    registry: SourceRegistry,
    clips: Vec<Clip>,
    active: Option<usize>,
    prefs: Prefs,
    lut: Option<Arc<CubeLut>>,
    saved: Vec<SavedFrame>,
    selected: Option<u64>,
    next_id: u64,
    exporter: Exporter,
    export_total: usize,
    export_done: usize,
    toasts: Vec<Toast>,
    zoom: Zoom,
    pan: Vec2,
    show_shortcuts: bool,
    backend_note: Option<String>,
    title: String,
    thumbs: Thumbnailer,
    /// Last time any decode/view setting changed (thumbnail refresh waits for it to settle).
    settings_changed_at: Instant,
    settings_fp: u64,
    /// Settings fingerprint of each frame currently being exported.
    export_fp: HashMap<u64, u64>,
    /// Clip row to bring into view in the Clips panel (after opening / activating).
    scroll_to_clip: Option<usize>,
    /// Grabbed frame to bring into view in the tray (after a grab).
    scroll_to_frame: Option<u64>,
    /// Open assist options popover, its anchor button and when it opened.
    assist_popover: Option<(AssistKind, Rect, Instant)>,
    /// Histogram cache: (frame/settings key, computed at, histogram).
    hist: Option<(u64, Instant, Histogram)>,
    audio: AudioOut,
    /// This instance owns the session file (a second instance doesn't overwrite it).
    primary: bool,
    /// Last session write, hash of what was written, and whether a write is pending.
    session_written_at: Instant,
    session_hash: u64,
    mem_checked: Instant,
    low_memory_warned: bool,
    /// Viewer decode-settings fingerprint and when it last changed (live preview).
    live_hash: u64,
    live_changed_at: Instant,
    /// Decode pipelines found on this machine (filled in by a background probe).
    pipelines: Arc<std::sync::Mutex<Option<Vec<rf_braw::Pipeline>>>>,
    /// What the codec actually uses ("CPU", "OpenCL · …").
    active_pipeline: String,
}

impl RawFramerApp {
    pub fn new(cc: &eframe::CreationContext<'_>, startup: crate::safety::Startup) -> Self {
        theme::install_fonts(&cc.egui_ctx);
        theme::apply_style(&cc.egui_ctx);
        if let Some(rs) = cc.wgpu_render_state.as_ref() {
            crate::viewer::init(rs);
        }
        // The session file is written within a second of every change; eframe's own storage
        // (saved every 30 s and on exit) is only a fallback for older installs.
        let mut session = if startup.primary { session::load() } else { None };
        let prefs: Prefs = session
            .as_mut()
            .and_then(|s| s.prefs.take())
            .or_else(|| cc.storage.and_then(|s| eframe::get_value(s, "prefs")))
            .unwrap_or_default();

        let mut registry = SourceRegistry::new();
        registry.register(
            "Blackmagic RAW",
            rf_braw::EXTENSIONS,
            Box::new(|p: &Path| Ok(Arc::new(rf_braw::BrawSource::open(p)?) as SharedSource)),
        );
        registry.register("Test pattern", &["rfpattern"], Box::new(|p: &Path| Ok(Arc::new(TestPatternSource::new(p.to_path_buf())) as SharedSource)));

        // Clips opened from here on use the chosen decode pipeline. Finding GPU devices takes
        // a moment, so the list for the menu is built in the background.
        rf_braw::set_pipeline(&prefs.decode_pipeline);
        let pipelines = Arc::new(std::sync::Mutex::new(None));
        {
            let slot = pipelines.clone();
            let ctx = cc.egui_ctx.clone();
            let _ = std::thread::Builder::new().name("pipelines".into()).spawn(move || {
                let list = rf_braw::pipelines();
                if let Ok(mut s) = slot.lock() {
                    *s = Some(list);
                }
                ctx.request_repaint();
            });
        }

        let backend_note = if !rf_braw::sdk_available() {
            Some("Built without the Blackmagic RAW SDK: .braw files can't be decoded. Rebuild with the SDK installed.".into())
        } else if rf_braw::find_runtime().is_none() {
            Some("Blackmagic RAW runtime not found. Install Blackmagic RAW or DaVinci Resolve to decode .braw files.".into())
        } else {
            None
        };

        let mut app = Self {
            registry,
            clips: Vec::new(),
            active: None,
            lut: None,
            saved: Vec::new(),
            selected: None,
            next_id: 1,
            exporter: Exporter::new(cc.egui_ctx.clone()),
            export_total: 0,
            export_done: 0,
            toasts: Vec::new(),
            zoom: Zoom::Fit,
            pan: Vec2::ZERO,
            show_shortcuts: false,
            backend_note,
            title: String::new(),
            thumbs: Thumbnailer::new(cc.egui_ctx.clone()),
            settings_changed_at: Instant::now(),
            settings_fp: 0,
            export_fp: HashMap::new(),
            scroll_to_clip: None,
            scroll_to_frame: None,
            assist_popover: None,
            hist: None,
            audio: AudioOut::new(),
            prefs,
            primary: startup.primary,
            session_written_at: Instant::now(),
            session_hash: 0,
            mem_checked: Instant::now(),
            low_memory_warned: false,
            live_hash: 0,
            live_changed_at: Instant::now(),
            pipelines,
            active_pipeline: "CPU".into(),
        };
        if let Some(s) = session {
            app.restore_session(&cc.egui_ctx, s);
        }
        app.active_pipeline = rf_braw::active_pipeline();
        if startup.previous_crashed {
            log::warn!("previous run ended unexpectedly (crash log: {:?})", startup.crash_log);
            app.toasts.push(Toast {
                text: "Raw Framer closed unexpectedly last time. Your clips and grabbed frames were restored.".into(),
                error: true,
                born: Instant::now(),
                reveal: startup.crash_log.clone(),
            });
        }
        if !startup.primary {
            app.toast("Another Raw Framer window is open, so this one won't remember its session.", false);
        }
        if let Some(p) = app.prefs.lut_path.clone() {
            if let Ok(l) = CubeLut::load(&p) {
                app.lut = Some(Arc::new(l));
            }
        }
        // Clips passed on the command line ("Open with").
        // `raw-framer [--lut file.cube] clip.braw ...`
        let mut args = std::env::args().skip(1);
        while let Some(a) = args.next() {
            if a == "--lut" {
                if let Some(p) = args.next().map(PathBuf::from) {
                    match CubeLut::load(&p) {
                        Ok(l) => {
                            app.lut = Some(Arc::new(l));
                            app.prefs.lut_path = Some(p);
                            app.prefs.view.lut_enabled = true;
                        }
                        Err(e) => app.toast(format!("Couldn't read LUT: {e}"), true),
                    }
                }
                continue;
            }
            let p = PathBuf::from(a);
            if p.exists() {
                app.open_path(&cc.egui_ctx, &p);
            }
        }
        app
    }

    fn toast(&mut self, text: impl Into<String>, error: bool) {
        self.toasts.push(Toast { text: text.into(), error, born: Instant::now(), reveal: None });
    }

    fn open_path(&mut self, ctx: &egui::Context, path: &Path) {
        if let Some(i) = self.clips.iter().position(|c| c.source.info().path == path) {
            self.activate(i);
            return;
        }
        if !self.registry.can_open(path) {
            self.toast(format!("{} isn't a supported clip (.braw)", path.file_name().unwrap_or_default().to_string_lossy()), true);
            return;
        }
        match self.registry.open(path) {
            Ok(source) => {
                self.active_pipeline = rf_braw::active_pipeline();
                let name = source.info().file_stem();
                let workers = std::thread::available_parallelism().map(|n| (n.get() / 4).clamp(2, 4)).unwrap_or(3);
                let player = Player::new(source.clone(), ctx.clone(), workers);
                let poster = source
                    .decode(&DecodeRequest { frame: 0, scale: ResolutionScale::Eighth, format: PixelFormat::RgbaF16, color: DecodeColor::default() })
                    .ok()
                    .map(|f| {
                        let (w, h, rgb) = rf_color::view::render_rgb8(&f, ViewSettings { lut_enabled: false, ..Default::default() }, None, 112);
                        ctx.load_texture(format!("poster-{name}"), egui::ColorImage::from_rgb([w as usize, h as usize], &rgb), egui::TextureOptions::LINEAR)
                    });
                // Restore this clip's Camera RAW corrections from a previous session.
                let saved = self.prefs.clip_settings.get(&path.to_string_lossy().to_string()).cloned().unwrap_or_default();
                let has_audio = source.audio_info().is_some();
                self.clips.push(Clip { source, name, player, custom: saved.custom, color: saved.color, poster, has_audio });
                self.activate(self.clips.len() - 1);
                if let Some(d) = path.parent() {
                    self.prefs.last_dir = Some(d.to_path_buf());
                }
            }
            Err(e) => self.toast(format!("Couldn't open {}: {e}", path.file_name().unwrap_or_default().to_string_lossy()), true),
        }
    }

    fn open_dialog(&mut self, ctx: &egui::Context) {
        let mut d = rfd::FileDialog::new().add_filter("Blackmagic RAW", &["braw"]).set_title("Open clips");
        if let Some(dir) = &self.prefs.last_dir {
            d = d.set_directory(dir);
        }
        if let Some(files) = d.pick_files() {
            for f in files {
                self.open_path(ctx, &f);
            }
        }
    }

    fn open_test_pattern(&mut self, ctx: &egui::Context) {
        self.open_path(ctx, Path::new("Test pattern.rfpattern"));
    }

    fn activate(&mut self, i: usize) {
        if let Some(a) = self.active {
            if a != i {
                if let Some(c) = self.clips.get_mut(a) {
                    c.player.pause();
                }
            }
        }
        if self.active != Some(i) {
            self.audio.stop();
        }
        self.active = Some(i);
        self.scroll_to_clip = Some(i);
        self.zoom = Zoom::Fit;
        self.pan = Vec2::ZERO;
    }

    fn close_clip(&mut self, i: usize) {
        self.audio.stop();
        self.clips.remove(i);
        self.active = match self.active {
            Some(a) if a == i => (!self.clips.is_empty()).then(|| i.min(self.clips.len() - 1)),
            Some(a) if a > i => Some(a - 1),
            other => other,
        };
    }

    // ------------------------------------------------------------------ session & memory

    /// Reopen the clips and grabbed frames of the last session.
    fn restore_session(&mut self, ctx: &egui::Context, s: Session) {
        let openable = |p: &Path| p.exists() || p.extension().is_some_and(|e| e == "rfpattern");
        let mut missing: Vec<String> = Vec::new();
        let mut index_map: HashMap<usize, usize> = HashMap::new();
        for (si, c) in s.clips.iter().enumerate() {
            if !openable(&c.path) {
                missing.push(c.path.file_name().unwrap_or_default().to_string_lossy().to_string());
                continue;
            }
            let before = self.clips.len();
            self.open_path(ctx, &c.path);
            if self.clips.len() > before {
                let i = self.clips.len() - 1;
                self.clips[i].player.seek(c.frame as i64);
                index_map.insert(si, i);
            }
        }
        // Grabbed frames: link to the reopened clip, or open the file just for them.
        let mut extra: HashMap<PathBuf, SharedSource> = HashMap::new();
        let mut skipped = 0usize;
        for f in s.frames {
            let source = if let Some(c) = self.clips.iter().find(|c| c.source.info().path == f.path) {
                c.source.clone()
            } else if let Some(src) = extra.get(&f.path) {
                src.clone()
            } else if openable(&f.path) {
                match self.registry.open(&f.path) {
                    Ok(src) => {
                        extra.insert(f.path.clone(), src.clone());
                        src
                    }
                    Err(_) => {
                        skipped += 1;
                        continue;
                    }
                }
            } else {
                skipped += 1;
                continue;
            };
            let info = source.info();
            if f.frame >= info.frame_count {
                skipped += 1;
                continue;
            }
            // Placeholder until the thumbnailer renders it (thumb_key 0 never matches).
            let (iw, ih) = (info.width.max(1) as f32, info.height.max(1) as f32);
            let thumb_size = if iw >= ih { Vec2::new(360.0, (360.0 * ih / iw).round()) } else { Vec2::new((360.0 * iw / ih).round(), 360.0) };
            let thumb = ctx.load_texture(format!("saved-{}", f.id), egui::ColorImage::from_rgb([1, 1], &[0x16, 0x16, 0x18]), egui::TextureOptions::LINEAR);
            let status = match (&f.exported_shown, &f.failed) {
                (Some((p, seen)), _) if p.exists() => Status::Exported { path: p.clone(), seen: *seen },
                (_, Some(e)) => Status::Failed(e.clone()),
                _ => Status::Ready,
            };
            self.saved.push(SavedFrame {
                id: f.id,
                clip: info.file_stem(),
                frame: f.frame,
                timecode: source.timecode(f.frame).unwrap_or_default(),
                color: f.own.clone().unwrap_or(f.color),
                own: f.own,
                thumb,
                thumb_size,
                thumb_key: 0,
                thumb_pending: None,
                status,
                exported: f.exported.filter(|(_, p)| p.exists()),
                source,
            });
        }
        let max_id = self.saved.iter().map(|f| f.id).max().unwrap_or(0);
        self.next_id = self.next_id.max(s.next_id).max(max_id + 1);
        self.selected = s.selected.filter(|id| self.saved.iter().any(|f| f.id == *id));
        if let Some(i) = s.active.and_then(|a| index_map.get(&a).copied()) {
            self.activate(i);
        }
        if !missing.is_empty() || skipped > 0 {
            let mut parts = Vec::new();
            if !missing.is_empty() {
                let names = if missing.len() <= 2 { missing.join(", ") } else { format!("{} clips", missing.len()) };
                parts.push(format!("{names} couldn't be found"));
            }
            if skipped > 0 {
                parts.push(format!("{skipped} grabbed frame{} from missing clips {} left out", if skipped == 1 { "" } else { "s" }, if skipped == 1 { "was" } else { "were" }));
            }
            self.toast(format!("Restored your last session, but {}.", parts.join(" and ")), true);
        }
        log::info!("session: restored {} clips, {} grabbed frames", self.clips.len(), self.saved.len());
    }

    fn session_bytes(&self) -> Vec<u8> {
        #[derive(Serialize)]
        struct Out<'a> {
            version: u32,
            prefs: &'a Prefs,
            clips: Vec<SessionClip>,
            active: Option<usize>,
            frames: Vec<SessionFrame>,
            selected: Option<u64>,
            next_id: u64,
        }
        let out = Out {
            version: session::VERSION,
            prefs: &self.prefs,
            clips: self.clips.iter().map(|c| SessionClip { path: c.source.info().path.clone(), frame: c.player.current_frame() }).collect(),
            active: self.active,
            frames: self
                .saved
                .iter()
                .map(|f| SessionFrame {
                    id: f.id,
                    path: f.source.info().path.clone(),
                    frame: f.frame,
                    color: f.color.clone(),
                    own: f.own.clone(),
                    exported: f.exported.clone(),
                    exported_shown: match &f.status {
                        Status::Exported { path, seen } => Some((path.clone(), *seen)),
                        _ => None,
                    },
                    failed: match &f.status {
                        Status::Failed(e) => Some(e.clone()),
                        _ => None,
                    },
                })
                .collect(),
            selected: self.selected,
            next_id: self.next_id,
        };
        serde_json::to_vec(&out).unwrap_or_default()
    }

    /// Write the session when it changed, at most once a second (immediately with `force`).
    fn autosave(&mut self, ctx: &egui::Context, force: bool) {
        if !self.primary {
            return;
        }
        let bytes = self.session_bytes();
        if bytes.is_empty() {
            return;
        }
        let hash = {
            use std::hash::{Hash, Hasher};
            let mut h = std::collections::hash_map::DefaultHasher::new();
            bytes.hash(&mut h);
            h.finish()
        };
        if hash == self.session_hash {
            return;
        }
        let since = self.session_written_at.elapsed();
        if !force && since < Duration::from_secs(1) {
            ctx.request_repaint_after(Duration::from_secs(1) - since);
            return;
        }
        self.session_written_at = Instant::now();
        match session::save_bytes(&session::path(), &bytes) {
            Ok(()) => self.session_hash = hash,
            Err(e) => log::warn!("session: couldn't save: {e}"),
        }
    }

    /// Keep frame memory bounded: one cache budget sized to the machine, only the clip on
    /// screen keeps decoded frames, and the cache shrinks when Windows runs low on memory.
    fn manage_memory(&mut self) {
        if self.mem_checked.elapsed() < Duration::from_millis(250) {
            return;
        }
        self.mem_checked = Instant::now();
        let (total, avail) = crate::safety::memory().unwrap_or((16 << 30, 8 << 30));
        let budget = ((total / 10) as usize).clamp(512 << 20, 2048 << 20);
        let active = self.active;
        for (i, c) in self.clips.iter_mut().enumerate() {
            if Some(i) == active {
                c.player.set_budget(budget);
            } else {
                c.player.release_memory();
            }
        }
        let low = avail < (1536u64 << 20);
        if low {
            if let Some(c) = active.and_then(|i| self.clips.get_mut(i)) {
                let target = (c.player.cache_bytes() / 2).max(256 << 20);
                c.player.set_budget(target);
                c.player.trim_to(target, 2);
            }
            if !self.low_memory_warned {
                log::warn!("low memory: {} MB available of {} MB; shrinking frame cache", avail >> 20, total >> 20);
            }
        }
        self.low_memory_warned = low;
    }

    fn load_lut_dialog(&mut self) {
        let mut d = rfd::FileDialog::new().add_filter("3D LUT", &["cube"]).set_title("Load LUT");
        if let Some(p) = &self.prefs.lut_path {
            if let Some(dir) = p.parent() {
                d = d.set_directory(dir);
            }
        }
        if let Some(p) = d.pick_file() {
            match CubeLut::load(&p) {
                Ok(l) => {
                    self.lut = Some(Arc::new(l));
                    self.prefs.lut_path = Some(p);
                    self.prefs.view.lut_enabled = true;
                }
                Err(e) => self.toast(format!("Couldn't read LUT: {e}"), true),
            }
        }
    }

    /// Decode settings for the viewer (see [`viewer_decode_color`]).
    fn preview_color(&self, clip: &Clip) -> DecodeColor {
        viewer_decode_color(clip.source.as_ref(), clip.decode_color(), &self.prefs.view, self.lut.is_some())
    }

    /// Keep grabbed frames linked to their clip's corrections, remember corrections per
    /// clip, and refresh tray thumbnails once settings stop changing.
    fn sync_saved(&mut self, ctx: &egui::Context) {
        // Remember every open clip's corrections (restored when it is opened again).
        for c in &self.clips {
            let key = c.source.info().path.to_string_lossy().to_string();
            let s = ClipSettings { custom: c.custom, color: c.color.clone() };
            if self.prefs.clip_settings.get(&key) != Some(&s) {
                self.prefs.clip_settings.insert(key, s);
            }
        }
        // Linked frames follow their (open) clip.
        for s in self.saved.iter_mut() {
            let effective = match &s.own {
                Some(own) => own.clone(),
                None => match self.clips.iter().find(|c| c.source.info().path == s.source.info().path) {
                    Some(c) => c.decode_color(),
                    None => s.color.clone(), // clip closed: keep the last known settings
                },
            };
            if effective != s.color {
                s.color = effective;
                if matches!(s.status, Status::Exported { .. } | Status::Failed(_)) {
                    s.status = Status::Ready; // needs exporting again with the new settings
                }
            }
        }
        // Thumbnails: wait until settings have been still for a moment (no work while dragging).
        let view = self.prefs.view;
        let fp = {
            use std::hash::{Hash, Hasher};
            let mut h = std::collections::hash_map::DefaultHasher::new();
            for s in &self.saved {
                thumb_key(&s.color, &view, &self.lut).hash(&mut h);
            }
            h.finish()
        };
        if fp != self.settings_fp {
            self.settings_fp = fp;
            self.settings_changed_at = Instant::now();
        }
        let dragging = ctx.input(|i| i.pointer.any_down());
        if !dragging && self.settings_changed_at.elapsed() > Duration::from_millis(250) {
            for s in self.saved.iter_mut() {
                let key = thumb_key(&s.color, &view, &self.lut);
                if key != s.thumb_key && s.thumb_pending != Some(key) {
                    s.thumb_pending = Some(key);
                    let has_lut = self.lut.is_some();
                    self.thumbs.submit(ThumbJob {
                        id: s.id,
                        key,
                        source: s.source.clone(),
                        frame: s.frame,
                        color: viewer_decode_color(s.source.as_ref(), s.color.clone(), &view, has_lut),
                        view,
                        lut: self.lut.clone(),
                    });
                }
            }
        } else if self.saved.iter().any(|s| s.thumb_key != thumb_key(&s.color, &view, &self.lut)) {
            ctx.request_repaint_after(Duration::from_millis(260));
        }
        while let Ok(r) = self.thumbs.rx.try_recv() {
            let Some(s) = self.saved.iter_mut().find(|s| s.id == r.id) else { continue };
            if s.thumb_pending == Some(r.key) {
                s.thumb_pending = None;
            }
            if let Some((w, h, rgb)) = r.image {
                s.thumb.set(egui::ColorImage::from_rgb([w as usize, h as usize], &rgb), egui::TextureOptions::LINEAR);
                s.thumb_size = Vec2::new(w as f32, h as f32);
                s.thumb_key = r.key;
            }
        }
    }

    // ------------------------------------------------------------------ grabbing & export

    fn grab(&mut self, ctx: &egui::Context) -> Option<u64> {
        let i = self.active?;
        let view = self.prefs.view;
        let lut = self.lut.clone();
        let clip = &mut self.clips[i];
        let frame = clip.player.current_frame();
        let color = clip.decode_color();
        // Thumbnail from what is on screen when it matches; otherwise a quick 1/4 decode.
        let shown = clip.player.shown.clone().filter(|f| f.frame == frame);
        let decoded = match shown {
            Some(f) => f,
            None => {
                let req = DecodeRequest { frame, scale: ResolutionScale::Quarter, format: PixelFormat::RgbaF16, color: self.preview_color(&self.clips[i]) };
                match self.clips[i].source.decode(&req) {
                    Ok(f) => Arc::new(f),
                    Err(e) => {
                        self.toast(format!("Couldn't grab frame {frame}: {e}"), true);
                        return None;
                    }
                }
            }
        };
        let clip = &self.clips[i];
        let (w, h, rgb) = rf_color::view::render_rgb8(&decoded, view, lut.as_deref(), 360);
        let img = egui::ColorImage::from_rgb([w as usize, h as usize], &rgb);
        let id = self.next_id;
        self.next_id += 1;
        let thumb = ctx.load_texture(format!("saved-{id}"), img, egui::TextureOptions::LINEAR);
        let key = thumb_key(&color, &view, &lut);
        self.saved.push(SavedFrame {
            id,
            source: clip.source.clone(),
            clip: clip.name.clone(),
            frame,
            timecode: clip.source.timecode(frame).unwrap_or_default(),
            color,
            own: None,
            thumb,
            thumb_size: Vec2::new(w as f32, h as f32),
            thumb_key: key,
            thumb_pending: None,
            status: Status::Ready,
            exported: None,
        });
        self.selected = Some(id);
        self.scroll_to_frame = Some(id);
        Some(id)
    }

    fn export_ids(&mut self, ids: Vec<u64>) {
        if ids.is_empty() {
            return;
        }
        let settings = self.prefs.export.clone();
        if let Err(e) = std::fs::create_dir_all(&settings.output_dir) {
            self.toast(format!("Can't use the export folder {}: {e}", settings.output_dir.display()), true);
            return;
        }
        let mut planned: Vec<PathBuf> = Vec::new();
        for id in ids {
            let Some(s) = self.saved.iter_mut().find(|s| s.id == id) else { continue };
            if matches!(s.status, Status::Queued | Status::Exporting) {
                continue;
            }
            let mut path = rf_export::output_path(s.source.as_ref(), s.frame, &settings);
            let first = path.clone();
            let mut n = 1;
            while planned.contains(&path) {
                let stem = first.file_stem().unwrap_or_default().to_string_lossy().to_string();
                let ext = first.extension().unwrap_or_default().to_string_lossy().to_string();
                path = first.with_file_name(format!("{stem}_{n}.{ext}"));
                n += 1;
            }
            planned.push(path.clone());
            s.status = Status::Queued;
            self.export_fp.insert(id, s.color.settings_hash());
            self.exporter.submit(ExportJob {
                id,
                source: s.source.clone(),
                frame: s.frame,
                color: s.color.clone(),
                view_color: viewer_decode_color(s.source.as_ref(), s.color.clone(), &self.prefs.view, self.lut.is_some()),
                settings: settings.clone(),
                view: self.prefs.view,
                lut: self.lut.clone(),
                path,
            });
            self.export_total += 1;
        }
    }

    fn export_all(&mut self) {
        let ids = self
            .saved
            .iter()
            .filter(|s| !matches!(s.status, Status::Exported { .. } | Status::Queued | Status::Exporting))
            .map(|s| s.id)
            .collect::<Vec<_>>();
        if ids.is_empty() {
            if self.saved.is_empty() {
                self.toast("Nothing to export yet. Press G to grab a frame.", false);
            } else {
                self.toast("Every grabbed frame is already exported", false);
            }
        }
        self.export_ids(ids);
    }

    fn poll_exports(&mut self) {
        while let Ok(ev) = self.exporter.rx.try_recv() {
            match ev {
                ExportEvent::Started(id) => {
                    if let Some(s) = self.saved.iter_mut().find(|s| s.id == id) {
                        s.status = Status::Exporting;
                    }
                }
                ExportEvent::Done(id, res) => {
                    self.exporter.pending = self.exporter.pending.saturating_sub(1);
                    self.export_done += 1;
                    let fp = self.export_fp.remove(&id);
                    let Some(s) = self.saved.iter_mut().find(|s| s.id == id) else { continue };
                    match res {
                        Ok(r) => {
                            if let Some(fp) = fp {
                                s.exported = Some((fp, r.path.clone()));
                            }
                            // Settings changed while it was exporting: leave it ready to export again.
                            s.status = if fp.is_some_and(|f| f != s.color.settings_hash()) {
                                Status::Ready
                            } else {
                                Status::Exported { path: r.path.clone(), seen: false }
                            };
                            let mut name = r.path.file_name().unwrap_or_default().to_string_lossy().to_string();
                            if let Some(g) = r.graded.as_ref().filter(|g| **g != r.path) {
                                name += &format!(" + .{}", g.extension().unwrap_or_default().to_string_lossy());
                            }
                            self.toasts.push(Toast {
                                text: format!("Exported {name} · {:.1} MB", r.bytes as f64 / 1e6),
                                error: false,
                                born: Instant::now(),
                                reveal: Some(r.path),
                            });
                        }
                        Err(e) => {
                            s.status = Status::Failed(e.clone());
                            let frame = s.frame;
                            self.toast(format!("Export failed for frame {frame}: {e}"), true);
                        }
                    }
                }
                ExportEvent::Cancelled(id) => {
                    self.export_fp.remove(&id);
                    self.exporter.pending = self.exporter.pending.saturating_sub(1);
                    self.export_done += 1;
                    if let Some(s) = self.saved.iter_mut().find(|s| s.id == id) {
                        s.status = Status::Ready;
                    }
                }
            }
        }
        if self.exporter.pending == 0 {
            self.export_total = 0;
            self.export_done = 0;
        }
    }

    fn jump_to_saved(&mut self, id: u64) {
        let Some(s) = self.saved.iter().find(|s| s.id == id) else { return };
        let path = s.source.info().path.clone();
        let frame = s.frame;
        if let Some(i) = self.clips.iter().position(|c| c.source.info().path == path) {
            if self.active != Some(i) {
                self.activate(i);
            }
            let p = &mut self.clips[i].player;
            p.pause();
            p.seek(frame as i64);
        }
    }

    // ------------------------------------------------------------------ input

    fn handle_shortcuts(&mut self, ctx: &egui::Context) {
        if ctx.egui_wants_keyboard_input() {
            return;
        }
        use egui::{Key as K, Modifiers as M};
        // Alt shortcuts first: egui's matching ignores extra Alt, so a plain `G` check
        // would otherwise swallow Alt+G (and grab a frame).
        let (mut ag, mut ap, mut ac, mut ah, mut a0, mut esc, mut mute) = (false, false, false, false, false, false, false);
        ctx.input_mut(|i| {
            ag = i.consume_key(M::ALT, K::G);
            ap = i.consume_key(M::ALT, K::P);
            ac = i.consume_key(M::ALT, K::C);
            ah = i.consume_key(M::ALT, K::H);
            a0 = i.consume_key(M::ALT, K::Num0);
            mute = i.consume_key(M::NONE, K::M);
            esc = self.assist_popover.is_some() && i.consume_key(M::NONE, K::Escape);
        });
        let (mut space, mut j, mut k, mut l, mut left, mut right, mut sleft, mut sright, mut home, mut end, mut g, mut f, mut del, mut open, mut exp_all, mut exp_now) =
            (false, false, false, false, false, false, false, false, false, false, false, false, false, false, false, false);
        ctx.input_mut(|i| {
            space = i.consume_key(M::NONE, K::Space);
            j = i.consume_key(M::NONE, K::J);
            k = i.consume_key(M::NONE, K::K);
            l = i.consume_key(M::NONE, K::L);
            sleft = i.consume_key(M::SHIFT, K::ArrowLeft);
            sright = i.consume_key(M::SHIFT, K::ArrowRight);
            left = i.consume_key(M::NONE, K::ArrowLeft);
            right = i.consume_key(M::NONE, K::ArrowRight);
            home = i.consume_key(M::NONE, K::Home);
            end = i.consume_key(M::NONE, K::End);
            g = i.consume_key(M::NONE, K::G);
            f = i.consume_key(M::NONE, K::F);
            del = i.consume_key(M::NONE, K::Delete);
            open = i.consume_key(M::COMMAND, K::O);
            exp_now = i.consume_key(M::COMMAND | M::SHIFT, K::E);
            exp_all = i.consume_key(M::COMMAND, K::E);
        });
        // Viewer assists (instant, no animation: keyboard actions never animate).
        for (hit, k) in [(ag, AssistKind::Guides), (ap, AssistKind::Peaking), (ac, AssistKind::FalseColor), (ah, AssistKind::Histogram)] {
            if hit {
                self.prefs.assists.toggle(k);
            }
        }
        if a0 {
            self.prefs.assists.all_off();
        }
        if mute {
            self.prefs.muted = !self.prefs.muted;
            if !self.prefs.muted && self.prefs.volume <= 0.01 {
                self.prefs.volume = 0.5;
            }
        }
        if esc {
            self.assist_popover = None;
        }
        if open {
            self.open_dialog(ctx);
        }
        if exp_now {
            if let Some(id) = self.grab(ctx) {
                self.export_ids(vec![id]);
            }
        } else if exp_all {
            self.export_all();
        }
        if g {
            self.grab(ctx);
        }
        if del {
            if let Some(id) = self.selected.take() {
                self.saved.retain(|s| s.id != id);
            }
        }
        if f {
            self.zoom = if self.zoom == Zoom::Fit { Zoom::Scale(1.0) } else { Zoom::Fit };
            self.pan = Vec2::ZERO;
        }
        let Some(i) = self.active else { return };
        let p = &mut self.clips[i].player;
        if space {
            p.toggle();
        }
        if k {
            p.pause();
        }
        if l {
            p.shuttle(true);
        }
        if j {
            p.shuttle(false);
        }
        if left {
            p.step(-1);
        }
        if right {
            p.step(1);
        }
        if sleft {
            p.step(-10);
        }
        if sright {
            p.step(10);
        }
        if home {
            p.pause();
            p.seek(0);
        }
        if end {
            p.pause();
            p.seek(i64::MAX / 2);
        }
    }

    // ------------------------------------------------------------------ panels

    fn top_bar(&mut self, ui: &mut Ui) {
        let ctx = ui.ctx().clone();
        // Chromeless menu bar: flat until hovered (Windows convention, OWN-WORLD).
        let w = &mut ui.visuals_mut().widgets;
        w.inactive.weak_bg_fill = egui::Color32::TRANSPARENT;
        w.inactive.bg_fill = egui::Color32::TRANSPARENT;
        ui.horizontal_centered(|ui| {
            ui.add_space(S1);
            ui.menu_button(RichText::new("File").color(SECONDARY), |ui| {
                if ui.button("Open clips…        Ctrl+O").clicked() {
                    ui.close();
                    self.open_dialog(&ctx);
                }
                if ui.button("Open test pattern").clicked() {
                    ui.close();
                    self.open_test_pattern(&ctx);
                }
                ui.separator();
                if ui.button("Load LUT…").clicked() {
                    ui.close();
                    self.load_lut_dialog();
                }
                ui.separator();
                if ui.button("Quit").clicked() {
                    ctx.send_viewport_cmd(egui::ViewportCommand::Close);
                }
            });
            ui.menu_button(RichText::new("View").color(SECONDARY), |ui| {
                if ui.button("Fit to window").clicked() {
                    self.zoom = Zoom::Fit;
                    self.pan = Vec2::ZERO;
                    ui.close();
                }
                if ui.button("Actual pixels (1:1)").clicked() {
                    self.zoom = Zoom::Scale(1.0);
                    self.pan = Vec2::ZERO;
                    ui.close();
                }
                ui.separator();
                for (m, label) in [(OutputMode::Rec709G24, "Rec.709 · Gamma 2.4"), (OutputMode::Srgb, "sRGB"), (OutputMode::AsDecoded, "Log (as decoded)")] {
                    if ui.radio(self.prefs.view.output == m, label).clicked() {
                        self.prefs.view.output = m;
                        ui.close();
                    }
                }
            });
            ui.menu_button(RichText::new("Export").color(SECONDARY), |ui| {
                if ui.button("Export all grabbed frames      Ctrl+E").clicked() {
                    ui.close();
                    self.export_all();
                }
                if ui.button("Grab + export current frame   Ctrl+Shift+E").clicked() {
                    ui.close();
                    if let Some(id) = self.grab(&ctx) {
                        self.export_ids(vec![id]);
                    }
                }
                ui.separator();
                if ui.button("Choose export folder…").clicked() {
                    ui.close();
                    self.choose_export_dir();
                }
                if ui.button("Open export folder").clicked() {
                    ui.close();
                    let _ = std::fs::create_dir_all(&self.prefs.export.output_dir);
                    let _ = std::process::Command::new("explorer").arg(&self.prefs.export.output_dir).spawn();
                }
            });
            ui.menu_button(RichText::new("Help").color(SECONDARY), |ui| {
                if ui.button("Keyboard shortcuts").clicked() {
                    self.show_shortcuts = true;
                    ui.close();
                }
            });
            ui.with_layout(Layout::right_to_left(Align::Center), |ui| {
                ui.add_space(S2);
                let label = match self.prefs.preview {
                    PreviewRes::Auto => "Preview auto".to_string(),
                    PreviewRes::Fixed(s) => format!("Preview {}", s.label()),
                };
                ui.menu_button(RichText::new(format!("{label}  {}", icon::CARET_DOWN)).color(MUTED).font(theme::ui(12.0)), |ui| {
                    if ui.radio(self.prefs.preview == PreviewRes::Auto, "Auto (fits the viewer)").clicked() {
                        self.prefs.preview = PreviewRes::Auto;
                        ui.close();
                    }
                    for s in ResolutionScale::ALL {
                        if ui.radio(self.prefs.preview == PreviewRes::Fixed(s), s.label()).clicked() {
                            self.prefs.preview = PreviewRes::Fixed(s);
                            ui.close();
                        }
                    }
                    ui.separator();
                    ui.checkbox(&mut self.prefs.full_when_paused, "Full resolution when paused");
                });
                self.decode_menu(ui);
            });
        });
    }

    /// Top-bar menu choosing the Blackmagic RAW decode pipeline (CPU / CUDA / OpenCL).
    fn decode_menu(&mut self, ui: &mut Ui) {
        let label = if self.active_pipeline == "CPU" { "CPU decode".to_string() } else { truncate(&self.active_pipeline, 34) };
        let list = self.pipelines.lock().ok().and_then(|g| g.clone());
        let mut pick: Option<String> = None;
        ui.menu_button(RichText::new(format!("{label}  {}", icon::CARET_DOWN)).color(MUTED).font(theme::ui(12.0)), |ui| {
            match &list {
                None => {
                    ui.label(RichText::new("Looking for GPUs…").color(MUTED));
                }
                Some(list) => {
                    for p in list {
                        if ui.radio(self.prefs.decode_pipeline == p.id, &p.name).clicked() {
                            pick = Some(p.id.clone());
                            ui.close();
                        }
                    }
                    if list.len() == 1 {
                        ui.label(RichText::new("No CUDA or OpenCL device found.").color(MUTED).font(theme::ui(11.5)));
                    }
                }
            }
            ui.separator();
            ui.label(RichText::new("GPU decode helps most on a dedicated graphics card.\nSwitching reopens the open clips.").color(MUTED).font(theme::ui(11.5)));
        })
        .response
        .on_hover_text(format!("Blackmagic RAW decoding: {}", self.active_pipeline));
        if let Some(id) = pick.filter(|id| *id != self.prefs.decode_pipeline) {
            self.set_decode_pipeline(ui.ctx(), &id);
        }
    }

    fn set_decode_pipeline(&mut self, ctx: &egui::Context, id: &str) {
        self.prefs.decode_pipeline = id.to_string();
        rf_braw::set_pipeline(id);
        for i in 0..self.clips.len() {
            self.reload_clip(ctx, i);
        }
        self.active_pipeline = rf_braw::active_pipeline();
        if self.clips.is_empty() {
            self.toast("Clips you open will use the new decoder", false);
        } else if id != "cpu" && self.active_pipeline.starts_with("CPU") {
            self.toast("That GPU decoder couldn't start, so decoding stays on the CPU", true);
        } else {
            self.toast(format!("Decoding with {}", self.active_pipeline), false);
        }
        log::info!("decode pipeline: {} (requested {id})", self.active_pipeline);
    }

    fn choose_export_dir(&mut self) {
        let mut d = rfd::FileDialog::new().set_title("Export folder");
        if self.prefs.export.output_dir.exists() {
            d = d.set_directory(&self.prefs.export.output_dir);
        }
        if let Some(p) = d.pick_folder() {
            self.prefs.export.output_dir = p;
        }
    }

    fn side_panel(&mut self, ui: &mut Ui) {
        const TITLES: [&str; 5] = ["Clips", "Camera RAW", "Viewer", "Export", "Metadata"];
        let full = ui.max_rect();
        let slots = self.prefs.dock.layout(full, TITLES.len());
        // Hairlines span the whole column, past the panel's inner margins.
        let line_x = egui::Rangef::new(full.left() - 16.0, full.right() + 12.0);
        for (i, slot) in slots.iter().enumerate() {
            let h = slot.header;
            if i > 0 {
                ui.painter().hline(line_x, h.top(), Stroke::new(1.0, HAIRLINE));
            }
            let hr = ui.interact(h, ui.scope_id().with(("dock_header", i)), Sense::click());
            let open = slot.body.is_some();
            let tint = if hr.hovered() { INK_DIM } else { MUTED };
            let caret = if open { icon::CARET_DOWN } else { icon::CARET_RIGHT };
            ui.painter().text(Pos2::new(h.left() + 5.0, h.center().y), Align2::CENTER_CENTER, caret, theme::ui(10.0), tint);
            let g = ui.painter().layout_no_wrap(TITLES[i].to_uppercase(), theme::strong(11.0), tint);
            ui.painter().galley(Pos2::new(h.left() + 16.0, h.center().y - g.size().y / 2.0), g, tint);
            ui.scope_builder(egui::UiBuilder::new().max_rect(h).layout(Layout::right_to_left(Align::Center)), |ui| self.panel_actions(ui, i));
            if hr.on_hover_text(if open { "Collapse" } else { "Expand" }).clicked() {
                self.prefs.dock.toggle(i);
            }
            if let Some(body) = slot.body {
                ui.scope_builder(egui::UiBuilder::new().max_rect(body).layout(Layout::top_down(Align::Min)), |ui| {
                    ui.set_clip_rect(body.intersect(ui.clip_rect()));
                    egui::ScrollArea::vertical().id_salt(("dock_body", i)).auto_shrink([false, false]).show(ui, |ui| {
                        ui.set_width(ui.available_width());
                        ui.spacing_mut().item_spacing.y = 0.0;
                        match i {
                            0 => self.clips_list(ui),
                            1 => self.raw_section(ui),
                            2 => self.viewer_section(ui),
                            3 => self.export_section(ui),
                            _ => self.metadata_section(ui),
                        }
                        ui.add_space(S4);
                    });
                });
            }
        }
        // Dividers between open neighbours: drag to trade height (on top of everything).
        for (i, slot) in slots.iter().enumerate() {
            let (Some(body), Some(_)) = (slot.body, self.prefs.dock.partner(i)) else { continue };
            let zone = Rect::from_min_max(Pos2::new(full.left(), body.bottom() - crate::dock::GRIP), Pos2::new(full.right(), body.bottom() + crate::dock::GRIP));
            let r = ui.interact(zone, ui.scope_id().with(("dock_split", i)), Sense::drag());
            if r.hovered() || r.dragged() {
                ui.ctx().set_cursor_icon(egui::CursorIcon::ResizeVertical);
                ui.painter().hline(line_x, body.bottom(), Stroke::new(2.0, CONTROL_HI));
            }
            if r.dragged() {
                self.prefs.dock.drag(i, r.drag_delta().y);
            }
        }
    }

    /// Buttons on the right of a dock panel's header.
    fn panel_actions(&mut self, ui: &mut Ui, panel: usize) {
        match panel {
            0 => {
                if widgets::icon_button(ui, icon::PLUS, "Open clips (Ctrl+O)", 24.0).clicked() {
                    let ctx = ui.ctx().clone();
                    self.open_dialog(&ctx);
                }
                if !self.clips.is_empty() {
                    ui.label(RichText::new(self.clips.len().to_string()).color(MUTED).font(theme::mono(11.0)));
                }
            }
            1 => {
                let Some(i) = self.active else { return };
                let clip = &mut self.clips[i];
                if clip.custom && widgets::icon_button(ui, icon::RESET, "Reset to the clip's values", 24.0).clicked() {
                    clip.color = DecodeColor::default();
                    clip.custom = false;
                }
            }
            _ => {}
        }
    }

    fn clips_list(&mut self, ui: &mut Ui) {
        ui.add_space(S1);
        if self.clips.is_empty() {
            ui.label(RichText::new("No clips open. Drop .braw files on the window or press Ctrl+O.").color(MUTED));
            return;
        }
        let mut activate = None;
        let mut close = None;
        let reveal = self.scroll_to_clip.take();
        for (i, c) in self.clips.iter().enumerate() {
            let info = c.source.info();
            let (rect, resp) = ui.allocate_exact_size(Vec2::new(ui.available_width(), 40.0), Sense::click());
            if reveal == Some(i) {
                ui.scroll_to_rect(rect, None);
            }
            let on = self.active == Some(i);
            if on || ui.rect_contains_pointer(rect) {
                ui.painter().rect_filled(rect, CornerRadius::same(RADIUS), if on { HOVER } else { theme::WELL });
            }
            let p = ui.painter();
            let thumb = Rect::from_min_size(rect.left_top() + Vec2::new(6.0, 6.0), Vec2::new(52.0, 28.0));
            p.rect_filled(thumb, CornerRadius::same(3), VIEWER);
            if let Some(t) = &c.poster {
                p.image(t.id(), thumb, Rect::from_min_max(Pos2::ZERO, Pos2::new(1.0, 1.0)), egui::Color32::WHITE);
            }
            let tx = 68.0;
            let name = widgets::elide(ui, &c.name, &theme::ui(13.0), rect.width() - tx - 30.0);
            p.text(rect.left_top() + Vec2::new(tx, 6.0), Align2::LEFT_TOP, name, theme::ui(13.0), if on { INK } else { INK_DIM });
            let sub = format!("{} · {}f · {}", info.start_timecode.clone().unwrap_or_default(), info.frame_count, short_res(info.width, info.height));
            p.text(rect.left_top() + Vec2::new(tx, 23.0), Align2::LEFT_TOP, sub, theme::mono(10.5), MUTED);
            // The close button must exist whenever the pointer is over the row: once the
            // pointer is on the button, the row itself no longer reports hover.
            let x = Rect::from_center_size(Pos2::new(rect.right() - 16.0, rect.center().y), Vec2::splat(24.0));
            let over_row = ui.rect_contains_pointer(rect);
            let xr = ui.interact(x, ui.scope_id().with(("close", i)), Sense::click());
            if over_row || xr.hovered() {
                if xr.hovered() {
                    ui.painter().rect_filled(x, CornerRadius::same(4), CONTROL);
                }
                ui.painter().text(x.center(), Align2::CENTER_CENTER, icon::X, theme::ui(12.0), if xr.hovered() { INK } else { MUTED });
            }
            if xr.on_hover_text("Close clip").clicked() {
                close = Some(i);
            } else if resp.clicked() {
                activate = Some(i);
            }
        }
        if let Some(i) = close {
            self.close_clip(i);
        } else if let Some(i) = activate {
            self.activate(i);
        }
    }

    fn raw_section(&mut self, ui: &mut Ui) {
        ui.add_space(S1);
        let Some(i) = self.active else {
            ui.label(RichText::new("Open a clip to see its decode settings").color(MUTED));
            return;
        };
        let n_clips = self.clips.len();
        let mut action: Option<RawAction> = None;
        let clip = &mut self.clips[i];
        let caps = clip.source.capabilities().clone();
        let a = clip.source.info().as_shot.clone();
        let sidecar_ok = clip.source.supports_sidecar();
        let attached = clip.source.sidecar_attached();
        let mut custom = clip.custom;
        row(ui, "Decode using", |ui| {
            widgets::segmented(ui, &mut custom, &[(false, "Clip"), (true, "Custom")]);
        });
        if sidecar_ok {
            row(ui, "Sidecar", |ui| {
                let (t, tip) = if attached {
                    ("Attached", "This clip's .sidecar file sets its Clip values (shared with DaVinci Resolve and Blackmagic RAW Player).")
                } else {
                    ("None", "Clip values are the camera's. Save to sidecar to store corrections next to the clip.")
                };
                value(ui, t).on_hover_text(tip);
            });
        }
        if custom != clip.custom {
            clip.custom = custom;
            if custom && clip.color == DecodeColor::default() {
                // Start from the clip's own values so nothing jumps.
                let c = &mut clip.color;
                c.color_science_gen = a.color_science_gen;
                c.gamma = a.gamma.clone();
                c.gamut = a.gamut.clone();
                c.iso = a.iso;
                c.kelvin = a.kelvin;
                c.tint = a.tint;
                c.set_exposure(a.exposure.or(Some(0.0)));
                c.highlight_recovery = a.highlight_recovery;
                c.gamut_compression = a.gamut_compression;
            }
        }
        let opt = |v: Option<String>| v.unwrap_or_else(|| "–".into());
        if !clip.custom {
            row(ui, "Color science", |ui| {
                value(ui, a.color_science_gen.map(|g| format!("Gen {g}")).unwrap_or("–".into()));
            });
            row(ui, "Gamut", |ui| {
                value(ui, opt(a.gamut.clone()));
            });
            row(ui, "Gamma", |ui| {
                value(ui, opt(a.gamma.clone()));
            });
            row(ui, "ISO", |ui| {
                value(ui, a.iso.map(|v| v.to_string()).unwrap_or("–".into()));
            });
            row(ui, "White balance", |ui| {
                value(ui, a.kelvin.map(|k| format!("{k}K")).unwrap_or("–".into()));
            });
            row(ui, "Tint", |ui| {
                value(ui, a.tint.map(|t| format!("{t:+}")).unwrap_or("–".into()));
            });
            row(ui, "Exposure", |ui| {
                value(ui, format!("{:+.2}", a.exposure.unwrap_or(0.0)));
            });
            row(ui, "Highlight recovery", |ui| {
                value(ui, if a.highlight_recovery == Some(true) { "On" } else { "Off" });
            });
            if attached {
                ui.add_space(S1);
                ui.horizontal(|ui| {
                    if widgets::quiet_button(ui, &format!("{}  Revert to camera original", icon::RESET))
                        .on_hover_text("Remove these corrections from the clip's .sidecar")
                        .clicked()
                    {
                        action = Some(RawAction::RevertSidecar);
                    }
                });
            }
            if let Some(act) = action {
                self.raw_action(ui.ctx(), i, act);
            }
            return;
        }
        let c = &mut clip.color;
        let combo = |ui: &mut Ui, id: &str, cur: &mut Option<String>, items: &[String]| {
            let text = truncate(&cur.clone().unwrap_or_else(|| "–".into()), 19);
            egui::ComboBox::from_id_salt(id).selected_text(RichText::new(text).font(theme::mono(12.0))).width(170.0).icon(widgets::combo_caret).show_ui(ui, |ui| {
                for it in items {
                    if ui.selectable_label(cur.as_deref() == Some(it), it).clicked() {
                        *cur = Some(it.clone());
                    }
                }
            });
        };
        row(ui, "Color science", |ui| {
            let mut g = c.color_science_gen.unwrap_or(5);
            egui::ComboBox::from_id_salt("gen").selected_text(RichText::new(format!("Gen {g}")).font(theme::mono(12.0))).width(170.0).icon(widgets::combo_caret).show_ui(ui, |ui| {
                for v in &caps.color_science_gens {
                    ui.selectable_value(&mut g, *v, format!("Gen {v}"));
                }
            });
            c.color_science_gen = Some(g);
        });
        row(ui, "Gamut", |ui| combo(ui, "gamut", &mut c.gamut, &caps.gamuts));
        row(ui, "Gamma", |ui| combo(ui, "gamma", &mut c.gamma, &caps.gammas));
        row(ui, "ISO", |ui| {
            let mut iso = c.iso.unwrap_or(800);
            egui::ComboBox::from_id_salt("iso").selected_text(RichText::new(iso.to_string()).font(theme::mono(12.0))).width(170.0).icon(widgets::combo_caret).show_ui(ui, |ui| {
                for v in &caps.isos {
                    ui.selectable_value(&mut iso, *v, v.to_string());
                }
            });
            c.iso = Some(iso);
        });
        let (kmin, kmax) = caps.kelvin_range.unwrap_or((2000, 50000));
        row_ltr(ui, "White balance", 176.0, |ui| {
            let mut k = c.kelvin.unwrap_or(5600) as f32;
            let fmt = |v: f32| format!("{v:.0}K");
            Slider { value: &mut k, range: kmin.max(2000) as f32..=kmax.min(15000) as f32, default: a.kelvin.unwrap_or(5600) as f32, step: 50.0, log: true, text: &fmt }.show(ui, 176.0);
            c.kelvin = Some(k as u32);
        });
        let (tmin, tmax) = caps.tint_range.unwrap_or((-50, 50));
        row_ltr(ui, "Tint", 176.0, |ui| {
            let mut t = c.tint.unwrap_or(0) as f32;
            let fmt = |v: f32| format!("{:+}", v as i32);
            Slider { value: &mut t, range: tmin as f32..=tmax as f32, default: a.tint.unwrap_or(0) as f32, step: 1.0, log: false, text: &fmt }.show(ui, 176.0);
            c.tint = Some(t as i16);
        });
        let (emin, emax) = caps.exposure_range.unwrap_or((-5.0, 5.0));
        row_ltr(ui, "Exposure", 176.0, |ui| {
            let mut e = c.exposure().unwrap_or(0.0);
            let fmt = |v: f32| format!("{v:+.2}");
            Slider { value: &mut e, range: emin..=emax, default: a.exposure.unwrap_or(0.0), step: 0.05, log: false, text: &fmt }.show(ui, 176.0);
            c.set_exposure(Some(e));
        });
        row(ui, "Highlight recovery", |ui| {
            let mut h = c.highlight_recovery.unwrap_or(false);
            ui.checkbox(&mut h, "");
            c.highlight_recovery = Some(h);
        });
        row(ui, "Gamut compression", |ui| {
            let mut h = c.gamut_compression.unwrap_or(false);
            ui.checkbox(&mut h, "");
            c.gamut_compression = Some(h);
        });
        if !caps.post3dlut_modes.is_empty() {
            row(ui, "Clip LUT", |ui| {
                let mut m = c.post3dlut_mode.clone().or(a.post3dlut_mode.clone());
                combo(ui, "lutmode", &mut m, &caps.post3dlut_modes);
                c.post3dlut_mode = m;
            });
        }
        ui.add_space(S1);
        ui.horizontal_wrapped(|ui| {
            ui.spacing_mut().item_spacing = Vec2::new(S1, S1);
            if sidecar_ok
                && widgets::quiet_button(ui, &format!("{}  Save to sidecar", icon::FLOPPY))
                    .on_hover_text("Store these settings in the clip's .sidecar, like Resolve's Update Sidecar.\nResolve and Blackmagic RAW Player will open the clip with them.")
                    .clicked()
            {
                action = Some(RawAction::SaveSidecar);
            }
            if n_clips > 1
                && widgets::quiet_button(ui, &format!("{}  Copy to all clips", icon::COPY))
                    .on_hover_text("Use these exact settings on every open clip")
                    .clicked()
            {
                action = Some(RawAction::CopyToAll);
            }
        });
        if let Some(act) = action {
            self.raw_action(ui.ctx(), i, act);
        }
    }

    fn raw_action(&mut self, ctx: &egui::Context, i: usize, act: RawAction) {
        let name = self.clips[i].name.clone();
        match act {
            RawAction::SaveSidecar => {
                let color = self.clips[i].decode_color();
                match self.clips[i].source.write_sidecar(&color) {
                    Ok(()) => {
                        if self.reload_clip(ctx, i) {
                            // The sidecar now is the clip's own look: decode using Clip.
                            self.clips[i].custom = false;
                            self.clips[i].color = DecodeColor::default();
                            self.relink_saved(i);
                        }
                        self.toast(format!("Saved to {name}.sidecar. Resolve and Blackmagic RAW Player will use these settings."), false);
                    }
                    Err(e) => self.toast(format!("Couldn't save the sidecar: {e}"), true),
                }
            }
            RawAction::RevertSidecar => match self.clips[i].source.clear_sidecar() {
                Ok(()) => {
                    self.reload_clip(ctx, i);
                    self.clips[i].custom = false;
                    self.clips[i].color = DecodeColor::default();
                    self.toast(format!("{name} is back to the camera original"), false);
                }
                Err(e) => self.toast(format!("Couldn't revert the sidecar: {e}"), true),
            },
            RawAction::CopyToAll => {
                let src = &self.clips[i];
                let color = src.decode_color().resolved(&src.source.info().as_shot);
                let mut n = 0;
                for (j, c) in self.clips.iter_mut().enumerate() {
                    if j != i {
                        c.custom = true;
                        c.color = color.clone();
                        n += 1;
                    }
                }
                self.toast(format!("Copied these settings to {n} clip{}", if n == 1 { "" } else { "s" }), false);
            }
        }
    }

    /// Reopen clip `i` from disk (after its sidecar changed) so it reports the new
    /// as-shot values. Frames with their own settings keep exactly what they had.
    fn reload_clip(&mut self, ctx: &egui::Context, i: usize) -> bool {
        let path = self.clips[i].source.info().path.clone();
        let old_as_shot = self.clips[i].source.info().as_shot.clone();
        match self.registry.open(&path) {
            Ok(src) => {
                self.audio.stop();
                for s in self.saved.iter_mut().filter(|s| s.source.info().path == path) {
                    if let Some(own) = &mut s.own {
                        *own = own.resolved(&old_as_shot);
                        s.color = own.clone();
                    }
                    s.source = src.clone();
                }
                let c = &mut self.clips[i];
                let pos = c.player.position;
                let workers = std::thread::available_parallelism().map(|n| (n.get() / 4).clamp(2, 4)).unwrap_or(3);
                c.player = Player::new(src.clone(), ctx.clone(), workers);
                c.player.position = pos;
                c.source = src;
                true
            }
            Err(e) => {
                self.toast(format!("Couldn't reopen {}: {e}", self.clips[i].name), true);
                false
            }
        }
    }

    /// After a sidecar save the clip's linked frames decode with "Clip" values that equal
    /// what they had, so keep their exported state instead of flagging them as changed.
    fn relink_saved(&mut self, i: usize) {
        let path = self.clips[i].source.info().path.clone();
        let now = self.clips[i].decode_color();
        for s in self.saved.iter_mut().filter(|s| s.own.is_none() && s.source.info().path == path) {
            let before = s.color.settings_hash();
            s.color = now.clone();
            if let Some((fp, _)) = &mut s.exported {
                if *fp == before {
                    *fp = now.settings_hash();
                }
            }
        }
    }

    fn viewer_section(&mut self, ui: &mut Ui) {
        let v = &mut self.prefs.view;
        ui.add_space(S1);
        row(ui, "Output", |ui| {
            widgets::segmented(ui, &mut v.output, &[(OutputMode::Rec709G24, "709 2.4"), (OutputMode::Srgb, "sRGB"), (OutputMode::AsDecoded, "Log")]);
        });
        row(ui, "Highlight rolloff", |ui| {
            let mut on = v.tonemap == ToneMap::Rolloff;
            ui.checkbox(&mut on, "").on_hover_text("A simple hue-preserving rolloff above 80%. An approximation, not DaVinci tone mapping.");
            v.tonemap = if on { ToneMap::Rolloff } else { ToneMap::None };
        });
        row_ltr(ui, "Viewer exposure", 176.0, |ui| {
            let fmt = |x: f32| format!("{x:+.1} EV");
            Slider { value: &mut v.exposure_ev, range: -4.0..=4.0, default: 0.0, step: 0.1, log: false, text: &fmt }.show(ui, 176.0);
        });
        let mut load = false;
        let mut clear = false;
        row(ui, "LUT", |ui| {
            if self.lut.is_some() {
                if widgets::icon_button(ui, icon::X, "Remove LUT", 24.0).clicked() {
                    clear = true;
                }
            }
            if widgets::icon_button(ui, icon::FOLDER_OPEN, "Load .cube LUT", 24.0).clicked() {
                load = true;
            }
            match &self.lut {
                Some(l) => {
                    let name = if l.title.is_empty() { l.name.clone() } else { l.title.clone() };
                    ui.label(RichText::new(truncate(&name, 22)).font(theme::mono(12.0)).color(INK)).on_hover_text(format!("{} · {}", l.name, l.description()));
                }
                None => {
                    ui.label(RichText::new("None").color(MUTED));
                }
            }
        });
        if self.lut.is_some() {
            row(ui, "Apply", |ui| {
                widgets::segmented(ui, &mut v.lut_placement, &[(LutPlacement::OnLog, "On log"), (LutPlacement::AfterCst, "After CST")]);
            });
            row_ltr(ui, "Strength", 176.0, |ui| {
                let fmt = |x: f32| format!("{:.0}%", x * 100.0);
                Slider { value: &mut v.lut_mix, range: 0.0..=1.0, default: 1.0, step: 0.01, log: false, text: &fmt }.show(ui, 176.0);
            });
            row(ui, "Show LUT", |ui| {
                ui.checkbox(&mut v.lut_enabled, "");
            });
        }
        if clear {
            self.lut = None;
            self.prefs.lut_path = None;
        }
        if load {
            self.load_lut_dialog();
        }
    }

    fn export_section(&mut self, ui: &mut Ui) {
        // What the graded copy will look like: the viewer's look, LUT included when shown.
        let v = self.prefs.view;
        let lut_name = self.lut.as_ref().map(|l| if l.title.is_empty() { l.name.clone() } else { l.title.clone() });
        let look = match (&lut_name, v.lut_enabled) {
            (Some(n), true) if v.lut_placement == LutPlacement::OnLog => format!("LUT {} on log", truncate(n, 28)),
            (Some(n), true) => format!("{} + LUT {}", v.output.label(), truncate(n, 28)),
            (Some(_), false) => format!("{}. The LUT is hidden in the viewer, so it isn't applied", v.output.label()),
            (None, _) => format!("{}, no LUT loaded", v.output.label()),
        };
        let e = &mut self.prefs.export;
        ui.add_space(S1);
        // DNG-only rows, dimmed when only the graded copy is written.
        ui.add_enabled_ui(e.writes_raw(), |ui| {
            row(ui, "Format", |ui| {
                widgets::segmented(ui, &mut e.kind, &[(ExportKind::Dng, "DNG"), (ExportKind::CinemaDng, "CinemaDNG")]);
            });
            row(ui, "Samples", |ui| {
                egui::ComboBox::from_id_salt("samples").selected_text(e.sample.label()).width(170.0).icon(widgets::combo_caret).show_ui(ui, |ui| {
                    for s in Sample::ALL {
                        ui.selectable_value(&mut e.sample, s, s.label());
                    }
                });
            });
            widgets::row_tip(
                ui,
                "White balance",
                "Baked: white balance is in the pixels, exactly as the viewer shows it. Lightroom's As Shot reads as a generic daylight value.\n\
                 Metadata: pixels stay at a 6500 K reference and your white balance is written as the DNG's As Shot, so \
                 Lightroom starts at (close to) your Kelvin and its Temp/Tint sliders move from there. Colors match the viewer within about 1 %.",
                |ui| {
                    use rf_export::WhiteBalanceMode as Wb;
                    widgets::segmented(ui, &mut e.white_balance, &[(Wb::Baked, Wb::Baked.label()), (Wb::Metadata, Wb::Metadata.label())]);
                },
            );
            if e.kind == ExportKind::Dng {
                row(ui, "Embedded preview", |ui| {
                    egui::ComboBox::from_id_salt("preview_size").selected_text(e.preview.label()).width(170.0).icon(widgets::combo_caret).show_ui(ui, |ui| {
                        for p in rf_export::PreviewSize::ALL {
                            ui.selectable_value(&mut e.preview, p, p.label());
                        }
                    })
                    .response
                    .on_hover_text("JPEG shown by Windows Photos, Explorer and other quick viewers. Lightroom and Photoshop use the raw data.");
                });
            }
        });
        widgets::row_tip(
            ui,
            "Graded copy",
            "Also save the frame exactly as the viewer shows it, with the output transform, LUT and viewer exposure applied.\n\
             It's display-referred and tagged with an ICC profile (Rec.709 Gamma 2.4 or sRGB), for sharing and review. \
             Use the DNG for raw editing.",
            |ui| {
                egui::ComboBox::from_id_salt("graded").selected_text(e.graded.label()).width(170.0).icon(widgets::combo_caret).show_ui(ui, |ui| {
                    for g in rf_export::GradedFormat::ALL {
                        ui.selectable_value(&mut e.graded, g, g.label());
                    }
                });
            },
        );
        if e.graded != rf_export::GradedFormat::None {
            widgets::row_tip(ui, "Raw file", "Untick to export only the graded copy.", |ui| {
                ui.checkbox(&mut e.raw, "");
            });
        }
        row(ui, "Lossless compression", |ui| {
            ui.checkbox(&mut e.compress, "").on_hover_text("Deflate for the DNG and the TIFF.");
        });
        let mut choose = false;
        row(ui, "Folder", |ui| {
            if widgets::icon_button(ui, icon::FOLDER_OPEN, "Choose export folder", 24.0).clicked() {
                choose = true;
            }
            let shown = e.output_dir.file_name().map(|s| s.to_string_lossy().to_string()).unwrap_or_else(|| e.output_dir.display().to_string());
            ui.label(RichText::new(truncate(&shown, 24)).font(theme::mono(12.0)).color(INK)).on_hover_text(e.output_dir.display().to_string());
        });
        row(ui, "File name", |ui| {
            ui.add(egui::TextEdit::singleline(&mut e.filename_template).desired_width(170.0).font(theme::mono(12.0)))
                .on_hover_text("Tokens: {clip} {frame} {tc} {date}");
        });
        let mut note = String::new();
        if e.writes_raw() {
            note += match e.white_balance {
                rf_export::WhiteBalanceMode::Baked => "Linear DNG: full resolution, scene-linear camera data, white balance as in the viewer.",
                rf_export::WhiteBalanceMode::Metadata => "Linear DNG: full resolution, scene-linear camera data. Lightroom's As Shot starts at your white balance.",
            };
        }
        if e.graded != rf_export::GradedFormat::None {
            if !note.is_empty() {
                note += "\n";
            }
            note += &format!("Graded {}: {look}, as in the viewer.", e.graded.label());
        }
        ui.label(RichText::new(note).color(MUTED).font(theme::ui(11.5)));
        if choose {
            self.choose_export_dir();
        }
    }

    fn metadata_section(&mut self, ui: &mut Ui) {
        let Some(i) = self.active else { return };
        let src = self.clips[i].source.clone();
        let info = src.info();
        ui.add_space(S1);
        {
            {
                // Fixed-width rows: long keys/values are elided (full text on hover) so the
                // column never grows wider than its 320px.
                let mut kv = |k: &str, v: String| {
                    let w = ui.available_width();
                    let (rect, resp) = ui.allocate_exact_size(Vec2::new(w, 20.0), Sense::hover());
                    let key_w = (w * 0.48).floor();
                    let kf = theme::ui(11.5);
                    let vf = theme::mono(11.0);
                    let ks = widgets::elide(ui, k, &kf, key_w - S2);
                    let vs = widgets::elide(ui, &v, &vf, w - key_w);
                    let p = ui.painter();
                    p.text(rect.left_center(), Align2::LEFT_CENTER, ks, kf, SECONDARY);
                    p.text(rect.right_center(), Align2::RIGHT_CENTER, vs, vf, INK_DIM);
                    resp.on_hover_text(format!("{k}: {v}"));
                };
                kv("Camera", info.camera_model.clone());
                kv("Resolution", format!("{}×{}", info.width, info.height));
                kv("Frame rate", format!("{:.3}", info.fps));
                kv("Frames", info.frame_count.to_string());
                for (k, v) in src.metadata().iter().filter(|(k, v)| !v.is_empty() && !k.contains("lut") && !k.contains("polynomial")) {
                    kv(&k.replace('_', " "), v.clone());
                }
            }
        }
    }

    fn transport(&mut self, ui: &mut Ui) {
        let full = ui.max_rect();
        let ctx = ui.ctx().clone();
        let Some(i) = self.active else {
            ui.painter().text(full.left_center() + Vec2::new(S4 + 2.0, 0.0), Align2::LEFT_CENTER, "--:--:--:--", theme::mono(20.0), CONTROL_HI);
            return;
        };
        let saved_marks: Vec<u64> = {
            let path = &self.clips[i].source.info().path;
            self.saved.iter().filter(|s| &s.source.info().path == path).map(|s| s.frame).collect()
        };
        let clip = &mut self.clips[i];
        let p = &mut clip.player;
        let n = p.frame_count();

        // Scrubber along the top edge (2px line, 14px hit band).
        let band = Rect::from_min_max(full.left_top() - Vec2::new(0.0, 6.0), Pos2::new(full.right(), full.top() + 8.0));
        let sr = ui.interact(band, ui.scope_id().with("scrub"), Sense::click_and_drag());
        let to_frame = |x: f32| (((x - full.left()) / full.width()).clamp(0.0, 1.0) * (n.saturating_sub(1)) as f32).round() as i64;
        if sr.dragged() || sr.clicked() {
            if let Some(pos) = sr.interact_pointer_pos() {
                p.pause();
                p.seek(to_frame(pos.x));
            }
        }
        let frac = if n > 1 { p.current_frame() as f32 / (n - 1) as f32 } else { 0.0 };
        let y = full.top();
        let painter = ui.painter();
        let hot = sr.hovered() || sr.dragged();
        let thick = if hot { 4.0 } else { 2.0 };
        painter.rect_filled(Rect::from_min_max(Pos2::new(full.left(), y), Pos2::new(full.right(), y + thick)), 0.0, CONTROL);
        let px = full.left() + frac * full.width();
        painter.rect_filled(Rect::from_min_max(Pos2::new(full.left(), y), Pos2::new(px, y + thick)), 0.0, SECONDARY);
        for m in saved_marks {
            let x = full.left() + (m as f32 / (n.max(2) - 1) as f32) * full.width();
            // Grab markers sit below the line (inside the transport clip), shorter than the playhead.
            painter.rect_filled(Rect::from_min_max(Pos2::new(x - 1.0, y + 4.0), Pos2::new(x + 1.0, y + 9.0)), 0.0, SECONDARY);
        }
        painter.rect_filled(Rect::from_min_max(Pos2::new(px - 1.0, y - 2.0), Pos2::new(px + 1.0, y + 12.0)), 1.0, INK);
        painter.rect_filled(Rect::from_min_max(Pos2::new(px - 4.5, y - 2.0), Pos2::new(px + 4.5, y + 3.0)), 1.5, INK);
        if hot {
            if let Some(pos) = sr.hover_pos() {
                let f = to_frame(pos.x) as u64;
                let tc = clip.source.timecode(f).unwrap_or_default();
                egui::Tooltip::always_open(ctx.clone(), ui.layer_id(), ui.scope_id().with("scrubtip"), egui::PopupAnchor::Pointer).show(|ui| {
                    ui.label(RichText::new(format!("{tc}  ·  F {f:04}")).font(theme::mono(11.5)));
                });
            }
        }

        // Left: timecode + frame
        let tc = clip.source.timecode(p.current_frame()).unwrap_or_else(|| "--:--:--:--".into());
        let left = full.left_center() + Vec2::new(S4 + 2.0, -7.0);
        let g = painter.text(left, Align2::LEFT_CENTER, &tc, theme::mono(20.0), INK);
        painter.text(Pos2::new(g.left(), left.y + 18.0), Align2::LEFT_CENTER, format!("F {:04} / {}", p.current_frame(), n - 1), theme::mono(11.0), MUTED);
        let strip_x = g.right() + S5;

        // Centre: transport buttons
        let size = 32.0;
        let play_d = 38.0;
        let total = size * 6.0 + play_d + S3 * 6.0;
        let start = Pos2::new(full.center().x - total / 2.0, full.center().y + 1.0);
        let mut x = start.x;
        let mut ui_at = |ui: &mut Ui, w: f32, f: &mut dyn FnMut(&mut Ui)| {
            let r = Rect::from_center_size(Pos2::new(x + w / 2.0, start.y), Vec2::splat(w));
            ui.scope_builder(egui::UiBuilder::new().max_rect(r), |ui| f(ui));
            x += w + S3;
        };
        let mut action: Option<u8> = None;
        ui_at(ui, size, &mut |ui| {
            if widgets::icon_button(ui, icon::SKIP_BACK, "First frame (Home)", size).clicked() {
                action = Some(0);
            }
        });
        ui_at(ui, size, &mut |ui| {
            if widgets::icon_button(ui, icon::REWIND, "Play reverse (J)", size).clicked() {
                action = Some(1);
            }
        });
        ui_at(ui, size, &mut |ui| {
            if widgets::icon_button(ui, icon::CARET_LEFT, "Previous frame (←)", size).clicked() {
                action = Some(2);
            }
        });
        let playing = p.is_playing();
        ui_at(ui, play_d, &mut |ui| {
            let (r, resp) = ui.allocate_exact_size(Vec2::splat(play_d), Sense::click());
            let held = resp.is_pointer_button_down_on();
            let rad = if held { play_d / 2.0 - 1.5 } else { play_d / 2.0 };
            ui.painter().circle_filled(r.center(), rad, if resp.hovered() { egui::Color32::WHITE } else { INK });
            // Drawn as geometry (not a font glyph) so it is exactly centred: the triangle's
            // centroid sits on the circle's centre.
            let c = r.center();
            if playing {
                for dx in [-3.5f32, 3.5] {
                    let bar = Rect::from_center_size(c + Vec2::new(dx, 0.0), Vec2::new(3.5, 13.0));
                    ui.painter().rect_filled(bar, CornerRadius::same(1), ROOM);
                }
            } else {
                let (w, h) = (12.0f32, 14.0f32);
                let pts = vec![
                    Pos2::new(c.x - w / 3.0, c.y - h / 2.0),
                    Pos2::new(c.x + 2.0 * w / 3.0, c.y),
                    Pos2::new(c.x - w / 3.0, c.y + h / 2.0),
                ];
                ui.painter().add(egui::Shape::convex_polygon(pts, ROOM, Stroke::NONE));
            }
            if resp.on_hover_text(if playing { "Pause (Space)" } else { "Play (Space)" }).clicked() {
                action = Some(3);
            }
        });
        ui_at(ui, size, &mut |ui| {
            if widgets::icon_button(ui, icon::CARET_RIGHT, "Next frame (→)", size).clicked() {
                action = Some(4);
            }
        });
        ui_at(ui, size, &mut |ui| {
            if widgets::icon_button(ui, icon::FAST_FORWARD, "Play forward faster (L)", size).clicked() {
                action = Some(5);
            }
        });
        ui_at(ui, size, &mut |ui| {
            if widgets::icon_button(ui, icon::SKIP_FORWARD, "Last frame (End)", size).clicked() {
                action = Some(6);
            }
        });
        match action {
            Some(0) => {
                p.pause();
                p.seek(0)
            }
            Some(1) => p.shuttle(false),
            Some(2) => p.step(-1),
            Some(3) => p.toggle(),
            Some(4) => p.step(1),
            Some(5) => p.shuttle(true),
            Some(6) => {
                p.pause();
                p.seek(n as i64)
            }
            _ => {}
        }
        if p.is_playing() && p.rate != 1.0 {
            let label = if p.rate < 0.0 { format!("◂ {}×", -p.rate) } else { format!("{}×", p.rate) };
            ui.painter().text(Pos2::new(x + 4.0, start.y), Align2::LEFT_CENTER, label, theme::mono(12.0), SECONDARY);
        }

        // Right: grab
        let grab_rect = Rect::from_min_size(Pos2::new(full.right() - 150.0, full.center().y - 15.0), Vec2::new(134.0, 30.0));
        let gr = ui.interact(grab_rect, ui.scope_id().with("grab"), Sense::click());
        let held = gr.is_pointer_button_down_on();
        let r = if held { grab_rect.shrink(1.0) } else { grab_rect };
        ui.painter().rect_filled(r, CornerRadius::same(RADIUS), if gr.hovered() { CONTROL } else { HOVER });
        ui.painter().text(r.left_center() + Vec2::new(12.0, 0.0), Align2::LEFT_CENTER, "Grab frame", theme::strong(13.0), INK);
        widgets::keycap(ui, ui.painter(), Pos2::new(r.right() - 26.0, r.center().y - 9.0), "G");
        if gr.on_hover_text("Add this frame to the lightbox (G)").clicked() {
            self.grab(&ctx);
        }
        self.assist_strip(ui, strip_x, full);
        self.volume_control(ui, Rect::from_min_max(full.min, Pos2::new(grab_rect.left(), full.bottom())));
    }

    /// Spotify-style volume: speaker button (mute toggle) + slim slider, right-aligned in `area`.
    fn volume_control(&mut self, ui: &mut Ui, area: Rect) {
        let has_audio = self.active.and_then(|i| self.clips.get(i)).is_some_and(|c| c.has_audio);
        let (slider_w, btn) = (96.0, 30.0);
        let right = area.right() - S5;
        let cy = area.center().y + 1.0;
        let br = Rect::from_center_size(Pos2::new(right - slider_w - 8.0 - btn / 2.0, cy), Vec2::splat(btn));
        let track = Rect::from_min_max(Pos2::new(right - slider_w, cy - 8.0), Pos2::new(right, cy + 8.0));
        let v = if self.prefs.muted { 0.0 } else { self.prefs.volume };

        // Speaker button: icon follows the level; click toggles mute.
        let resp = ui.interact(br, ui.scope_id().with("mute"), Sense::click());
        let held = resp.is_pointer_button_down_on();
        if resp.hovered() || held {
            ui.painter().rect_filled(if held { br.shrink(1.0) } else { br }, CornerRadius::same(RADIUS), if held { CONTROL } else { HOVER });
        }
        let ic = if v <= 0.0 {
            "\u{E45C}" // speaker-x
        } else if v < 0.5 {
            "\u{E44C}" // speaker-low
        } else {
            "\u{E44A}" // speaker-high
        };
        let col = if !has_audio { CONTROL_HI } else if resp.hovered() { INK } else { INK_DIM };
        ui.painter().text(br.center(), Align2::CENTER_CENTER, ic, theme::ui(16.0), col);
        let tip = if !has_audio {
            "This clip has no audio"
        } else if self.prefs.muted {
            "Unmute (M)"
        } else {
            "Mute (M)"
        };
        if resp.on_hover_text(tip).clicked() {
            self.prefs.muted = !self.prefs.muted;
            if !self.prefs.muted && self.prefs.volume <= 0.01 {
                self.prefs.volume = 0.5;
            }
        }

        // Slider: thin rail, brightens and shows its knob on hover (Spotify).
        let sr = ui.interact(track, ui.scope_id().with("volume"), Sense::click_and_drag());
        if let Some(p) = sr.interact_pointer_pos().filter(|_| sr.dragged() || sr.clicked()) {
            self.prefs.volume = ((p.x - track.left()) / track.width()).clamp(0.0, 1.0);
            self.prefs.muted = self.prefs.volume <= 0.0;
        }
        if sr.hovered() {
            let scroll = ui.input(|i| i.smooth_scroll_delta.y);
            if scroll != 0.0 {
                self.prefs.volume = (self.prefs.volume + scroll * 0.0015).clamp(0.0, 1.0);
                self.prefs.muted = self.prefs.volume <= 0.0;
            }
        }
        let v = if self.prefs.muted { 0.0 } else { self.prefs.volume };
        let hot = sr.hovered() || sr.dragged();
        let y = track.center().y;
        let x = track.left() + v * track.width();
        let p = ui.painter();
        p.rect_filled(Rect::from_min_max(Pos2::new(track.left(), y - 2.0), Pos2::new(track.right(), y + 2.0)), 2.0, CONTROL);
        p.rect_filled(Rect::from_min_max(Pos2::new(track.left(), y - 2.0), Pos2::new(x, y + 2.0)), 2.0, if hot { INK } else { SECONDARY });
        if hot {
            p.circle_filled(Pos2::new(x, y), 6.0, INK);
        }
        sr.on_hover_text(format!("Volume {:.0}%", v * 100.0));
    }

    /// Assist toggles in the transport bar. Click toggles; the caret (or right-click) opens
    /// the tool's options in a popover anchored to the button.
    fn assist_strip(&mut self, ui: &mut Ui, x0: f32, full: Rect) {
        let (bw, bh, gap) = (40.0, 30.0, 4.0);
        let cy = full.center().y + 1.0;
        let mut x = x0;
        for k in AssistKind::ALL {
            let r = Rect::from_min_size(Pos2::new(x, cy - bh / 2.0), Vec2::new(bw, bh));
            let caret = Rect::from_min_max(Pos2::new(r.right() - 13.0, r.top()), r.right_bottom());
            let main = Rect::from_min_max(r.min, Pos2::new(caret.left(), r.bottom()));
            let mr = ui.interact(main, ui.scope_id().with(("assist", k)), Sense::click());
            let cr = ui.interact(caret, ui.scope_id().with(("assist-opts", k)), Sense::click());
            let on = self.prefs.assists.is_on(k);
            let open = matches!(self.assist_popover, Some((o, _, _)) if o == k);
            let hot = mr.hovered() || cr.hovered();
            let held = mr.is_pointer_button_down_on();
            let p = ui.painter();
            let fill = if on || open { CONTROL } else if hot { HOVER } else { egui::Color32::TRANSPARENT };
            p.rect_filled(if held { r.shrink(1.0) } else { r }, CornerRadius::same(RADIUS), fill);
            p.text(main.center() + Vec2::new(2.0, 0.0), Align2::CENTER_CENTER, k.icon(), theme::ui(15.0), if on { INK } else if hot { INK_DIM } else { SECONDARY });
            p.text(caret.center(), Align2::CENTER_CENTER, icon::CARET_DOWN, theme::ui(9.0), if cr.hovered() || open { INK } else { MUTED });
            if on {
                // Lit indicator, like a camera button LED.
                p.rect_filled(Rect::from_center_size(Pos2::new(main.center().x + 2.0, r.bottom() - 3.0), Vec2::new(10.0, 2.0)), 1.0, INK);
            }
            let tip = format!("{}  ({})\nClick to toggle · arrow or right-click for options", k.label(), k.shortcut());
            if mr.clicked() {
                self.prefs.assists.toggle(k);
            }
            if cr.clicked() || mr.secondary_clicked() {
                self.assist_popover = if open { None } else { Some((k, r, Instant::now())) };
            }
            if !open {
                mr.on_hover_text(tip);
            }
            x += bw + gap;
        }
    }

    /// The options popover for the open assist, anchored above its button.
    fn assist_popover_ui(&mut self, ctx: &egui::Context) {
        let Some((k, anchor, t0)) = self.assist_popover else { return };
        // 150 ms ease-out from the button (origin-aware); closing is instant.
        let t = (t0.elapsed().as_secs_f32() / 0.15).min(1.0);
        let e = 1.0 - (1.0 - t).powi(3);
        if t < 1.0 {
            ctx.request_repaint();
        }
        let pos = Pos2::new(anchor.left() - 4.0, anchor.top() - 10.0);
        let mut close = false;
        let resp = egui::Area::new(egui::Id::unique("assist-popover"))
            .order(egui::Order::Foreground)
            .fixed_pos(pos)
            .pivot(Align2::LEFT_BOTTOM)
            .constrain(true)
            .show(ctx, |ui| {
                ui.multiply_opacity(e);
                egui::Frame::popup(ui.style()).inner_margin(egui::Margin::same(14)).show(ui, |ui| {
                    ui.set_width(300.0);
                    ui.spacing_mut().item_spacing.y = 2.0;
                    ui.horizontal(|ui| {
                        ui.label(RichText::new(k.icon()).font(theme::ui(15.0)).color(INK));
                        ui.label(RichText::new(k.label()).font(theme::strong(13.0)).color(INK));
                        let (kr, _) = ui.allocate_exact_size(Vec2::new(46.0, 18.0), Sense::hover());
                        widgets::keycap(ui, ui.painter(), kr.left_top(), k.shortcut());
                        ui.with_layout(Layout::right_to_left(Align::Center), |ui| {
                            if widgets::icon_button(ui, icon::X, "Close (Esc)", 22.0).clicked() {
                                close = true;
                            }
                        });
                    });
                    ui.add_space(S2);
                    let mut on = self.prefs.assists.is_on(k);
                    widgets::row(ui, "Show in viewer", |ui| {
                        ui.checkbox(&mut on, "");
                    });
                    if on != self.prefs.assists.is_on(k) {
                        self.prefs.assists.toggle(k);
                    }
                    let a = &mut self.prefs.assists;
                    match k {
                        AssistKind::Guides => assists::guides_options(ui, &mut a.guides),
                        AssistKind::Peaking => assists::peaking_options(ui, &mut a.peaking),
                        AssistKind::FalseColor => assists::false_color_options(ui, &mut a.false_color),
                        AssistKind::Histogram => assists::histogram_options(ui, &mut a.histogram),
                    }
                });
            });
        // Click outside (and not on a combo/color popup opened from inside) closes it.
        let area = resp.response.rect;
        let clicked_outside = ctx.input(|i| {
            i.pointer.any_pressed() && i.pointer.interact_pos().is_some_and(|p| !area.contains(p) && !anchor.contains(p))
        });
        if close || (clicked_outside && !egui::Popup::is_any_open(ctx)) {
            self.assist_popover = None;
        }
    }

    fn tray(&mut self, ui: &mut Ui) {
        let full = ui.max_rect();
        let busy = self.exporter.pending > 0;
        // Right end: summary + export
        let right_w = 260.0;
        let right = Rect::from_min_max(Pos2::new(full.right() - right_w, full.top()), full.right_bottom());
        let mut export_clicked = false;
        let mut cancel_clicked = false;
        ui.scope_builder(egui::UiBuilder::new().max_rect(right.shrink2(Vec2::new(S4, S3))).layout(Layout::top_down(Align::Max)), |ui| {
            ui.add_space(S2);
            let fmt = format!(
                "{} · {}",
                match self.prefs.export.kind {
                    ExportKind::Dng => "DNG",
                    ExportKind::CinemaDng => "CinemaDNG",
                },
                self.prefs.export.sample.label()
            );
            ui.label(RichText::new(format!("{} frame{} · {fmt}", self.saved.len(), if self.saved.len() == 1 { "" } else { "s" })).color(MUTED).font(theme::ui(12.0)));
            ui.add_space(S2);
            if busy {
                ui.label(RichText::new(format!("Exporting {} of {}", (self.export_done + 1).min(self.export_total), self.export_total)).color(INK).font(theme::ui(12.5)));
                ui.add_space(S1);
                if widgets::quiet_button(ui, "Cancel").clicked() {
                    cancel_clicked = true;
                }
            } else {
                let pending = self.saved.iter().filter(|s| !matches!(s.status, Status::Exported { .. })).count();
                let label = if pending == 0 && !self.saved.is_empty() { "All exported".to_string() } else { if pending == 0 { "Export".to_string() } else { format!("Export {pending}") } };
                if widgets::primary_button(ui, &label, pending > 0).on_hover_text("Export all grabbed frames (Ctrl+E)").clicked() {
                    export_clicked = true;
                }
            }
        });
        if export_clicked {
            self.export_all();
        }
        if cancel_clicked {
            self.exporter.cancel_all();
        }

        let area = Rect::from_min_max(full.left_top(), Pos2::new(right.left(), full.bottom()));
        if self.saved.is_empty() {
            // Empty mounts teach the lightbox before the first grab.
            let painter = ui.painter();
            let mut x = area.left() + S4;
            for _ in 0..3 {
                let r = Rect::from_min_size(Pos2::new(x, full.top() + S3 + 4.0), Vec2::new(150.0, 79.0));
                painter.rect_stroke(r, CornerRadius::same(3), Stroke::new(1.0, HAIRLINE), egui::StrokeKind::Inside);
                painter.rect_filled(Rect::from_min_size(Pos2::new(r.left(), r.bottom() + 9.0), Vec2::new(46.0, 3.0)), 1.0, HAIRLINE);
                painter.rect_filled(Rect::from_min_size(Pos2::new(r.right() - 38.0, r.bottom() + 9.0), Vec2::new(38.0, 3.0)), 1.0, HAIRLINE);
                x += 150.0 + S3;
            }
            let hint = Pos2::new(x + S2, full.top() + S3 + 4.0 + 39.5);
            painter.text(hint + Vec2::new(0.0, -9.0), Align2::LEFT_CENTER, "Grabbed frames land here", theme::ui(13.0), SECONDARY);
            let g = painter.text(hint + Vec2::new(0.0, 11.0), Align2::LEFT_CENTER, "Press", theme::ui(12.5), MUTED);
            let kr = widgets::keycap(ui, painter, Pos2::new(g.right() + 5.0, hint.y + 2.0), "G");
            painter.text(Pos2::new(kr.right() + 5.0, hint.y + 11.0), Align2::LEFT_CENTER, "to grab the current frame", theme::ui(12.5), MUTED);
            return;
        }
        let mut select = None;
        let mut remove = None;
        let mut export_one = None;
        let mut reveal = None;
        let mut toggle_pin = None;
        ui.scope_builder(egui::UiBuilder::new().max_rect(area), |ui| {
            // A plain mouse wheel scrolls the strip sideways (Shift+wheel and touchpads already do).
            if ui.rect_contains_pointer(area) {
                let d = ui.input(|i| i.smooth_scroll_delta);
                if d.y != 0.0 && d.x == 0.0 {
                    ui.input_mut(|i| {
                        i.smooth_scroll_delta = Vec2::new(d.y, 0.0);
                    });
                }
            }
            let reveal_frame = self.scroll_to_frame.take();
            egui::ScrollArea::horizontal().auto_shrink([false, false]).show(ui, |ui| {
                ui.horizontal(|ui| {
                    ui.add_space(S4);
                    for s in self.saved.iter_mut() {
                        let mount = Vec2::new(150.0, 79.0);
                        let (rect, resp) = ui.allocate_exact_size(Vec2::new(mount.x, full.height() - 2.0 * S3), Sense::click());
                        if reveal_frame == Some(s.id) {
                            ui.scroll_to_rect(rect.expand2(Vec2::new(S4, 0.0)), None);
                        }
                        let img_rect = Rect::from_min_size(Pos2::new(rect.left(), full.top() + S3 + 4.0), mount);
                        let painter = ui.painter();
                        painter.rect_filled(img_rect, CornerRadius::same(3), VIEWER);
                        // Same registration for every frame: fit inside the mount, centred.
                        let a = s.thumb_size.x / s.thumb_size.y.max(1.0);
                        let fit = if a > mount.x / mount.y { Vec2::new(mount.x, mount.x / a) } else { Vec2::new(mount.y * a, mount.y) };
                        let ir = Rect::from_center_size(img_rect.center(), fit);
                        painter.image(s.thumb.id(), ir, Rect::from_min_max(Pos2::ZERO, Pos2::new(1.0, 1.0)), egui::Color32::WHITE);
                        let sel = self.selected == Some(s.id);
                        let stroke = if sel { Stroke::new(1.0, INK) } else if resp.hovered() { Stroke::new(1.0, SECONDARY) } else { Stroke::new(1.0, CONTROL) };
                        painter.rect_stroke(img_rect, CornerRadius::same(3), stroke, egui::StrokeKind::Outside);
                        if sel {
                            painter.circle_filled(img_rect.right_top() + Vec2::new(-7.0, 7.0), 3.0, HERO);
                        }
                        // Imprint
                        let iy = img_rect.bottom() + 7.0;
                        painter.text(Pos2::new(img_rect.left(), iy), Align2::LEFT_TOP, format!("{} · {:04}", s.short_clip(), s.frame).to_uppercase(), theme::imprint(12.0), MUTED);
                        painter.text(Pos2::new(img_rect.right(), iy), Align2::RIGHT_TOP, &s.timecode, theme::imprint(12.0), MUTED);
                        // Status badge, held until seen.
                        let badge = |text: &str, color: egui::Color32| {
                            let g = painter.layout_no_wrap(text.to_string(), theme::imprint(11.0), color);
                            let br = Rect::from_min_size(img_rect.left_top() + Vec2::new(5.0, 5.0), g.size() + Vec2::new(8.0, 2.0));
                            painter.rect_filled(br, CornerRadius::same(2), egui::Color32::from_black_alpha(190));
                            painter.galley(br.min + Vec2::new(4.0, 1.0), g, color);
                        };
                        let (edited, summary) = summarize(&s.color, &s.source.info().as_shot);
                        // Bottom-left marks: corrected (sliders) and frame-specific settings (pin).
                        let mut mx = img_rect.left() + 5.0;
                        let my = img_rect.bottom() - 5.0;
                        for (show, ic) in [(edited, icon::SLIDERS), (s.own.is_some(), icon::PIN)] {
                            if show {
                                let r = Rect::from_min_max(Pos2::new(mx, my - 16.0), Pos2::new(mx + 18.0, my));
                                painter.rect_filled(r, CornerRadius::same(3), egui::Color32::from_black_alpha(190));
                                painter.text(r.center(), Align2::CENTER_CENTER, ic, theme::ui(11.0), INK_DIM);
                                mx += 21.0;
                            }
                        }
                        match &mut s.status {
                            Status::Ready if s.exported.is_some() => badge("CHANGED", INK),
                            Status::Ready => {}
                            Status::Queued => badge("QUEUED", SECONDARY),
                            Status::Exporting => badge("EXPORTING…", INK),
                            Status::Exported { seen, .. } => {
                                if !*seen {
                                    badge("EXPORTED", INK);
                                    if resp.hovered() {
                                        *seen = true;
                                    }
                                } else {
                                    painter.text(img_rect.left_top() + Vec2::new(9.0, 8.0), Align2::LEFT_TOP, icon::CHECK, theme::ui(12.0), INK_DIM);
                                }
                            }
                            Status::Failed(_) => badge("FAILED", HERO),
                        }
                        if resp.hovered() {
                            let xr = Rect::from_center_size(img_rect.right_bottom() + Vec2::new(-12.0, -12.0), Vec2::splat(20.0));
                            painter.rect_filled(xr, CornerRadius::same(4), egui::Color32::from_black_alpha(190));
                            painter.text(xr.center(), Align2::CENTER_CENTER, icon::X, theme::ui(11.0), INK);
                            if resp.clicked() && resp.interact_pointer_pos().is_some_and(|p| xr.contains(p)) {
                                remove = Some(s.id);
                            }
                        }
                        let link = if s.own.is_some() { "Own settings (pinned)" } else { "Follows the clip's Camera RAW settings" };
                        let state = match &s.status {
                            Status::Failed(e) => format!("Export failed: {e}"),
                            Status::Exported { path, .. } => format!("Exported: {}", path.display()),
                            Status::Ready if s.exported.is_some() => "Settings changed since export: export again".to_string(),
                            _ => "Click to view · double-click to export".to_string(),
                        };
                        let tip = format!(
                            "{} · frame {}\n{}{}\n{link}\n{state}",
                            s.clip,
                            s.frame,
                            summary,
                            if edited { "  (corrected)" } else { "  (as shot)" }
                        );
                        let resp = resp.on_hover_text(tip);
                        if resp.double_clicked() {
                            export_one = Some(s.id);
                        } else if resp.clicked() && remove != Some(s.id) {
                            select = Some(s.id);
                        }
                        let id = s.id;
                        let exported = s.exported.as_ref().map(|(_, p)| p.clone());
                        let pinned = s.own.is_some();
                        resp.context_menu(|ui| {
                            if ui.button("Export").clicked() {
                                export_one = Some(id);
                                ui.close();
                            }
                            ui.separator();
                            if !pinned {
                                if ui.button(format!("{}  Keep these settings for this frame", icon::PIN)).on_hover_text("Later changes to the clip's Camera RAW settings won't affect this frame").clicked() {
                                    toggle_pin = Some(id);
                                    ui.close();
                                }
                            } else if ui.button("Follow the clip's settings again").clicked() {
                                toggle_pin = Some(id);
                                ui.close();
                            }
                            ui.separator();
                            if let Some(p) = &exported {
                                if ui.button("Show in Explorer").clicked() {
                                    reveal = Some(p.clone());
                                    ui.close();
                                }
                            }
                            if ui.button("Remove").clicked() {
                                remove = Some(id);
                                ui.close();
                            }
                        });
                        ui.add_space(S3);
                    }
                });
            });
        });
        if let Some(id) = remove {
            self.saved.retain(|s| s.id != id);
            if self.selected == Some(id) {
                self.selected = None;
            }
        } else if let Some(id) = select {
            self.selected = Some(id);
            self.jump_to_saved(id);
        }
        if let Some(id) = export_one {
            if let Some(s) = self.saved.iter_mut().find(|s| s.id == id) {
                if matches!(s.status, Status::Exported { .. } | Status::Failed(_)) {
                    s.status = Status::Ready;
                }
            }
            self.export_ids(vec![id]);
        }
        if let Some(p) = reveal {
            reveal_in_explorer(&p);
        }
        if let Some(id) = toggle_pin {
            if let Some(s) = self.saved.iter_mut().find(|s| s.id == id) {
                s.own = if s.own.is_some() { None } else { Some(s.color.clone()) };
            }
        }
    }

    fn viewer(&mut self, ui: &mut Ui) {
        let rect = ui.max_rect();
        ui.painter().rect_filled(rect, 0.0, VIEWER);
        let ctx = ui.ctx().clone();

        let Some(i) = self.active else {
            self.empty_state(ui, rect);
            return;
        };
        let view = self.prefs.view;
        let lut = self.lut.clone();
        let preview_color = self.preview_color(&self.clips[i]);
        // Live adjusting (a Camera RAW control held and changing): decode small and fast,
        // refine to full resolution once released.
        let settings_now = preview_color.settings_hash();
        if settings_now != self.live_hash {
            self.live_hash = settings_now;
            self.live_changed_at = Instant::now();
        }
        let adjusting = ctx.input(|inp| inp.pointer.any_down()) && self.live_changed_at.elapsed() < Duration::from_millis(500);
        if adjusting {
            ctx.request_repaint_after(Duration::from_millis(520)); // refine even if held still
        }
        let ppp = ctx.pixels_per_point();
        let clip = &mut self.clips[i];
        let info = clip.source.info().clone();
        let (iw, ih) = (info.width.max(1) as f32, info.height.max(1) as f32);

        // Interactions: wheel zoom around the cursor, drag to pan, double-click toggles fit/1:1.
        let resp = ui.interact(rect, ui.scope_id().with("viewer"), Sense::click_and_drag());
        let fit_scale = ((rect.width() - 48.0) / iw).min((rect.height() - 48.0) / ih).max(0.01); // points per image px
        let mut scale = match self.zoom {
            Zoom::Fit => fit_scale,
            Zoom::Scale(z) => z / ppp,
        };
        if resp.hovered() {
            let scroll = ui.input(|i| i.smooth_scroll_delta.y);
            if scroll != 0.0 {
                let old = scale;
                scale = (scale * (scroll * 0.002).exp()).clamp(fit_scale.min(0.05), 16.0 / ppp);
                if let Some(pos) = resp.hover_pos() {
                    let c = rect.center() + self.pan;
                    self.pan += (pos - c) * (1.0 - scale / old);
                }
                self.zoom = Zoom::Scale(scale * ppp);
            }
        }
        if resp.dragged() && self.zoom != Zoom::Fit {
            self.pan += resp.drag_delta();
        }
        if resp.double_clicked() {
            self.zoom = if self.zoom == Zoom::Fit { Zoom::Scale(1.0) } else { Zoom::Fit };
            self.pan = Vec2::ZERO;
            scale = match self.zoom {
                Zoom::Fit => fit_scale,
                Zoom::Scale(z) => z / ppp,
            };
        }
        if self.zoom == Zoom::Fit {
            self.pan = Vec2::ZERO;
        }
        let img = Rect::from_center_size(rect.center() + self.pan, Vec2::new(iw * scale, ih * scale));

        // Which decode resolution do we need?
        let p = &mut clip.player;
        let playing = p.advance();
        // Audio: plays at 1x forward and is the master clock then (picture follows sound).
        let audio = &mut self.audio;
        audio.set_gain(if self.prefs.muted { 0.0 } else { self.prefs.volume * self.prefs.volume });
        let want_audio = playing && p.rate == 1.0 && clip.has_audio && audio.available;
        match (want_audio, audio.playing) {
            (true, Some((k, _))) if k == i => {}
            (true, _) => audio.start(i, clip.source.clone(), p.position, p.fps()),
            (false, Some(_)) => audio.stop(),
            _ => {}
        }
        if let Some((k, start)) = audio.playing {
            if k == i {
                let last = (p.frame_count() - 1) as f64;
                let pos = start + audio.clock().unwrap_or(0.0) * p.fps();
                p.position = pos.min(last);
                if pos >= last {
                    p.pause();
                    audio.stop();
                }
            }
        }
        p.collect();
        let shown_px = img.width() * ppp;
        let preview_scale = match self.prefs.preview {
            PreviewRes::Fixed(s) => s,
            PreviewRes::Auto => {
                let mut best = ResolutionScale::Full;
                for s in [ResolutionScale::Eighth, ResolutionScale::Quarter, ResolutionScale::Half, ResolutionScale::Full] {
                    if iw / s.divisor() as f32 >= shown_px {
                        best = s;
                        break;
                    }
                }
                if playing && best == ResolutionScale::Full {
                    ResolutionScale::Half
                } else {
                    best
                }
            }
        };
        let preview_scale = if adjusting && preview_scale.divisor() < ResolutionScale::Quarter.divisor() { ResolutionScale::Quarter } else { preview_scale };
        let hash = preview_color.settings_hash();
        let cur = p.current_frame();
        let key = |frame: u64, scale: ResolutionScale| Key { frame, settings: hash, scale };
        let want_full = !playing && !adjusting && self.prefs.full_when_paused && preview_scale != ResolutionScale::Full && p.paused_for() > Duration::from_millis(150);
        let mut wanted: Vec<(Key, DecodeColor)> = Vec::new();
        if want_full {
            wanted.push((key(cur, ResolutionScale::Full), preview_color.clone()));
        }
        wanted.push((key(cur, preview_scale), preview_color.clone()));
        let n = p.frame_count();
        if playing {
            let dir: i64 = if p.rate < 0.0 { -1 } else { 1 };
            let stride = p.rate.abs().max(1.0) as i64;
            for k in 1..=8i64 {
                let f = cur as i64 + dir * k * stride;
                if f >= 0 && (f as u64) < n {
                    wanted.push((key(f as u64, preview_scale), preview_color.clone()));
                }
            }
        } else if !adjusting {
            for d in [1i64, -1, 2] {
                let f = cur as i64 + d;
                if f >= 0 && (f as u64) < n {
                    wanted.push((key(f as u64, preview_scale), preview_color.clone()));
                }
            }
        }
        let decoding = !p.has(&key(cur, preview_scale)) && !p.has(&key(cur, ResolutionScale::Full));
        p.request(wanted);
        let best = p.get(&key(cur, ResolutionScale::Full)).or_else(|| p.get(&key(cur, preview_scale)));
        if let Some(f) = best {
            p.shown = Some(f);
        }
        let err = p.error_for(&key(cur, preview_scale)).cloned();

        if let Some(frame) = p.shown.clone() {
            let uv = [
                (img.left() - rect.left()) / rect.width(),
                (img.top() - rect.top()) / rect.height(),
                (img.right() - rect.left()) / rect.width(),
                (img.bottom() - rect.top()) / rect.height(),
            ];
            let assists = self.prefs.assists;
            ui.painter().add(egui_wgpu::Callback::new_paint_callback(
                rect,
                ViewerCallback { frame: frame.clone(), view, lut: lut.clone(), image_uv_rect: uv, assist: assists.gpu() },
            ));
            // Assist overlays (viewer-only), clipped to the viewer.
            let overlay = ui.painter_at(rect);
            let pic = img.intersect(rect);
            if assists.guides_on {
                assists::draw_guides(&overlay, img, &assists.guides);
            }
            if assists.false_color_on && assists.false_color.legend {
                assists::draw_false_color_legend(&overlay, pic);
            }
            if assists.histogram_on {
                use std::hash::{Hash, Hasher};
                let mut hsh = std::collections::hash_map::DefaultHasher::new();
                (Arc::as_ptr(&frame) as usize).hash(&mut hsh);
                format!("{view:?}").hash(&mut hsh);
                lut.as_ref().map(|l| Arc::as_ptr(l) as usize).hash(&mut hsh);
                let key = hsh.finish();
                // Recompute when the picture changes, at most ~15 times a second.
                let stale = !matches!(&self.hist, Some((k, _, _)) if *k == key);
                let due = self.hist.as_ref().is_none_or(|(_, at, _)| at.elapsed() > Duration::from_millis(66));
                if stale && due {
                    self.hist = Some((key, Instant::now(), assists::compute_histogram(&frame, view, lut.as_deref())));
                } else if stale {
                    ctx.request_repaint_after(Duration::from_millis(70));
                }
                if let Some((_, _, h)) = &self.hist {
                    let hr = Rect::from_min_size(Pos2::new(pic.right() - S3 - 220.0, pic.bottom() - S3 - 90.0), Vec2::new(220.0, 90.0));
                    assists::draw_histogram(&overlay, hr, h, assists.histogram.mode);
                }
            }
            // HUD
            let hud = theme::mono(11.0);
            let left = format!("{} · {}×{} · {:.3}", info.camera_model, info.width, info.height, info.fps);
            ui.painter().text(rect.left_top() + Vec2::new(S4, S3), Align2::LEFT_TOP, left, hud.clone(), MUTED);
            let mut right = match view.output {
                OutputMode::Rec709G24 => "Rec.709 · Gamma 2.4".to_string(),
                OutputMode::Srgb => "sRGB".to_string(),
                OutputMode::AsDecoded => "Log · as decoded".to_string(),
            };
            if let (Some(l), true) = (&lut, view.lut_enabled) {
                right = if view.lut_placement == LutPlacement::OnLog {
                    format!("LUT {}", truncate(&l.name, 28))
                } else {
                    format!("{right} · LUT {}", truncate(&l.name, 20))
                };
            }
            if view.exposure_ev != 0.0 {
                right = format!("{right} · {:+.1} EV", view.exposure_ev);
            }
            ui.painter().text(rect.right_top() + Vec2::new(-S4, S3), Align2::RIGHT_TOP, right, hud.clone(), MUTED);
            if matches!(frame.encoding.transfer, Transfer::Unknown(_)) && view.output != OutputMode::AsDecoded {
                ui.painter().text(rect.center_top() + Vec2::new(0.0, S3), Align2::CENTER_TOP, "No CST for this gamma: shown as decoded", hud.clone(), SECONDARY);
            }
            let active = assists.active_labels();
            let res_label = format!(
                "{}{}{}",
                if frame.width as f32 >= iw { "Full".to_string() } else { format!("{}×{}", frame.width, frame.height) },
                match self.zoom {
                    Zoom::Fit => "  ·  Fit".to_string(),
                    Zoom::Scale(z) => format!("  ·  {:.0}%", z * 100.0),
                },
                if active.is_empty() { String::new() } else { format!("  ·  {}", active.join(" · ")) }
            );
            ui.painter().text(rect.left_bottom() + Vec2::new(S4, -S3), Align2::LEFT_BOTTOM, res_label, hud.clone(), MUTED);
            if decoding {
                ui.painter().text(rect.right_bottom() + Vec2::new(-S4, -S3), Align2::RIGHT_BOTTOM, "Decoding…", hud, SECONDARY);
            }
        } else if let Some(e) = err {
            ui.painter().text(rect.center(), Align2::CENTER_CENTER, format!("Can't decode this frame\n{e}"), theme::ui(13.0), SECONDARY);
        } else {
            ui.painter().text(rect.center(), Align2::CENTER_CENTER, "Decoding…", theme::ui(13.0), MUTED);
        }
        if playing || decoding {
            ctx.request_repaint();
        }
        self.draw_toasts(ui, rect);
    }

    fn empty_state(&mut self, ui: &mut Ui, rect: Rect) {
        let ctx = ui.ctx().clone();
        let hovering_files = ctx.input(|i| !i.raw.hovered_files.is_empty());
        let c = rect.center();
        if hovering_files {
            ui.painter().rect_stroke(rect.shrink(S5), CornerRadius::same(10), Stroke::new(1.0, SECONDARY), egui::StrokeKind::Inside);
            ui.painter().text(c, Align2::CENTER_CENTER, "Drop to open", theme::strong(15.0), INK);
            return;
        }
        ui.painter().text(c + Vec2::new(0.0, -52.0), Align2::CENTER_CENTER, icon::FILM_STRIP, theme::ui(30.0), CONTROL_HI);
        ui.painter().text(c + Vec2::new(0.0, -14.0), Align2::CENTER_CENTER, "Drop .braw clips here", theme::strong(16.0), INK);
        ui.painter().text(c + Vec2::new(0.0, 10.0), Align2::CENTER_CENTER, "Find a frame, press G, export it as a Linear DNG.", theme::ui(13.0), SECONDARY);
        let mut open = false;
        let mut pattern = false;
        ui.scope_builder(egui::UiBuilder::new().max_rect(Rect::from_center_size(c + Vec2::new(0.0, 52.0), Vec2::new(320.0, 34.0))).layout(Layout::left_to_right(Align::Center)), |ui| {
            ui.add_space(28.0);
            if widgets::primary_button(ui, "Open clips…", true).clicked() {
                open = true;
            }
            ui.add_space(S2);
            if widgets::quiet_button(ui, "Try the test pattern").clicked() {
                pattern = true;
            }
        });
        if let Some(note) = &self.backend_note {
            ui.painter().text(c + Vec2::new(0.0, 100.0), Align2::CENTER_CENTER, note, theme::ui(12.0), HERO);
        }
        if open {
            self.open_dialog(&ctx);
        }
        if pattern {
            self.open_test_pattern(&ctx);
        }
        self.draw_toasts(ui, rect);
    }

    fn draw_toasts(&mut self, ui: &mut Ui, rect: Rect) {
        const LIFE: f32 = 4.0;
        self.toasts.retain(|t| t.born.elapsed().as_secs_f32() < if t.error { LIFE * 2.0 } else { LIFE });
        let mut y = rect.bottom() - S5;
        let mut reveal = None;
        for (i, t) in self.toasts.iter().enumerate().rev().take(4) {
            let age = t.born.elapsed().as_secs_f32();
            let life = if t.error { LIFE * 2.0 } else { LIFE };
            // Enter 200ms ease-out, exit 150ms (exits faster than entrances).
            let a_in = 1.0 - (1.0 - (age / 0.2).min(1.0)).powi(3);
            let a_out = ((life - age) / 0.15).clamp(0.0, 1.0);
            let a = a_in.min(a_out);
            let font = theme::ui(13.0);
            let g = ui.painter().layout(t.text.clone(), font, if t.error { HERO } else { INK }, 380.0);
            let action_w = if t.reveal.is_some() { 64.0 } else { 0.0 };
            let size = g.size() + Vec2::new(28.0 + action_w, 20.0);
            let r = Rect::from_min_size(Pos2::new(rect.right() - S5 - size.x, y - size.y + (1.0 - a_in) * 8.0), size);
            let painter = ui.painter().clone().with_layer_id(egui::LayerId::new(egui::Order::Foreground, egui::Id::unique("toasts")));
            painter.add(egui::epaint::Shadow { offset: [0, 6], blur: 18, spread: 0, color: egui::Color32::from_black_alpha((150.0 * a) as u8) }.as_shape(r, CornerRadius::same(8)));
            painter.rect_filled(r, CornerRadius::same(8), egui::Color32::from_rgb(0x18, 0x18, 0x1a).gamma_multiply(a));
            painter.rect_stroke(r, CornerRadius::same(8), Stroke::new(1.0, HAIRLINE.gamma_multiply(a)), egui::StrokeKind::Inside);
            painter.galley(r.min + Vec2::new(14.0, 10.0), g, INK.gamma_multiply(a));
            if let Some(p) = &t.reveal {
                let br = Rect::from_min_size(Pos2::new(r.right() - action_w - 6.0, r.center().y - 12.0), Vec2::new(action_w, 24.0));
                let resp = ui.interact(br, ui.scope_id().with(("toast", i)), Sense::click());
                painter.rect_filled(br, CornerRadius::same(5), if resp.hovered() { CONTROL } else { HOVER });
                painter.text(br.center(), Align2::CENTER_CENTER, "Show", theme::ui(12.0), INK.gamma_multiply(a));
                if resp.clicked() {
                    reveal = Some(p.clone());
                }
            }
            y = r.top() - S2;
        }
        if let Some(p) = reveal {
            reveal_in_explorer(&p);
        }
        if !self.toasts.is_empty() {
            ui.ctx().request_repaint_after(Duration::from_millis(16));
        }
    }

    fn shortcuts_window(&mut self, ctx: &egui::Context) {
        if !self.show_shortcuts {
            return;
        }
        let mut open = true;
        egui::Window::new("Keyboard shortcuts").open(&mut open).collapsible(false).resizable(false).anchor(Align2::CENTER_CENTER, Vec2::ZERO).show(ctx, |ui| {
            egui::Grid::new("keys").num_columns(2).spacing([S5, 6.0]).show(ui, |ui| {
                for (k, d) in [
                    ("Space", "Play / pause"),
                    ("J  /  L", "Play reverse / forward; press again for 2× 4× 8×"),
                    ("K", "Pause"),
                    ("←  /  →", "Previous / next frame"),
                    ("Shift + ←  /  →", "Back / forward 10 frames"),
                    ("Home  /  End", "First / last frame"),
                    ("G", "Grab frame to the lightbox"),
                    ("Ctrl + Shift + E", "Grab and export the current frame"),
                    ("Ctrl + E", "Export all grabbed frames"),
                    ("Delete", "Remove the selected grabbed frame"),
                    ("F  ·  double-click", "Fit / actual pixels"),
                    ("Alt + G / P / C / H", "Guides / focus peaking / false color / histogram"),
                    ("Alt + 0", "All viewer assists off"),
                    ("M", "Mute / unmute audio"),
                    ("Ctrl + O", "Open clips"),
                ] {
                    ui.label(RichText::new(k).font(theme::mono(12.0)).color(INK));
                    ui.label(RichText::new(d).color(SECONDARY));
                    ui.end_row();
                }
            });
        });
        if !open {
            self.show_shortcuts = false;
        }
    }
}

fn short_res(w: u32, h: u32) -> String {
    if w >= 1000 {
        format!("{:.1}K", w as f32 / 1024.0).replace(".0K", "K")
    } else {
        format!("{w}×{h}")
    }
}

fn truncate(s: &str, n: usize) -> String {
    if s.chars().count() <= n {
        s.to_string()
    } else {
        let t: String = s.chars().take(n.saturating_sub(1)).collect();
        format!("{t}…")
    }
}

fn reveal_in_explorer(p: &Path) {
    let _ = std::process::Command::new("explorer").arg(format!("/select,{}", p.display())).spawn();
}

impl eframe::App for RawFramerApp {
    fn ui(&mut self, ui: &mut Ui, _frame: &mut eframe::Frame) {
        let ctx = ui.ctx().clone();
        // Dropped files
        let dropped: Vec<PathBuf> = ctx.input(|i| i.raw.dropped_files.iter().map(|f| f.path().to_path_buf()).filter(|p| !p.as_os_str().is_empty()).collect());
        for p in dropped {
            self.open_path(&ctx, &p);
        }
        self.poll_exports();
        self.sync_saved(&ctx);
        self.handle_shortcuts(&ctx);
        self.manage_memory();
        let title = self.active.map(|i| format!("{} — Raw Framer", self.clips[i].name)).unwrap_or_else(|| "Raw Framer".into());
        if title != self.title {
            ctx.send_viewport_cmd(egui::ViewportCommand::Title(title.clone()));
            self.title = title;
        }

        let bar = |fill| egui::Frame::new().fill(fill);
        egui::Panel::top("top")
            .exact_size(TOP_BAR_H)
            .frame(bar(ROOM).inner_margin(egui::Margin::symmetric(8, 0)).stroke(Stroke::new(1.0, HAIRLINE)))
            .show(ui, |ui| self.top_bar(ui));
        egui::Panel::right("side")
            .exact_size(SIDE_W)
            .resizable(false)
            .frame(bar(ROOM).inner_margin(egui::Margin { left: 16, right: 12, top: 4, bottom: 0 }).stroke(Stroke::new(1.0, HAIRLINE)))
            .show(ui, |ui| self.side_panel(ui));
        egui::Panel::bottom("tray")
            .exact_size(TRAY_H)
            .frame(bar(TRAY).stroke(Stroke::new(1.0, HAIRLINE)))
            .show(ui, |ui| self.tray(ui));
        egui::Panel::bottom("transport")
            .exact_size(TRANSPORT_H)
            .frame(bar(ROOM))
            .show(ui, |ui| self.transport(ui));
        egui::CentralPanel::no_frame().show(ui, |ui| self.viewer(ui));
        self.assist_popover_ui(&ctx);
        self.shortcuts_window(&ctx);
        self.autosave(&ctx, false);
    }

    fn save(&mut self, storage: &mut dyn eframe::Storage) {
        eframe::set_value(storage, "prefs", &self.prefs);
    }

    fn on_exit(&mut self) {
        self.audio.stop();
        let ctx = egui::Context::default(); // only used for repaint requests, which are moot now
        self.autosave(&ctx, true);
        crate::safety::end(self.primary);
        log::info!("clean exit");
    }

    fn clear_color(&self, _v: &egui::Visuals) -> [f32; 4] {
        ROOM.to_normalized_gamma_f32()
    }
}

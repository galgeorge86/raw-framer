//! Background re-rendering of tray thumbnails when a grabbed frame's settings change.

use crossbeam_channel::{unbounded, Receiver, Sender};
use rf_color::{CubeLut, ViewSettings};
use rf_core::{DecodeColor, DecodeRequest, PixelFormat, ResolutionScale, SharedSource};
use std::sync::Arc;

pub struct ThumbJob {
    pub id: u64,
    pub key: u64,
    pub source: SharedSource,
    pub frame: u64,
    /// Decode settings as the viewer would use them (may be linear for unknown curves).
    pub color: DecodeColor,
    pub view: ViewSettings,
    pub lut: Option<Arc<CubeLut>>,
}

pub struct ThumbResult {
    pub id: u64,
    pub key: u64,
    pub image: Option<(u32, u32, Vec<u8>)>,
}

pub struct Thumbnailer {
    tx: Sender<ThumbJob>,
    pub rx: Receiver<ThumbResult>,
}

impl Thumbnailer {
    pub fn new(ctx: egui::Context) -> Self {
        let (tx, jobs) = unbounded::<ThumbJob>();
        let (res_tx, rx) = unbounded();
        std::thread::Builder::new()
            .name("thumbs".into())
            .spawn(move || {
                for j in jobs {
                    let req = DecodeRequest { frame: j.frame, scale: ResolutionScale::Quarter, format: PixelFormat::RgbaF16, color: j.color };
                    let image = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
                        j.source.decode(&req).ok().map(|f| rf_color::view::render_rgb8(&f, j.view, j.lut.as_deref(), 360))
                    }))
                    .unwrap_or(None);
                    let _ = res_tx.send(ThumbResult { id: j.id, key: j.key, image });
                    ctx.request_repaint();
                }
            })
            .expect("spawn thumbnail worker");
        Self { tx, rx }
    }

    pub fn submit(&self, job: ThumbJob) {
        let _ = self.tx.send(job);
    }
}

//! Background export queue (one worker; each DNG already uses all cores internally).

use crossbeam_channel::{unbounded, Receiver, Sender};
use rf_color::{CubeLut, ViewSettings};
use rf_core::{DecodeColor, SharedSource};
use rf_export::{ExportReport, ExportSettings};
use std::path::PathBuf;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;

pub struct ExportJob {
    pub id: u64,
    pub source: SharedSource,
    pub frame: u64,
    pub color: DecodeColor,
    /// What the viewer decodes for this frame (graded copy / previews follow it).
    pub view_color: DecodeColor,
    pub settings: ExportSettings,
    pub view: ViewSettings,
    pub lut: Option<Arc<CubeLut>>,
    pub path: PathBuf,
}

pub enum ExportEvent {
    Started(u64),
    Done(u64, Result<ExportReport, String>),
    Cancelled(u64),
}

pub struct Exporter {
    tx: Sender<ExportJob>,
    pub rx: Receiver<ExportEvent>,
    cancel: Arc<AtomicBool>,
    pub pending: usize,
}

impl Exporter {
    pub fn new(ctx: egui::Context) -> Self {
        let (tx, job_rx) = unbounded::<ExportJob>();
        let (ev_tx, rx) = unbounded();
        let cancel = Arc::new(AtomicBool::new(false));
        let c = cancel.clone();
        std::thread::Builder::new()
            .name("export".into())
            .spawn(move || {
                for job in job_rx {
                    if c.load(Ordering::Relaxed) {
                        let _ = ev_tx.send(ExportEvent::Cancelled(job.id));
                        ctx.request_repaint();
                        continue;
                    }
                    let _ = ev_tx.send(ExportEvent::Started(job.id));
                    ctx.request_repaint();
                    let res = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
                        rf_export::export_frame(
                            job.source.as_ref(),
                            job.frame,
                            &job.color,
                            &job.settings,
                            Some(rf_export::Look { view: job.view, lut: job.lut.as_deref(), decode: Some(&job.view_color) }),
                            &job.path,
                        )
                        .map_err(|e| e.to_string())
                    }))
                    .unwrap_or_else(|_| Err("the exporter crashed on this frame (details in the log)".into()));
                    let _ = ev_tx.send(ExportEvent::Done(job.id, res));
                    ctx.request_repaint();
                }
            })
            .expect("spawn export worker");
        Self { tx, rx, cancel, pending: 0 }
    }

    pub fn submit(&mut self, job: ExportJob) {
        self.cancel.store(false, Ordering::Relaxed);
        self.pending += 1;
        let _ = self.tx.send(job);
    }

    pub fn cancel_all(&self) {
        self.cancel.store(true, Ordering::Relaxed);
    }
}

//! Clip audio playback. A feeder thread reads clip audio ahead of the playhead (resampled
//! and channel-mapped to the output device), the device callback drains it, and the number
//! of frames actually played is the master clock while playing at 1x.

use cpal::traits::{DeviceTrait, HostTrait, StreamTrait};
use crossbeam_channel::{unbounded, Receiver, Sender};
use parking_lot::Mutex;
use rf_core::SharedSource;
use std::collections::VecDeque;
use std::sync::atomic::{AtomicBool, AtomicU32, AtomicU64, Ordering};
use std::sync::Arc;
use std::time::Duration;

struct Shared {
    buf: Mutex<VecDeque<f32>>,
    /// Device frames played since `start` (underruns don't count).
    played: AtomicU64,
    active: AtomicBool,
    /// Output gain (f32 bits), 0 when muted.
    gain: AtomicU32,
    /// Bumped on every start/stop so a stale feeder never pushes old audio.
    session: AtomicU64,
}

enum Cmd {
    Start { source: SharedSource, start_frame: u64, session: u64 },
    Stop,
}

pub struct AudioOut {
    _stream: Option<cpal::Stream>,
    shared: Arc<Shared>,
    tx: Sender<Cmd>,
    pub device_rate: u32,
    /// Which source is playing and from which video frame it started.
    pub playing: Option<(usize, f64)>,
    pub available: bool,
}

impl AudioOut {
    pub fn new() -> Self {
        let shared = Arc::new(Shared {
            buf: Mutex::new(VecDeque::new()),
            played: AtomicU64::new(0),
            active: AtomicBool::new(false),
            gain: AtomicU32::new(0.8f32.to_bits()),
            session: AtomicU64::new(0),
        });
        let (tx, rx) = unbounded();
        let (stream, rate, channels) = match open_stream(shared.clone()) {
            Ok(v) => (Some(v.0), v.1, v.2),
            Err(e) => {
                log::warn!("audio output unavailable: {e}");
                (None, 48_000, 2)
            }
        };
        let available = stream.is_some();
        log::info!("audio output: {} ({} Hz, {} ch)", if available { "ready" } else { "unavailable" }, rate, channels);
        let s2 = shared.clone();
        std::thread::Builder::new()
            .name("audio-feeder".into())
            .spawn(move || feeder(rx, s2, rate, channels))
            .expect("spawn audio feeder");
        Self { _stream: stream, shared, tx, device_rate: rate, playing: None, available }
    }

    pub fn set_gain(&self, gain: f32) {
        self.shared.gain.store(gain.clamp(0.0, 1.0).to_bits(), Ordering::Relaxed);
    }

    /// Start playing `source` from video frame `frame` (at `fps`). `key` identifies the clip.
    pub fn start(&mut self, key: usize, source: SharedSource, frame: f64, fps: f64) {
        let Some(info) = source.audio_info() else { return };
        let start_frame = ((frame / fps.max(1.0)) * info.sample_rate as f64).max(0.0) as u64;
        let session = self.shared.session.fetch_add(1, Ordering::SeqCst) + 1;
        self.shared.active.store(false, Ordering::SeqCst);
        self.shared.buf.lock().clear();
        self.shared.played.store(0, Ordering::SeqCst);
        let _ = self.tx.send(Cmd::Start { source, start_frame, session });
        self.shared.active.store(true, Ordering::SeqCst);
        log::info!("audio: start at frame {frame:.1} (source sample {start_frame}, device {} Hz)", self.device_rate);
        self.playing = Some((key, frame));
    }

    pub fn stop(&mut self) {
        if self.playing.take().is_some() {
            log::info!("audio: stopped after {:.2} s played", self.clock().unwrap_or(0.0));
            self.shared.session.fetch_add(1, Ordering::SeqCst);
            self.shared.active.store(false, Ordering::SeqCst);
            self.shared.buf.lock().clear();
            let _ = self.tx.send(Cmd::Stop);
        }
    }

    /// Seconds of audio actually played since `start` (None until the first samples play).
    pub fn clock(&self) -> Option<f64> {
        let p = self.shared.played.load(Ordering::Relaxed);
        (p > 0).then(|| p as f64 / self.device_rate as f64)
    }
}

impl Drop for AudioOut {
    fn drop(&mut self) {
        self.stop();
    }
}

fn open_stream(shared: Arc<Shared>) -> Result<(cpal::Stream, u32, u16), String> {
    let host = cpal::default_host();
    let device = host.default_output_device().ok_or("no output device")?;
    let supported = device.default_output_config().map_err(|e| e.to_string())?;
    let config: cpal::StreamConfig = supported.config();
    let rate = config.sample_rate;
    let channels = config.channels;
    let s = shared;
    let stream = device
        .build_output_stream::<f32, _, _>(
            config,
            move |data: &mut [f32], _| {
                let gain = f32::from_bits(s.gain.load(Ordering::Relaxed));
                let ch = channels as usize;
                if !s.active.load(Ordering::Relaxed) {
                    data.fill(0.0);
                    return;
                }
                let mut buf = match s.buf.try_lock() {
                    Some(b) => b,
                    None => {
                        data.fill(0.0);
                        return;
                    }
                };
                let mut frames = 0u64;
                for frame in data.chunks_mut(ch) {
                    if buf.len() >= ch {
                        for v in frame.iter_mut() {
                            *v = buf.pop_front().unwrap_or(0.0) * gain;
                        }
                        frames += 1;
                    } else {
                        frame.fill(0.0);
                    }
                }
                s.played.fetch_add(frames, Ordering::Relaxed);
            },
            |e| log::warn!("audio stream error: {e}"),
            None,
        )
        .map_err(|e| e.to_string())?;
    stream.play().map_err(|e| e.to_string())?;
    Ok((stream, rate, channels))
}

/// Keeps ~0.35 s of device-format audio queued ahead of the playhead.
fn feeder(rx: Receiver<Cmd>, s: Arc<Shared>, out_rate: u32, out_ch: u16) {
    let mut current: Option<(SharedSource, u64, u64, f64)> = None; // source, next src frame, session, frac
    let target = (out_rate as usize * out_ch as usize * 35) / 100;
    loop {
        let cmd = if current.is_some() { rx.recv_timeout(Duration::from_millis(8)).ok() } else { rx.recv().ok() };
        match cmd {
            Some(Cmd::Start { source, start_frame, session }) => current = Some((source, start_frame, session, 0.0)),
            Some(Cmd::Stop) => current = None,
            None if current.is_none() => return, // channel closed
            None => {}
        }
        let Some((src, next, session, frac)) = current.as_mut() else { continue };
        if s.session.load(Ordering::SeqCst) != *session {
            current = None;
            continue;
        }
        let Some(info) = src.audio_info() else {
            current = None;
            continue;
        };
        if s.buf.lock().len() >= target {
            continue;
        }
        let chunk = 4096usize;
        let read = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| src.read_audio(*next, chunk + 1)));
        let Ok(Ok(samples)) = read else {
            current = None;
            continue;
        };
        let in_ch = info.channels.max(1) as usize;
        let got = samples.len() / in_ch;
        if got < 2 {
            continue; // end of clip: the callback drains what's left, then plays silence
        }
        // Linear resample (no-op when rates match) and map channels (mono -> all, extra dropped).
        let step = info.sample_rate as f64 / out_rate as f64;
        let mut out = Vec::with_capacity(((got as f64 / step) as usize + 2) * out_ch as usize);
        let mut t = *frac;
        while t < (got - 1) as f64 {
            let i = t as usize;
            let f = (t - i as f64) as f32;
            for c in 0..out_ch as usize {
                let sc = if in_ch == 1 { 0 } else { c.min(in_ch - 1) };
                let a = samples[i * in_ch + sc];
                let b = samples[(i + 1) * in_ch + sc];
                out.push(a + (b - a) * f);
            }
            t += step;
        }
        let consumed = t.floor() as u64;
        *frac = t - consumed as f64;
        *next += consumed;
        if s.session.load(Ordering::SeqCst) == *session {
            s.buf.lock().extend(out);
        }
    }
}

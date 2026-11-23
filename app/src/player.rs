//! Playback engine: decode worker pool, priority request queue, LRU frame cache and the
//! real-time clock (.claude/ARCHITECTURE.md §4.1).

use parking_lot::{Condvar, Mutex};
use rf_core::*;
use std::collections::{HashMap, HashSet, VecDeque};
use std::sync::Arc;
use std::time::{Duration, Instant};

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub struct Key {
    pub frame: u64,
    pub settings: u64,
    pub scale: ResolutionScale,
}

struct Job {
    key: Key,
    color: DecodeColor,
}

#[derive(Default)]
struct Queue {
    jobs: VecDeque<Job>,
    inflight: HashSet<Key>,
    shutdown: bool,
}

struct Shared {
    q: Mutex<Queue>,
    cv: Condvar,
    results: Mutex<Vec<(Key, std::result::Result<Arc<DecodedFrame>, String>)>>,
}

pub struct Player {
    pub source: SharedSource,
    shared: Arc<Shared>,
    workers: Vec<std::thread::JoinHandle<()>>,
    cache: HashMap<Key, (Arc<DecodedFrame>, u64)>,
    cache_bytes: usize,
    cache_budget: usize,
    tick: u64,
    errors: HashMap<Key, String>,

    /// Playhead in frames (fractional while playing).
    pub position: f64,
    /// Playback rate: 0 = paused, ±1, ±2, ±4, ±8.
    pub rate: f32,
    pub looping: bool,
    last_tick: Option<Instant>,
    paused_since: Option<Instant>,
    /// Last frame shown (kept while the wanted one decodes).
    pub shown: Option<Arc<DecodedFrame>>,
    pub last_error: Option<String>,
}

impl Player {
    pub fn new(source: SharedSource, ctx: egui::Context, workers: usize) -> Self {
        let shared = Arc::new(Shared { q: Mutex::new(Queue::default()), cv: Condvar::new(), results: Mutex::new(Vec::new()) });
        let mut handles = Vec::new();
        for i in 0..workers.max(1) {
            let shared = shared.clone();
            let src = source.clone();
            let ctx = ctx.clone();
            handles.push(
                std::thread::Builder::new()
                    .name(format!("decode-{i}"))
                    .spawn(move || worker(shared, src, ctx))
                    .expect("spawn decode worker"),
            );
        }
        Self {
            source,
            shared,
            workers: handles,
            cache: HashMap::new(),
            cache_bytes: 0,
            cache_budget: 1536 << 20,
            tick: 0,
            errors: HashMap::new(),
            position: 0.0,
            rate: 0.0,
            looping: false,
            last_tick: None,
            paused_since: Some(Instant::now()),
            shown: None,
            last_error: None,
        }
    }

    pub fn frame_count(&self) -> u64 {
        self.source.info().frame_count.max(1)
    }
    pub fn fps(&self) -> f64 {
        let f = self.source.info().fps as f64;
        if f > 0.0 {
            f
        } else {
            24.0
        }
    }
    pub fn current_frame(&self) -> u64 {
        (self.position.floor().max(0.0) as u64).min(self.frame_count() - 1)
    }
    pub fn is_playing(&self) -> bool {
        self.rate != 0.0
    }

    pub fn seek(&mut self, frame: i64) {
        let max = self.frame_count() as i64 - 1;
        self.position = frame.clamp(0, max) as f64;
    }
    pub fn step(&mut self, delta: i64) {
        self.pause();
        let f = self.current_frame() as i64 + delta;
        self.seek(f);
    }
    pub fn pause(&mut self) {
        if self.rate != 0.0 {
            self.rate = 0.0;
            self.position = self.position.floor();
            self.paused_since = Some(Instant::now());
        }
    }
    pub fn play(&mut self, rate: f32) {
        if self.rate == 0.0 && rate > 0.0 && self.current_frame() + 1 >= self.frame_count() {
            self.position = 0.0; // restart from the top when playing at the end
        }
        self.rate = rate;
        self.paused_since = None;
        self.last_tick = Some(Instant::now());
    }
    pub fn toggle(&mut self) {
        if self.is_playing() {
            self.pause()
        } else {
            self.play(1.0)
        }
    }
    /// J/K/L shuttle: repeated presses double the speed up to 8x.
    pub fn shuttle(&mut self, forward: bool) {
        let r = self.rate;
        let next = if forward {
            if r <= 0.0 { 1.0 } else { (r * 2.0).min(8.0) }
        } else if r >= 0.0 {
            -1.0
        } else {
            (r * 2.0).max(-8.0)
        };
        self.play(next);
    }
    pub fn paused_for(&self) -> Duration {
        self.paused_since.map(|t| t.elapsed()).unwrap_or_default()
    }

    /// Advance the clock. Returns true while playing (caller should keep repainting).
    pub fn advance(&mut self) -> bool {
        let now = Instant::now();
        let dt = self.last_tick.map(|t| now.duration_since(t).as_secs_f64()).unwrap_or(0.0).min(0.25);
        self.last_tick = Some(now);
        if self.rate == 0.0 {
            return false;
        }
        self.position += self.rate as f64 * self.fps() * dt;
        let n = self.frame_count() as f64;
        if self.position >= n {
            if self.looping {
                self.position %= n;
            } else {
                self.position = n - 1.0;
                self.pause();
            }
        } else if self.position < 0.0 {
            if self.looping {
                self.position = n + self.position;
            } else {
                self.position = 0.0;
                self.pause();
            }
        }
        true
    }

    /// Collect finished decodes into the cache.
    pub fn collect(&mut self) {
        let done: Vec<_> = std::mem::take(&mut *self.shared.results.lock());
        for (key, res) in done {
            match res {
                Ok(f) => {
                    self.tick += 1;
                    self.cache_bytes += f.pixels.byte_len();
                    if let Some((old, _)) = self.cache.insert(key, (f, self.tick)) {
                        self.cache_bytes -= old.pixels.byte_len();
                    }
                    self.errors.remove(&key);
                }
                Err(e) => {
                    self.last_error = Some(e.clone());
                    self.errors.insert(key, e);
                }
            }
        }
        self.trim_to(self.cache_budget, 2);
    }

    /// Evict least-recently-used frames until the cache fits `budget` bytes (keeping at
    /// least `keep` frames).
    pub fn trim_to(&mut self, budget: usize, keep: usize) {
        while self.cache_bytes > budget && self.cache.len() > keep {
            let Some(k) = self.cache.iter().min_by_key(|(_, (_, t))| *t).map(|(k, _)| *k) else { break };
            if let Some((f, _)) = self.cache.remove(&k) {
                self.cache_bytes -= f.pixels.byte_len();
            }
        }
    }

    pub fn set_budget(&mut self, bytes: usize) {
        self.cache_budget = bytes;
    }

    pub fn cache_bytes(&self) -> usize {
        self.cache_bytes
    }

    /// Give back everything held for display while this clip isn't the one on screen.
    pub fn release_memory(&mut self) {
        if self.cache.is_empty() && self.shown.is_none() {
            return;
        }
        self.shared.q.lock().jobs.clear();
        self.trim_to(0, 0);
        self.shown = None;
    }

    pub fn get(&mut self, key: &Key) -> Option<Arc<DecodedFrame>> {
        self.tick += 1;
        let t = self.tick;
        self.cache.get_mut(key).map(|(f, used)| {
            *used = t;
            f.clone()
        })
    }

    pub fn has(&self, key: &Key) -> bool {
        self.cache.contains_key(key)
    }

    pub fn error_for(&self, key: &Key) -> Option<&String> {
        self.errors.get(key)
    }

    /// Replace the pending queue with `wanted` (highest priority first). Cached and
    /// in-flight keys are skipped; stale requests are dropped.
    pub fn request(&self, wanted: Vec<(Key, DecodeColor)>) {
        let mut q = self.shared.q.lock();
        q.jobs.clear();
        for (key, color) in wanted {
            if self.cache.contains_key(&key) || q.inflight.contains(&key) || self.errors.contains_key(&key) {
                continue;
            }
            q.jobs.push_back(Job { key, color });
        }
        drop(q);
        self.shared.cv.notify_all();
    }

    #[allow(dead_code)]
    pub fn clear_errors(&mut self) {
        self.errors.clear();
        self.last_error = None;
    }
}

impl Drop for Player {
    fn drop(&mut self) {
        {
            let mut q = self.shared.q.lock();
            q.shutdown = true;
            q.jobs.clear();
        }
        self.shared.cv.notify_all();
        for h in self.workers.drain(..) {
            let _ = h.join();
        }
    }
}

fn worker(shared: Arc<Shared>, src: SharedSource, ctx: egui::Context) {
    loop {
        let job = {
            let mut q = shared.q.lock();
            loop {
                if q.shutdown {
                    return;
                }
                if let Some(j) = q.jobs.pop_front() {
                    q.inflight.insert(j.key);
                    break j;
                }
                shared.cv.wait(&mut q);
            }
        };
        let req = DecodeRequest { frame: job.key.frame, scale: job.key.scale, format: PixelFormat::RgbaF16, color: job.color };
        // A panic in the decoder must not take the worker (or the app) down with it.
        let res = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| src.decode(&req)))
            .unwrap_or_else(|_| Err(rf_core::Error::Decode("the decoder crashed on this frame".into())))
            .map(Arc::new)
            .map_err(|e| e.to_string());
        shared.results.lock().push((job.key, res));
        shared.q.lock().inflight.remove(&job.key);
        ctx.request_repaint();
    }
}

//! Crash safety: a log file, crash reports, unclean-exit detection, atomic file writes and
//! system memory queries. Everything here must work even when the rest of the app is in a
//! bad state, so it only uses std and a few kernel32 calls.

use std::io::Write;
use std::path::{Path, PathBuf};
use std::time::{SystemTime, UNIX_EPOCH};

/// `%APPDATA%\Raw Framer` (eframe keeps its own storage in `data\` under it).
pub fn data_dir() -> PathBuf {
    std::env::var_os("APPDATA").map(PathBuf::from).unwrap_or_else(std::env::temp_dir).join("Raw Framer")
}

pub fn log_dir() -> PathBuf {
    data_dir().join("logs")
}

/// What `begin()` found about the previous run.
pub struct Startup {
    /// The previous run ended without shutting down cleanly.
    pub previous_crashed: bool,
    /// Its log, kept for diagnosis.
    pub crash_log: Option<PathBuf>,
    /// This instance owns the session file (false when another instance is already running).
    pub primary: bool,
}

/// Call first thing in `main`: rotates logs, redirects stderr to the log file (release
/// builds), installs the panic hook and claims the running marker.
pub fn begin() -> Startup {
    let logs = log_dir();
    let _ = std::fs::create_dir_all(&logs);
    let marker = data_dir().join("running.pid");
    let other_alive = std::fs::read_to_string(&marker).ok().and_then(|s| s.trim().parse::<u32>().ok()).is_some_and(|pid| pid != std::process::id() && process_alive(pid));
    let previous_crashed = marker.exists() && !other_alive;
    let log = logs.join("raw-framer.log");
    let mut crash_log = None;
    if !other_alive && log.exists() {
        if previous_crashed {
            let dst = logs.join(format!("crash-{}.log", stamp(SystemTime::now())));
            if std::fs::rename(&log, &dst).is_ok() {
                crash_log = Some(dst);
            }
            prune(&logs, "crash-", 8);
        } else {
            let _ = std::fs::rename(&log, logs.join("raw-framer.prev.log"));
        }
    }
    let log_path = if other_alive { logs.join(format!("raw-framer.{}.log", std::process::id())) } else { log };
    redirect_stderr(&log_path);
    install_panic_hook();
    if !other_alive {
        let _ = std::fs::write(&marker, std::process::id().to_string());
    }
    Startup { previous_crashed, crash_log, primary: !other_alive }
}

/// Clean shutdown: the next start won't report a crash.
pub fn end(primary: bool) {
    if primary {
        let _ = std::fs::remove_file(data_dir().join("running.pid"));
    }
}

fn install_panic_hook() {
    let default = std::panic::take_hook();
    std::panic::set_hook(Box::new(move |info| {
        // Default output goes to stderr, i.e. the log file.
        default(info);
        let thread = std::thread::current().name().unwrap_or("unnamed").to_string();
        let msg = info
            .payload()
            .downcast_ref::<&str>()
            .map(|s| s.to_string())
            .or_else(|| info.payload().downcast_ref::<String>().cloned())
            .unwrap_or_else(|| "(no message)".into());
        let loc = info.location().map(|l| format!("{}:{}", l.file(), l.line())).unwrap_or_default();
        let bt = std::backtrace::Backtrace::force_capture();
        let report = format!(
            "Raw Framer {} panic\nthread: {thread}\nat: {loc}\nmessage: {msg}\n\nbacktrace:\n{bt}\n",
            env!("CARGO_PKG_VERSION")
        );
        let _ = std::fs::write(log_dir().join(format!("panic-{}.txt", stamp(SystemTime::now()))), report);
        prune(&log_dir(), "panic-", 8);
    }));
}

/// Keep the newest `keep` files starting with `prefix`.
fn prune(dir: &Path, prefix: &str, keep: usize) {
    let Ok(rd) = std::fs::read_dir(dir) else { return };
    let mut v: Vec<PathBuf> = rd.flatten().map(|e| e.path()).filter(|p| p.file_name().and_then(|n| n.to_str()).is_some_and(|n| n.starts_with(prefix))).collect();
    v.sort();
    while v.len() > keep {
        let _ = std::fs::remove_file(v.remove(0));
    }
}

/// Write `bytes` to `path` so that a crash mid-write never leaves a broken file: write a
/// temp file, flush it to disk, then atomically replace the target (keeping the previous
/// version as `.bak`).
pub fn write_atomic(path: &Path, bytes: &[u8]) -> std::io::Result<()> {
    if let Some(d) = path.parent() {
        std::fs::create_dir_all(d)?;
    }
    let tmp = path.with_extension("tmp");
    {
        let mut f = std::fs::File::create(&tmp)?;
        f.write_all(bytes)?;
        f.sync_all()?;
    }
    if path.exists() {
        let _ = std::fs::copy(path, path.with_extension("bak"));
    }
    std::fs::rename(&tmp, path)
}

/// `YYYYMMDD-HHMMSS` (UTC), for file names.
pub fn stamp(t: SystemTime) -> String {
    let s = t.duration_since(UNIX_EPOCH).map(|d| d.as_secs()).unwrap_or(0) as i64;
    let (days, rem) = (s.div_euclid(86_400), s.rem_euclid(86_400));
    // Civil-from-days (Howard Hinnant).
    let z = days + 719_468;
    let era = z.div_euclid(146_097);
    let doe = z - era * 146_097;
    let yoe = (doe - doe / 1460 + doe / 36_524 - doe / 146_096) / 365;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
    let mp = (5 * doy + 2) / 153;
    let d = doy - (153 * mp + 2) / 5 + 1;
    let m = if mp < 10 { mp + 3 } else { mp - 9 };
    let y = yoe + era * 400 + (m <= 2) as i64;
    format!("{y:04}{m:02}{d:02}-{:02}{:02}{:02}", rem / 3600, rem % 3600 / 60, rem % 60)
}

// ------------------------------------------------------------------ Windows specifics

#[cfg(windows)]
#[allow(dead_code)] // SetStdHandle is only used by release builds
mod win {
    #[repr(C)]
    pub struct MemoryStatusEx {
        pub length: u32,
        pub memory_load: u32,
        pub total_phys: u64,
        pub avail_phys: u64,
        pub total_page_file: u64,
        pub avail_page_file: u64,
        pub total_virtual: u64,
        pub avail_virtual: u64,
        pub avail_extended_virtual: u64,
    }
    #[link(name = "kernel32")]
    extern "system" {
        pub fn GlobalMemoryStatusEx(buf: *mut MemoryStatusEx) -> i32;
        pub fn SetStdHandle(id: u32, h: *mut core::ffi::c_void) -> i32;
        pub fn OpenProcess(access: u32, inherit: i32, pid: u32) -> *mut core::ffi::c_void;
        pub fn GetExitCodeProcess(h: *mut core::ffi::c_void, code: *mut u32) -> i32;
        pub fn CloseHandle(h: *mut core::ffi::c_void) -> i32;
    }
    pub const STD_ERROR_HANDLE: u32 = -12i32 as u32;
    pub const PROCESS_QUERY_LIMITED_INFORMATION: u32 = 0x1000;
    pub const STILL_ACTIVE: u32 = 259;
}

/// Physical memory (total, available) in bytes.
pub fn memory() -> Option<(u64, u64)> {
    #[cfg(windows)]
    unsafe {
        let mut m: win::MemoryStatusEx = std::mem::zeroed();
        m.length = std::mem::size_of::<win::MemoryStatusEx>() as u32;
        if win::GlobalMemoryStatusEx(&mut m) != 0 {
            return Some((m.total_phys, m.avail_phys));
        }
    }
    None
}

fn process_alive(pid: u32) -> bool {
    #[cfg(windows)]
    unsafe {
        let h = win::OpenProcess(win::PROCESS_QUERY_LIMITED_INFORMATION, 0, pid);
        if h.is_null() {
            return false;
        }
        let mut code = 0u32;
        let ok = win::GetExitCodeProcess(h, &mut code) != 0;
        win::CloseHandle(h);
        // The PID may have been reused by an unrelated process; only trust a Raw Framer.
        return ok && code == win::STILL_ACTIVE && process_is_raw_framer(pid);
    }
    #[allow(unreachable_code)]
    {
        let _ = pid;
        false
    }
}

#[cfg(windows)]
fn process_is_raw_framer(pid: u32) -> bool {
    // `tasklist` is slow-ish (~100 ms) but only runs when a marker from a live PID exists.
    std::process::Command::new("tasklist")
        .args(["/FI", &format!("PID eq {pid}"), "/FO", "CSV", "/NH"])
        .creation_flags_no_window()
        .output()
        .map(|o| String::from_utf8_lossy(&o.stdout).to_ascii_lowercase().contains("raw-framer"))
        .unwrap_or(false)
}

#[cfg(windows)]
trait NoWindow {
    fn creation_flags_no_window(&mut self) -> &mut Self;
}
#[cfg(windows)]
impl NoWindow for std::process::Command {
    fn creation_flags_no_window(&mut self) -> &mut Self {
        use std::os::windows::process::CommandExt;
        self.creation_flags(0x0800_0000) // CREATE_NO_WINDOW
    }
}

/// Send everything written to stderr (log records, panic messages and Rust's own
/// "memory allocation failed" message) to the log file. Release builds have no console,
/// so stderr would otherwise go nowhere. Debug builds keep the console.
fn redirect_stderr(path: &Path) {
    #[cfg(all(windows, not(debug_assertions)))]
    {
        use std::os::windows::io::IntoRawHandle;
        if let Ok(f) = std::fs::OpenOptions::new().create(true).append(true).open(path) {
            let h = f.into_raw_handle(); // leaked on purpose: lives for the process
            unsafe {
                win::SetStdHandle(win::STD_ERROR_HANDLE, h as *mut _);
            }
        }
    }
    #[cfg(not(all(windows, not(debug_assertions))))]
    let _ = path;
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::time::Duration;

    #[test]
    fn stamp_formats_utc() {
        assert_eq!(stamp(UNIX_EPOCH), "19700101-000000");
        // 2026-11-22 16:45:56 UTC
        assert_eq!(stamp(UNIX_EPOCH + Duration::from_secs(1_795_365_956)), "20261122-164556");
    }

    #[test]
    fn atomic_write_replaces_and_keeps_backup() {
        let dir = std::env::temp_dir().join(format!("rf-atomic-{}", std::process::id()));
        let p = dir.join("s.json");
        write_atomic(&p, b"one").unwrap();
        write_atomic(&p, b"two").unwrap();
        assert_eq!(std::fs::read(&p).unwrap(), b"two");
        assert_eq!(std::fs::read(p.with_extension("bak")).unwrap(), b"one");
        assert!(!p.with_extension("tmp").exists());
        let _ = std::fs::remove_dir_all(dir);
    }

    #[test]
    fn memory_query_works() {
        let (total, avail) = memory().expect("GlobalMemoryStatusEx");
        assert!(total > 0 && avail <= total);
    }
}

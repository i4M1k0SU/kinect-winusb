//! Backend for the `log` facade: appends to the file named by
//! `KINECT_WINUSB_LOG`, mirrors errors and info to `OutputDebugString`, and
//! optionally echoes to stderr (CLI tool).
//! `KINECT_WINUSB_LOG_LEVEL` = 0 (errors) .. 3 (trace), default 1.

use std::fs::{File, OpenOptions};
use std::io::Write;
use std::sync::Mutex;
use std::time::Instant;

use log::{Level, LevelFilter, Log, Metadata, Record};
use windows::Win32::System::Diagnostics::Debug::OutputDebugStringW;
use windows::Win32::System::Threading::GetCurrentThreadId;
use windows::core::HSTRING;

struct Logger {
    file: Option<Mutex<File>>,
    start: Instant,
    stderr: bool,
}

impl Log for Logger {
    fn enabled(&self, m: &Metadata) -> bool {
        m.level() <= log::max_level()
    }

    fn log(&self, r: &Record) {
        if !self.enabled(r.metadata()) {
            return;
        }
        let t = self.start.elapsed();
        let tid = unsafe { GetCurrentThreadId() };
        let line = format!("[kinect-winusb {:>4}.{:03} {tid:>5}] {}\n", t.as_secs(), t.subsec_millis(), r.args());
        if self.stderr {
            eprint!("{line}");
        }
        // During process exit a terminated thread may hold the lock (see `process_exiting`).
        if let Some(f) = &self.file
            && let Ok(mut f) =
                if crate::process_exiting() { f.try_lock().map_err(drop) } else { f.lock().map_err(drop) }
        {
            let _ = f.write_all(line.as_bytes());
        }
        if r.level() <= Level::Info {
            unsafe { OutputDebugStringW(&HSTRING::from(line)) };
        }
    }

    fn flush(&self) {}
}

/// Install the logger once per process (later calls do nothing). With `stderr`
/// set, records are also printed to stderr.
pub fn init(stderr: bool) {
    let level = std::env::var("KINECT_WINUSB_LOG_LEVEL").ok().and_then(|v| v.parse().ok()).unwrap_or(1u8);
    let file = std::env::var_os("KINECT_WINUSB_LOG")
        .and_then(|p| OpenOptions::new().create(true).append(true).open(p).ok())
        .map(Mutex::new);
    let logger = Logger { file, start: Instant::now(), stderr };
    if log::set_logger(Box::leak(Box::new(logger))).is_ok() {
        log::set_max_level(match level {
            0 => LevelFilter::Error,
            1 => LevelFilter::Info,
            2 => LevelFilter::Debug,
            _ => LevelFilter::Trace,
        });
    }
}

//! Logging to stderr and (optionally) a rotating file, mirroring player.py.

use std::fmt;
use std::fs::{self, OpenOptions};
use std::io::Write;
use std::path::{Path, PathBuf};
use std::sync::{Mutex, OnceLock};
use std::time::{SystemTime, UNIX_EPOCH};

const MAX_LOG_BYTES: u64 = 1024 * 1024;

struct Logger {
    file: Option<PathBuf>,
    to_stderr: bool,
}

static LOGGER: OnceLock<Mutex<Logger>> = OnceLock::new();

pub fn init(log_dir: Option<&Path>, to_stderr: bool) {
    let file = log_dir.and_then(|dir| {
        fs::create_dir_all(dir).ok()?;
        Some(dir.join("player.log"))
    });
    let _ = LOGGER.set(Mutex::new(Logger { file, to_stderr }));
}

/// Switches the log file target (used by tests to capture per-test logs).
pub fn set_log_file(path: Option<PathBuf>) {
    if let Some(l) = LOGGER.get() {
        l.lock().unwrap().file = path;
    }
}

/// Initializes a stderr-only logger; safe to call multiple times.
pub fn init_test() {
    let _ = LOGGER.set(Mutex::new(Logger {
        file: None,
        to_stderr: false,
    }));
}

fn rotate_if_needed(path: &Path) {
    if let Ok(md) = fs::metadata(path) {
        if md.len() > MAX_LOG_BYTES {
            let _ = fs::rename(path, path.with_extension("log.1"));
        }
    }
}

pub fn log(args: fmt::Arguments<'_>) {
    let secs = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0);
    let line = format!("[player] {}", args);
    let line = {
        let t = secs % 86400;
        let date = secs / 86400;
        // civil-from-days (Howard Hinnant's algorithm), avoids chrono dep
        let z = date as i64 + 719468;
        let era = if z >= 0 { z } else { z - 146096 } / 146097;
        let doe = (z - era * 146097) as u64;
        let yoe = (doe - doe / 1460 + doe / 36524 - doe / 146096) / 365;
        let y = yoe as i64 + era * 400;
        let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
        let mp = (5 * doy + 2) / 153;
        let d = doy - (153 * mp + 2) / 5 + 1;
        let m = if mp < 10 { mp + 3 } else { mp - 9 };
        let y = if m <= 2 { y + 1 } else { y };
        format!(
            "[player] {:04}-{:02}-{:02} {:02}:{:02}:{:02} {}",
            y,
            m,
            d,
            t / 3600,
            (t / 60) % 60,
            t % 60,
            line.trim_start_matches("[player] ")
        )
    };
    if let Some(l) = LOGGER.get() {
        let l = l.lock().unwrap();
        if l.to_stderr {
            eprintln!("{}", line);
        }
        if let Some(path) = &l.file {
            rotate_if_needed(path);
            if let Ok(mut f) = OpenOptions::new().create(true).append(true).open(path) {
                let _ = f.write_all(line.as_bytes());
                let _ = f.write_all(b"\n");
            }
        }
    } else {
        eprintln!("{}", line);
    }
}

#[macro_export]
macro_rules! log {
    ($($arg:tt)*) => { $crate::log::log(format_args!($($arg)*)) };
}

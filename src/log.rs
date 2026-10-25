//! Activity log at ~/.local/state/echolink-tui/activity.log, for following
//! what the program does (`tail -f`) and for troubleshooting connections.
//! The previous run's log is kept as activity.log.old.

use std::fs::File;
use std::io::Write;
use std::path::PathBuf;
use std::sync::{Mutex, OnceLock};
use std::time::{SystemTime, UNIX_EPOCH};

static LOG: OnceLock<Mutex<(File, i64)>> = OnceLock::new();

pub fn path() -> PathBuf {
    dirs::state_dir().unwrap_or_else(|| PathBuf::from(".")).join("echolink-tui/activity.log")
}

/// Seconds east of UTC, from `date +%z` (e.g. "-0500").
fn utc_offset() -> i64 {
    let z = std::process::Command::new("date")
        .arg("+%z")
        .output()
        .ok()
        .and_then(|o| String::from_utf8(o.stdout).ok())
        .unwrap_or_default();
    let z = z.trim();
    if z.len() != 5 {
        return 0;
    }
    let sign = if z.starts_with('-') { -1 } else { 1 };
    let h: i64 = z[1..3].parse().unwrap_or(0);
    let m: i64 = z[3..5].parse().unwrap_or(0);
    sign * (h * 3600 + m * 60)
}

pub fn init() {
    let p = path();
    if let Some(dir) = p.parent() {
        let _ = std::fs::create_dir_all(dir);
    }
    let _ = std::fs::rename(&p, p.with_extension("log.old"));
    if let Ok(f) = File::create(&p) {
        let _ = LOG.set(Mutex::new((f, utc_offset())));
    }
}

pub fn write(msg: &str) {
    let Some(lock) = LOG.get() else { return };
    let Ok(mut g) = lock.lock() else { return };
    let now = SystemTime::now().duration_since(UNIX_EPOCH).unwrap_or_default();
    let secs = now.as_secs() as i64 + g.1;
    let line = format!(
        "{:02}:{:02}:{:02}.{:03} {msg}\n",
        secs / 3600 % 24,
        secs / 60 % 60,
        secs % 60,
        now.subsec_millis()
    );
    let _ = g.0.write_all(line.as_bytes());
    let _ = g.0.flush();
}

#[macro_export]
macro_rules! log {
    ($($t:tt)*) => { $crate::log::write(&format!($($t)*)) };
}

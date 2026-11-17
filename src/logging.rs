//! Logging: stderr (journald on Linux) plus a small rolling file, so the
//! Windows build and the GUI's "log" button have something to show.

use std::fs::{self, OpenOptions};
use std::io::Write;
use std::time::{SystemTime, UNIX_EPOCH};

use crate::config;

const MAX_LOG_BYTES: u64 = 2 * 1024 * 1024;

pub fn log(msg: &str) {
    eprintln!("RustMIC: {msg}");
    write_file(msg);
}

fn write_file(msg: &str) {
    let path = config::log_path();
    if let Some(dir) = path.parent() {
        let _ = fs::create_dir_all(dir);
    }
    if fs::metadata(&path).map(|m| m.len()).unwrap_or(0) > MAX_LOG_BYTES {
        let _ = fs::remove_file(&path);
    }
    if let Ok(mut file) = OpenOptions::new().create(true).append(true).open(&path) {
        let _ = writeln!(file, "[{}] {}", timestamp(), msg);
    }
}

fn timestamp() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0)
}

/// Last `lines` lines of the log file (diagnostics on Windows).
pub fn tail(lines: usize) -> String {
    let text = fs::read_to_string(config::log_path()).unwrap_or_default();
    let all: Vec<&str> = text.lines().collect();
    let start = all.len().saturating_sub(lines);
    all[start..].join("\n")
}

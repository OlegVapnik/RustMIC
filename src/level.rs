//! Tiny cross-process level meter: the daemon writes the current output RMS to
//! a file (10 times a second), the GUI reads it. Works the same on both
//! platforms and costs nothing on the audio path.

use std::fs;
use std::sync::Mutex;
use std::time::{Duration, Instant};

use crate::config;

static LAST: Mutex<Option<Instant>> = Mutex::new(None);

pub fn publish(rms: f32) {
    {
        let mut last = match LAST.lock() {
            Ok(last) => last,
            Err(poisoned) => poisoned.into_inner(),
        };
        let now = Instant::now();
        if let Some(previous) = *last {
            if now.duration_since(previous) < Duration::from_millis(100) {
                return;
            }
        }
        *last = Some(now);
    }
    let path = config::level_path();
    if let Some(dir) = path.parent() {
        let _ = fs::create_dir_all(dir);
    }
    let _ = fs::write(path, format!("{rms:.5}"));
}

pub fn read() -> f32 {
    fs::read_to_string(config::level_path())
        .ok()
        .and_then(|text| text.trim().parse::<f32>().ok())
        .unwrap_or(0.0)
}

pub fn clear() {
    let _ = fs::remove_file(config::level_path());
}

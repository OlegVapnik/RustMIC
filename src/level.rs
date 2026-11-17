//! Tiny cross-process level meter: the daemon writes the current output RMS,
//! peak, VAD probability and gate state to a file (10 times a second), the GUI
//! reads it. Works the same on both platforms and costs nothing on the audio
//! path.
//!
//! The first token is still the plain RMS, so old readers keep working.

use std::fs;
use std::sync::Mutex;
use std::time::{Duration, Instant};

use crate::config;

static LAST: Mutex<Option<Instant>> = Mutex::new(None);

#[derive(Clone, Copy, Debug, Default)]
pub struct Meter {
    pub rms: f32,
    pub peak: f32,
    pub vad: f32,
    pub gate_open: bool,
}

fn should_publish() -> bool {
    let mut last = match LAST.lock() {
        Ok(last) => last,
        Err(poisoned) => poisoned.into_inner(),
    };
    let now = Instant::now();
    if let Some(previous) = *last {
        if now.duration_since(previous) < Duration::from_millis(100) {
            return false;
        }
    }
    *last = Some(now);
    true
}

/// Compatibility helper for callers that only have RMS.
pub fn publish(rms: f32) {
    publish_frame(rms, rms, 0.0, true);
}

pub fn publish_frame(rms: f32, peak: f32, vad: f32, gate_open: bool) {
    if !should_publish() {
        return;
    }
    let path = config::level_path();
    if let Some(dir) = path.parent() {
        let _ = fs::create_dir_all(dir);
    }
    let _ = fs::write(
        path,
        format!(
            "{:.5} {:.5} {:.3} {}\n",
            rms.clamp(0.0, 1.0),
            peak.clamp(0.0, 1.0),
            vad.clamp(0.0, 1.0),
            if gate_open { 1 } else { 0 }
        ),
    );
}

pub fn read() -> f32 {
    read_meter().rms
}

pub fn read_meter() -> Meter {
    let text = match fs::read_to_string(config::level_path()) {
        Ok(text) => text,
        Err(_) => return Meter::default(),
    };
    let mut parts = text.split_whitespace();
    let rms = parts.next().and_then(|value| value.parse().ok()).unwrap_or(0.0);
    let peak = parts.next().and_then(|value| value.parse().ok()).unwrap_or(rms);
    let vad = parts.next().and_then(|value| value.parse().ok()).unwrap_or(0.0);
    let gate_open = parts
        .next()
        .map(|value| value != "0")
        .unwrap_or(rms > 0.0);
    Meter {
        rms,
        peak,
        vad,
        gate_open,
    }
}

pub fn clear() {
    let _ = fs::remove_file(config::level_path());
}

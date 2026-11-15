//! RNNoise denoise stage (nnnoiseless, pure Rust).
//!
//! Works on 480-sample frames (10 ms @ 48 kHz), which is exactly RNNoise's
//! frame size, so no buffering/resampling is needed.

use std::sync::{Arc, RwLock};

use nnnoiseless::DenoiseState;

use crate::level;

pub const FRAME: usize = 480;

/// Settings the daemon picks up without a restart (the GUI writes config.toml).
#[derive(Clone, Copy)]
pub struct Live {
    pub denoise: bool,
    pub gate: f32,
    pub volume: u32,
}

impl Default for Live {
    fn default() -> Self {
        Self {
            denoise: true,
            gate: 0.0,
            volume: 100,
        }
    }
}

pub struct Denoiser {
    live: Arc<RwLock<Live>>,
    state: Box<DenoiseState<'static>>,
    pending: Vec<i16>,
    fin: [f32; FRAME],
    fout: [f32; FRAME],
    gain: f32,
    pub stats: bool,
    frames: u32,
    acc_out: f64,
    acc_vad: f64,
}

impl Denoiser {
    pub fn new(live: Arc<RwLock<Live>>) -> Self {
        Self {
            live,
            state: DenoiseState::new(),
            pending: Vec::with_capacity(FRAME),
            fin: [0.0; FRAME],
            fout: [0.0; FRAME],
            gain: 1.0,
            stats: false,
            frames: 0,
            acc_out: 0.0,
            acc_vad: 0.0,
        }
    }

    fn settings(&self) -> Live {
        self.live.read().map(|live| *live).unwrap_or_default()
    }

    /// Push mono s16 samples, append denoised s16 samples to `out`.
    pub fn process(&mut self, samples: &[i16], out: &mut Vec<i16>) {
        let Live { denoise, gate, .. } = self.settings();
        out.reserve(samples.len());
        if !denoise {
            out.extend_from_slice(samples);
            let sum: f64 = samples
                .iter()
                .map(|&sample| (sample as f64 / 32768.0).powi(2))
                .sum();
            if !samples.is_empty() {
                level::publish((sum / samples.len() as f64).sqrt() as f32);
            }
            return;
        }
        for &sample in samples {
            self.pending.push(sample);
            if self.pending.len() < FRAME {
                continue;
            }
            for i in 0..FRAME {
                self.fin[i] = self.pending[i] as f32;
            }
            let vad = self.state.process_frame(&mut self.fout, &self.fin);
            // optional soft gate driven by RNNoise's voice activity probability
            let target = if gate > 0.0 && vad < gate { 0.0 } else { 1.0 };
            self.gain += (target - self.gain) * 0.3;
            let mut sum = 0.0f64;
            for i in 0..FRAME {
                let value = (self.fout[i] * self.gain).clamp(-32768.0, 32767.0);
                sum += (value as f64 / 32768.0).powi(2);
                out.push(value as i16);
            }
            level::publish((sum / FRAME as f64).sqrt() as f32);
            if self.stats {
                let mut input = 0.0f64;
                for i in 0..FRAME {
                    input += (self.fin[i] as f64).powi(2);
                }
                self.acc_out += (sum / FRAME as f64).sqrt();
                self.acc_vad += vad as f64;
                self.frames += 1;
                if self.frames >= 50 {
                    crate::logging::log(&format!(
                        "stats: in_rms={:.1} out_rms={:.1} vad={:.2}",
                        (input / FRAME as f64).sqrt(),
                        self.acc_out / 50.0,
                        self.acc_vad / 50.0
                    ));
                    self.frames = 0;
                    self.acc_out = 0.0;
                    self.acc_vad = 0.0;
                }
            }
            self.pending.clear();
        }
    }
}

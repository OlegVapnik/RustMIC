//! RNNoise denoise stage (nnnoiseless, pure Rust).
//!
//! Works on 480-sample frames (10 ms @ 48 kHz), which is exactly RNNoise's
//! frame size, so no buffering/resampling is needed.
//!
//! The gate has two independent modes:
//! * `voice` — RNNoise voice-activity probability, threshold 0..100 %;
//! * `level` — output RMS in dBFS with 3 dB hysteresis.

use std::sync::{Arc, RwLock};

use nnnoiseless::DenoiseState;

use crate::config::GateMode;
use crate::level;

pub const FRAME: usize = 480;
const GATE_HYSTERESIS_DB: f32 = 3.0;

/// Settings the daemon picks up without a restart (the GUI writes config.toml).
#[derive(Clone, Copy)]
pub struct Live {
    pub denoise: bool,
    pub gate_mode: GateMode,
    pub gate_voice: u32,
    pub gate_level_db: f32,
    pub mic_volume: u32,
    /// Runtime mute, not persisted in config.toml.
    pub muted: bool,
}

impl Default for Live {
    fn default() -> Self {
        Self {
            denoise: true,
            gate_mode: GateMode::Voice,
            gate_voice: 0,
            gate_level_db: -45.0,
            mic_volume: 100,
            muted: false,
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
    gate_open: bool,
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
            gate_open: true,
            stats: false,
            frames: 0,
            acc_out: 0.0,
            acc_vad: 0.0,
        }
    }

    fn settings(&self) -> Live {
        self.live.read().map(|live| *live).unwrap_or_default()
    }

    fn gate_open(&mut self, live: Live, vad: f32, level_db: f32) -> bool {
        match live.gate_mode {
            GateMode::Voice => {
                if live.gate_voice == 0 {
                    true
                } else {
                    vad >= live.gate_voice as f32 / 100.0
                }
            }
            GateMode::Level => {
                if live.gate_level_db >= 0.0 {
                    true
                } else if self.gate_open {
                    level_db >= live.gate_level_db - GATE_HYSTERESIS_DB
                } else {
                    level_db > live.gate_level_db
                }
            }
        }
    }

    /// Push mono s16 samples, append denoised s16 samples to `out`.
    pub fn process(&mut self, samples: &[i16], out: &mut Vec<i16>) {
        let live = self.settings();
        out.reserve(samples.len());
        for &sample in samples {
            self.pending.push(sample);
            if self.pending.len() < FRAME {
                continue;
            }
            for i in 0..FRAME {
                self.fin[i] = self.pending[i] as f32;
            }
            let vad = self.state.process_frame(&mut self.fout, &self.fin);

            // RMS of the signal we are about to emit (denoised or original).
            let mut sum = 0.0f64;
            let mut peak = 0.0f32;
            for i in 0..FRAME {
                let sample = if live.muted {
                    0.0
                } else if live.denoise {
                    self.fout[i]
                } else {
                    self.fin[i]
                };
                sum += (sample as f64 / 32768.0).powi(2);
                peak = peak.max((sample / 32768.0).abs());
            }
            let rms = (sum / FRAME as f64).sqrt() as f32;
            let level_db = if rms > 0.0 {
                20.0 * rms.log10()
            } else {
                -90.0
            };

            self.gate_open = self.gate_open(live, vad, level_db);
            let target = if self.gate_open { 1.0 } else { 0.0 };
            self.gain += (target - self.gain) * 0.3;

            for i in 0..FRAME {
                let sample = if live.muted {
                    0.0
                } else if live.denoise {
                    self.fout[i]
                } else {
                    self.fin[i]
                };
                let value = (sample * self.gain).clamp(-32768.0, 32767.0);
                out.push(value as i16);
            }
            level::publish_frame(rms, peak, vad, self.gate_open);

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
                        "stats: in_rms={:.1} out_rms={:.1} vad={:.2} gate={}",
                        (input / FRAME as f64).sqrt(),
                        self.acc_out / 50.0,
                        self.acc_vad / 50.0,
                        self.gate_open
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

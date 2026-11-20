//! Loopback capture: "everything the computer plays" as a microphone source.
//!
//! This is what makes browser tabs (music.youtube.com, uwupad.me, any player)
//! audible to the other side of a call without any per-site integration: the
//! default playback device is captured and its PCM is fed through the same
//! denoise/mix/sink chain as the phone microphone.
//!
//! * Linux: `parec` records `@DEFAULT_MONITOR@` (PipeWire/Pulse monitor).
//! * Windows: cpal opens the default WASAPI *output* device as an input
//!   stream, which transparently enables loopback mode.

use std::io;
use std::sync::mpsc::Receiver;

/// Mono s16 little-endian at 48 kHz, plus a human readable description.
pub type Stream = (Receiver<io::Result<Vec<u8>>>, String);

#[cfg(unix)]
pub fn stream() -> Result<Stream, String> {
    unix::stream()
}

#[cfg(windows)]
pub fn stream() -> Result<Stream, String> {
    windows::stream()
}

// ---------------------------------------------------------------- linux

#[cfg(unix)]
mod unix {
    use std::io::{self, Read};
    use std::process::{Command, Stdio};
    use std::sync::mpsc::channel;
    use std::thread;

    use super::Stream;

    fn default_monitor() -> Result<String, String> {
        let output = Command::new("pactl")
            .arg("get-default-sink")
            .output()
            .map_err(|error| format!("pactl недоступен: {error}"))?;
        let sink = String::from_utf8_lossy(&output.stdout).trim().to_string();
        if sink.is_empty() {
            return Err("pactl не вернул устройство вывода по умолчанию".into());
        }
        Ok(format!("{sink}.monitor"))
    }

    pub fn stream() -> Result<Stream, String> {
        let monitor = default_monitor()?;
        let mut child = Command::new("parec")
            .args([
                "--device",
                &monitor,
                "--format=s16le",
                "--rate=48000",
                "--channels=1",
                "--latency-msec=40",
                "--raw",
            ])
            .stdin(Stdio::null())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .spawn()
            .map_err(|error| format!("не удалось запустить parec: {error}"))?;

        if let Some(mut stderr) = child.stderr.take() {
            thread::spawn(move || {
                let mut text = String::new();
                let _ = stderr.read_to_string(&mut text);
                let text = text.trim();
                if !text.is_empty() {
                    crate::logging::log(&format!("parec: {text}"));
                }
            });
        }

        let mut stdout = child.stdout.take().ok_or("parec: нет stdout")?;
        let (tx, rx) = channel();
        thread::spawn(move || {
            let mut buf = vec![0u8; 16 * 1024];
            loop {
                match stdout.read(&mut buf) {
                    Ok(0) => {
                        let _ = tx.send(Ok(Vec::new()));
                        break;
                    }
                    Ok(n) => {
                        if tx.send(Ok(buf[..n].to_vec())).is_err() {
                            break;
                        }
                    }
                    Err(error) if error.kind() == io::ErrorKind::Interrupted => continue,
                    Err(error) => {
                        let _ = tx.send(Err(error));
                        break;
                    }
                }
            }
            let _ = child.kill();
            let _ = child.wait();
        });

        Ok((rx, format!("системный звук ({monitor})")))
    }
}

// ---------------------------------------------------------------- windows

#[cfg(windows)]
mod windows {
    use std::sync::mpsc::channel;
    use std::sync::{Arc, Mutex};
    use std::thread;
    use std::time::Duration;

    use cpal::traits::{DeviceTrait, HostTrait, StreamTrait};
    use cpal::{FromSample, Sample, SampleFormat, SizedSample, Stream as CpalStream, StreamConfig};

    use super::Stream;
    use crate::STOP;

    /// Linear resampler from the device mix rate to 48 kHz (the rate the sink
    /// and the gate expect).
    struct Resampler {
        step: f32,
        phase: f32,
        previous: f32,
    }

    impl Resampler {
        fn new(source_rate: u32) -> Self {
            Self {
                step: source_rate as f32 / 48_000.0,
                phase: 0.0,
                previous: 0.0,
            }
        }

        fn push(&mut self, input: &[f32]) -> Vec<f32> {
            let mut out = Vec::with_capacity(input.len() + 2);
            for &sample in input {
                while self.phase < 1.0 {
                    out.push(self.previous + (sample - self.previous) * self.phase);
                    self.phase += self.step;
                }
                self.phase -= 1.0;
                self.previous = sample;
            }
            out
        }
    }

    struct Capture {
        _stream: CpalStream,
        error: Arc<Mutex<Option<String>>>,
    }

    fn build<T>(
        device: &cpal::Device,
        config: &StreamConfig,
        channels: usize,
        tx: std::sync::mpsc::Sender<std::io::Result<Vec<u8>>>,
        resampler: Arc<Mutex<Resampler>>,
        error: Arc<Mutex<Option<String>>>,
    ) -> Result<CpalStream, cpal::Error>
    where
        T: SizedSample,
        f32: FromSample<T>,
    {
        let channels = channels.max(1);
        let error_cb = error.clone();
        let error_tx = tx.clone();
        device.build_input_stream(
            config.clone(),
            move |data: &[T], _: &cpal::InputCallbackInfo| {
                let mut mono = Vec::with_capacity(data.len() / channels + 1);
                for frame in data.chunks(channels) {
                    let mut sum = 0.0f32;
                    for sample in frame {
                        sum += f32::from_sample(*sample);
                    }
                    mono.push(sum / frame.len() as f32);
                }
                let mono = resampler
                    .lock()
                    .unwrap_or_else(|poisoned| poisoned.into_inner())
                    .push(&mono);
                let mut bytes = Vec::with_capacity(mono.len() * 2);
                for value in mono {
                    let sample = (value.clamp(-1.0, 1.0) * 32767.0) as i16;
                    bytes.extend_from_slice(&sample.to_le_bytes());
                }
                let _ = tx.send(Ok(bytes));
            },
            move |err| {
                let mut slot = error_cb
                    .lock()
                    .unwrap_or_else(|poisoned| poisoned.into_inner());
                *slot = Some(err.to_string());
                let _ = error_tx.send(Err(std::io::Error::other(err.to_string())));
            },
            None,
        )
    }

    pub fn stream() -> Result<Stream, String> {
        let host = cpal::default_host();
        let device = host
            .default_output_device()
            .ok_or_else(|| "нет устройства вывода: нечего захватывать".to_string())?;
        let name = device
            .description()
            .map(|description| description.name().to_string())
            .unwrap_or_else(|_| "устройство по умолчанию".to_string());
        let supported = device
            .default_output_config()
            .map_err(|error| format!("не удалось прочитать формат \"{name}\": {error}"))?;
        let sample_format = supported.sample_format();
        let source_rate = supported.sample_rate();
        let channels = supported.channels() as usize;

        // Shared-mode WASAPI is usually 48 kHz; resample anyway so odd
        // devices (44.1 kHz) do not play at the wrong pitch.
        let config = StreamConfig {
            channels: supported.channels(),
            sample_rate: supported.sample_rate(),
            buffer_size: cpal::BufferSize::Default,
        };
        let (tx, rx) = channel();
        let thread_name = name.clone();
        thread::spawn(move || {
            let resampler = Arc::new(Mutex::new(Resampler::new(source_rate)));
            let error: Arc<Mutex<Option<String>>> = Arc::new(Mutex::new(None));
            let stream = match sample_format {
                SampleFormat::F32 => build::<f32>(&device, &config, channels, tx, resampler, error.clone()),
                SampleFormat::I16 => build::<i16>(&device, &config, channels, tx, resampler, error.clone()),
                SampleFormat::U16 => build::<u16>(&device, &config, channels, tx, resampler, error.clone()),
                SampleFormat::F64 => build::<f64>(&device, &config, channels, tx, resampler, error.clone()),
                other => Err(cpal::Error::with_message(
                    cpal::ErrorKind::UnsupportedConfig,
                    format!("unsupported sample format {other:?} on \"{thread_name}\""),
                )),
            };
            let capture = match stream.and_then(|stream| {
                stream.play().map(|()| Capture {
                    _stream: stream,
                    error: error.clone(),
                })
            }) {
                Ok(capture) => capture,
                Err(error) => {
                    crate::logging::log(&format!("system capture failed: {error}"));
                    return;
                }
            };
            crate::logging::log(&format!(
                "system capture: {thread_name} ({sample_format:?}, {source_rate} Hz)"
            ));
            while !STOP.load(std::sync::atomic::Ordering::SeqCst) {
                if let Some(error) = capture
                    .error
                    .lock()
                    .unwrap_or_else(|poisoned| poisoned.into_inner())
                    .clone()
                {
                    crate::logging::log(&format!("system capture stopped: {error}"));
                    return;
                }
                thread::sleep(Duration::from_millis(200));
            }
        });

        Ok((rx, format!("системный звук ({name})")))
    }
}

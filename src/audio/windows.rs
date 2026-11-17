//! Windows backend: WASAPI playback into a virtual audio cable.
//!
//! Windows has no user-space "virtual microphone" API: capture endpoints only
//! come from drivers. The cheap, reliable route is a cable driver such as
//! VB-CABLE: we play the denoised PCM into its playback side ("CABLE Input")
//! and every application can record from its capture side ("CABLE Output").
//!
//! If no cable is installed the daemon falls back to the default playback
//! device, so you can at least hear that the chain works.

use std::collections::VecDeque;
use std::sync::atomic::{AtomicU32, Ordering};
use std::sync::{Arc, Mutex, RwLock};
use std::thread;
use std::time::Duration;

use cpal::traits::{DeviceTrait, HostTrait, StreamTrait};
use cpal::{BufferSize, FromSample, Sample, SampleFormat, SizedSample, Stream, StreamConfig};

use super::Sink;
use crate::config::Settings;
use crate::denoise::Live;

/// One second of audio; beyond that we drop the oldest samples so latency to
/// the virtual microphone cannot grow without bound.
const MAX_QUEUED: usize = 48_000;

struct State {
    queue: Mutex<VecDeque<f32>>,
    /// `f32::to_bits` of the live gain (volume / 100).
    gain: AtomicU32,
}

impl State {
    fn new() -> Self {
        Self {
            queue: Mutex::new(VecDeque::with_capacity(MAX_QUEUED)),
            gain: AtomicU32::new(1.0f32.to_bits()),
        }
    }

    fn queue(&self) -> std::sync::MutexGuard<'_, VecDeque<f32>> {
        self.queue.lock().unwrap_or_else(|poisoned| poisoned.into_inner())
    }

    fn gain(&self) -> f32 {
        f32::from_bits(self.gain.load(Ordering::Relaxed))
    }

    fn set_gain(&self, gain: f32) {
        self.gain.store(gain.to_bits(), Ordering::Relaxed);
    }
}

fn device_name(device: &cpal::Device) -> String {
    device
        .description()
        .map(|description| description.name().to_string())
        .unwrap_or_else(|_| "unknown device".to_string())
}

pub fn output_names() -> Vec<String> {
    let host = cpal::default_host();
    match host.output_devices() {
        Ok(devices) => devices.map(|device| device_name(&device)).collect(),
        Err(_) => Vec::new(),
    }
}

pub fn input_names() -> Vec<String> {
    let host = cpal::default_host();
    match host.input_devices() {
        Ok(devices) => devices.map(|device| device_name(&device)).collect(),
        Err(_) => Vec::new(),
    }
}

/// Does an output endpoint whose name contains `needle` exist (or, when
/// `needle` is empty, any output endpoint at all)?
pub fn device_present(needle: &str) -> bool {
    let host = cpal::default_host();
    if needle.trim().is_empty() {
        return host.default_output_device().is_some();
    }
    let needle = needle.to_lowercase();
    output_names().iter().any(|name| name.to_lowercase().contains(&needle))
}

fn pick_output(host: &cpal::Host, wanted: &str) -> Result<(cpal::Device, String, bool), String> {
    let wanted_lower = wanted.trim().to_lowercase();
    if !wanted_lower.is_empty() {
        if let Ok(devices) = host.output_devices() {
            for device in devices {
                let name = device_name(&device);
                if name.to_lowercase().contains(&wanted_lower) {
                    return Ok((device, name, true));
                }
            }
        }
    }
    match host.default_output_device() {
        Some(device) => {
            let name = device_name(&device);
            Ok((device, name, false))
        }
        None => Err("no audio output device found".into()),
    }
}

struct CpalSink {
    /// Keep the stream alive for the lifetime of the sink.
    _stream: Stream,
    state: Arc<State>,
    error: Arc<Mutex<Option<String>>>,
    live: Arc<RwLock<Live>>,
    description: String,
}

impl Sink for CpalSink {
    fn write(&mut self, samples: &[i16]) -> std::io::Result<()> {
        if let Some(error) = self
            .error
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
            .clone()
        {
            return Err(std::io::Error::new(std::io::ErrorKind::BrokenPipe, error));
        }
        if let Ok(live) = self.live.read() {
            self.state.set_gain(live.mic_volume as f32 / 100.0);
        }
        let mut queue = self.state.queue();
        for &sample in samples {
            queue.push_back(sample as f32 / 32768.0);
        }
        while queue.len() > MAX_QUEUED {
            queue.pop_front();
        }
        Ok(())
    }

    fn describe(&self) -> String {
        self.description.clone()
    }
}

fn build_output<T>(
    device: &cpal::Device,
    config: StreamConfig,
    state: Arc<State>,
    error: Arc<Mutex<Option<String>>>,
) -> Result<Stream, cpal::Error>
where
    T: SizedSample + FromSample<f32>,
{
    let channels = config.channels as usize;
    device.build_output_stream(
        config,
        move |output: &mut [T], _: &cpal::OutputCallbackInfo| {
            let gain = state.gain();
            let mut queue = state.queue();
            for frame in output.chunks_mut(channels) {
                let value = queue.pop_front().unwrap_or(0.0) * gain;
                let sample = T::from_sample(value.clamp(-1.0, 1.0));
                for slot in frame.iter_mut() {
                    *slot = sample;
                }
            }
        },
        move |err| {
            let mut slot = error.lock().unwrap_or_else(|poisoned| poisoned.into_inner());
            *slot = Some(err.to_string());
        },
        None,
    )
}

pub fn open(settings: &Settings, live: &Arc<RwLock<Live>>) -> Result<Box<dyn Sink>, String> {
    let host = cpal::default_host();
    let (device, name, matched) = pick_output(&host, &settings.device)?;
    if !matched {
        crate::logging::log(&format!(
            "no output device matching \"{}\", using \"{name}\" (install a virtual cable for a real microphone)",
            settings.device
        ));
    }
    let supported = device
        .default_output_config()
        .map_err(|error| format!("cannot query \"{name}\": {error}"))?;
    let sample_format = supported.sample_format();
    let config = StreamConfig {
        channels: 1,
        sample_rate: cpal::SAMPLE_RATE_48K,
        buffer_size: BufferSize::Default,
    };
    let state = Arc::new(State::new());
    let error: Arc<Mutex<Option<String>>> = Arc::new(Mutex::new(None));
    let stream = match sample_format {
        SampleFormat::F32 => build_output::<f32>(&device, config, state.clone(), error.clone()),
        SampleFormat::I16 => build_output::<i16>(&device, config, state.clone(), error.clone()),
        SampleFormat::U16 => build_output::<u16>(&device, config, state.clone(), error.clone()),
        SampleFormat::F64 => build_output::<f64>(&device, config, state.clone(), error.clone()),
        other => Err(cpal::Error::with_message(
            cpal::ErrorKind::UnsupportedConfig,
            format!("unsupported sample format {other:?} on \"{name}\""),
        )),
    }
    .map_err(|error| format!("cannot open \"{name}\": {error}"))?;
    stream
        .play()
        .map_err(|error| format!("cannot start \"{name}\": {error}"))?;
    crate::logging::log(&format!("audio output: {name} ({sample_format:?})"));
    Ok(Box::new(CpalSink {
        _stream: stream,
        state,
        error,
        live: live.clone(),
        description: format!("WASAPI -> {name}"),
    }))
}

// ---------------------------------------------------------------- test tone

/// Record ~2 s from the cable's capture side and play it back on the default
/// output, so the user can check the phone actually picks up sound.
pub fn loopback_test() {
    thread::spawn(|| {
        if let Err(error) = run_loopback() {
            crate::logging::log(&format!("test mic failed: {error}"));
        }
    });
}

fn run_loopback() -> Result<(), String> {
    let host = cpal::default_host();
    let wanted = "CABLE Output".to_lowercase();
    let mut input = None;
    if let Ok(devices) = host.input_devices() {
        for device in devices {
            let name = device_name(&device);
            if name.to_lowercase().contains(&wanted) {
                input = Some(device);
                break;
            }
        }
    }
    let input = match input.or_else(|| host.default_input_device()) {
        Some(device) => device,
        None => return Err("no recording device".into()),
    };
    let supported = input
        .default_input_config()
        .map_err(|error| format!("cannot query \"{}\": {error}", device_name(&input)))?;
    let sample_rate = supported.sample_rate();
    let channels = supported.channels();
    let config: StreamConfig = supported.clone().into();
    let recorded: Arc<Mutex<Vec<f32>>> = Arc::new(Mutex::new(Vec::new()));
    let error: Arc<Mutex<Option<String>>> = Arc::new(Mutex::new(None));
    let stream = match supported.sample_format() {
        SampleFormat::F32 => build_input::<f32>(&input, config, recorded.clone(), error.clone()),
        SampleFormat::I16 => build_input::<i16>(&input, config, recorded.clone(), error.clone()),
        SampleFormat::U16 => build_input::<u16>(&input, config, recorded.clone(), error.clone()),
        other => Err(format!("unsupported recording format {other:?}")),
    }?;
    stream.play().map_err(|error| error.to_string())?;
    thread::sleep(Duration::from_millis(2000));
    drop(stream);
    let frames = recorded
        .lock()
        .unwrap_or_else(|poisoned| poisoned.into_inner())
        .clone();
    if frames.is_empty() {
        return Err("nothing was recorded".into());
    }
    let output = host
        .default_output_device()
        .ok_or_else(|| "no playback device".to_string())?;
    let out_supported = output
        .default_output_config()
        .map_err(|error| error.to_string())?;
    let out_config: StreamConfig = out_supported.clone().into();
    let playback = match out_supported.sample_format() {
        SampleFormat::F32 => build_playback::<f32>(&output, out_config, frames.clone(), channels as usize),
        SampleFormat::I16 => build_playback::<i16>(&output, out_config, frames.clone(), channels as usize),
        SampleFormat::U16 => build_playback::<u16>(&output, out_config, frames.clone(), channels as usize),
        other => Err(format!("unsupported playback format {other:?}")),
    }?;
    playback.play().map_err(|error| error.to_string())?;
    let seconds = frames.len() as f32 / channels as f32 / sample_rate as f32;
    thread::sleep(Duration::from_secs_f32(seconds + 0.4));
    Ok(())
}

fn build_input<T>(
    device: &cpal::Device,
    config: StreamConfig,
    recorded: Arc<Mutex<Vec<f32>>>,
    error: Arc<Mutex<Option<String>>>,
) -> Result<Stream, String>
where
    T: SizedSample,
    f32: FromSample<T>,
{
    device
        .build_input_stream(
            config,
            move |data: &[T], _: &cpal::InputCallbackInfo| {
                let mut target = recorded
                    .lock()
                    .unwrap_or_else(|poisoned| poisoned.into_inner());
                target.extend(data.iter().map(|&sample| f32::from_sample(sample)));
            },
            move |err| {
                let mut slot = error.lock().unwrap_or_else(|poisoned| poisoned.into_inner());
                *slot = Some(err.to_string());
            },
            None,
        )
        .map_err(|error| error.to_string())
}

fn build_playback<T>(
    device: &cpal::Device,
    config: StreamConfig,
    source: Vec<f32>,
    source_channels: usize,
) -> Result<Stream, String>
where
    T: SizedSample + FromSample<f32>,
{
    let out_channels = config.channels as usize;
    let mut position = 0usize;
    let error: Arc<Mutex<Option<String>>> = Arc::new(Mutex::new(None));
    device
        .build_output_stream(
            config,
            move |output: &mut [T], _: &cpal::OutputCallbackInfo| {
                for frame in output.chunks_mut(out_channels) {
                    let base = position * source_channels;
                    position += 1;
                    for (channel, slot) in frame.iter_mut().enumerate() {
                        let index = base + channel.min(source_channels.saturating_sub(1));
                        let value = source.get(index).copied().unwrap_or(0.0);
                        *slot = T::from_sample(value.clamp(-1.0, 1.0));
                    }
                }
            },
            move |err| {
                let mut slot = error.lock().unwrap_or_else(|poisoned| poisoned.into_inner());
                *slot = Some(err.to_string());
            },
            None,
        )
        .map_err(|error| error.to_string())
}

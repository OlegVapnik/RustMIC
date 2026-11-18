//! PCM mixer in the daemon: microphone voice + an external translator.
//!
//! The translator talks to two local byte streams, both carrying raw
//! s16le 48 kHz mono PCM (the same format the WO Mic client produces):
//!
//! * `RustMIC-voice`  — the daemon publishes denoised voice here (read it);
//! * `RustMIC-translate` — the translator writes synthesised speech here.
//!
//! `publish_voice()` is called in the audio pump **before** `process()` mixes
//! the translation in, so the translator never hears its own output. The queue
//! is bounded to one second and drops the oldest samples on overflow; an empty
//! queue produces digital silence instead of repeating the last chunk.
//!
//! Linux uses Unix sockets in the cache dir (mode 0600); Windows uses named
//! pipes created with `PIPE_REJECT_REMOTE_CLIENTS` and a DACL that grants
//! access to the current user only. No cloud service is baked in: any
//! STT/translation/TTS pipeline that speaks this tiny protocol can be plugged in.

use std::collections::VecDeque;
#[cfg(unix)]
use std::fs;
use std::io::{self, Read, Write};
use std::sync::atomic::{AtomicBool, AtomicU32, AtomicU64, AtomicUsize, Ordering};
use std::sync::mpsc::{self, SyncSender};
use std::sync::{Mutex, OnceLock, RwLock};
use std::thread;
use std::time::Duration;

#[cfg(unix)]
use interprocess::local_socket::GenericFilePath;
#[cfg(windows)]
use interprocess::local_socket::GenericNamespaced;
use interprocess::local_socket::prelude::*;
#[cfg(windows)]
use interprocess::os::windows::local_socket::ListenerOptionsExt;
#[cfg(windows)]
use interprocess::os::windows::security_descriptor::SecurityDescriptor;
#[cfg(windows)]
use widestring::U16CString;
use interprocess::local_socket::{Listener, ListenerOptions, Stream};
use serde::{Deserialize, Serialize};

#[cfg(unix)]
use crate::config;
use crate::config::{Settings, TranslateMode};
use crate::logging;
use crate::STOP;

/// One second of buffered translated audio. Anything older is dropped so a
/// stalled translator cannot introduce unbounded latency.
const MAX_BUFFER_SAMPLES: usize = 48_000;
const VOICE_QUEUE_CHUNKS: usize = 16;
const ACTIVE_DB: f32 = -50.0;

#[derive(Clone, Copy, Debug, Default)]
struct MixConfig {
    enabled: bool,
    mode: TranslateMode,
    volume: u32,
    duck_db: f32,
}

static CONFIG: OnceLock<RwLock<MixConfig>> = OnceLock::new();

struct Voice {
    stream: Mutex<Option<Stream>>,
    tx: SyncSender<Vec<u8>>,
}

static VOICE: OnceLock<Voice> = OnceLock::new();

struct Translate {
    samples: Mutex<VecDeque<i16>>,
    clients: AtomicUsize,
    connected: AtomicBool,
    level_bits: AtomicU32,
    active: AtomicBool,
    dropped_samples: AtomicU64,
    underruns: AtomicU64,
}

static TRANSLATE: OnceLock<Translate> = OnceLock::new();

/// Status exposed to the GUI and `ctl state`.
#[derive(Clone, Debug, Default, Serialize, Deserialize)]
pub struct TranslateStatus {
    pub enabled: bool,
    pub voice_connected: bool,
    pub translate_connected: bool,
    /// True while synthesised speech is being received.
    pub active: bool,
    /// RMS of the translated input in dBFS (or -100 when silent).
    pub level_db: f32,
    pub mode: TranslateMode,
    pub volume: u32,
    pub from: String,
    pub to: String,
    /// How much translated audio is buffered right now (0..=1000 ms).
    pub queued_ms: u32,
    /// Samples discarded because the translator got ahead of the microphone.
    pub dropped_samples: u64,
    /// Samples replaced with silence because the translator was behind.
    pub underruns: u64,
}

fn config() -> &'static RwLock<MixConfig> {
    CONFIG.get_or_init(|| RwLock::new(MixConfig::default()))
}

fn translate() -> &'static Translate {
    TRANSLATE.get_or_init(|| Translate {
        samples: Mutex::new(VecDeque::with_capacity(MAX_BUFFER_SAMPLES)),
        clients: AtomicUsize::new(0),
        connected: AtomicBool::new(false),
        level_bits: AtomicU32::new(f32::NEG_INFINITY.to_bits()),
        active: AtomicBool::new(false),
        dropped_samples: AtomicU64::new(0),
        underruns: AtomicU64::new(0),
    })
}

/// Called at startup and whenever settings change.
pub fn configure(settings: &Settings) {
    let next = MixConfig {
        enabled: settings.translate_enabled,
        mode: settings.translate_mode,
        volume: settings.translate_volume.min(100),
        duck_db: settings.ducking_db,
    };
    if let Ok(mut current) = config().write() {
        *current = next;
    }
}

#[cfg(unix)]
fn voice_file() -> std::path::PathBuf {
    config::cache_dir().join("RustMIC-voice.sock")
}

#[cfg(unix)]
fn translate_file() -> std::path::PathBuf {
    config::cache_dir().join("RustMIC-translate.sock")
}

#[cfg(unix)]
fn make_listener(path: &std::path::Path) -> io::Result<Listener> {
    if let Some(dir) = path.parent() {
        fs::create_dir_all(dir)?;
    }
    let name = path.as_os_str().to_fs_name::<GenericFilePath>()?;
    let listener = ListenerOptions::new()
        .name(name)
        .try_overwrite(true)
        .create_sync()?;
    use std::os::unix::fs::PermissionsExt;
    let _ = fs::set_permissions(path, fs::Permissions::from_mode(0o600));
    Ok(listener)
}

#[cfg(windows)]
fn current_user_security_descriptor() -> io::Result<SecurityDescriptor> {
    use windows_sys::Win32::Foundation::{CloseHandle, LocalFree, HANDLE};
    use windows_sys::Win32::Security::Authorization::ConvertSidToStringSidW;
    use windows_sys::Win32::Security::{GetTokenInformation, TokenUser, TOKEN_QUERY, TOKEN_USER};
    use windows_sys::Win32::System::Threading::{GetCurrentProcess, OpenProcessToken};

    unsafe {
        let mut token: HANDLE = std::ptr::null_mut();
        if OpenProcessToken(GetCurrentProcess(), TOKEN_QUERY, &mut token) == 0 {
            return Err(io::Error::last_os_error());
        }
        let mut size = 0u32;
        let _ = GetTokenInformation(token, TokenUser, std::ptr::null_mut(), 0, &mut size);
        let mut buffer = vec![0u8; size as usize];
        if GetTokenInformation(token, TokenUser, buffer.as_mut_ptr().cast(), size, &mut size) == 0 {
            CloseHandle(token);
            return Err(io::Error::last_os_error());
        }
        CloseHandle(token);

        let user = &*(buffer.as_ptr() as *const TOKEN_USER);
        let mut sid: *mut u16 = std::ptr::null_mut();
        if ConvertSidToStringSidW(user.User.Sid, &mut sid) == 0 {
            return Err(io::Error::last_os_error());
        }
        let mut len = 0usize;
        while *sid.add(len) != 0 {
            len += 1;
        }
        let sid_text = String::from_utf16_lossy(std::slice::from_raw_parts(sid, len));
        LocalFree(sid.cast());

        let sddl = format!("O:{sid_text}G:{sid_text}D:P(A;;GA;;;{sid_text})");
        let wide = U16CString::from_str(&sddl)
            .map_err(|error| io::Error::new(io::ErrorKind::InvalidInput, error.to_string()))?;
        SecurityDescriptor::deserialize(wide.as_ucstr())
    }
}

#[cfg(windows)]
fn make_listener(pipe: &str) -> io::Result<Listener> {
    let name = pipe.to_ns_name::<GenericNamespaced>()?;
    // ListenerOptions defaults to accept_remote = false, which makes
    // interprocess pass PIPE_REJECT_REMOTE_CLIENTS to CreateNamedPipeW.
    ListenerOptions::new()
        .name(name)
        .security_descriptor(current_user_security_descriptor()?)
        .create_sync()
}

/// Human-readable endpoints for the settings tab and README.
pub fn endpoints() -> (String, String) {
    #[cfg(unix)]
    {
        (
            voice_file().display().to_string(),
            translate_file().display().to_string(),
        )
    }
    #[cfg(windows)]
    {
        (
            r"\\.\pipe\RustMIC-voice".to_string(),
            r"\\.\pipe\RustMIC-translate".to_string(),
        )
    }
}

/// Start both local stream servers. Safe to call once, at daemon startup.
pub fn start_servers() {
    #[cfg(unix)]
    let voice_listener = match make_listener(&voice_file()) {
        Ok(listener) => listener,
        Err(error) => {
            logging::log(&format!("mixer: voice socket failed: {error}"));
            return;
        }
    };
    #[cfg(windows)]
    let voice_listener = match make_listener("RustMIC-voice") {
        Ok(listener) => listener,
        Err(error) => {
            logging::log(&format!("mixer: voice pipe failed: {error}"));
            return;
        }
    };

    let (tx, rx) = mpsc::sync_channel::<Vec<u8>>(VOICE_QUEUE_CHUNKS);
    let voice = Voice {
        stream: Mutex::new(None),
        tx,
    };
    let _ = VOICE.set(voice);
    let voice = match VOICE.get() {
        Some(voice) => voice,
        None => return,
    };

    // Voice publisher: never block the audio pump on a slow translator.
    let _ = thread::Builder::new()
        .name("RustMIC-voice-pub".to_string())
        .spawn(move || {
            while !STOP.load(Ordering::SeqCst) {
                let bytes = match rx.recv_timeout(Duration::from_millis(100)) {
                    Ok(bytes) => bytes,
                    Err(mpsc::RecvTimeoutError::Timeout) => continue,
                    Err(mpsc::RecvTimeoutError::Disconnected) => break,
                };
                let mut stream = match voice.stream.lock() {
                    Ok(stream) => stream,
                    Err(poisoned) => poisoned.into_inner(),
                };
                if let Some(client) = stream.as_mut() {
                    if let Err(error) = client.write_all(&bytes) {
                        logging::log(&format!("mixer: voice publish failed: {error}"));
                        *stream = None;
                    }
                }
            }
        });

    // One translator at a time; a new connection replaces the previous one.
    let _ = thread::Builder::new()
        .name("RustMIC-voice-accept".to_string())
        .spawn(move || {
            for connection in voice_listener.incoming() {
                match connection {
                    Ok(stream) => {
                        let mut current = match voice.stream.lock() {
                            Ok(current) => current,
                            Err(poisoned) => poisoned.into_inner(),
                        };
                        logging::log("mixer: translator connected to voice stream");
                        *current = Some(stream);
                    }
                    Err(error) => {
                        logging::log(&format!("mixer: voice accept failed: {error}"));
                        thread::sleep(Duration::from_millis(200));
                    }
                }
            }
        });

    #[cfg(unix)]
    let translate_listener = match make_listener(&translate_file()) {
        Ok(listener) => listener,
        Err(error) => {
            logging::log(&format!("mixer: translate socket failed: {error}"));
            return;
        }
    };
    #[cfg(windows)]
    let translate_listener = match make_listener("RustMIC-translate") {
        Ok(listener) => listener,
        Err(error) => {
            logging::log(&format!("mixer: translate pipe failed: {error}"));
            return;
        }
    };

    let _ = thread::Builder::new()
        .name("RustMIC-translate-accept".to_string())
        .spawn(move || {
            let state = translate();
            for connection in translate_listener.incoming() {
                match connection {
                    Ok(stream) => {
                        logging::log("mixer: translator connected to translate stream");
                        state.clients.fetch_add(1, Ordering::SeqCst);
                        state.connected.store(true, Ordering::SeqCst);
                        thread::spawn(move || read_translated(stream));
                    }
                    Err(error) => {
                        logging::log(&format!("mixer: translate accept failed: {error}"));
                        thread::sleep(Duration::from_millis(200));
                    }
                }
            }
        });
}

fn read_translated(mut stream: Stream) {
    let state = translate();
    let mut buffer = [0u8; 9600]; // 100 ms of 48 kHz mono s16le
    let mut carry: Option<u8> = None;
    loop {
        let count = match stream.read(&mut buffer) {
            Ok(0) => break,
            Ok(count) => count,
            Err(error) => {
                logging::log(&format!("mixer: translate read failed: {error}"));
                break;
            }
        };
        let mut samples = match state.samples.lock() {
            Ok(samples) => samples,
            Err(poisoned) => poisoned.into_inner(),
        };
        let mut bytes: &[u8] = &buffer[..count];
        if let Some(first) = carry.take() {
            if let Some((&low, rest)) = bytes.split_first() {
                samples.push_back(i16::from_le_bytes([first, low]));
                bytes = rest;
            }
        }
        if bytes.len() % 2 == 1 {
            carry = bytes.last().copied();
        }
        for pair in bytes.chunks_exact(2) {
            samples.push_back(i16::from_le_bytes([pair[0], pair[1]]));
        }
        let excess = samples.len().saturating_sub(MAX_BUFFER_SAMPLES);
        for _ in 0..excess {
            samples.pop_front();
        }
        if excess > 0 {
            state.dropped_samples.fetch_add(excess as u64, Ordering::Relaxed);
        }
    }
    if state.clients.fetch_sub(1, Ordering::SeqCst) <= 1 {
        state.connected.store(false, Ordering::SeqCst);
        state.active.store(false, Ordering::SeqCst);
    }
    logging::log("mixer: translator disconnected from translate stream");
}

fn take(count: usize) -> Vec<i16> {
    let state = translate();
    let mut queue = match state.samples.lock() {
        Ok(queue) => queue,
        Err(poisoned) => poisoned.into_inner(),
    };
    let mut output = Vec::with_capacity(count);
    let mut silent = 0u64;
    for _ in 0..count {
        match queue.pop_front() {
            Some(sample) => output.push(sample),
            None => {
                // Underrun: insert digital silence, never repeat the last chunk.
                output.push(0);
                silent += 1;
            }
        }
    }
    state.underruns.fetch_add(silent, Ordering::Relaxed);
    let sum: f64 = output.iter().map(|sample| *sample as f64).map(|s| s * s).sum();
    let rms = if output.is_empty() {
        f32::NEG_INFINITY
    } else {
        (sum / output.len() as f64).sqrt() as f32
    };
    let db = if rms <= 0.0 {
        -100.0
    } else {
        // s16 full scale is 32768; report dBFS, not a raw magnitude in dB.
        let normalized = rms / 32768.0;
        (20.0 * normalized.log10()).clamp(-100.0, 0.0)
    };
    state.level_bits.store(db.to_bits(), Ordering::SeqCst);
    state
        .active
        .store(db > ACTIVE_DB, Ordering::SeqCst);
    output
}

/// Publish clean voice to the external translator. While it is speaking we
/// send silence so its output cannot feed back into its own input.
pub fn publish_voice(samples: &[i16]) {
    let enabled = config()
        .read()
        .map(|config| config.enabled)
        .unwrap_or(false);
    let voice = match VOICE.get() {
        Some(voice) if enabled => voice,
        _ => return,
    };
    let mut bytes = Vec::with_capacity(samples.len() * 2);
    let silent = translate().active.load(Ordering::SeqCst);
    for sample in samples {
        let value = if silent { 0i16 } else { *sample };
        bytes.extend_from_slice(&value.to_le_bytes());
    }
    let _ = voice.tx.try_send(bytes);
}

fn gain_from_db(db: f32) -> f32 {
    10f32.powf(-db / 20.0)
}

/// Cubic soft clipper: unity for small signals, approaches +/-1 smoothly,
/// and saturates only beyond full scale. Mix mode uses this instead of a hard
/// clamp so translation on top of voice does not crackle.
fn soft_clip(x: f32) -> f32 {
    // tanh is a smooth limiter: nearly transparent for small signals and
    // asymptotically bounded by +/-1, so summing two full-scale voices never
    // hard-clips into crackling distortion.
    x.tanh()
}

fn mix_sample(voice: i16, translated: i16, gain: f32, duck: f32) -> i16 {
    let voice = voice as f32 * duck / 32768.0;
    let translated = translated as f32 * gain / 32768.0;
    let value = (soft_clip(voice + translated) * 32767.0).round();
    value.clamp(i16::MIN as f32, i16::MAX as f32) as i16
}

/// Mix one frame of voice with whatever translated audio has arrived.
///
/// * `replace`: while translation is active the original voice is ducked by
///   `ducking_db`; between phrases it passes through untouched.
/// * `mix`: the original stays at full level and translation is added on top
///   through the soft clipper.
///
/// Translation disabled is a fast path returning the untouched voice.
pub fn process(voice: &[i16]) -> Vec<i16> {
    let (enabled, mode, volume, duck_db) = match config().read() {
        Ok(config) => (config.enabled, config.mode, config.volume, config.duck_db),
        Err(poisoned) => {
            let config = poisoned.into_inner();
            (config.enabled, config.mode, config.volume, config.duck_db)
        }
    };
    if !enabled {
        return voice.to_vec();
    }
    let translated = take(voice.len());
    let gain = volume as f32 / 100.0;
    let duck = if translate().active.load(Ordering::SeqCst) {
        gain_from_db(duck_db)
    } else {
        1.0
    };
    match mode {
        TranslateMode::Replace => voice
            .iter()
            .zip(translated.iter())
            .map(|(voice, translated)| mix_sample(*voice, *translated, gain, duck))
            .collect(),
        TranslateMode::Mix => voice
            .iter()
            .zip(translated.iter())
            .map(|(voice, translated)| mix_sample(*voice, *translated, gain, 1.0))
            .collect(),
    }
}

pub fn status() -> TranslateStatus {
    let (enabled, mode, volume) = match config().read() {
        Ok(config) => (config.enabled, config.mode, config.volume),
        Err(poisoned) => {
            let config = poisoned.into_inner();
            (config.enabled, config.mode, config.volume)
        }
    };
    let settings = Settings::load();
    let state = translate();
    let queued = state
        .samples
        .lock()
        .map(|samples| samples.len())
        .unwrap_or(0);
    let level_db = f32::from_bits(state.level_bits.load(Ordering::SeqCst));
    TranslateStatus {
        enabled,
        voice_connected: VOICE
            .get()
            .and_then(|voice| voice.stream.lock().ok())
            .map(|stream| stream.is_some())
            .unwrap_or(false),
        translate_connected: state.connected.load(Ordering::SeqCst),
        active: state.active.load(Ordering::SeqCst),
        level_db: if level_db.is_finite() { level_db } else { -100.0 },
        mode,
        volume,
        from: settings.translate_from,
        to: settings.translate_to,
        queued_ms: ((queued as u32) * 1000 / MAX_BUFFER_SAMPLES as u32).min(1000),
        dropped_samples: state.dropped_samples.load(Ordering::Relaxed),
        underruns: state.underruns.load(Ordering::Relaxed),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn disabled_mixer_passes_voice_through() {
        let config = config();
        *config.write().unwrap() = MixConfig {
            enabled: false,
            mode: TranslateMode::Replace,
            volume: 100,
            duck_db: 12.0,
        };
        assert_eq!(process(&[1, 2, 3]), vec![1, 2, 3]);
    }

    #[test]
    fn soft_clip_stays_within_full_scale() {
        assert!((soft_clip(0.25) - 0.25).abs() < 0.02);
        assert!(soft_clip(1.0) > 0.7 && soft_clip(1.0) < 1.0);
        assert!(soft_clip(2.0) <= 1.0);
        assert!(soft_clip(-2.0) >= -1.0);
        let peak = mix_sample(i16::MAX, i16::MAX, 1.0, 1.0);
        let trough = mix_sample(i16::MIN, i16::MIN, 1.0, 1.0);
        assert!(peak > 30000 && peak <= i16::MAX);
        assert!(trough < -30000 && trough >= i16::MIN);
    }

    #[test]
    fn duck_gain_matches_decibels() {
        assert!((gain_from_db(0.0) - 1.0).abs() < 0.0001);
        assert!((gain_from_db(12.0) - 0.2512).abs() < 0.001);
    }

    #[test]
    fn status_defaults_are_offline() {
        let status = status();
        assert!(!status.voice_connected);
        assert!(!status.translate_connected);
        assert!(status.level_db <= -100.0);
    }
}

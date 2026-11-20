//! Settings file and per-platform paths.
//!
//! Linux:   `~/.config/RustMIC/config.toml`, `~/.cache/RustMIC/`
//! Windows: `%APPDATA%\RustMIC\config.toml`, `%LOCALAPPDATA%\RustMIC\`

use std::env;
use std::fs;
use std::io;
use std::path::PathBuf;

use serde::{Deserialize, Serialize};

/// Bump whenever the meaning of a key changes. Older files are upgraded on load.
pub const CONFIG_VERSION: u32 = 7;

/// PipeWire null sink fed by `pacat` (Linux).
pub const SINK: &str = "rustmic_sink";
/// Remapped source apps record from (Linux).
pub const SRC: &str = "rustmic_mic";
/// Separate null sink for music; mixed into `SINK` and mirrored to speakers.
pub const MUSIC_SINK: &str = "rustmic_music";
/// Transports understood by the bundled `rustmic` client.
pub const TRANSPORTS: &[&str] = &["usb", "wifi", "wifi-direct", "bluetooth"];
/// Where the audio fed into the virtual microphone comes from: the phone or
/// everything the computer plays (browser tabs, players, calls).
pub const SOURCES: &[&str] = &["phone", "system"];
/// Default substring of the Windows playback endpoint that feeds the cable.
pub const DEFAULT_DEVICE: &str = "CABLE Input";
/// Name of the scheduled task / autostart entry.
pub const SERVICE: &str = "RustMIC";

/// How the noise gate decides whether the microphone is open.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum GateMode {
    /// RNNoise voice-activity probability (0..100 %, legacy `gate`).
    #[default]
    Voice,
    /// Output RMS in dBFS with 3 dB hysteresis.
    Level,
}

impl GateMode {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Voice => "voice",
            Self::Level => "level",
        }
    }

    pub fn parse(value: &str) -> Self {
        match value.trim().trim_matches('"').to_lowercase().as_str() {
            "level" | "db" | "db_level" => Self::Level,
            _ => Self::Voice,
        }
    }
}

#[derive(Clone, Debug)]
pub struct Settings {
    /// Schema version read from `config.toml`.
    pub config_version: u32,
    /// `phone` (WO Mic capture) or `system` (loopback of the default output).
    pub source: String,
    /// `usb` (adb), `wifi` / `wifi-direct` (phone IP) or `bluetooth`.
    pub transport: String,
    /// IP of the phone running the WO Mic server (Wi-Fi transports).
    pub host: String,
    /// Bluetooth address of the phone (bluetooth transport).
    pub bt_address: String,
    /// Path to `adb` (or just "adb" when it is on PATH).
    pub adb: String,
    /// Windows: playback device that feeds the virtual cable; ignored on Linux.
    pub device: String,
    pub denoise: bool,
    /// Voice-activity mode or RMS level mode.
    pub gate_mode: GateMode,
    /// VAD threshold 0..100 %. 0 disables the voice gate.
    pub gate_voice: u32,
    /// RMS threshold in dBFS, -60..0. 0 disables the level gate.
    pub gate_level_db: f32,
    /// Microphone gain 0..100 %.
    pub mic_volume: u32,
    /// Music-to-mic gain 0..100 %.
    pub music_volume: u32,
    /// Monitor gain: what you hear in the speakers, 0..100 %.
    ///
    /// Independent of `music_volume` so the stream keeps playing at full
    /// level while you turn the local playback down or mute it.
    pub monitor_volume: u32,
    /// Real-time loudness normalisation for music (mpv lavfi/loudnorm).
    pub loudnorm: bool,
    /// Lower the music while the voice gate is open.
    pub ducking: bool,
    /// How many dB the music is lowered when ducking is active.
    pub ducking_db: f32,
    /// Mirror music to the default output (Linux only).
    pub monitor_music: bool,
}

impl Default for Settings {
    /// Defaults only; `load()` additionally reads `config.toml`.
    fn default() -> Self {
        Self {
            config_version: CONFIG_VERSION,
            source: "phone".to_string(),
            transport: "usb".to_string(),
            host: String::new(),
            bt_address: String::new(),
            adb: default_adb(),
            device: DEFAULT_DEVICE.to_string(),
            denoise: true,
            gate_mode: GateMode::Voice,
            gate_voice: 0,
            gate_level_db: -45.0,
            mic_volume: 100,
            music_volume: 100,
            monitor_volume: 100,
            loudnorm: false,
            ducking: true,
            ducking_db: 12.0,
            monitor_music: true,
        }
    }
}

impl Settings {
    /// Read the file, upgrade an old schema and write it back once.
    ///
    /// Unknown keys (including settings from removed features) are ignored by
    /// `apply()`; upgrading rewrites the file without them.
    pub fn load() -> Self {
        let path = settings_path();
        let mut settings = Self::default();
        if let Ok(text) = fs::read_to_string(&path) {
            settings.apply(&text);
        }
        let needs_migration = settings.config_version < CONFIG_VERSION;
        settings.config_version = CONFIG_VERSION;
        if needs_migration && path.exists() {
            let _ = settings.save();
        }
        settings
    }

    /// Parse the settings file, ignoring comments and unknown keys.
    fn apply(&mut self, text: &str) {
        let mut legacy_gate: Option<f32> = None;
        let mut legacy_volume: Option<u32> = None;
        let mut gate_voice_set = false;

        for line in text.lines() {
            let line = line.split('#').next().unwrap_or("").trim();
            let Some((key, value)) = line.split_once('=') else {
                continue;
            };
            let value = value.trim().trim_matches('"');
            match key.trim() {
                "config_version" => {
                    self.config_version = value.parse::<u32>().unwrap_or(0);
                }
                "source" => self.source = normalize_source(value),
                "transport" => self.transport = normalize_transport(value),
                "host" => {
                    if !value.is_empty() {
                        self.host = value.to_string();
                    }
                }
                "bt_address" | "bt-address" => {
                    if !value.is_empty() {
                        self.bt_address = value.to_string();
                    }
                }
                "denoise" => self.denoise = parse_bool(value, true),
                "gate_mode" => self.gate_mode = GateMode::parse(value),
                "gate_voice" => {
                    self.gate_voice = parse_percent(value);
                    gate_voice_set = true;
                }
                "gate_level_db" | "gate_level" => {
                    self.gate_level_db = parse_db(value);
                }
                // v1 key: probability 0..1, now expressed as 0..100 %.
                "gate" => legacy_gate = value.parse::<f32>().ok(),
                "mic_volume" => self.mic_volume = parse_percent(value),
                // v1 key.
                "volume" => legacy_volume = value.parse::<u32>().ok(),
                "music_volume" => self.music_volume = parse_percent(value),
                "monitor_volume" => self.monitor_volume = parse_percent(value),
                "loudnorm" => self.loudnorm = parse_bool(value, false),
                "ducking" => self.ducking = parse_bool(value, true),
                "ducking_db" => self.ducking_db = parse_db_amount(value),
                "monitor_music" => self.monitor_music = parse_bool(value, true),
                "device" => {
                    if !value.is_empty() {
                        self.device = value.to_string();
                    }
                }
                "adb" if !value.is_empty() => self.adb = value.to_string(),
                _ => {}
            }
        }

        // v1 `gate = 0.46` is the same VAD threshold as 46 %. It is not
        // converted to dB - only the display unit changed.
        if !gate_voice_set {
            if let Some(gate) = legacy_gate {
                self.gate_voice = if gate > 1.0 {
                    gate.clamp(0.0, 100.0).round() as u32
                } else {
                    (gate.clamp(0.0, 1.0) * 100.0).round() as u32
                };
                self.gate_mode = GateMode::Voice;
            }
        }
        if let Some(volume) = legacy_volume {
            self.mic_volume = volume.min(100);
        }
    }

    pub fn save(&self) -> io::Result<()> {
        let path = settings_path();
        if let Some(dir) = path.parent() {
            fs::create_dir_all(dir)?;
        }
        let mut text = String::from("# RustMIC settings (managed by RustMIC-gui)\n");
        text.push_str(&format!("config_version = {CONFIG_VERSION}\n"));
        text.push_str(&format!("source = \"{}\"\n", self.source));
        text.push_str(&format!("transport = \"{}\"\n", self.transport));
        text.push_str(&format!("host = \"{}\"\n", self.host));
        text.push_str(&format!("bt_address = \"{}\"\n", self.bt_address));
        text.push_str(&format!("denoise = {}\n", self.denoise));
        text.push_str(&format!("gate_mode = \"{}\"\n", self.gate_mode.as_str()));
        text.push_str(&format!("gate_voice = {}\n", self.gate_voice));
        text.push_str(&format!("gate_level_db = {:.1}\n", self.gate_level_db));
        text.push_str(&format!("mic_volume = {}\n", self.mic_volume));
        text.push_str(&format!("music_volume = {}\n", self.music_volume));
        text.push_str(&format!("monitor_volume = {}\n", self.monitor_volume));
        text.push_str(&format!("loudnorm = {}\n", self.loudnorm));
        text.push_str(&format!("ducking = {}\n", self.ducking));
        text.push_str(&format!("ducking_db = {:.1}\n", self.ducking_db));
        text.push_str(&format!("monitor_music = {}\n", self.monitor_music));
        if cfg!(windows) {
            text.push_str(&format!("device = \"{}\"\n", self.device));
        }
        fs::write(path, text)
    }
}

fn parse_bool(value: &str, default: bool) -> bool {
    match value.trim().to_lowercase().as_str() {
        "true" | "1" | "yes" | "on" => true,
        "false" | "0" | "no" | "off" => false,
        _ => default,
    }
}

fn parse_percent(value: &str) -> u32 {
    value
        .trim()
        .trim_end_matches('%')
        .parse::<f32>()
        .map(|number| number.clamp(0.0, 100.0).round() as u32)
        .unwrap_or(100)
}

fn parse_db(value: &str) -> f32 {
    value
        .trim()
        .trim_end_matches("dB")
        .trim_end_matches("db")
        .parse::<f32>()
        .map(|number| number.clamp(-60.0, 0.0))
        .unwrap_or(-45.0)
}

fn parse_db_amount(value: &str) -> f32 {
    value
        .trim()
        .trim_end_matches("dB")
        .trim_end_matches("db")
        .parse::<f32>()
        .map(|number| number.clamp(0.0, 30.0))
        .unwrap_or(12.0)
}

/// Lowercase transport name, with common aliases folded in.
/// `source` accepts `phone` and `system` (alias `loopback`).
pub fn normalize_source(value: &str) -> String {
    match value.trim().trim_matches('"').to_lowercase().as_str() {
        "system" | "loopback" | "pc" | "desktop" => "system".to_string(),
        _ => "phone".to_string(),
    }
}

pub fn normalize_transport(value: &str) -> String {
    let value = value.trim().trim_matches('"').to_lowercase();
    match value.as_str() {
        "wireless" | "wi-fi" | "wifi" => "wifi".to_string(),
        "wifi-direct" | "wifidirect" | "p2p" => "wifi-direct".to_string(),
        "bt" | "bluetooth" => "bluetooth".to_string(),
        _ if TRANSPORTS.contains(&value.as_str()) => value,
        _ => "usb".to_string(),
    }
}

/// Legacy helper kept for callers that only need the microphone percentage.
pub fn parse_volume(text: &str) -> u32 {
    text.lines()
        .find_map(|line| {
            let line = line.split('#').next().unwrap_or("").trim();
            let (key, value) = line.split_once('=')?;
            (key.trim() == "mic_volume" || key.trim() == "volume")
                .then(|| value.trim().parse::<u32>().ok())
                .flatten()
        })
        .unwrap_or(100)
        .clamp(0, 100)
}

fn env_path(key: &str) -> Option<PathBuf> {
    env::var_os(key).map(PathBuf::from)
}

pub fn home() -> PathBuf {
    if cfg!(windows) {
        env_path("USERPROFILE").unwrap_or_else(|| PathBuf::from(r"C:\"))
    } else {
        env_path("HOME").unwrap_or_else(|| PathBuf::from("/"))
    }
}

pub fn config_dir() -> PathBuf {
    if cfg!(windows) {
        env_path("APPDATA")
            .unwrap_or_else(|| home().join(r"AppData\Roaming"))
            .join("RustMIC")
    } else {
        env_path("XDG_CONFIG_HOME")
            .unwrap_or_else(|| home().join(".config"))
            .join("RustMIC")
    }
}

/// Directory containing the running executable (portable payload lives here).
pub fn program_dir() -> PathBuf {
    env::current_exe()
        .ok()
        .and_then(|path| path.parent().map(PathBuf::from))
        .unwrap_or_else(|| PathBuf::from("."))
}

/// Client executable. The portable Windows bundle ships it as
/// `RustMIC-client.exe` (plain `rustmic.exe` would collide with our own
/// `RustMIC.exe` on a case-insensitive filesystem); elsewhere the upstream
/// name `rustmic` is kept.
pub fn client_program() -> String {
    if cfg!(windows) {
        let bundled = program_dir().join("RustMIC-client.exe");
        if bundled.is_file() {
            return bundled.to_string_lossy().into_owned();
        }
        "rustmic.exe".to_string()
    } else {
        "rustmic".to_string()
    }
}

pub fn cache_dir() -> PathBuf {
    if cfg!(windows) {
        env_path("LOCALAPPDATA")
            .unwrap_or_else(|| home().join(r"AppData\Local"))
            .join("RustMIC")
    } else {
        env_path("XDG_CACHE_HOME")
            .unwrap_or_else(|| home().join(".cache"))
            .join("RustMIC")
    }
}

pub fn settings_path() -> PathBuf {
    config_dir().join("config.toml")
}

pub fn pid_path() -> PathBuf {
    cache_dir().join("RustMIC.pid")
}

pub fn log_path() -> PathBuf {
    cache_dir().join("RustMIC.log")
}

pub fn level_path() -> PathBuf {
    cache_dir().join("RustMIC.level")
}

/// Local control socket. Kept in the user's runtime dir on Linux; the Windows
/// implementation maps it to a named pipe with the same base name.
pub fn socket_path() -> PathBuf {
    if cfg!(windows) {
        PathBuf::from("RustMIC")
    } else {
        env_path("XDG_RUNTIME_DIR")
            .unwrap_or_else(cache_dir)
            .join("RustMIC.sock")
    }
}

/// Where Android's platform-tools usually live, or plain "adb" from PATH.
pub fn default_adb() -> String {
    let mut candidates: Vec<PathBuf> = Vec::new();
    if cfg!(windows) {
        if let Some(local) = env_path("LOCALAPPDATA") {
            candidates.push(local.join(r"Android\Sdk\platform-tools\adb.exe"));
        }
        candidates.push(home().join(r"AppData\Local\Android\Sdk\platform-tools\adb.exe"));
    } else {
        candidates.push(home().join("Android/Sdk/platform-tools/adb"));
    }
    candidates
        .into_iter()
        .find(|candidate| candidate.exists())
        .map(|candidate| candidate.to_string_lossy().into_owned())
        .unwrap_or_else(|| "adb".to_string())
}

#[cfg(test)]
mod tests {
    use super::{normalize_transport, parse_volume, GateMode, Settings};

    #[test]
    fn volume_defaults_to_100_for_missing_or_invalid_values() {
        assert_eq!(parse_volume("denoise = true\ngate = 0.2\n"), 100);
        assert_eq!(parse_volume("volume = loud\n"), 100);
    }

    #[test]
    fn volume_is_clamped_to_supported_range() {
        assert_eq!(parse_volume("mic_volume = 0\n"), 0);
        assert_eq!(parse_volume("mic_volume = 78\n"), 78);
        assert_eq!(parse_volume("volume = 200\n"), 100);
    }

    #[test]
    fn transport_aliases_are_normalized() {
        assert_eq!(normalize_transport("Wi-Fi"), "wifi");
        assert_eq!(normalize_transport("wifidirect"), "wifi-direct");
        assert_eq!(normalize_transport("BT"), "bluetooth");
        assert_eq!(normalize_transport("nonsense"), "usb");
    }

    #[test]
    fn settings_parse_transport_and_host() {
        let mut settings = Settings::default();
        settings.apply("transport = wifi\nhost = \"192.168.1.7\"\nbt_address = \"AA:BB\"\n");
        assert_eq!(settings.transport, "wifi");
        assert_eq!(settings.host, "192.168.1.7");
        assert_eq!(settings.bt_address, "AA:BB");
    }

    #[test]
    fn legacy_gate_and_volume_are_upgraded() {
        let mut settings = Settings::default();
        settings.apply("gate = 0.46\nvolume = 78\n");
        assert_eq!(settings.gate_voice, 46);
        assert_eq!(settings.mic_volume, 78);
        assert_eq!(settings.gate_mode, GateMode::Voice);
        assert!((settings.gate_level_db + 45.0).abs() < 0.01);
    }

    #[test]
    fn new_gate_keys_win_over_legacy() {
        let mut settings = Settings::default();
        settings.apply("gate = 0.46\ngate_mode = level\ngate_voice = 70\ngate_level_db = -42\n");
        assert_eq!(settings.gate_mode, GateMode::Level);
        assert_eq!(settings.gate_voice, 70);
        assert!((settings.gate_level_db + 42.0).abs() < 0.01);
    }

    #[test]
    fn unknown_keys_are_ignored() {
        let mut settings = Settings::default();
        settings.apply(
            "obsolete_flag = true\nold_bundle = \"cloud\"\n\
             secret_key = \"secret\"\nold_model = \"gpt\"\n",
        );
        assert_eq!(settings.mic_volume, 100);
        assert_eq!(settings.transport, "usb");
    }
}

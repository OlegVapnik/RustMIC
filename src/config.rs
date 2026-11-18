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
pub const CONFIG_VERSION: u32 = 4;

/// PipeWire null sink fed by `pacat` (Linux).
pub const SINK: &str = "rustmic_sink";
/// Remapped source apps record from (Linux).
pub const SRC: &str = "rustmic_mic";
/// Separate null sink for music; mixed into `SINK` and mirrored to speakers.
pub const MUSIC_SINK: &str = "rustmic_music";
/// Transports understood by the bundled `rustmic` client.
pub const TRANSPORTS: &[&str] = &["usb", "wifi", "wifi-direct", "bluetooth"];
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

/// How the daemon combines your voice with audio from the external translator.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum TranslateMode {
    /// Mute the original voice while translated speech is playing.
    #[default]
    Replace,
    /// Play both sources together.
    Mix,
}

impl TranslateMode {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Replace => "replace",
            Self::Mix => "mix",
        }
    }

    pub fn parse(value: &str) -> Self {
        match value.trim().trim_matches('"').to_lowercase().as_str() {
            "mix" | "mixed" | "смешивать" => Self::Mix,
            _ => Self::Replace,
        }
    }
}

/// Speech recognition backend used by the built-in translator pipeline.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum AsrEngine {
    #[default]
    FasterWhisper,
    Groq,
}

impl AsrEngine {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::FasterWhisper => "faster_whisper",
            Self::Groq => "groq",
        }
    }
    pub fn parse(value: &str) -> Self {
        match value.trim().trim_matches('"').to_lowercase().as_str() {
            "groq" | "groq_whisper" => Self::Groq,
            _ => Self::FasterWhisper,
        }
    }
}

/// Machine-translation backend.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum MtEngine {
    #[default]
    OpenAi,
    Argos,
}

impl MtEngine {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::OpenAi => "openai",
            Self::Argos => "argos",
        }
    }
    pub fn parse(value: &str) -> Self {
        match value.trim().trim_matches('"').to_lowercase().as_str() {
            "argos" | "argostranslate" => Self::Argos,
            _ => Self::OpenAi,
        }
    }
}

/// Speech synthesis backend. Chatterbox and XTTS clone a supplied voice;
/// Kokoro only uses fixed voices.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum TtsEngine {
    #[default]
    Kokoro,
    Chatterbox,
    Xtts,
}

impl TtsEngine {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Chatterbox => "chatterbox",
            Self::Xtts => "xtts",
            Self::Kokoro => "kokoro",
        }
    }
    pub fn parse(value: &str) -> Self {
        match value.trim().trim_matches('"').to_lowercase().as_str() {
            "chatterbox" => Self::Chatterbox,
            "xtts" | "xtts_v2" | "coqui" => Self::Xtts,
            _ => Self::Kokoro,
        }
    }
    pub fn clones_voice(self) -> bool {
        matches!(self, Self::Chatterbox | Self::Xtts)
    }
}

/// One-click engine bundles offered in the GUI.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum EnginePreset {
    /// Free cloud ASR/MT with a cloned local voice.
    FreeVoice,
    /// Fast local ASR/TTS, optionally cloud MT.
    #[default]
    Light,
    /// Everything local; no API keys.
    Offline,
    /// Cloud ASR + cloud MT, local TTS.
    Cloud,
    Custom,
}

impl EnginePreset {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::FreeVoice => "free_voice",
            Self::Light => "light",
            Self::Offline => "offline",
            Self::Cloud => "cloud",
            Self::Custom => "custom",
        }
    }
    pub fn parse(value: &str) -> Self {
        match value.trim().trim_matches('"').to_lowercase().as_str() {
            "free_voice" | "free" | "бесплатный со своим голосом" => Self::FreeVoice,
            "offline" | "офлайн" => Self::Offline,
            "cloud" | "облако" => Self::Cloud,
            "custom" | "свой" => Self::Custom,
            _ => Self::Light,
        }
    }
    pub fn label(self) -> &'static str {
        match self {
            Self::FreeVoice => "Бесплатный со своим голосом",
            Self::Light => "Лёгкий",
            Self::Offline => "Офлайн",
            Self::Cloud => "Облако",
            Self::Custom => "Своя связка",
        }
    }
    pub const ALL: &'static [Self] = &[
        Self::FreeVoice,
        Self::Light,
        Self::Offline,
        Self::Cloud,
        Self::Custom,
    ];
}

#[derive(Clone, Debug)]
pub struct Settings {
    /// Schema version read from `config.toml`.
    pub config_version: u32,
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
    /// Real-time loudness normalisation for music (mpv lavfi/loudnorm).
    pub loudnorm: bool,
    /// Lower the music while the voice gate is open.
    pub ducking: bool,
    /// How many dB the music is lowered when ducking is active.
    pub ducking_db: f32,
    /// Mirror music to the default output (Linux only).
    pub monitor_music: bool,
    /// Feed voice to / mix translated speech from the local PCM sockets.
    pub translate_enabled: bool,
    /// Language code of the spoken language, passed to the translator.
    pub translate_from: String,
    /// Target language code, passed to the translator.
    pub translate_to: String,
    /// Replace the original voice or mix both sources.
    pub translate_mode: TranslateMode,
    /// Translated speech gain 0..100 %.
    pub translate_volume: u32,

    // ---- step 6: built-in translator engines ----
    pub engine_preset: EnginePreset,
    pub asr_engine: AsrEngine,
    pub mt_engine: MtEngine,
    pub tts_engine: TtsEngine,
    /// Groq API key for Whisper. Falls back to local ASR on 429.
    pub groq_api_key: String,
    pub groq_model: String,
    /// Any OpenAI-compatible chat/completions endpoint.
    pub openai_base_url: String,
    pub openai_api_key: String,
    pub openai_model: String,
    /// Path to a voice sample (wav/mp3) for Chatterbox / XTTS cloning.
    pub voice_sample: String,
}

impl Default for Settings {
    /// Defaults only; `load()` additionally reads `config.toml`.
    fn default() -> Self {
        Self {
            config_version: CONFIG_VERSION,
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
            loudnorm: false,
            ducking: true,
            ducking_db: 12.0,
            monitor_music: true,
            translate_enabled: false,
            translate_from: "ru".to_string(),
            translate_to: "en".to_string(),
            translate_mode: TranslateMode::Replace,
            translate_volume: 100,
            engine_preset: EnginePreset::Light,
            asr_engine: AsrEngine::FasterWhisper,
            mt_engine: MtEngine::Argos,
            tts_engine: TtsEngine::Kokoro,
            groq_api_key: String::new(),
            groq_model: "whisper-large-v3".to_string(),
            openai_base_url: "https://api.openai.com/v1".to_string(),
            openai_api_key: String::new(),
            openai_model: "gpt-4o-mini".to_string(),
            voice_sample: String::new(),
        }
    }
}

impl Settings {
    /// Read the file, upgrade an old schema and write it back once.
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
                "loudnorm" => self.loudnorm = parse_bool(value, false),
                "ducking" => self.ducking = parse_bool(value, true),
                "ducking_db" => self.ducking_db = parse_db_amount(value),
                "monitor_music" => self.monitor_music = parse_bool(value, true),
                "translate_enabled" | "translate" => {
                    self.translate_enabled = parse_bool(value, false)
                }
                "translate_from" | "translate_from_lang" => {
                    if !value.is_empty() {
                        self.translate_from = value.to_string();
                    }
                }
                "translate_to" | "translate_to_lang" => {
                    if !value.is_empty() {
                        self.translate_to = value.to_string();
                    }
                }
                "translate_mode" => self.translate_mode = TranslateMode::parse(value),
                "translate_volume" => self.translate_volume = parse_percent(value),
                "engine_preset" => self.engine_preset = EnginePreset::parse(value),
                "asr_engine" => self.asr_engine = AsrEngine::parse(value),
                "mt_engine" => self.mt_engine = MtEngine::parse(value),
                "tts_engine" => self.tts_engine = TtsEngine::parse(value),
                "groq_api_key" => self.groq_api_key = value.to_string(),
                "groq_model" => {
                    if !value.is_empty() {
                        self.groq_model = value.to_string();
                    }
                }
                "openai_base_url" => {
                    if !value.is_empty() {
                        self.openai_base_url = value.to_string();
                    }
                }
                "openai_api_key" => self.openai_api_key = value.to_string(),
                "openai_model" => {
                    if !value.is_empty() {
                        self.openai_model = value.to_string();
                    }
                }
                "voice_sample" => self.voice_sample = value.to_string(),
                "device" => {
                    if !value.is_empty() {
                        self.device = value.to_string();
                    }
                }
                "adb" => {
                    if !value.is_empty() {
                        self.adb = value.to_string();
                    }
                }
                _ => {}
            }
        }

        // v1 `gate = 0.46` is the same VAD threshold as 46 %. It is not
        // converted to dB — only the display unit changed.
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
        text.push_str(&format!("transport = \"{}\"\n", self.transport));
        text.push_str(&format!("host = \"{}\"\n", self.host));
        text.push_str(&format!("bt_address = \"{}\"\n", self.bt_address));
        text.push_str(&format!("denoise = {}\n", self.denoise));
        text.push_str(&format!("gate_mode = \"{}\"\n", self.gate_mode.as_str()));
        text.push_str(&format!("gate_voice = {}\n", self.gate_voice));
        text.push_str(&format!("gate_level_db = {:.1}\n", self.gate_level_db));
        text.push_str(&format!("mic_volume = {}\n", self.mic_volume));
        text.push_str(&format!("music_volume = {}\n", self.music_volume));
        text.push_str(&format!("loudnorm = {}\n", self.loudnorm));
        text.push_str(&format!("ducking = {}\n", self.ducking));
        text.push_str(&format!("ducking_db = {:.1}\n", self.ducking_db));
        text.push_str(&format!("monitor_music = {}\n", self.monitor_music));
        text.push_str(&format!("translate_enabled = {}\n", self.translate_enabled));
        text.push_str(&format!("translate_from = \"{}\"\n", self.translate_from));
        text.push_str(&format!("translate_to = \"{}\"\n", self.translate_to));
        text.push_str(&format!("translate_mode = \"{}\"\n", self.translate_mode.as_str()));
        text.push_str(&format!("translate_volume = {}\n", self.translate_volume));
        text.push_str(&format!("engine_preset = \"{}\"\n", self.engine_preset.as_str()));
        text.push_str(&format!("asr_engine = \"{}\"\n", self.asr_engine.as_str()));
        text.push_str(&format!("mt_engine = \"{}\"\n", self.mt_engine.as_str()));
        text.push_str(&format!("tts_engine = \"{}\"\n", self.tts_engine.as_str()));
        text.push_str(&format!("groq_api_key = \"{}\"\n", self.groq_api_key));
        text.push_str(&format!("groq_model = \"{}\"\n", self.groq_model));
        text.push_str(&format!("openai_base_url = \"{}\"\n", self.openai_base_url));
        text.push_str(&format!("openai_api_key = \"{}\"\n", self.openai_api_key));
        text.push_str(&format!("openai_model = \"{}\"\n", self.openai_model));
        text.push_str(&format!("voice_sample = \"{}\"\n", self.voice_sample));
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
    use super::{
        normalize_transport, parse_volume, AsrEngine, EnginePreset, GateMode, MtEngine, Settings,
        TtsEngine, TranslateMode,
    };

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
    fn translator_settings_are_parsed() {
        let mut settings = Settings::default();
        settings.apply(
            "translate_enabled = true\ntranslate_from = \"ru\"\ntranslate_to = \"de\"\n\
             translate_mode = mix\ntranslate_volume = 65\n",
        );
        assert!(settings.translate_enabled);
        assert_eq!(settings.translate_from, "ru");
        assert_eq!(settings.translate_to, "de");
        assert_eq!(settings.translate_mode, TranslateMode::Mix);
        assert_eq!(settings.translate_volume, 65);
    }

    #[test]
    fn engine_settings_are_parsed() {
        let mut settings = Settings::default();
        settings.apply(
            "engine_preset = \"free_voice\"\nasr_engine = \"groq\"\nmt_engine = \"openai\"\n\
             tts_engine = \"chatterbox\"\ngroq_api_key = \"gsk_test\"\n\
             openai_base_url = \"https://api.example.com/v1\"\nvoice_sample = \"me.wav\"\n",
        );
        assert_eq!(settings.engine_preset, EnginePreset::FreeVoice);
        assert_eq!(settings.asr_engine, AsrEngine::Groq);
        assert_eq!(settings.mt_engine, MtEngine::OpenAi);
        assert_eq!(settings.tts_engine, TtsEngine::Chatterbox);
        assert_eq!(settings.groq_api_key, "gsk_test");
        assert_eq!(settings.openai_base_url, "https://api.example.com/v1");
        assert_eq!(settings.voice_sample, "me.wav");
    }
}

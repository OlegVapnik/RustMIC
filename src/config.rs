//! Settings file and per-platform paths.
//!
//! Linux:   `~/.config/RustMIC/config.toml`, `~/.cache/RustMIC/`
//! Windows: `%APPDATA%\RustMIC\config.toml`, `%LOCALAPPDATA%\RustMIC\`

use std::env;
use std::fs;
use std::io;
use std::path::PathBuf;

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

#[derive(Clone, Debug)]
pub struct Settings {
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
    pub gate: f32,
    pub volume: u32,
}

impl Default for Settings {
    /// Defaults plus whatever `config.toml` says. A missing file keeps defaults.
    fn default() -> Self {
        let text = fs::read_to_string(settings_path()).unwrap_or_default();
        let mut s = Self {
            transport: "usb".to_string(),
            host: String::new(),
            bt_address: String::new(),
            adb: default_adb(),
            device: DEFAULT_DEVICE.to_string(),
            denoise: true,
            gate: 0.0,
            volume: 100,
        };
        s.apply(&text);
        s
    }
}

impl Settings {
    pub fn load() -> Self {
        Self::default()
    }

    /// Parse the settings file, ignoring comments and unknown keys.
    fn apply(&mut self, text: &str) {
        for line in text.lines() {
            let line = line.split('#').next().unwrap_or("").trim();
            let Some((key, value)) = line.split_once('=') else {
                continue;
            };
            let value = value.trim().trim_matches('"');
            match key.trim() {
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
                "denoise" => self.denoise = value != "false" && value != "0",
                "gate" => self.gate = value.parse::<f32>().unwrap_or(0.0).clamp(0.0, 1.0),
                "volume" => self.volume = value.parse::<u32>().unwrap_or(100).clamp(0, 150),
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
    }

    pub fn save(&self) -> io::Result<()> {
        let path = settings_path();
        if let Some(dir) = path.parent() {
            fs::create_dir_all(dir)?;
        }
        let mut text = String::from("# RustMIC settings (managed by RustMIC-gui)\n");
        text.push_str(&format!("transport = \"{}\"\n", self.transport));
        text.push_str(&format!("host = \"{}\"\n", self.host));
        text.push_str(&format!("bt_address = \"{}\"\n", self.bt_address));
        text.push_str(&format!("denoise = {}\n", self.denoise));
        text.push_str(&format!("gate = {:.2}\n", self.gate));
        text.push_str(&format!("volume = {}\n", self.volume));
        if cfg!(windows) {
            text.push_str(&format!("device = \"{}\"\n", self.device));
        }
        fs::write(path, text)
    }
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

/// `volume = N` clamped to 0..=150; anything unparsable means 100.
pub fn parse_volume(text: &str) -> u32 {
    text.lines()
        .find_map(|line| {
            let line = line.split('#').next().unwrap_or("").trim();
            let (key, value) = line.split_once('=')?;
            (key.trim() == "volume")
                .then(|| value.trim().parse::<u32>().ok())
                .flatten()
        })
        .unwrap_or(100)
        .clamp(0, 150)
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
    use super::{normalize_transport, parse_volume, Settings};

    #[test]
    fn volume_defaults_to_100_for_missing_or_invalid_values() {
        assert_eq!(parse_volume("denoise = true\ngate = 0.2\n"), 100);
        assert_eq!(parse_volume("volume = loud\n"), 100);
    }

    #[test]
    fn volume_is_clamped_to_supported_range() {
        assert_eq!(parse_volume("volume = 0\n"), 0);
        assert_eq!(parse_volume("volume = 125\n"), 125);
        assert_eq!(parse_volume("volume = 200\n"), 150);
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
        let mut settings = Settings {
            transport: "usb".into(),
            host: String::new(),
            bt_address: String::new(),
            adb: "adb".into(),
            device: "CABLE Input".into(),
            denoise: true,
            gate: 0.0,
            volume: 100,
        };
        settings.apply("transport = wifi\nhost = \"192.168.1.7\"\nbt_address = \"AA:BB\"\n");
        assert_eq!(settings.transport, "wifi");
        assert_eq!(settings.host, "192.168.1.7");
        assert_eq!(settings.bt_address, "AA:BB");
    }
}

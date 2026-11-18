//! Shared state published by the daemon to GUI and CLI clients.

use serde::{Deserialize, Serialize};

use crate::config::{GateMode, Settings};
use crate::{level, mixer, music, tiles};

pub const PROTOCOL_VERSION: u32 = 1;

#[derive(Clone, Debug, Default, Serialize, Deserialize)]
pub struct ConnectionState {
    /// usb / wifi / wifi-direct / bluetooth
    pub transport: String,
    /// Phone IP or Bluetooth MAC, for display.
    pub target: String,
    /// Human-readable phone line: "подключён (Infinix X6836)".
    pub phone: String,
    pub phone_connected: bool,
    pub client_running: bool,
    pub sink_present: bool,
    pub source_present: bool,
    pub default_source: String,
    /// Filled in only when a real measurement exists.
    pub latency_ms: Option<u32>,
}

#[derive(Clone, Debug, Default, Serialize, Deserialize)]
pub struct Levels {
    pub rms: f32,
    pub peak: f32,
    pub vad: f32,
    pub gate_open: bool,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct AudioState {
    pub denoise: bool,
    pub gate_mode: GateMode,
    pub gate_voice: u32,
    pub gate_level_db: f32,
    pub mic_volume: u32,
    pub music_volume: u32,
    pub muted: bool,
    pub loudnorm: bool,
    pub ducking: bool,
    pub ducking_db: f32,
    pub monitor_music: bool,
}

impl From<&Settings> for AudioState {
    fn from(settings: &Settings) -> Self {
        Self {
            denoise: settings.denoise,
            gate_mode: settings.gate_mode,
            gate_voice: settings.gate_voice,
            gate_level_db: settings.gate_level_db,
            mic_volume: settings.mic_volume,
            music_volume: settings.music_volume,
            muted: false,
            loudnorm: settings.loudnorm,
            ducking: settings.ducking,
            ducking_db: settings.ducking_db,
            monitor_music: settings.monitor_music,
        }
    }
}

#[derive(Clone, Debug, Default, Serialize, Deserialize)]
pub struct TrackState {
    pub query: String,
    pub title: String,
    pub duration: f64,
    pub position: f64,
    pub paused: bool,
    pub cover: Option<String>,
}

#[derive(Clone, Debug, Default, Serialize, Deserialize)]
pub struct Tile {
    pub id: String,
    pub title: String,
    pub query: String,
    pub cover: Option<String>,
    pub duration: f64,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct AppState {
    /// Wire protocol version; clients must reject a mismatch.
    pub protocol: u32,
    pub config_version: u32,
    pub daemon_pid: u32,
    pub service_running: bool,
    pub connection: ConnectionState,
    pub levels: Levels,
    pub audio: AudioState,
    pub track: Option<TrackState>,
    pub tiles: Vec<Tile>,
    /// External translator / PCM mixer status.
    pub translate: mixer::TranslateStatus,
    /// ID of the last command this daemon has applied.
    pub last_command_id: u64,
    pub updated_at: u64,
}

impl Default for AppState {
    fn default() -> Self {
        Self {
            protocol: PROTOCOL_VERSION,
            config_version: crate::config::CONFIG_VERSION,
            daemon_pid: std::process::id(),
            service_running: false,
            connection: ConnectionState::default(),
            levels: Levels::default(),
            audio: AudioState::from(&Settings::default()),
            track: None,
            tiles: Vec::new(),
            translate: mixer::status(),
            last_command_id: 0,
            updated_at: 0,
        }
    }
}

impl AppState {
    pub fn from_settings(settings: &Settings) -> Self {
        let target = match settings.transport.as_str() {
            "wifi" | "wifi-direct" => settings.host.clone(),
            "bluetooth" => settings.bt_address.clone(),
            _ => settings.adb.clone(),
        };
        Self {
            protocol: PROTOCOL_VERSION,
            config_version: settings.config_version,
            daemon_pid: std::process::id(),
            service_running: false,
            connection: ConnectionState {
                transport: settings.transport.clone(),
                target,
                ..ConnectionState::default()
            },
            levels: Levels::default(),
            audio: AudioState::from(settings),
            track: None,
            tiles: Vec::new(),
            translate: mixer::status(),
            last_command_id: 0,
            updated_at: now(),
        }
    }

    /// Copy settings and metering data that can change without a restart.
    pub fn refresh(&mut self, settings: &Settings) {
        self.config_version = settings.config_version;
        self.audio = AudioState::from(settings);
        let meter = level::read_meter();
        self.levels = Levels {
            rms: meter.rms,
            peak: meter.peak,
            vad: meter.vad,
            gate_open: meter.gate_open,
        };
        self.track = music::status();
        self.tiles = tiles::load();
        self.translate = mixer::status();
        self.updated_at = now();
    }
}

pub fn now() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|duration| duration.as_secs())
        .unwrap_or(0)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn state_json_roundtrip_keeps_protocol_version() {
        let state = AppState::default();
        let text = serde_json::to_string(&state).unwrap();
        let back: AppState = serde_json::from_str(&text).unwrap();
        assert_eq!(back.protocol, PROTOCOL_VERSION);
        assert_eq!(back.config_version, crate::config::CONFIG_VERSION);
    }
}

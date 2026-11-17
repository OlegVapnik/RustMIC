//! Commands accepted by the daemon over the local control socket.
//!
//! Every line is a JSON object with a protocol version and a tagged command:
//!
//! ```json
//! {"protocol":1,"cmd":"set_mic_volume","value":80}
//! ```

use serde::{Deserialize, Serialize};

use crate::config::GateMode;

pub use super::state::PROTOCOL_VERSION;

#[derive(Clone, Debug, Serialize, Deserialize, PartialEq)]
#[serde(tag = "cmd", rename_all = "snake_case")]
pub enum Command {
    Ping,
    GetState,
    SetMicVolume { value: u32 },
    SetMusicVolume { value: u32 },
    SetGateMode { mode: GateMode },
    SetGateVoice { value: u32 },
    SetGateLevel { value: f32 },
    SetDenoise { enabled: bool },
    SetMuted { muted: bool },
    SetLoudnorm { enabled: bool },
    SetDucking { enabled: bool },
    SetMonitor { enabled: bool },
    Play { query: String },
    StopMusic,
    Pause { paused: bool },
    Seek { seconds: f64 },
    Restart,
    StartService,
    StopService,
}

impl Command {
    pub fn name(&self) -> &'static str {
        match self {
            Self::Ping => "ping",
            Self::GetState => "get_state",
            Self::SetMicVolume { .. } => "set_mic_volume",
            Self::SetMusicVolume { .. } => "set_music_volume",
            Self::SetGateMode { .. } => "set_gate_mode",
            Self::SetGateVoice { .. } => "set_gate_voice",
            Self::SetGateLevel { .. } => "set_gate_level",
            Self::SetDenoise { .. } => "set_denoise",
            Self::SetMuted { .. } => "set_muted",
            Self::SetLoudnorm { .. } => "set_loudnorm",
            Self::SetDucking { .. } => "set_ducking",
            Self::SetMonitor { .. } => "set_monitor",
            Self::Play { .. } => "play",
            Self::StopMusic => "stop_music",
            Self::Pause { .. } => "pause",
            Self::Seek { .. } => "seek",
            Self::Restart => "restart",
            Self::StartService => "start_service",
            Self::StopService => "stop_service",
        }
    }
}

/// Wire envelope. `protocol` must match the daemon's `PROTOCOL_VERSION`.
/// `id` lets a client wait until the daemon has applied its command.
#[derive(Clone, Debug, Serialize, Deserialize, PartialEq)]
pub struct CommandMessage {
    pub protocol: u32,
    pub id: u64,
    #[serde(flatten)]
    pub command: Command,
}

impl CommandMessage {
    pub fn new(command: Command) -> Self {
        Self::with_id(0, command)
    }

    pub fn with_id(id: u64, command: Command) -> Self {
        Self {
            protocol: PROTOCOL_VERSION,
            id,
            command,
        }
    }

    pub fn validate(&self) -> Result<(), String> {
        if self.protocol == PROTOCOL_VERSION {
            Ok(())
        } else {
            Err(format!(
                "protocol version {} is not supported (expected {})",
                self.protocol, PROTOCOL_VERSION
            ))
        }
    }
}

impl From<Command> for CommandMessage {
    fn from(command: Command) -> Self {
        Self::new(command)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn command_json_roundtrip() {
        let message = CommandMessage::new(Command::SetGateMode {
            mode: GateMode::Level,
        });
        let text = serde_json::to_string(&message).unwrap();
        assert!(text.contains("\"cmd\":\"set_gate_mode\""));
        assert!(text.contains("\"mode\":\"level\""));
        let back: CommandMessage = serde_json::from_str(&text).unwrap();
        assert_eq!(back, message);
        assert_eq!(back.id, 0);
    }

    #[test]
    fn play_command_carries_the_query() {
        let message = CommandMessage::new(Command::Play {
            query: "https://youtu.be/abc".to_string(),
        });
        let text = serde_json::to_string(&message).unwrap();
        let back: CommandMessage = serde_json::from_str(&text).unwrap();
        assert_eq!(
            back.command,
            Command::Play {
                query: "https://youtu.be/abc".to_string()
            }
        );
    }

    #[test]
    fn protocol_version_is_validated() {
        let mut message = CommandMessage::new(Command::Ping);
        assert!(message.validate().is_ok());
        message.protocol += 1;
        assert!(message.validate().is_err());
    }
}

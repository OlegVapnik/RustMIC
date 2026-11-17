//! Shared daemon state and the command protocol used by the GUI and CLI.

pub mod command;
pub mod state;

pub use command::{Command, CommandMessage, PROTOCOL_VERSION};
pub use state::{
    now, AppState, AudioState, ConnectionState, Levels, Tile, TrackState,
};

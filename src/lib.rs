//! RustMIC — WO Mic (Android) as a system microphone.
//!
//! One binary, two platforms:
//!
//! * Linux:  rustmic -> RNNoise -> pacat -> rustmic_sink -> remap -> rustmic_mic
//! * Windows: rustmic -> RNNoise -> WASAPI playback to a virtual audio cable
//!   (VB-CABLE and friends), which apps then see as a recording device.
//!
//! Everything that is not audio plumbing (config, RNNoise, supervisor loop,
//! CLI, GUI glue) lives here and is shared by both platforms.

pub mod adb;
pub mod audio;
pub mod config;
pub mod core;
pub mod denoise;
pub mod ipc;
pub mod level;
pub mod logging;
pub mod music;
pub mod platform;

use std::sync::atomic::AtomicBool;

/// Set by the signal handler / GUI "stop": every loop in the supervisor and in
/// the pump checks it and winds down.
pub static STOP: AtomicBool = AtomicBool::new(false);

pub fn stopping() -> bool {
    STOP.load(std::sync::atomic::Ordering::SeqCst)
}

/// Build a child process that stays invisible on Windows.
///
/// Console helpers (adb, tasklist, taskkill, reg, rustmic) would otherwise pop a
/// black window every time the GUI polls them. On Linux this is a plain
/// `Command::new`.
pub fn command(program: impl AsRef<std::ffi::OsStr>) -> std::process::Command {
    #[allow(unused_mut)]
    let mut command = std::process::Command::new(program);
    #[cfg(windows)]
    {
        use std::os::windows::process::CommandExt;
        const CREATE_NO_WINDOW: u32 = 0x0800_0000;
        command.creation_flags(CREATE_NO_WINDOW);
    }
    command
}

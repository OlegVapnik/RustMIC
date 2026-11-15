//! Audio output: where the denoised PCM goes.
//!
//! * Linux: a `pacat` child writes into the `womic_sink` null sink, which a
//!   remap source exposes as `womic_mic`.
//! * Windows: WASAPI playback into a virtual audio cable (VB-CABLE by
//!   default); recording apps pick the cable's capture side.

use std::io;
use std::sync::{Arc, RwLock};

use crate::config::Settings;
use crate::denoise::Live;

pub trait Sink {
    /// Write mono s16 samples at 48 kHz.
    fn write(&mut self, samples: &[i16]) -> io::Result<()>;
    fn describe(&self) -> String;
}

#[cfg(unix)]
pub mod linux;
#[cfg(windows)]
pub mod windows;

/// Open the platform sink. Linux blocks until `pacat` is running; Windows
/// builds the WASAPI stream right away.
pub fn open(settings: &Settings, live: &Arc<RwLock<Live>>) -> Result<Box<dyn Sink>, String> {
    #[cfg(unix)]
    {
        let _ = live;
        linux::open(settings)
    }
    #[cfg(windows)]
    {
        windows::open(settings, live)
    }
}

/// One-time setup: on Linux this creates the PipeWire nodes, keeps the sink
/// awake and recreates the nodes if PipeWire forgets them. No-op elsewhere.
pub fn prepare() {
    #[cfg(unix)]
    linux::prepare();
}

/// Is the virtual microphone available right now?
pub fn backend_ready(settings: &Settings) -> bool {
    #[cfg(unix)]
    {
        let _ = settings;
        linux::nodes_present()
    }
    #[cfg(windows)]
    {
        windows::device_present(&settings.device)
    }
}

pub fn backend_description(settings: &Settings) -> String {
    #[cfg(unix)]
    {
        let _ = settings;
        format!("PipeWire {} -> {}", crate::config::SINK, crate::config::SRC)
    }
    #[cfg(windows)]
    {
        format!("WASAPI -> {}", settings.device)
    }
}

/// Ask the system to route the microphone to applications by default.
/// Windows has no such global switch, so there it only reports the limitation.
pub fn set_default(on: bool) -> Result<(), String> {
    #[cfg(unix)]
    {
        linux::set_default(on)
    }
    #[cfg(windows)]
    {
        let _ = on;
        Err("Windows: select the cable's recording device in the app you use".into())
    }
}

pub fn default_is_womic() -> bool {
    #[cfg(unix)]
    {
        linux::default_is_womic()
    }
    #[cfg(windows)]
    {
        false
    }
}

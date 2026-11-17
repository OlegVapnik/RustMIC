//! RustMIC-gui — platform dispatcher.
//!
//! Linux:   GTK4 window + StatusNotifier tray (feature `gui`).
//! Windows: egui window (feature `gui`).

// The Windows build is a GUI app: no console window should flash on start.
#![cfg_attr(all(windows, feature = "gui"), windows_subsystem = "windows")]

#[cfg(all(feature = "gui", unix))]
#[path = "../gui/linux.rs"]
mod imp;

#[cfg(all(feature = "gui", windows))]
#[path = "../gui/windows.rs"]
mod imp;

#[cfg(feature = "gui")]
fn main() {
    imp::run();
}

#[cfg(not(feature = "gui"))]
fn main() {
    eprintln!("RustMIC-gui: build with --features gui");
    std::process::exit(1);
}

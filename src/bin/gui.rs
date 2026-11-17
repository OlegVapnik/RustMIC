//! RustMIC-gui — the shared egui window for Linux and Windows.

// The Windows build is a GUI app: no console window should flash on start.
#![cfg_attr(all(windows, feature = "gui"), windows_subsystem = "windows")]

#[cfg(feature = "gui")]
fn main() {
    rustmic_mic::gui::run();
}

#[cfg(not(feature = "gui"))]
fn main() {
    eprintln!("RustMIC-gui: build with --features gui");
    std::process::exit(1);
}

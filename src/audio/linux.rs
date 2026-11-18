//! Linux backend: PipeWire (via pactl/pacat/parec).
//!
//! `pacat` cannot write to a suspended null sink, so a `parec` keepalive runs
//! for the whole life of the daemon and the sink/source pair is recreated
//! whenever it disappears (PipeWire restart, node removal).

use std::io::Write;
use std::process::{Child, Command, Stdio};
use std::thread;
use std::time::Duration;

use super::Sink;
use crate::config::{Settings, MUSIC_SINK, SINK, SRC};
use crate::{logging, STOP};

fn pactl(args: &[&str]) -> bool {
    Command::new("pactl")
        .args(args)
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .status()
        .map(|status| status.success())
        .unwrap_or(false)
}

pub fn pactl_out(args: &[&str]) -> String {
    match Command::new("pactl").args(args).output() {
        Ok(output) => String::from_utf8_lossy(&output.stdout).into_owned(),
        Err(_) => String::new(),
    }
}

pub fn node_exists(kind: &str, name: &str) -> bool {
    pactl_out(&["list", "short", kind])
        .lines()
        .any(|line| line.split('\t').nth(1) == Some(name))
}

pub fn nodes_present() -> bool {
    node_exists("sinks", SINK) && node_exists("sources", SRC)
}

/// Idempotently make sure the virtual sink + remapped source exist and apply
/// the configured volumes.
pub fn ensure_devices() {
    if !node_exists("sinks", SINK)
        && pactl(&[
            "load-module",
            "module-null-sink",
            &format!("sink_name={SINK}"),
            "sink_properties=device.description=RustMIC_Phone",
        ])
    {
        logging::log("created null sink rustmic_sink");
    }
    if !node_exists("sources", SRC)
        && pactl(&[
            "load-module",
            "module-remap-source",
            &format!("master={SINK}.monitor"),
            &format!("source_name={SRC}"),
            "source_properties=device.description=RustMIC_Microphone",
        ])
    {
        logging::log("created remap source rustmic_mic");
    }
    // Separate sink for `RustMIC play`: keeps the level meter/mic path clean
    // and lets us mirror the music to speakers only while it is playing.
    if !node_exists("sinks", MUSIC_SINK)
        && pactl(&[
            "load-module",
            "module-null-sink",
            &format!("sink_name={MUSIC_SINK}"),
            "sink_properties=device.description=RustMIC_Music",
        ])
    {
        logging::log("created null sink rustmic_music");
    }
    pactl(&["set-sink-volume", SINK, "100%"]);
    pactl(&["set-sink-mute", SINK, "0"]);
    let volume = crate::config::Settings::load().mic_volume;
    pactl(&["set-source-volume", SRC, &format!("{volume}%")]);
    pactl(&["set-source-mute", SRC, "0"]);
}

pub fn set_source_volume(percent: u32) -> bool {
    pactl(&["set-source-volume", SRC, &format!("{percent}%")])
}

pub fn default_is_rustmic() -> bool {
    pactl_out(&["get-default-source"]).trim() == SRC
}

pub fn default_source() -> String {
    pactl_out(&["get-default-source"]).trim().to_string()
}

pub fn set_default(on: bool) -> Result<(), String> {
    let target = if on {
        SRC.to_string()
    } else {
        crate::platform::builtin_mic()
    };
    if pactl(&["set-default-source", &target]) {
        Ok(())
    } else {
        Err(format!("pactl could not select {target}"))
    }
}

pub fn prepare() {
    ensure_devices();
    // watchdog: recreate the nodes if PipeWire/WirePlumber dropped them, and
    // keep `parec` on the sink monitor so pacat never blocks on a suspended node
    thread::spawn(|| {
        let mut keepalive: Option<Child> = None;
        while !STOP.load(std::sync::atomic::Ordering::SeqCst) {
            ensure_devices();
            let dead = match keepalive.as_mut() {
                Some(child) => !matches!(child.try_wait(), Ok(None)),
                None => true,
            };
            if dead {
                if let Some(mut child) = keepalive.take() {
                    let _ = child.kill();
                    let _ = child.wait();
                }
                keepalive = Command::new("parec")
                    .args([&format!("--device={SINK}.monitor"), "--raw"])
                    .stdin(Stdio::null())
                    .stdout(Stdio::null())
                    .stderr(Stdio::null())
                    .spawn()
                    .ok();
            }
            thread::sleep(Duration::from_secs(3));
        }
        if let Some(mut child) = keepalive {
            let _ = child.kill();
            let _ = child.wait();
        }
    });
}

struct PacatSink {
    child: Child,
}

impl Sink for PacatSink {
    fn write(&mut self, samples: &[i16]) -> std::io::Result<()> {
        let mut bytes = Vec::with_capacity(samples.len() * 2);
        for sample in samples {
            bytes.extend_from_slice(&sample.to_le_bytes());
        }
        let stdin = self
            .child
            .stdin
            .as_mut()
            .ok_or_else(|| std::io::Error::new(std::io::ErrorKind::BrokenPipe, "pacat is gone"))?;
        stdin.write_all(&bytes)?;
        stdin.flush()
    }

    fn describe(&self) -> String {
        format!("pacat -> {SINK}")
    }
}

impl Drop for PacatSink {
    fn drop(&mut self) {
        let _ = self.child.kill();
        let _ = self.child.wait();
    }
}

pub fn open(_settings: &Settings) -> Result<Box<dyn Sink>, String> {
    ensure_devices();
    let child = Command::new("pacat")
        .args([
            &format!("--device={SINK}"),
            "--format=s16le",
            "--rate=48000",
            "--channels=1",
            "--raw",
        ])
        .stdin(Stdio::piped())
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .spawn()
        .map_err(|error| format!("cannot start pacat: {error}"))?;
    Ok(Box::new(PacatSink { child }))
}

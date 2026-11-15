//! adb helpers shared by both platforms. Only devices attached over USB count:
//! `adb devices` also lists Wi-Fi devices, and a stale TCP entry used to be
//! reported as "connected" long after unplugging.

use std::process::Stdio;

use crate::command;

pub struct Phone {
    pub serial: String,
    pub model: Option<String>,
}

/// Forward rustmic's TCP port. rustmic does this itself; doing it here
/// survives a stale forward left over after a crash.
pub fn forward(adb: &str) -> bool {
    command(adb)
        .args(["forward", "tcp:8125", "tcp:8125"])
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .status()
        .map(|status| status.success())
        .unwrap_or(false)
}

pub fn devices(adb: &str) -> Vec<Phone> {
    let output = match command(adb).args(["devices", "-l"]).output() {
        Ok(output) => output,
        Err(_) => return Vec::new(),
    };
    let listing = String::from_utf8_lossy(&output.stdout);
    let mut phones = Vec::new();
    for line in listing.lines().skip(1) {
        let mut fields = line.split_whitespace();
        let Some(serial) = fields.next() else {
            continue;
        };
        let rest: Vec<&str> = fields.collect();
        if rest.first() != Some(&"device") {
            continue;
        }
        let model = rest
            .iter()
            .find_map(|field| field.strip_prefix("model:"))
            .map(|model| model.replace('_', " "));
        phones.push(Phone {
            serial: serial.to_string(),
            model,
        });
    }
    phones
}

/// First phone that is currently attached (adb only lists attached devices, so
/// any `device` entry with a USB transport counts; Wi-Fi shows up as host:port).
pub fn usb_phone(adb: &str) -> Option<Phone> {
    devices(adb)
        .into_iter()
        .find(|phone| !phone.serial.contains(':') && !phone.serial.contains('.'))
}

pub fn phone_ready(adb: &str) -> bool {
    usb_phone(adb).is_some()
}


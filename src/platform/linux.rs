//! Linux platform glue: systemd user unit, PipeWire source selection, tools.

use std::env;
use std::fs;
use std::path::PathBuf;
use std::process::{Command, Stdio};

use crate::audio::linux;
use crate::config::{self, Settings, SERVICE, SINK, SRC};
use crate::adb;

pub fn service_active() -> bool {
    out("systemctl", &["--user", "is-active", SERVICE]).trim() == "active"
}

pub fn start() -> Result<(), String> {
    if run("systemctl", &["--user", "start", SERVICE]) {
        Ok(())
    } else {
        Err("systemctl --user start RustMIC failed".into())
    }
}

pub fn stop() -> Result<(), String> {
    if run("systemctl", &["--user", "stop", SERVICE]) {
        Ok(())
    } else {
        Err("systemctl --user stop RustMIC failed".into())
    }
}

pub fn restart() -> Result<(), String> {
    if run("systemctl", &["--user", "restart", SERVICE]) {
        Ok(())
    } else {
        Err("systemctl --user restart RustMIC failed".into())
    }
}

fn unit_text(exe: &str) -> String {
    format!(
        "[Unit]\n\
         Description=WO Mic phone microphone (RustMIC)\n\
         After=pipewire.service wireplumber.service graphical-session.target\n\
         Wants=pipewire.service\n\
         StartLimitIntervalSec=0\n\
         \n\
         [Service]\n\
         Type=simple\n\
         ExecStart={exe} run\n\
         Restart=always\n\
         RestartSec=5\n\
         Nice=-5\n\
         \n\
         [Install]\n\
         WantedBy=default.target\n"
    )
}

pub fn install(enable: bool) -> Result<(), String> {
    let exe = env::current_exe().unwrap_or_else(|_| PathBuf::from("RustMIC"));
    let unit_dir = config::home().join(".config/systemd/user");
    fs::create_dir_all(&unit_dir).map_err(|error| format!("cannot create {unit_dir:?}: {error}"))?;
    let unit = unit_dir.join("RustMIC.service");
    fs::write(&unit, unit_text(&exe.to_string_lossy()))
        .map_err(|error| format!("cannot write {unit:?}: {error}"))?;
    println!("wrote {unit:?} (ExecStart={} run)", exe.to_string_lossy());
    let _ = Command::new("systemctl")
        .args(["--user", "daemon-reload"])
        .status();
    if enable {
        let _ = Command::new("systemctl")
            .args(["--user", "enable", "--now", "RustMIC"])
            .status();
        println!("enabled and started RustMIC.service");
    }
    Ok(())
}

pub fn uninstall() -> Result<(), String> {
    let _ = Command::new("systemctl")
        .args(["--user", "disable", "--now", "RustMIC"])
        .status();
    let unit = config::home().join(".config/systemd/user/RustMIC.service");
    let _ = fs::remove_file(&unit);
    let _ = Command::new("systemctl")
        .args(["--user", "daemon-reload"])
        .status();
    println!("removed RustMIC.service");
    Ok(())
}

/// First real microphone on the system, used by `unset-default`.
pub fn builtin_mic() -> String {
    linux::pactl_out(&["list", "short", "sources"])
        .lines()
        .filter_map(|line| line.split('\t').nth(1))
        .find(|name| *name != SRC && *name != format!("{SINK}.monitor"))
        .unwrap_or("")
        .to_string()
}

pub fn test_mic() {
    std::thread::spawn(|| {
        let pipeline = format!(
            "parec --device={SRC} --format=s16le --rate=48000 --channels=1 \
             --raw --latency-msec=50 | head -c 192000 | \
             paplay --raw --format=s16le --rate=48000 --channels=1"
        );
        let _ = Command::new("sh").arg("-c").arg(pipeline).status();
    });
}

pub fn open_log() {
    let _ = Command::new("kitty")
        .args(["-e", "journalctl", "--user", "-u", SERVICE, "-f"])
        .spawn();
}

pub fn open_settings_folder() {
    let _ = Command::new("xdg-open").arg(config::config_dir()).spawn();
}

pub fn rustmic_running() -> bool {
    Command::new("pgrep")
        .args(["-f", "rustmic --transport"])
        .stdout(Stdio::null())
        .status()
        .map(|status| status.success())
        .unwrap_or(false)
}

pub fn log_tail(lines: usize) -> String {
    out(
        "journalctl",
        &[
            "--user",
            "-u",
            SERVICE,
            "-n",
            &lines.to_string(),
            "--no-pager",
            "-o",
            "cat",
        ],
    )
}

pub fn diagnostics(settings: &Settings) -> String {
    let mut text = String::from("RustMIC diagnostics (linux)\n");
    text.push_str("--- dependencies ---\n");
    let client = config::client_program();
    for (name, command) in [
        ("client", client.as_str()),
        ("pacat", "pacat"),
        ("parec", "parec"),
        ("pactl", "pactl"),
        ("adb", settings.adb.as_str()),
        ("systemctl", "systemctl"),
        ("journalctl", "journalctl"),
    ] {
        text.push_str(&format!(
            "{name}: {}\n",
            if executable_available(command) {
                "available"
            } else {
                "missing"
            }
        ));
    }
    text.push_str(&format!(
        "PipeWire/PulseAudio: {}\n",
        if command_ok("pactl", &["info"]) {
            "available"
        } else {
            "unavailable"
        }
    ));
    text.push_str(&format!(
        "{}\n",
        status_text(settings)
    ));
    text.push_str("--- journal ---\n");
    text.push_str(&log_tail(15));
    text
}

/// Shared status block used by `RustMIC status` and the GUI diagnostics.
pub fn status_text(settings: &Settings) -> String {
    let mut text = String::new();
    let alive = service_active();
    let pid = fs::read_to_string(config::pid_path()).unwrap_or_default();
    text.push_str(&format!(
        "supervisor: {}\n",
        if alive {
            format!("running (pid {})", pid.trim())
        } else {
            "stopped".to_string()
        }
    ));
    text.push_str(&format!(
        "sink {}: {}\n",
        SINK,
        if linux::node_exists("sinks", SINK) {
            "present"
        } else {
            "absent"
        }
    ));
    text.push_str(&format!(
        "source {}: {}\n",
        SRC,
        if linux::node_exists("sources", SRC) {
            "present"
        } else {
            "absent"
        }
    ));
    text.push_str(&format!("configured source volume: {}%\n", settings.volume));
    text.push_str(&format!("default source: {}\n", linux::default_source()));
    let phone = match settings.transport.as_str() {
        "wifi" | "wifi-direct" => format!("телефон (Wi-Fi): {}", settings.host),
        "bluetooth" => format!("телефон (Bluetooth): {}", settings.bt_address),
        _ => format!(
            "телефон (USB): {}",
            match adb::usb_phone(&settings.adb) {
                Some(phone) => match phone.model {
                    Some(model) => format!("подключён ({model})"),
                    None => "подключён".to_string(),
                },
                None => "не подключён".to_string(),
            }
        ),
    };
    text.push_str(&format!("{phone}\n"));
    text.push_str(&format!(
        "client: {}\n",
        if rustmic_running() {
            "running"
        } else {
            "stopped"
        }
    ));
    text
}

fn run(command: &str, args: &[&str]) -> bool {
    Command::new(command)
        .args(args)
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .status()
        .map(|status| status.success())
        .unwrap_or(false)
}

fn command_ok(command: &str, args: &[&str]) -> bool {
    run(command, args)
}

fn out(command: &str, args: &[&str]) -> String {
    Command::new(command)
        .args(args)
        .output()
        .map(|output| String::from_utf8_lossy(&output.stdout).into_owned())
        .unwrap_or_default()
}

pub fn executable_available(command: &str) -> bool {
    use std::os::unix::fs::PermissionsExt;
    let candidates = if command.contains('/') {
        vec![PathBuf::from(command)]
    } else {
        let Some(path) = env::var_os("PATH") else {
            return false;
        };
        env::split_paths(&path)
            .map(|dir| dir.join(command))
            .collect()
    };
    candidates.into_iter().any(|candidate| {
        fs::metadata(candidate)
            .map(|metadata| metadata.is_file() && metadata.permissions().mode() & 0o111 != 0)
            .unwrap_or(false)
    })
}

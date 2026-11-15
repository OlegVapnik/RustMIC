//! Windows platform glue: a detached background process instead of systemd,
//! autostart through the per-user Run key, and WASAPI checks.

use std::env;
use std::fs;
use std::os::windows::process::CommandExt;
use std::path::PathBuf;
use std::process::{Command, Stdio};
use std::thread;
use std::time::{Duration, Instant};

use crate::audio::windows as wasapi;
use crate::config::{self, Settings, SERVICE};
use crate::{adb, logging};

const CREATE_NO_WINDOW: u32 = 0x0800_0000;
const CREATE_NEW_PROCESS_GROUP: u32 = 0x0000_0200;
const RUN_KEY: &str = r"HKCU\Software\Microsoft\Windows\CurrentVersion\Run";

// ---------------------------------------------------------------- process

pub fn pid() -> Option<u32> {
    fs::read_to_string(config::pid_path())
        .ok()
        .and_then(|text| text.trim().parse::<u32>().ok())
}

fn tasklist(args: &[&str]) -> String {
    crate::command("tasklist")
        .args(args)
        .output()
        .map(|output| String::from_utf8_lossy(&output.stdout).into_owned())
        .unwrap_or_default()
}

fn process_alive(pid: u32) -> bool {
    let listing = tasklist(&["/FI", &format!("PID eq {pid}"), "/NH", "/FO", "CSV"]);
    listing.contains(&format!("\"{pid}\""))
}

pub fn service_active() -> bool {
    pid().map(process_alive).unwrap_or(false)
}

fn daemon_command() -> Result<Command, String> {
    let exe = env::current_exe().map_err(|error| format!("cannot find own executable: {error}"))?;
    let mut command = crate::command(exe);
    command
        .arg("run")
        .creation_flags(CREATE_NO_WINDOW | CREATE_NEW_PROCESS_GROUP)
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null());
    Ok(command)
}

pub fn start() -> Result<(), String> {
    if service_active() {
        return Ok(());
    }
    daemon_command()?
        .spawn()
        .map_err(|error| format!("cannot start the background process: {error}"))?;
    let deadline = Instant::now() + Duration::from_secs(5);
    while Instant::now() < deadline {
        if service_active() {
            return Ok(());
        }
        thread::sleep(Duration::from_millis(150));
    }
    Err("the background process did not come up (see the log)".into())
}

pub fn stop() -> Result<(), String> {
    if let Some(pid) = pid() {
        // /T also takes down rustmic, /F avoids a hung console-less process.
        let _ = crate::command("taskkill")
            .args(["/PID", &pid.to_string(), "/T", "/F"])
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .status();
        let deadline = Instant::now() + Duration::from_secs(3);
        while Instant::now() < deadline && process_alive(pid) {
            thread::sleep(Duration::from_millis(100));
        }
    }
    let _ = fs::remove_file(config::pid_path());
    Ok(())
}

pub fn restart() -> Result<(), String> {
    let _ = stop();
    start()
}

// ---------------------------------------------------------------- autostart

fn autostart_command() -> Result<String, String> {
    let exe = env::current_exe().map_err(|error| format!("cannot find own executable: {error}"))?;
    Ok(format!("\"{}\" run", exe.display()))
}

fn autostart_installed() -> bool {
    crate::command("reg")
        .args(["query", RUN_KEY, "/v", SERVICE])
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .status()
        .map(|status| status.success())
        .unwrap_or(false)
}

pub fn install(enable: bool) -> Result<(), String> {
    if enable {
        let value = autostart_command()?;
        let status = crate::command("reg")
            .args([
                "add", RUN_KEY, "/v", SERVICE, "/t", "REG_SZ", "/d", &value, "/f",
            ])
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .status()
            .map_err(|error| format!("cannot run reg.exe: {error}"))?;
        if !status.success() {
            return Err("reg add failed".into());
        }
        println!("autostart registered: {SERVICE} = {value}");
        println!("settings: {}", config::settings_path().display());
        start()?;
        println!("started womic-mic in the background");
    } else {
        println!("autostart not registered (install-no-enable)");
        println!("start it with: womic-mic.exe run");
    }
    Ok(())
}

pub fn uninstall() -> Result<(), String> {
    let _ = stop();
    let _ = crate::command("reg")
        .args(["delete", RUN_KEY, "/v", SERVICE, "/f"])
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .status();
    println!("autostart entry removed");
    Ok(())
}

// ---------------------------------------------------------------- tools

pub fn test_mic() {
    wasapi::loopback_test();
}

pub fn open_log() {
    let path = config::log_path();
    let _ = crate::command("notepad").arg(&path).spawn();
}

pub fn open_settings_folder() {
    let _ = crate::command("explorer").arg(config::config_dir()).spawn();
}

pub fn rustmic_running() -> bool {
    tasklist(&["/FI", "IMAGENAME eq rustmic.exe", "/NH"]).contains("rustmic")
}

pub fn log_tail(lines: usize) -> String {
    logging::tail(lines)
}

pub fn status_text(settings: &Settings) -> String {
    let mut text = String::new();
    let alive = service_active();
    text.push_str(&format!(
        "supervisor: {}\n",
        match pid() {
            Some(pid) if alive => format!("running (pid {pid})"),
            _ => "stopped".to_string(),
        }
    ));
    text.push_str(&format!(
        "audio output ({}): {}\n",
        settings.device,
        if wasapi::device_present(&settings.device) {
            "present"
        } else {
            "absent"
        }
    ));
    let capture = wasapi::input_names()
        .into_iter()
        .find(|name| name.to_lowercase().contains("cable output"));
    text.push_str(&format!(
        "virtual microphone: {}\n",
        capture.unwrap_or_else(|| {
            "not found - install a virtual cable (VB-CABLE) and pick its Capture side in the app"
                .to_string()
        })
    ));
    text.push_str(&format!(
        "phone (adb): {}\n",
        match adb::usb_phone(&settings.adb) {
            Some(phone) => match phone.model {
                Some(model) => format!("connected ({model})"),
                None => "connected".to_string(),
            },
            None => "not connected".to_string(),
        }
    ));
    text.push_str(&format!(
        "client: {}\n",
        if rustmic_running() {
            "running"
        } else {
            "stopped"
        }
    ));
    text.push_str(&format!("configured volume: {}%\n", settings.volume));
    text.push_str(&format!(
        "autostart: {}\n",
        if autostart_installed() {
            "enabled"
        } else {
            "disabled"
        }
    ));
    text
}

pub fn diagnostics(settings: &Settings) -> String {
    let mut text = String::from("womic-mic diagnostics (windows)\n");
    text.push_str("--- dependencies ---\n");
    for (name, command) in [
        ("rustmic", "rustmic"),
        ("adb", settings.adb.as_str()),
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
    text.push_str("--- audio outputs ---\n");
    for name in wasapi::output_names() {
        text.push_str(&format!("{name}\n"));
    }
    text.push_str("--- audio inputs ---\n");
    for name in wasapi::input_names() {
        text.push_str(&format!("{name}\n"));
    }
    text.push_str(&status_text(settings));
    text.push_str(&format!("log: {}\n", config::log_path().display()));
    text.push_str("--- log ---\n");
    text.push_str(&log_tail(20));
    text
}

pub fn executable_available(command: &str) -> bool {
    let path = PathBuf::from(command);
    if path.components().count() > 1 {
        return path.is_file();
    }
    let Some(search) = env::var_os("PATH") else {
        return false;
    };
    let extensions = env::var("PATHEXT").unwrap_or_else(|_| ".EXE;.CMD;.BAT".to_string());
    env::split_paths(&search).any(|dir| {
        extensions.split(';').filter(|ext| !ext.is_empty()).any(|ext| {
            dir.join(format!("{command}{}", ext.to_lowercase()))
                .is_file()
                || dir.join(format!("{command}{ext}")).is_file()
        })
    })
}

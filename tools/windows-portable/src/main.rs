//! RustMIC — one-file Windows launcher.
//!
//! The finished `RustMIC-portable.exe` is this executable with a payload
//! container appended (see `pack.py`). On first run it extracts itself to
//! `%LOCALAPPDATA%\RustMIC`, installs VB-CABLE once (single UAC prompt) and
//! starts the daemon and the GUI. Every later run is portable: no setup, no
//! UAC, no downloads.
//!
//! Uses only `std` so it cross-compiles with rust-lld without extra crates.

#![windows_subsystem = "windows"]

use std::env;
use std::fs;
use std::io::Write;
use std::os::windows::process::CommandExt;
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};

const MARKER: &[u8] = b"RUSTMIC-PAYLOAD-V1\n";
const CREATE_NO_WINDOW: u32 = 0x0800_0000;

fn alert(message: &str) {
    let script = format!(
        "Add-Type -AssemblyName System.Windows.Forms; \
         [System.Windows.Forms.MessageBox]::Show('{}', 'RustMIC', 'OK', 'Information')",
        message.replace('\'', "''")
    );
    let _ = Command::new("powershell")
        .args(["-NoProfile", "-Command", &script])
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .status();
}

fn install_dir() -> PathBuf {
    env::var_os("LOCALAPPDATA")
        .map(PathBuf::from)
        .unwrap_or_else(|| PathBuf::from(r"C:\"))
        .join("RustMIC")
}

fn read_u32(bytes: &[u8], at: &mut usize) -> Option<u32> {
    let slice = bytes.get(*at..*at + 4)?;
    *at += 4;
    Some(u32::from_le_bytes(slice.try_into().ok()?))
}

fn read_u64(bytes: &[u8], at: &mut usize) -> Option<u64> {
    let slice = bytes.get(*at..*at + 8)?;
    *at += 8;
    Some(u64::from_le_bytes(slice.try_into().ok()?))
}

/// Parse the appended container and write every file, creating subdirectories.
fn extract(exe: &Path, target: &Path) -> Result<usize, String> {
    let bytes = fs::read(exe).map_err(|error| format!("не удалось прочитать {exe:?}: {error}"))?;
    let start = bytes
        .windows(MARKER.len())
        .position(|window| window == MARKER)
        .ok_or_else(|| "в exe нет встроенного контейнера (нужен RustMIC-portable.exe)".to_string())?
        + MARKER.len();
    let mut at = start;
    let count = read_u32(&bytes, &mut at).ok_or("битый контейнер: нет счётчика")?;
    let mut written = 0;
    for _ in 0..count {
        let name_len = read_u32(&bytes, &mut at).ok_or("битый контейнер: нет имени")? as usize;
        let name = std::str::from_utf8(
            bytes
                .get(at..at + name_len)
                .ok_or("битый контейнер: имя обрезано")?,
        )
        .map_err(|_| "битый контейнер: имя не UTF-8")?;
        at += name_len;
        let data_len = read_u64(&bytes, &mut at).ok_or("битый контейнер: нет размера")? as usize;
        let data = bytes
            .get(at..at + data_len)
            .ok_or("битый контейнер: данные обрезаны")?;
        at += data_len;

        // Names are relative POSIX-style paths from pack.py.
        let relative = PathBuf::from(name.replace('/', r"\"));
        let path = target.join(&relative);
        if let Some(parent) = path.parent() {
            fs::create_dir_all(parent)
                .map_err(|error| format!("не удалось создать {parent:?}: {error}"))?;
        }
        fs::write(&path, data)
            .map_err(|error| format!("не удалось записать {path:?}: {error}"))?;
        written += 1;
    }
    Ok(written)
}

fn run_quiet(program: &str, args: &[&str]) -> String {
    Command::new(program)
        .args(args)
        .creation_flags(CREATE_NO_WINDOW)
        .output()
        .map(|out| {
            let mut text = String::from_utf8_lossy(&out.stdout).into_owned();
            text.push_str(&String::from_utf8_lossy(&out.stderr));
            text
        })
        .unwrap_or_default()
}

fn cable_installed(dir: &Path) -> bool {
    let status = run_quiet(
        &dir.join("RustMIC.exe").to_string_lossy(),
        &["status"],
    );
    status.to_lowercase().contains("cable output")
}

fn install_cable(dir: &Path) {
    let installer = dir.join("VBCABLE_Setup_x64.exe");
    if !installer.is_file() {
        alert("В комплекте нет VBCABLE_Setup_x64.exe — виртуальный микрофон не будет доступен.");
        return;
    }
    // One UAC prompt, once. `-Wait` keeps this launcher from racing the
    // driver installation.
    let script = format!(
        "Start-Process -FilePath '{}' -Verb RunAs -Wait",
        installer.to_string_lossy().replace('\'', "''")
    );
    let _ = Command::new("powershell")
        .args(["-NoProfile", "-Command", &script])
        .status();
}

fn write_default_config(dir: &Path) {
    let path = dir.join("config.toml");
    if path.exists() {
        return;
    }
    // Wi-Fi = no cable. The GUI asks for the phone IP on first launch.
    let text = concat!(
        "# RustMIC settings (managed by RustMIC-gui)\n",
        "transport = \"wifi\"\n",
        "host = \"\"\n",
        "bt_address = \"\"\n",
        "denoise = true\n",
        "gate = 0.00\n",
        "volume = 100\n",
        "device = \"CABLE Input\"\n",
    );
    let _ = fs::write(path, text);
}

fn spawn(dir: &Path, program: &str, args: &[&str]) {
    let exe = dir.join(program);
    let mut command = Command::new(exe);
    command
        .args(args)
        .creation_flags(CREATE_NO_WINDOW)
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null());
    if let Err(error) = command.spawn() {
        alert(&format!("Не удалось запустить {program}: {error}"));
    }
}

fn main() {
    let exe = match env::current_exe() {
        Ok(exe) => exe,
        Err(error) => {
            alert(&format!("Не удалось найти свой exe: {error}"));
            return;
        }
    };
    let dir = install_dir();
    if let Err(error) = fs::create_dir_all(&dir) {
        alert(&format!("Не удалось создать {dir:?}: {error}"));
        return;
    }
    // mpv resolves yt-dlp and its DLLs from PATH; the portable directory must
    // come first so we never pick up a stale system copy.
    let path = env::var("PATH").unwrap_or_default();
    env::set_var("PATH", format!("{};{}", dir.display(), path));

    let first_run = !dir.join("RustMIC.exe").exists();
    match extract(&exe, &dir) {
        Ok(count) => {
            let mut note = format!("RustMIC распакован: {count} файлов в {dir:?}");
            let mut need_note = first_run;
            if !cable_installed(&dir) {
                install_cable(&dir);
                note.push_str("\n\nВиртуальный кабель установлен. Если приложения не видят CABLE Output, перезагрузитесь.");
                need_note = true;
            }
            write_default_config(&dir);
            if need_note {
                alert(&note);
            }
        }
        Err(error) => {
            alert(&error);
            return;
        }
    }

    // The GUI is always available; the daemon starts only when the chosen
    // transport is fully configured (Wi-Fi needs the phone IP in the GUI).
    spawn(&dir, "RustMIC-gui.exe", &[]);
    spawn(&dir, "RustMIC.exe", &["run"]);
    let _ = std::io::stdout().flush();
}

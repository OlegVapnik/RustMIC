//! Music-to-microphone: play a track from YouTube / YouTube Music / SoundCloud
//! (or any URL mpv understands) straight into the virtual mic.
//!
//! Linux:  mpv -> `rustmic_music` null sink
//!                 ├── loopback -> `rustmic_sink` (apps hear it via `rustmic_mic`)
//!                 └── loopback -> default sink (you hear it in headphones)
//! Windows:mpv -> the same WASAPI cable endpoint the phone stream uses.
//!
//! No files are downloaded manually: a plain name becomes `ytsearch1:<name>`
//! and mpv/yt-dlp resolve it on the fly.

use std::fs::{self, File};
#[cfg(unix)]
use std::fs::OpenOptions;
use std::path::PathBuf;
#[cfg(unix)]
use std::process::Command;
use std::process::Stdio;

#[cfg(unix)]
use crate::config::SINK;
use crate::config::{self, Settings, MUSIC_SINK};
use crate::{command, logging};

fn cache(name: &str) -> PathBuf {
    config::cache_dir().join(name)
}

fn pid_path() -> PathBuf {
    cache("music.pid")
}

fn query_path() -> PathBuf {
    cache("music.query")
}

#[cfg(unix)]
fn loopback_path() -> PathBuf {
    cache("music.loopback")
}

fn log_path() -> PathBuf {
    cache("music.log")
}

fn mpv_program() -> PathBuf {
    let bundled = config::program_dir().join(if cfg!(windows) { "mpv.exe" } else { "mpv" });
    if bundled.is_file() {
        bundled
    } else if cfg!(windows) {
        PathBuf::from("mpv.exe")
    } else {
        PathBuf::from("mpv")
    }
}

fn is_url(query: &str) -> bool {
    let lower = query.to_lowercase();
    lower.starts_with("http://")
        || lower.starts_with("https://")
        || lower.contains("://")
        || lower.starts_with("ytsearch")
        || lower.starts_with("scsearch")
        || lower.starts_with("youtube.com/")
        || lower.starts_with("youtu.be/")
        || lower.starts_with("soundcloud.com/")
}

/// Drop playlist/radio/timestamp junk from YouTube links: a single track is
/// what we want, and `?list=RD...&start_radio=1` makes yt-dlp parse a whole
/// radio playlist before playback starts (tens of seconds of silence).
fn clean_url(query: &str) -> String {
    let lower = query.to_lowercase();

    if let Some(pos) = lower.find("youtube.com/watch?") {
        let params = &query[pos + "youtube.com/watch?".len()..];
        for pair in params.split('&') {
            let mut kv = pair.splitn(2, '=');
            if kv.next() == Some("v") {
                if let Some(id) = kv.next() {
                    let id = id.split('#').next().unwrap_or(id);
                    if !id.is_empty() {
                        return format!("https://www.youtube.com/watch?v={id}");
                    }
                }
            }
        }
    }

    if lower.contains("youtu.be/") || lower.contains("youtube.com/shorts/") {
        let marker = if lower.contains("youtu.be/") {
            "youtu.be/"
        } else {
            "youtube.com/shorts/"
        };
        if let Some(pos) = lower.find(marker) {
            let rest = &query[pos + marker.len()..];
            let id = rest.split(['?', '&', '#']).next().unwrap_or("");
            if !id.is_empty() {
                return format!("https://youtu.be/{id}");
            }
        }
    }

    query.trim().to_string()
}

fn resolve_query(query: &str) -> String {
    let query = clean_url(query);
    if is_url(&query) {
        query
    } else {
        format!("ytsearch1:{query}")
    }
}

fn pid() -> Option<i32> {
    fs::read_to_string(pid_path())
        .ok()
        .and_then(|text| text.trim().parse().ok())
}

fn alive(pid: i32) -> bool {
    #[cfg(unix)]
    {
        unsafe { libc::kill(pid, 0) == 0 }
    }
    #[cfg(windows)]
    {
        let listing = command("tasklist")
            .args(["/FI", &format!("PID eq {pid}"), "/NH", "/FO", "CSV"])
            .output()
            .ok()
            .map(|out| String::from_utf8_lossy(&out.stdout).into_owned())
            .unwrap_or_default();
        listing.contains(&format!("\"{pid}\""))
    }
}

fn kill_pid(pid: i32) {
    #[cfg(unix)]
    {
        unsafe {
            libc::kill(pid, libc::SIGTERM);
        }
    }
    #[cfg(windows)]
    {
        let _ = command("taskkill")
            .args(["/PID", &pid.to_string(), "/T", "/F"])
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .status();
    }
}

/// Load the two monitor loopbacks and remember their module indices.
#[cfg(unix)]
fn link(on: bool) {
    if on {
        unlink();
        crate::audio::linux::ensure_devices();
        for (source, sink, latency) in [
            (&format!("{MUSIC_SINK}.monitor"), SINK, "25"),
            (
                &format!("{MUSIC_SINK}.monitor"),
                "@DEFAULT_SINK@",
                "30",
            ),
        ] {
            let spec = format!("source={source} sink={sink} latency_msec={latency}");
            let output = Command::new("pactl")
                .args(["load-module", "module-loopback", &spec])
                .output();
            if let Ok(output) = output {
                let index = String::from_utf8_lossy(&output.stdout).trim().to_string();
                if !index.is_empty() {
                    if let Ok(mut file) = OpenOptions::new()
                        .create(true)
                        .append(true)
                        .open(loopback_path())
                    {
                        use std::io::Write;
                        let _ = writeln!(file, "{index}");
                    }
                }
            }
        }
    } else {
        unlink();
    }
}

#[cfg(unix)]
fn unlink() {
    let Ok(text) = fs::read_to_string(loopback_path()) else {
        return;
    };
    for index in text.lines().filter(|line| !line.trim().is_empty()) {
        let _ = Command::new("pactl")
            .args(["unload-module", index.trim()])
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .status();
    }
    let _ = fs::remove_file(loopback_path());
}

#[cfg(windows)]
fn link(_on: bool) {}
#[cfg(windows)]
#[allow(dead_code)]
fn unlink() {}

/// Start playback. Stops the previous track first, so one hotkey = "replace".
pub fn play(settings: &Settings, query: &str) -> Result<String, String> {
    let query = query.trim();
    if query.is_empty() {
        return Err("пустая ссылка или название".into());
    }
    let _ = stop();

    let _ = fs::create_dir_all(config::cache_dir());
    let log = File::create(log_path()).map_err(|error| format!("music log: {error}"))?;
    let err_log = log.try_clone().map_err(|error| format!("music log: {error}"))?;

    let audio_device = if cfg!(windows) {
        format!("wasapi/{}", settings.device)
    } else {
        format!("pulse/{MUSIC_SINK}")
    };

    let child = command(mpv_program())
        .args([
            "--no-config",
            "--no-video",
            "--input-terminal=no",
            "--loop-file=no",
            "--keep-open=no",
            "--force-window=no",
            "--cache=yes",
            "--cache-secs=30",
            "--demuxer-readahead-secs=20",
            "--msg-level=all=warn",
        ])
        .arg(format!("--audio-device={audio_device}"))
        .arg(resolve_query(query))
        .stdin(Stdio::null())
        .stdout(Stdio::from(log))
        .stderr(Stdio::from(err_log))
        .spawn()
        .map_err(|error| format!("mpv not found or failed to start: {error}"))?;

    let child_pid = child.id() as i32;
    // Detach: dropping Child would leave a zombie until this process exits.
    #[cfg(unix)]
    std::mem::forget(child);

    let _ = fs::write(pid_path(), child_pid.to_string());
    let _ = fs::write(query_path(), query);
    link(true);
    let message = format!("играет: {query}");
    logging::log(&message);
    Ok(message)
}

/// Stop playback and remove the monitor loopbacks.
pub fn stop() -> Result<(), String> {
    if let Some(pid) = pid() {
        if alive(pid) {
            kill_pid(pid);
        }
    }
    let _ = fs::remove_file(pid_path());
    let _ = fs::remove_file(query_path());
    link(false);
    logging::log("music stopped");
    Ok(())
}

/// Currently playing query, if the mpv process is still alive.
pub fn now_playing() -> Option<String> {
    let pid = pid()?;
    if !alive(pid) {
        let _ = fs::remove_file(pid_path());
        let _ = fs::remove_file(query_path());
        unlink();
        return None;
    }
    fs::read_to_string(query_path()).ok().map(|text| {
        let text = text.trim().to_string();
        if text.is_empty() { "неизвестно".into() } else { text }
    })
}

#[cfg(test)]
mod tests {
    use super::{is_url, resolve_query};

    #[test]
    fn plain_names_become_youtube_search() {
        assert_eq!(resolve_query("  Harvest Moon  "), "ytsearch1:Harvest Moon");
    }

    #[test]
    fn links_are_passed_through() {
        assert!(is_url("https://youtu.be/dQw4w9WgXcQ"));
        assert!(is_url("https://soundcloud.com/foo/bar"));
        assert_eq!(
            resolve_query("https://youtu.be/x"),
            "https://youtu.be/x"
        );
    }

    #[test]
    fn youtube_playlist_junk_is_stripped() {
        assert_eq!(
            resolve_query("https://www.youtube.com/watch?v=u_WMfX-8toE&list=RDu_WMfX-8toE&start_radio=1"),
            "https://www.youtube.com/watch?v=u_WMfX-8toE"
        );
        assert_eq!(
            resolve_query("https://youtu.be/abc123?t=42"),
            "https://youtu.be/abc123"
        );
        assert_eq!(
            resolve_query("https://www.youtube.com/shorts/abc123?feature=share"),
            "https://youtu.be/abc123"
        );
    }
}

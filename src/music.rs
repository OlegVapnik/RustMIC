//! Music-to-microphone: play a track from YouTube / YouTube Music / SoundCloud
//! (or any URL mpv understands) straight into the virtual mic.
//!
//! Linux:  mpv -> `rustmic_music` null sink
//!                 ├── loopback -> `rustmic_sink` (apps hear it via `rustmic_mic`)
//!                 └── loopback -> default sink (you hear it in headphones)
//! Windows:mpv -> the same WASAPI cable endpoint the phone stream uses.
//!
//! No files are downloaded manually: a plain name becomes `ytsearch1:<name>`
//! and mpv/yt-dlp resolve it on the fly. Playback is controlled through mpv's
//! own IPC socket, so title/duration/position/pause/seek come from mpv itself;
//! `yt-dlp -J` supplies metadata (title, cover URL) in the background.

use std::fs::{self, File};
#[cfg(unix)]
use std::fs::OpenOptions;
use std::io::{self, BufRead, BufReader, Write};
use std::path::PathBuf;
#[cfg(unix)]
use std::process::Command;
use std::process::{Child, Stdio};
use std::sync::{OnceLock, RwLock};
use std::thread;
use std::time::Duration;

use interprocess::local_socket::prelude::*;
#[cfg(windows)]
use interprocess::local_socket::GenericNamespaced;
#[cfg(unix)]
use interprocess::local_socket::GenericFilePath;
use interprocess::local_socket::{ConnectOptions, Stream as IpcStream};
use serde::{Deserialize, Serialize};
use serde_json::{json, Value};

#[cfg(unix)]
use crate::config::SINK;
use crate::config::{self, GateMode, Settings, MUSIC_SINK};
use crate::core::TrackState;
use crate::mixer;
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

#[cfg(unix)]
fn monitor_path() -> PathBuf {
    cache("music.monitor.loopback")
}

fn log_path() -> PathBuf {
    cache("music.log")
}

fn meta_path() -> PathBuf {
    cache("music.meta.json")
}

#[cfg(unix)]
fn mpv_socket_path() -> PathBuf {
    cache("mpv.sock")
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

fn yt_dlp_program() -> PathBuf {
    let bundled = config::program_dir().join(if cfg!(windows) {
        "yt-dlp.exe"
    } else {
        "yt-dlp"
    });
    if bundled.is_file() {
        bundled
    } else if cfg!(windows) {
        PathBuf::from("yt-dlp.exe")
    } else {
        PathBuf::from("yt-dlp")
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
    // Local files (soundpad tiles, drag&drop, CLI paths) must not be sent to
    // yt-dlp as a search term.
    let local = query.starts_with('/')
        || query.starts_with("./")
        || query.starts_with("../")
        || query.starts_with("file:")
        || (cfg!(windows) && query.len() > 1 && query.as_bytes()[1] == b':')
        || PathBuf::from(&query).is_file();
    if local || is_url(&query) {
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
        if unsafe { libc::kill(pid, 0) } != 0 {
            return false;
        }
        // kill(2) also succeeds for zombies. mpv is reaped by us (see `reap`),
        // but between its exit and the next poll a zombie must not look alive.
        let Ok(stat) = fs::read_to_string(format!("/proc/{pid}/stat")) else {
            return true;
        };
        let Some((_, rest)) = stat.rsplit_once(')') else {
            return true;
        };
        rest.trim_start().chars().next().map(|c| c != 'Z').unwrap_or(true)
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

/// Reap a detached mpv child so it does not stay in the process table.
#[cfg(unix)]
fn reap(pid: i32) {
    let mut status = 0;
    unsafe {
        libc::waitpid(pid, &mut status, libc::WNOHANG);
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

fn playing() -> bool {
    pid().map(alive).unwrap_or(false)
}

// ---------------------------------------------------------------- runtime

#[derive(Clone)]
struct Runtime {
    music_volume: u32,
    ducking: bool,
    ducking_db: f32,
    loudnorm: bool,
    monitor_music: bool,
    gate_mode: GateMode,
    gate_voice: u32,
    gate_level_db: f32,
}

impl From<&Settings> for Runtime {
    fn from(settings: &Settings) -> Self {
        Self {
            music_volume: settings.music_volume,
            ducking: settings.ducking,
            ducking_db: settings.ducking_db,
            loudnorm: settings.loudnorm,
            monitor_music: settings.monitor_music,
            gate_mode: settings.gate_mode,
            gate_voice: settings.gate_voice,
            gate_level_db: settings.gate_level_db,
        }
    }
}

fn runtime() -> &'static RwLock<Runtime> {
    static RUNTIME: OnceLock<RwLock<Runtime>> = OnceLock::new();
    RUNTIME.get_or_init(|| RwLock::new(Runtime::from(&Settings::default())))
}

/// Called by the daemon whenever settings change. Reconfigures ducking and,
/// if a track is playing, adds/removes the speaker monitor loopback.
pub fn configure(settings: &Settings) {
    let next = Runtime::from(settings);
    let previous_monitor = {
        let mut current = runtime().write().unwrap_or_else(|e| e.into_inner());
        let previous = current.monitor_music;
        *current = next.clone();
        previous
    };
    if playing() && previous_monitor != next.monitor_music {
        #[cfg(unix)]
        set_monitor(next.monitor_music);
    }
}

/// Start the background tick that applies music volume / VAD ducking.
pub fn start_control() {
    static STARTED: OnceLock<()> = OnceLock::new();
    if STARTED.set(()).is_err() {
        return;
    }
    thread::Builder::new()
        .name("RustMIC-music".to_string())
        .spawn(|| {
            while !crate::stopping() {
                if playing() {
                    apply_volume();
                    thread::sleep(Duration::from_millis(200));
                } else {
                    thread::sleep(Duration::from_millis(500));
                }
            }
        })
        .ok();
}

fn gate_is_active(current: &Runtime) -> bool {
    match current.gate_mode {
        GateMode::Voice => current.gate_voice > 0,
        GateMode::Level => current.gate_level_db < 0.0,
    }
}

/// Music is ducked while the microphone gate is open, and also while the
/// external translator is speaking. The latter is independent of gate mode so
/// ducking still works when VAD is disabled.
fn effective_volume(current: &Runtime, gate_open: bool, translate_active: bool) -> u32 {
    let duck = current.ducking
        && ((gate_is_active(current) && gate_open) || translate_active);
    if duck {
        let factor = 10f32.powf(-current.ducking_db / 20.0);
        (current.music_volume as f32 * factor).round() as u32
    } else {
        current.music_volume
    }
}

fn apply_volume() {
    let current = runtime().read().map(|r| r.clone()).unwrap_or_else(|e| e.into_inner().clone());
    let gate_open = crate::level::read_meter().gate_open;
    let translate_active = mixer::status().active;
    let value = effective_volume(&current, gate_open, translate_active);
    let _ = mpv_request(&json!(["set_property", "volume", value]));
}

// ---------------------------------------------------------------- mpv IPC

fn mpv_ipc_arg() -> String {
    #[cfg(unix)]
    {
        format!("--input-ipc-server={}", mpv_socket_path().display())
    }
    #[cfg(windows)]
    {
        "--input-ipc-server=\\\\.\\pipe\\RustMIC-mpv".to_string()
    }
}

fn connect_mpv() -> io::Result<IpcStream> {
    #[cfg(unix)]
    let socket = mpv_socket_path();
    #[cfg(unix)]
    let name = socket.as_os_str().to_fs_name::<GenericFilePath>()?;
    #[cfg(windows)]
    let name = "RustMIC-mpv".to_ns_name::<GenericNamespaced>()?;
    let stream = ConnectOptions::new().name(name).connect_sync()?;
    stream.set_recv_timeout(Some(Duration::from_millis(400)))?;
    Ok(stream)
}

/// One request/response against mpv's JSON IPC. Events are skipped until the
/// reply with our `request_id` arrives.
fn mpv_request(cmd: &Value) -> Option<Value> {
    static NEXT_ID: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(1);
    use std::sync::atomic::Ordering;

    let stream = connect_mpv().ok()?;
    let mut reader = BufReader::new(stream);
    let id = NEXT_ID.fetch_add(1, Ordering::Relaxed);
    let mut line = serde_json::to_string(&json!({ "command": cmd, "request_id": id })).ok()?;
    line.push('\n');
    reader.get_mut().write_all(line.as_bytes()).ok()?;

    let mut reply = String::new();
    while reader.read_line(&mut reply).ok()? > 0 {
        let value: Value = match serde_json::from_str(reply.trim()) {
            Ok(value) => value,
            Err(_) => {
                reply.clear();
                continue;
            }
        };
        if value.get("request_id").and_then(Value::as_u64) == Some(id) {
            if value.get("error").and_then(Value::as_str) == Some("success") {
                return value.get("data").cloned();
            }
            return None;
        }
        reply.clear();
    }
    None
}

fn mpv_string(property: &str) -> Option<String> {
    mpv_request(&json!(["get_property", property]))?
        .as_str()
        .map(str::to_string)
}

fn mpv_number(property: &str) -> Option<f64> {
    mpv_request(&json!(["get_property", property]))?.as_f64()
}

fn mpv_bool(property: &str) -> Option<bool> {
    mpv_request(&json!(["get_property", property]))?.as_bool()
}

/// Pause / resume the current track.
pub fn set_paused(paused: bool) -> bool {
    mpv_request(&json!(["set_property", "pause", paused])).is_some()
}

/// Relative seek in seconds (negative rewinds).
pub fn seek(seconds: f64) -> bool {
    mpv_request(&json!(["seek", seconds, "relative"])).is_some()
}

// ---------------------------------------------------------------- metadata

#[derive(Clone, Debug, Default, Serialize, Deserialize)]
struct Meta {
    query: String,
    title: String,
    duration: f64,
    cover: Option<String>,
}

fn write_meta(meta: &Meta) {
    if let Ok(text) = serde_json::to_string(meta) {
        let _ = fs::write(meta_path(), text);
    }
}

fn read_meta() -> Meta {
    fs::read_to_string(meta_path())
        .ok()
        .and_then(|text| serde_json::from_str(&text).ok())
        .unwrap_or_default()
}

fn cover_from(value: &Value) -> Option<String> {
    if let Some(url) = value.get("thumbnail").and_then(Value::as_str) {
        return Some(url.to_string());
    }
    let thumbnails = value.get("thumbnails")?.as_array()?;
    thumbnails
        .iter()
        .filter_map(|item| item.get("url").and_then(Value::as_str))
        .last()
        .map(str::to_string)
}

/// Save the remote thumbnail locally. The GUI only enables egui's file loader
/// (no network stack), so a `file://` URI is all it needs to draw the cover.
fn download_cover(url: &str) -> Option<String> {
    let path = cache("cover.jpg");
    let status = command("curl")
        .args([
            "-L",
            "-sS",
            "--max-time",
            "15",
            "-o",
            &path.to_string_lossy(),
            url,
        ])
        .stdin(Stdio::null())
        .stderr(Stdio::null())
        .status()
        .ok()?;
    if !status.success() || !path.is_file() {
        return None;
    }
    #[cfg(unix)]
    {
        Some(format!("file://{}", path.display()))
    }
    #[cfg(windows)]
    {
        Some(format!(
            "file:///{}",
            path.display().to_string().replace('\\', "/")
        ))
    }
}

/// Fetch title/duration/cover without blocking playback. This is a detached
/// helper: if yt-dlp is slow or offline, mpv keeps playing.
fn fetch_metadata(query: String, target: String) {
    thread::Builder::new()
        .name("RustMIC-meta".to_string())
        .spawn(move || {
            let output = command(yt_dlp_program())
                .args(["-J", "--no-warnings", "--no-playlist", &target])
                .stdin(Stdio::null())
                .stderr(Stdio::null())
                .output();
            let Ok(output) = output else { return };
            if !output.status.success() {
                return;
            }
            let Ok(value) = serde_json::from_slice::<Value>(&output.stdout) else {
                return;
            };
            let title = value
                .get("title")
                .and_then(Value::as_str)
                .unwrap_or(&query)
                .to_string();
            let duration = value
                .get("duration")
                .and_then(Value::as_f64)
                .unwrap_or(0.0);
            write_meta(&Meta {
                query,
                title,
                duration,
                cover: cover_from(&value).and_then(|url| download_cover(&url)),
            });
        })
        .ok();
}

/// Full track state: mpv owns position/pause, yt-dlp metadata owns title/cover.
pub fn status() -> Option<TrackState> {
    let query = now_playing()?;
    let meta = read_meta();
    let position = mpv_number("time-pos").unwrap_or(0.0);
    let duration = mpv_number("duration").filter(|d| *d > 0.0).unwrap_or(meta.duration);
    let paused = mpv_bool("pause").unwrap_or(false);
    let title = mpv_string("media-title")
        .filter(|title| !title.is_empty())
        .or_else(|| mpv_string("metadata/title"))
        .filter(|title| !title.is_empty())
        .unwrap_or_else(|| if meta.title.is_empty() { query.clone() } else { meta.title.clone() });
    let cover = meta.cover.or_else(|| mpv_string("metadata/artwork_url"));
    Some(TrackState {
        query,
        title,
        duration,
        position,
        paused,
        cover,
    })
}

// ---------------------------------------------------------------- loopbacks

/// Load one monitor loopback and remember its module index.
#[cfg(unix)]
fn load_loopback(source: &str, sink: &str, latency: &str, path: PathBuf) {
    let spec = format!("source={source} sink={sink} latency_msec={latency}");
    let output = Command::new("pactl")
        .args(["load-module", "module-loopback", &spec])
        .output();
    if let Ok(output) = output {
        let index = String::from_utf8_lossy(&output.stdout).trim().to_string();
        if !index.is_empty() {
            if let Ok(mut file) = OpenOptions::new().create(true).append(true).open(path) {
                use std::io::Write;
                let _ = writeln!(file, "{index}");
            }
        }
    }
}

#[cfg(unix)]
fn unload_saved(path: &PathBuf) {
    let Ok(text) = fs::read_to_string(path) else {
        return;
    };
    for index in text.lines().filter(|line| !line.trim().is_empty()) {
        let _ = Command::new("pactl")
            .args(["unload-module", index.trim()])
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .status();
    }
    let _ = fs::remove_file(path);
}

#[cfg(unix)]
fn unload_loopbacks() {
    unload_saved(&loopback_path());
    unload_saved(&monitor_path());
}

#[cfg(unix)]
fn set_monitor(on: bool) {
    if !on {
        unload_saved(&monitor_path());
        return;
    }
    if monitor_path().exists() {
        return;
    }
    crate::audio::linux::ensure_devices();
    load_loopback(
        &format!("{MUSIC_SINK}.monitor"),
        "@DEFAULT_SINK@",
        "30",
        monitor_path(),
    );
}

#[cfg(unix)]
fn link(on: bool, monitor: bool) {
    unload_loopbacks();
    if !on {
        return;
    }
    crate::audio::linux::ensure_devices();
    load_loopback(&format!("{MUSIC_SINK}.monitor"), SINK, "25", loopback_path());
    set_monitor(monitor);
}

#[cfg(windows)]
fn unload_loopbacks() {}
#[cfg(windows)]
fn link(_on: bool, _monitor: bool) {}

// ---------------------------------------------------------------- playback

fn spawn_mpv(args: &[String]) -> io::Result<Child> {
    let log = File::create(log_path())?;
    let err_log = log.try_clone()?;
    command(mpv_program())
        .args(args)
        .stdin(Stdio::null())
        .stdout(Stdio::from(log))
        .stderr(Stdio::from(err_log))
        .spawn()
}

/// Start playback. Stops the previous track first, so one hotkey = "replace".
pub fn play(settings: &Settings, query: &str) -> Result<String, String> {
    let query = query.trim();
    if query.is_empty() {
        return Err("пустая ссылка или название".into());
    }
    configure(settings);
    let _ = stop();

    let _ = fs::create_dir_all(config::cache_dir());
    let _ = fs::remove_file(meta_path());
    #[cfg(unix)]
    let _ = fs::remove_file(mpv_socket_path());

    let current = runtime().read().map(|r| r.clone()).unwrap_or_else(|e| e.into_inner().clone());
    let target = resolve_query(query);
    let audio_device = if cfg!(windows) {
        format!("wasapi/{}", settings.device)
    } else {
        format!("pulse/{MUSIC_SINK}")
    };

    let mut args: Vec<String> = [
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
    ]
    .iter()
    .map(|s| s.to_string())
    .collect();
    args.push(format!("--audio-device={audio_device}"));
    args.push(mpv_ipc_arg());
    args.push(format!("--volume={}", current.music_volume));
    if current.loudnorm {
        args.push("--af=lavfi=[loudnorm=I=-16:TP=-1.5:LRA=11]".to_string());
    }
    args.push(target.clone());

    let mut child = spawn_mpv(&args).map_err(|error| format!("mpv not found or failed to start: {error}"))?;
    // Diagnose "mpv exits immediately" failures (e.g. a missing audio device)
    // instead of leaving the user with an empty log and a stale track.
    thread::sleep(Duration::from_millis(350));
    if let Ok(Some(status)) = child.try_wait() {
        let detail = fs::read_to_string(log_path()).unwrap_or_default();
        logging::log(&format!(
            "mpv exited early ({status}) with args: {} | log: {}",
            args.join(" "),
            detail.trim()
        ));
        if !current.loudnorm {
            return Err(format!("mpv не запустился ({status}): {}", detail.trim().lines().last().unwrap_or("нет подробностей")));
        }
    }
    // A bad audio filter exits instantly; retry once without loudnorm rather
    // than leaving the user with silence and an empty log.
    if current.loudnorm {
        thread::sleep(Duration::from_millis(250));
        let exited = matches!(child.try_wait(), Ok(Some(_)));
        if exited {
            let _ = child.wait();
            logging::log("mpv: lavfi loudnorm unavailable, playing without it");
            args.retain(|arg| !arg.starts_with("--af="));
            child = spawn_mpv(&args)
                .map_err(|error| format!("mpv failed to start without loudnorm: {error}"))?;
        }
    }

    let child_pid = child.id() as i32;
    // Detach: dropping Child would leave a zombie until this process exits.
    #[cfg(unix)]
    std::mem::forget(child);

    let _ = fs::write(pid_path(), child_pid.to_string());
    let _ = fs::write(query_path(), query);
    link(true, current.monitor_music);

    write_meta(&Meta {
        query: query.to_string(),
        title: query.to_string(),
        duration: 0.0,
        cover: None,
    });
    fetch_metadata(query.to_string(), target);

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
        #[cfg(unix)]
        reap(pid);
    }
    let _ = fs::remove_file(pid_path());
    let _ = fs::remove_file(query_path());
    let _ = fs::remove_file(meta_path());
    #[cfg(unix)]
    let _ = fs::remove_file(mpv_socket_path());
    unload_loopbacks();
    logging::log("music stopped");
    Ok(())
}

/// Currently playing query, if the mpv process is still alive.
pub fn now_playing() -> Option<String> {
    let pid = pid()?;
    if !alive(pid) {
        #[cfg(unix)]
        reap(pid);
        let _ = fs::remove_file(pid_path());
        let _ = fs::remove_file(query_path());
        let _ = fs::remove_file(meta_path());
        #[cfg(unix)]
        let _ = fs::remove_file(mpv_socket_path());
        unload_loopbacks();
        return None;
    }
    fs::read_to_string(query_path()).ok().map(|text| {
        let text = text.trim().to_string();
        if text.is_empty() { "неизвестно".into() } else { text }
    })
}

#[cfg(test)]
mod tests {
    use super::{clean_url, effective_volume, is_url, resolve_query, Runtime, GateMode};

    #[test]
    fn plain_names_become_youtube_search() {
        assert_eq!(resolve_query("  Harvest Moon  "), "ytsearch1:Harvest Moon");
    }

    #[test]
    fn links_are_passed_through() {
        assert!(is_url("https://youtu.be/dQw4w9WgXcQ"));
        assert!(is_url("https://soundcloud.com/foo/bar"));
        assert_eq!(resolve_query("https://youtu.be/x"), "https://youtu.be/x");
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

    #[test]
    fn local_paths_are_not_searched_on_youtube() {
        assert_eq!(resolve_query("/tmp/tone.wav"), "/tmp/tone.wav");
        assert_eq!(resolve_query("./sounds/hit.mp3"), "./sounds/hit.mp3");
    }

    #[test]
    fn ducking_follows_gate_and_translation() {
        let mut rt = Runtime {
            music_volume: 100,
            ducking: true,
            ducking_db: 12.0,
            loudnorm: false,
            monitor_music: true,
            gate_mode: GateMode::Voice,
            gate_voice: 40,
            gate_level_db: -45.0,
        };
        assert!(effective_volume(&rt, true, false) < 100);
        assert_eq!(effective_volume(&rt, false, false), 100);
        assert!(effective_volume(&rt, false, true) < 100);
        rt.gate_voice = 0;
        assert_eq!(effective_volume(&rt, true, false), 100);
        assert!(effective_volume(&rt, true, true) < 100);
        rt.ducking = false;
        assert_eq!(effective_volume(&rt, true, true), 100);
    }

    #[test]
    fn clean_url_keeps_plain_names() {
        assert_eq!(clean_url(" Harvest Moon "), "Harvest Moon");
    }
}

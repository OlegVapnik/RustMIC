//! RustMIC — WO Mic (Android) as a system microphone.
//!
//! Pipeline:  rustmic (adb/USB transport, Opus -> PCM s16le 48k mono)
//!            -> RNNoise denoise (nnnoiseless)
//!            -> platform sink:
//!               Linux   pacat -> rustmic_sink -> (remap) -> source rustmic_mic
//!               Windows WASAPI -> virtual cable -> capture side in apps
//!
//! The supervisor restarts every stage on failure, re-checks adb and recreates
//! the virtual devices when the audio server forgets them, so the microphone
//! survives cable pulls, phone reboots and PipeWire restarts.

use std::env;
use std::fs;
use std::io::{self, Read, Write};
use std::process::Stdio;
use std::sync::atomic::Ordering;
use std::sync::mpsc::{channel, Receiver};
use std::sync::{Arc, RwLock};
use std::thread;
use std::time::{Duration, Instant};

use rustmic_mic::audio::{self, Sink};
use rustmic_mic::config::{self, GateMode, Settings};
use rustmic_mic::core::{AppState, Command};
use rustmic_mic::denoise::{Denoiser, Live};
use rustmic_mic::{adb, ipc, level, logging, music, platform, tiles, STOP};

fn install_signals() {
    #[cfg(unix)]
    unsafe {
        libc::signal(libc::SIGPIPE, libc::SIG_IGN);
    }
    // Ctrl+C on both platforms, and SIGTERM from systemd on Linux.
    let _ = ctrlc::set_handler(|| STOP.store(true, Ordering::SeqCst));
}

fn log(msg: &str) {
    logging::log(msg);
}

fn live_from(settings: &Settings, muted: bool) -> Live {
    Live {
        denoise: settings.denoise,
        gate_mode: settings.gate_mode,
        gate_voice: settings.gate_voice,
        gate_level_db: settings.gate_level_db,
        mic_volume: settings.mic_volume,
        muted,
    }
}

fn parse_percent(value: &str) -> u32 {
    value
        .trim()
        .trim_end_matches('%')
        .parse::<f32>()
        .map(|number| number.clamp(0.0, 100.0).round() as u32)
        .unwrap_or(100)
}

/// `--gate` keeps the v1 meaning (0..1 probability); values above 1 are percent.
fn parse_gate_voice(value: &str) -> u32 {
    value
        .trim()
        .trim_end_matches('%')
        .parse::<f32>()
        .map(|number| {
            if number <= 1.0 {
                (number.clamp(0.0, 1.0) * 100.0).round() as u32
            } else {
                number.clamp(0.0, 100.0).round() as u32
            }
        })
        .unwrap_or(0)
}

fn parse_db(value: &str) -> f32 {
    value
        .trim()
        .trim_end_matches("dB")
        .trim_end_matches("db")
        .parse::<f32>()
        .map(|number| number.clamp(-60.0, 0.0))
        .unwrap_or(-45.0)
}

fn parse_bool(value: &str) -> bool {
    matches!(
        value.trim().to_lowercase().as_str(),
        "1" | "true" | "yes" | "on"
    )
}

fn phone_status(settings: &Settings, client_running: bool) -> (String, bool) {
    match settings.transport.as_str() {
        "wifi" | "wifi-direct" => {
            let host = if settings.host.is_empty() {
                "адрес не задан"
            } else {
                settings.host.as_str()
            };
            (
                format!("телефон (Wi-Fi): {host}"),
                client_running && !settings.host.is_empty(),
            )
        }
        "bluetooth" => {
            let address = if settings.bt_address.is_empty() {
                "адрес не задан"
            } else {
                settings.bt_address.as_str()
            };
            (
                format!("телефон (Bluetooth): {address}"),
                client_running && !settings.bt_address.is_empty(),
            )
        }
        _ => match adb::usb_phone(&settings.adb) {
            Some(phone) => (
                match phone.model {
                    Some(model) => format!("телефон (USB): подключён ({model})"),
                    None => "телефон (USB): подключён".to_string(),
                },
                true,
            ),
            None => ("телефон (USB): не подключён".to_string(), false),
        },
    }
}

fn target_of(settings: &Settings) -> String {
    match settings.transport.as_str() {
        "wifi" | "wifi-direct" => settings.host.clone(),
        "bluetooth" => settings.bt_address.clone(),
        _ => settings.adb.clone(),
    }
}

fn update_live(live: &Arc<RwLock<Live>>, settings: &Settings, muted: bool) {
    if let Ok(mut current) = live.write() {
        *current = live_from(settings, muted);
    }
}

// ---------------------------------------------------------------- pump

/// Read a byte stream on its own thread so the supervisor can notice SIGTERM
/// while it is waiting for audio (a blocking `read` would swallow the signal).
fn reader(mut src: impl Read + Send + 'static) -> Receiver<io::Result<Vec<u8>>> {
    let (tx, rx) = channel();
    thread::spawn(move || {
        let mut buf = vec![0u8; 16 * 1024];
        loop {
            match src.read(&mut buf) {
                Ok(0) => {
                    let _ = tx.send(Ok(Vec::new()));
                    return;
                }
                Ok(n) => {
                    if tx.send(Ok(buf[..n].to_vec())).is_err() {
                        return;
                    }
                }
                Err(e) if e.kind() == io::ErrorKind::Interrupted => continue,
                Err(e) => {
                    let _ = tx.send(Err(e));
                    return;
                }
            }
        }
    });
    rx
}

fn pump(
    chunks: &Receiver<io::Result<Vec<u8>>>,
    sink: &mut dyn Sink,
    den: &mut Denoiser,
) -> io::Result<()> {
    let mut carry: Option<u8> = None;
    let mut samples: Vec<i16> = Vec::with_capacity(8192);
    let mut out: Vec<i16> = Vec::with_capacity(8192);
    loop {
        if STOP.load(Ordering::SeqCst) {
            return Ok(());
        }
        let raw = match chunks.recv_timeout(Duration::from_millis(200)) {
            Ok(Ok(raw)) => raw,
            Ok(Err(e)) => return Err(e),
            Err(std::sync::mpsc::RecvTimeoutError::Timeout) => continue,
            Err(std::sync::mpsc::RecvTimeoutError::Disconnected) => return Ok(()),
        };
        let n = raw.len();
        if n == 0 {
            return Ok(());
        }
        samples.clear();
        out.clear();
        let mut idx = 0;
        if let Some(b) = carry.take() {
            samples.push(i16::from_le_bytes([b, raw[0]]));
            idx = 1;
        }
        let mut rest = &raw[idx..n];
        if rest.len() % 2 == 1 {
            carry = Some(rest[rest.len() - 1]);
            rest = &rest[..rest.len() - 1];
        }
        samples.extend(
            rest.chunks(2)
                .map(|chunk| i16::from_le_bytes([chunk[0], chunk[1]])),
        );
        if samples.is_empty() {
            continue;
        }
        den.process(&samples, &mut out);
        sink.write(&out)?;
    }
}

// ---------------------------------------------------------------- session

/// Build the `rustmic` client arguments for the configured transport.
/// Wi-Fi and Bluetooth need no cable and no adb at all.
fn rustmic_args(cfg: &Settings) -> Result<Vec<String>, String> {
    let mut args = vec!["--transport".to_string(), cfg.transport.clone()];
    match cfg.transport.as_str() {
        "usb" => {
            args.extend(["--adb".to_string(), cfg.adb.clone()]);
        }
        "wifi" | "wifi-direct" => {
            if cfg.host.trim().is_empty() {
                return Err("Wi-Fi transport needs host = <phone IP> in config.toml".into());
            }
            args.extend(["--host".to_string(), cfg.host.clone()]);
        }
        "bluetooth" => {
            if cfg.bt_address.trim().is_empty() {
                return Err("bluetooth transport needs bt_address = <AA:BB:...>".into());
            }
            args.extend(["--bt-address".to_string(), cfg.bt_address.clone()]);
        }
        other => return Err(format!("unknown transport: {other}")),
    }
    // `--reconnect` defaults to 3 retries in rustmic 0.1.x; no need to pass it.
    args.push("--stdout".to_string());
    Ok(args)
}

fn session(cfg: &Settings, live: &Arc<RwLock<Live>>, stats: bool) -> Result<(), String> {
    let args = rustmic_args(cfg)?;
    if cfg.transport == "usb" {
        adb::forward(&cfg.adb);
    }

    let mut sink = audio::open(cfg, live)?;
    log(&format!("sink: {}", sink.describe()));

    let client = config::client_program();
    let mut mic = rustmic_mic::command(&client)
        .args(&args)
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::inherit())
        .spawn()
        .map_err(|error| format!("cannot start {client}: {error}"))?;

    let mut den = Denoiser::new(live.clone());
    den.stats = stats;
    let result = {
        let out = mic.stdout.take().expect("rustmic stdout");
        pump(&reader(out), sink.as_mut(), &mut den)
    };

    let _ = mic.kill();
    let _ = mic.wait();
    result.map_err(|error| format!("audio pump: {error}"))
}

// ---------------------------------------------------------------- run

fn run(cfg: Settings, stats: bool) -> i32 {
    install_signals();
    let _ = fs::create_dir_all(config::cache_dir());
    audio::prepare();
    log(&format!(
        "starting (transport={}, host={}, adb={}, device={}, denoise={}, gate_mode={}, gate_voice={}%, gate_level={:.1}dB, mic_volume={}%)",
        cfg.transport,
        cfg.host,
        cfg.adb,
        cfg.device,
        cfg.denoise,
        cfg.gate_mode.as_str(),
        cfg.gate_voice,
        cfg.gate_level_db,
        cfg.mic_volume
    ));

    let pidfile = config::pid_path();
    let _ = fs::write(&pidfile, std::process::id().to_string());
    level::clear();
    music::configure(&cfg);
    music::start_control();

    #[cfg(unix)]
    rustmic_mic::audio::linux::set_source_volume(cfg.mic_volume);

    let live = Arc::new(RwLock::new(live_from(&cfg, false)));
    let state = Arc::new(RwLock::new(AppState::from_settings(&cfg)));
    let (commands_tx, commands_rx) = channel::<(u64, Command)>();

    if let Err(error) = ipc::start_server(state.clone(), commands_tx) {
        log(&format!("ipc server failed: {error}"));
    }

    // Commands from the GUI / CLI: apply to Live immediately and persist the
    // configuration keys that belong in config.toml.
    {
        let live = live.clone();
        let state = state.clone();
        thread::spawn(move || {
            while !STOP.load(Ordering::SeqCst) {
                let (id, command) = match commands_rx.recv_timeout(Duration::from_millis(500)) {
                    Ok(request) => request,
                    Err(std::sync::mpsc::RecvTimeoutError::Timeout) => continue,
                    Err(std::sync::mpsc::RecvTimeoutError::Disconnected) => break,
                };
                let command_name = command.name().to_string();
                log(&format!("command {command_name} (id={id})"));
                let mut settings = Settings::load();
                let mut muted = live.read().map(|live| live.muted).unwrap_or(false);
                let mut save = false;
                match command {
                    Command::Ping | Command::GetState => {}
                    Command::SetMicVolume { value } => {
                        let value = value.min(100);
                        settings.mic_volume = value;
                        #[cfg(unix)]
                        rustmic_mic::audio::linux::set_source_volume(value);
                        save = true;
                    }
                    Command::SetMusicVolume { value } => {
                        settings.music_volume = value.min(100);
                        save = true;
                    }
                    Command::SetGateMode { mode } => {
                        settings.gate_mode = mode;
                        save = true;
                    }
                    Command::SetGateVoice { value } => {
                        settings.gate_voice = value.min(100);
                        save = true;
                    }
                    Command::SetGateLevel { value } => {
                        settings.gate_level_db = value.clamp(-60.0, 0.0);
                        save = true;
                    }
                    Command::SetDenoise { enabled } => {
                        settings.denoise = enabled;
                        save = true;
                    }
                    Command::SetMuted { muted: value } => muted = value,
                    Command::SetLoudnorm { enabled } => {
                        settings.loudnorm = enabled;
                        save = true;
                    }
                    Command::SetDucking { enabled } => {
                        settings.ducking = enabled;
                        save = true;
                    }
                    Command::SetDuckingDb { value } => {
                        settings.ducking_db = value.clamp(0.0, 30.0);
                        save = true;
                    }
                    Command::SetMonitor { enabled } => {
                        settings.monitor_music = enabled;
                        save = true;
                    }
                    Command::SetMonitorVolume { value } => {
                        settings.monitor_volume = value.min(100);
                        save = true;
                    }
                    Command::AddTile { title, query } => {
                        let list = tiles::add(&title, &query, None, 0.0);
                        log(&format!("tile added: {} ({})", title, list.len()));
                    }
                    Command::RemoveTile { tile_id } => {
                        tiles::remove(&tile_id);
                    }
                    Command::Play { query } => {
                        if let Err(error) = music::play(&settings, &query) {
                            log(&format!("play failed: {error}"));
                        }
                    }
                    Command::StopMusic => {
                        if let Err(error) = music::stop() {
                            log(&format!("stop-music failed: {error}"));
                        }
                    }
                    Command::Pause { paused } => {
                        if !music::set_paused(paused) {
                            log("pause: mpv IPC is not available");
                        }
                    }
                    Command::Seek { seconds } => {
                        if !music::seek(seconds) {
                            log("seek: mpv IPC is not available");
                        }
                    }
                    Command::Restart => {
                        if let Err(error) = platform::restart() {
                            log(&format!("restart failed: {error}"));
                        }
                    }
                    Command::StartService => {
                        if let Err(error) = platform::start() {
                            log(&format!("start failed: {error}"));
                        }
                    }
                    Command::StopService => {
                        if let Err(error) = platform::stop() {
                            log(&format!("stop failed: {error}"));
                        }
                    }
                }
                if save {
                    if let Err(error) = settings.save() {
                        log(&format!("cannot save settings: {error}"));
                    }
                }
                music::configure(&settings);
                update_live(&live, &settings, muted);
                if let Ok(mut state) = state.write() {
                    state.refresh(&settings);
                    state.audio.muted = muted;
                    state.last_command_id = id;
                }
                log(&format!("command {command_name} applied (id={id})"));
            }
        });
    }

    // Publish a fresh AppState for GUI clients even when nothing changes.
    {
        let state = state.clone();
        thread::spawn(move || {
            while !STOP.load(Ordering::SeqCst) {
                let settings = Settings::load();
                let client_running = platform::rustmic_running();
                let (phone, phone_connected) = phone_status(&settings, client_running);
                let sink_present: bool = {
                    #[cfg(unix)]
                    {
                        rustmic_mic::audio::linux::node_exists("sinks", config::SINK)
                    }
                    #[cfg(windows)]
                    {
                        rustmic_mic::audio::windows::device_present(&settings.device)
                    }
                };
                let source_present: bool = {
                    #[cfg(unix)]
                    {
                        rustmic_mic::audio::linux::node_exists("sources", config::SRC)
                    }
                    #[cfg(windows)]
                    {
                        rustmic_mic::audio::windows::input_names()
                            .iter()
                            .any(|name| name.to_lowercase().contains("cable output"))
                    }
                };
                #[cfg(unix)]
                let default_source = rustmic_mic::audio::linux::default_source();
                #[cfg(windows)]
                let default_source = String::new();

                if let Ok(mut state) = state.write() {
                    let muted = state.audio.muted;
                    // This state is only served by a live daemon, so the
                    // microphone service is by definition running.
                    state.service_running = true;
                    state.daemon_pid = std::process::id();
                    state.connection.transport = settings.transport.clone();
                    state.connection.target = target_of(&settings);
                    state.connection.phone = phone;
                    state.connection.phone_connected = phone_connected;
                    state.connection.client_running = client_running;
                    state.connection.sink_present = sink_present;
                    state.connection.source_present = source_present;
                    state.connection.default_source = default_source;
                    state.connection.latency_ms = None;
                    state.refresh(&settings);
                    state.audio.muted = muted;
                }
                thread::sleep(Duration::from_millis(500));
            }
        });
    }

    // Settings hot-reload: the GUI writes config.toml, the daemon picks it up
    // without a restart (a restart storm used to trip systemd's start limit).
    {
        let live = live.clone();
        thread::spawn(move || {
            let path = config::settings_path();
            let mut stamp = fs::metadata(&path).and_then(|m| m.modified()).ok();
            while !STOP.load(Ordering::SeqCst) {
                thread::sleep(Duration::from_secs(1));
                let now = fs::metadata(&path).and_then(|m| m.modified()).ok();
                if now == stamp {
                    continue;
                }
                stamp = now;
                let settings = Settings::load();
                let muted = live.read().map(|live| live.muted).unwrap_or(false);
                music::configure(&settings);
                update_live(&live, &settings, muted);
                #[cfg(unix)]
                rustmic_mic::audio::linux::set_source_volume(settings.mic_volume);
                log(&format!(
                    "settings reloaded: denoise={}, gate_mode={}, gate_voice={}%, mic_volume={}%",
                    settings.denoise,
                    settings.gate_mode.as_str(),
                    settings.gate_voice,
                    settings.mic_volume
                ));
            }
        });
    }

    let mut backoff = 1u64;
    let mut last_devices_check = Instant::now() - Duration::from_secs(60);

    while !STOP.load(Ordering::SeqCst) {
        // Only USB needs adb. Wi-Fi / Bluetooth are handled by rustmic's own
        // reconnect loop, so the supervisor never blocks on a missing cable.
        if cfg.transport == "usb" && last_devices_check.elapsed() >= Duration::from_secs(3) {
            last_devices_check = Instant::now();
            if !adb::phone_ready(&cfg.adb) {
                log("phone not connected over adb, waiting");
                thread::sleep(Duration::from_secs(3));
                continue;
            }
        }

        let started = Instant::now();
        match session(&cfg, &live, stats) {
            Ok(()) => log("session ended, reconnecting"),
            Err(error) => log(&format!("session error: {error}")),
        }
        if started.elapsed() > Duration::from_secs(60) {
            backoff = 1; // it ran fine for a while: no need to back off hard
        }
        if STOP.load(Ordering::SeqCst) {
            break;
        }
        thread::sleep(Duration::from_secs(backoff));
        backoff = (backoff * 2).min(30);
    }

    level::clear();
    ipc::cleanup();
    let _ = fs::remove_file(&pidfile);
    log("stopped");
    0
}

// ---------------------------------------------------------------- filter

/// `filter` writes plain s16le to stdout: stdin -> RNNoise -> stdout.
struct StdoutSink;

impl Sink for StdoutSink {
    fn write(&mut self, samples: &[i16]) -> io::Result<()> {
        let mut bytes = Vec::with_capacity(samples.len() * 2);
        for sample in samples {
            bytes.extend_from_slice(&sample.to_le_bytes());
        }
        let stdout = io::stdout();
        let mut lock = stdout.lock();
        lock.write_all(&bytes)?;
        lock.flush()
    }

    fn describe(&self) -> String {
        "stdout".into()
    }
}

fn filter(cfg: &Settings, stats: bool) -> i32 {
    let live = Arc::new(RwLock::new(live_from(cfg, false)));
    let mut den = Denoiser::new(live);
    den.stats = stats;
    match pump(&reader(io::stdin()), &mut StdoutSink, &mut den) {
        Ok(()) => 0,
        Err(error) => {
            log(&format!("filter error: {error}"));
            1
        }
    }
}

// ---------------------------------------------------------------- ctl

/// `RustMIC ctl ...` talks to the running daemon over the local socket.
fn ctl(args: &[String]) -> i32 {
    let action = args.first().map(String::as_str).unwrap_or("state");
    let value = args.get(1).map(String::as_str).unwrap_or("");
    let command = match action {
        "state" | "get" => Command::GetState,
        "ping" => Command::Ping,
        "mic" => match value.parse::<u32>() {
            Ok(value) => Command::SetMicVolume { value },
            Err(_) => {
                log("ctl mic: нужен процент 0..100");
                return 2;
            }
        },
        "music" => match value.parse::<u32>() {
            Ok(value) => Command::SetMusicVolume { value },
            Err(_) => {
                log("ctl music: нужен процент 0..100");
                return 2;
            }
        },
        "gate-mode" => Command::SetGateMode {
            mode: GateMode::parse(value),
        },
        "gate-voice" => Command::SetGateVoice {
            value: parse_gate_voice(value),
        },
        "gate-level" => Command::SetGateLevel {
            value: parse_db(value),
        },
        "denoise" => Command::SetDenoise {
            enabled: parse_bool(value),
        },
        "mute" => Command::SetMuted { muted: true },
        "unmute" => Command::SetMuted { muted: false },
        "loudnorm" => Command::SetLoudnorm {
            enabled: parse_bool(value),
        },
        "ducking" => Command::SetDucking {
            enabled: parse_bool(value),
        },
        "ducking-db" => Command::SetDuckingDb {
            value: value.parse::<f32>().unwrap_or(12.0).clamp(0.0, 30.0),
        },
        "tile-add" => {
            if args.len() < 3 {
                log("ctl tile-add: нужен заголовок и ссылка/название");
                return 2;
            }
            Command::AddTile {
                title: value.to_string(),
                query: args[2..].join(" "),
            }
        }
        "tile-remove" => Command::RemoveTile {
            tile_id: value.to_string(),
        },
        "monitor" => Command::SetMonitor {
            enabled: parse_bool(value),
        },
        "monitor-volume" => match value.parse::<u32>() {
            Ok(value) => Command::SetMonitorVolume { value },
            Err(_) => {
                log("ctl monitor-volume: нужен процент 0..100");
                return 2;
            }
        },
        "play" => {
            let query = args[1..].join(" ");
            if query.trim().is_empty() {
                log("ctl play: нужен URL или название");
                return 2;
            }
            Command::Play { query }
        }
        "stop-music" => Command::StopMusic,
        "pause" => Command::Pause {
            paused: parse_bool(if value.is_empty() { "true" } else { value }),
        },
        "seek" => match value.parse::<f64>() {
            Ok(seconds) => Command::Seek { seconds },
            Err(_) => {
                log("ctl seek: нужно число секунд");
                return 2;
            }
        },
        "restart" => Command::Restart,
        "start" => Command::StartService,
        "stop-service" => Command::StopService,
        other => {
            log(&format!("ctl: неизвестная команда {other}"));
            return 2;
        }
    };

    let mut client = match ipc::Client::connect() {
        Ok(client) => client,
        Err(error) => {
            log(&format!(
                "служба не запущена ({}): {error}",
                ipc::socket_display()
            ));
            return 1;
        }
    };
    match client.request(command) {
        Ok(state) => {
            if action == "state" || action == "get" {
                println!(
                    "{}",
                    serde_json::to_string_pretty(&state).unwrap_or_else(|_| "{}".to_string())
                );
            } else if action == "ping" {
                println!("pong (protocol {})", state.protocol);
            } else {
                println!("ok: {action}");
            }
            0
        }
        Err(error) => {
            log(&format!("ctl: request failed: {error}"));
            1
        }
    }
}

// ---------------------------------------------------------------- main

fn main() {
    let args: Vec<String> = env::args().skip(1).collect();
    let cmd = args.first().map(String::as_str).unwrap_or("run");
    let rest = if args.is_empty() { &args[..] } else { &args[1..] };

    // The whole remainder is the query for `play` (quotes are already joined
    // into one argument by the shell).
    let play_query = if cmd == "play" { rest.join(" ") } else { String::new() };

    let mut cfg = Settings::load();
    let mut stats = false;
    let mut it = rest.iter().peekable();
    while let Some(arg) = it.next() {
        match arg.as_str() {
            "--transport" => {
                if let Some(value) = it.next() {
                    cfg.transport = config::normalize_transport(value);
                }
            }
            "--host" => {
                if let Some(value) = it.next() {
                    cfg.host = value.clone();
                    if cfg.transport == "usb" {
                        cfg.transport = "wifi".to_string();
                    }
                }
            }
            "--bt-address" => {
                if let Some(value) = it.next() {
                    cfg.bt_address = value.clone();
                    cfg.transport = "bluetooth".to_string();
                }
            }
            "--adb" => {
                if let Some(value) = it.next() {
                    cfg.adb = value.clone();
                }
            }
            "--device" => {
                if let Some(value) = it.next() {
                    cfg.device = value.clone();
                }
            }
            "--denoise" => cfg.denoise = true,
            "--no-denoise" => cfg.denoise = false,
            "--stats" => stats = true,
            "--gate" => {
                if let Some(value) = it.next() {
                    cfg.gate_voice = parse_gate_voice(value);
                }
            }
            "--gate-mode" => {
                if let Some(value) = it.next() {
                    cfg.gate_mode = GateMode::parse(value);
                }
            }
            "--gate-voice" => {
                if let Some(value) = it.next() {
                    cfg.gate_voice = parse_gate_voice(value);
                }
            }
            "--gate-level" => {
                if let Some(value) = it.next() {
                    cfg.gate_level_db = parse_db(value);
                }
            }
            "--mic-volume" => {
                if let Some(value) = it.next() {
                    cfg.mic_volume = parse_percent(value);
                }
            }
            "--music-volume" => {
                if let Some(value) = it.next() {
                    cfg.music_volume = parse_percent(value);
                }
            }
            "--loudnorm" => cfg.loudnorm = true,
            "--no-loudnorm" => cfg.loudnorm = false,
            "--no-ducking" => cfg.ducking = false,
            "--monitor" => cfg.monitor_music = true,
            "--no-monitor" => cfg.monitor_music = false,
            other if other.starts_with("--gate=") => {
                cfg.gate_voice = parse_gate_voice(&other[7..]);
            }
            other if other.starts_with("--gate-mode=") => {
                cfg.gate_mode = GateMode::parse(&other[12..]);
            }
            other if other.starts_with("--gate-voice=") => {
                cfg.gate_voice = parse_gate_voice(&other[13..]);
            }
            other if other.starts_with("--gate-level=") => {
                cfg.gate_level_db = parse_db(&other[13..]);
            }
            other if other.starts_with("--mic-volume=") => {
                cfg.mic_volume = parse_percent(&other[13..]);
            }
            other if other.starts_with("--music-volume=") => {
                cfg.music_volume = parse_percent(&other[15..]);
            }
            other if other.starts_with("--transport=") => {
                cfg.transport = config::normalize_transport(&other[12..]);
            }
            other if other.starts_with("--host=") => {
                cfg.host = other[7..].to_string();
                if cfg.transport == "usb" {
                    cfg.transport = "wifi".to_string();
                }
            }
            other if other.starts_with("--bt-address=") => {
                cfg.bt_address = other[13..].to_string();
                cfg.transport = "bluetooth".to_string();
            }
            other if other.starts_with("--adb=") => cfg.adb = other[6..].to_string(),
            other if other.starts_with("--device=") => cfg.device = other[9..].to_string(),
            _ => {}
        }
    }

    let code = match cmd {
        "run" => run(cfg, stats),
        "status" => {
            print!("{}", platform::status_text(&cfg));
            if platform::service_active() {
                0
            } else {
                1
            }
        }
        "settings" => {
            println!("transport: {}", cfg.transport);
            if cfg.transport == "wifi" || cfg.transport == "wifi-direct" {
                println!("host: {}", cfg.host);
            } else if cfg.transport == "bluetooth" {
                println!("bt-address: {}", cfg.bt_address);
            }
            println!("denoise: {}", cfg.denoise);
            println!("gate_mode: {}", cfg.gate_mode.as_str());
            println!("gate_voice: {}%", cfg.gate_voice);
            println!("gate_level: {:.1} dB", cfg.gate_level_db);
            println!("mic_volume: {}%", cfg.mic_volume);
            println!("music_volume: {}%", cfg.music_volume);
            println!("monitor_volume: {}%", cfg.monitor_volume);
            println!("loudnorm: {}", cfg.loudnorm);
            println!("ducking: {} ({:.1} dB)", cfg.ducking, cfg.ducking_db);
            println!("monitor_music: {}", cfg.monitor_music);
            if cfg!(windows) {
                println!("device: {}", cfg.device);
            }
            println!("adb: {}", cfg.adb);
            println!("socket: {}", ipc::socket_display());
            println!("config: {}", config::settings_path().display());
            println!("log: {}", config::log_path().display());
            0
        }
        "ctl" => ctl(rest),
        "play" => match music::play(&cfg, &play_query) {
            Ok(message) => {
                println!("{message}");
                0
            }
            Err(error) => {
                log(&error);
                1
            }
        },
        "stop-music" => report(music::stop()),
        "now-playing" => match music::now_playing() {
            Some(query) => {
                println!("{query}");
                0
            }
            None => {
                println!("ничего не играет");
                1
            }
        },
        "filter" => filter(&cfg, stats),
        "install" => report(platform::install(true)),
        "install-no-enable" => report(platform::install(false)),
        "uninstall" => report(platform::uninstall()),
        "start" => report(platform::start()),
        "stop" => report(platform::stop()),
        "restart" => report(platform::restart()),
        "set-default" => match audio::set_default(true) {
            Ok(()) => {
                println!("default source -> {}", config::SRC);
                0
            }
            Err(error) => {
                log(&error);
                1
            }
        },
        "unset-default" => match audio::set_default(false) {
            Ok(()) => {
                println!("default source -> system microphone");
                0
            }
            Err(error) => {
                log(&error);
                1
            }
        },
        "-h" | "--help" | "help" => {
            println!(
                "RustMIC {}\n\
                 \n\
                 usage:\n\
                 \x20 RustMIC run [--transport usb|wifi|wifi-direct|bluetooth] [--host IP] [--bt-address MAC]\n\
                 \x20             [--gate-mode voice|level] [--gate-voice 0..100] [--gate-level -60..0]\n\
                 \x20             [--mic-volume 0..100] [--music-volume 0..100] [--loudnorm]\n\
                 \x20             [--no-ducking] [--no-monitor] [--no-denoise] [--stats]\n\
                 \x20 RustMIC status | settings\n\
                 \x20 RustMIC ctl state | mic N | music N | gate-mode voice|level | gate-voice N\n\
                 \x20             | gate-level -45 | denoise on|off | mute | unmute | play ...\n\
                 \x20             | stop-music | pause | seek N | start | stop-service | restart\n\
                 \x20 RustMIC play <название или URL>   (песня сразу в микрофон, без скачивания)\n\
                 \x20 RustMIC stop-music | now-playing\n\
                 \x20 RustMIC filter [--gate-voice N] [--no-denoise]   (stdin -> stdout, для проверки)\n\
                 \x20 RustMIC install | install-no-enable | uninstall\n\
                 \x20 RustMIC start | stop | restart\n\
                 \x20 RustMIC set-default | unset-default   (только Linux)\n\
                 \n\
                 Без кабеля: включите WO Mic на телефоне -> Wi-Fi -> IP телефона,\n\
                 \x20 RustMIC run --transport wifi --host 192.168.1.50\n\
                 \n\
                 Linux:   rustmic --stdout | rnnoise | pacat -> rustmic_sink -> rustmic_mic\n\
                 Windows: rustmic --stdout | rnnoise | WASAPI -> виртуальный кабель (VB-CABLE)",
                env!("CARGO_PKG_VERSION")
            );
            0
        }
        other => {
            log(&format!("unknown command: {other}"));
            2
        }
    };
    std::process::exit(code);
}

fn report(result: Result<(), String>) -> i32 {
    match result {
        Ok(()) => 0,
        Err(error) => {
            log(&error);
            1
        }
    }
}

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
use rustmic_mic::config::{self, Settings};
use rustmic_mic::denoise::{Denoiser, Live};
use rustmic_mic::{adb, level, logging, music, platform, STOP};

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
            rest.chunks_exact(2)
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
        "starting (transport={}, host={}, adb={}, device={}, denoise={}, gate={}, volume={})",
        cfg.transport, cfg.host, cfg.adb, cfg.device, cfg.denoise, cfg.gate, cfg.volume
    ));

    let pidfile = config::pid_path();
    let _ = fs::write(&pidfile, std::process::id().to_string());
    level::clear();

    // Settings hot-reload: the GUI writes config.toml, the daemon picks it up
    // without a restart (a restart storm used to trip systemd's start limit).
    let live = Arc::new(RwLock::new(Live {
        denoise: cfg.denoise,
        gate: cfg.gate,
        volume: cfg.volume,
    }));
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
                if let Ok(mut current) = live.write() {
                    current.denoise = settings.denoise;
                    current.gate = settings.gate;
                    current.volume = settings.volume;
                }
                log(&format!(
                    "settings reloaded: denoise={}, gate={}, volume={}",
                    settings.denoise, settings.gate, settings.volume
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
    let live = Arc::new(RwLock::new(Live {
        denoise: cfg.denoise,
        gate: cfg.gate,
        volume: cfg.volume,
    }));
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
            "--no-denoise" => cfg.denoise = false,
            "--stats" => stats = true,
            "--gate" => {
                if let Some(value) = it.next() {
                    cfg.gate = value.parse().unwrap_or(0.0);
                }
            }
            other if other.starts_with("--gate=") => {
                cfg.gate = other[7..].parse().unwrap_or(0.0);
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
            println!("gate: {}", cfg.gate);
            println!("volume: {}%", cfg.volume);
            if cfg!(windows) {
                println!("device: {}", cfg.device);
            }
            println!("adb: {}", cfg.adb);
            println!("config: {}", config::settings_path().display());
            println!("log: {}", config::log_path().display());
            0
        }
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
                 \x20             [--adb PATH] [--device NAME] [--gate 0.0..1.0] [--no-denoise] [--stats]\n\
                 \x20 RustMIC status | settings\n\
                 \x20 RustMIC play <название или URL>   (песня сразу в микрофон, без скачивания)\n\
                 \x20 RustMIC stop-music | now-playing\n\
                 \x20 RustMIC filter [--gate N] [--no-denoise]   (stdin -> stdout, для проверки)\n\
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

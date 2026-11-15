//! womic-mic — WO Mic (Android) as a system microphone.
//!
//! Pipeline:  rustmic (adb/USB transport, Opus -> PCM s16le 48k mono)
//!            -> RNNoise denoise (nnnoiseless)
//!            -> platform sink:
//!               Linux   pacat -> womic_sink -> (remap) -> source womic_mic
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
use std::sync::{Arc, RwLock};
use std::thread;
use std::time::{Duration, Instant};

use womic_mic::audio::{self, Sink};
use womic_mic::config::{self, Settings};
use womic_mic::denoise::{Denoiser, Live};
use womic_mic::{adb, level, logging, platform, STOP};

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

fn pump(mut src: impl Read, sink: &mut dyn Sink, den: &mut Denoiser) -> io::Result<()> {
    let mut raw = vec![0u8; 16 * 1024];
    let mut carry: Option<u8> = None;
    let mut samples: Vec<i16> = Vec::with_capacity(8192);
    let mut out: Vec<i16> = Vec::with_capacity(8192);
    loop {
        if STOP.load(Ordering::SeqCst) {
            return Ok(());
        }
        let n = match src.read(&mut raw) {
            Ok(0) => return Ok(()),
            Ok(n) => n,
            Err(e) if e.kind() == io::ErrorKind::Interrupted => continue,
            Err(e) => return Err(e),
        };
        samples.clear();
        out.clear();
        let mut idx = 0;
        if let Some(b) = carry.take() {
            if n == 0 {
                return Ok(());
            }
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

fn session(cfg: &Settings, live: &Arc<RwLock<Live>>, stats: bool) -> Result<(), String> {
    adb::forward(&cfg.adb);

    let mut sink = audio::open(cfg, live)?;
    log(&format!("sink: {}", sink.describe()));

    let mut mic = womic_mic::command("rustmic")
        .args(["--transport", "usb", "--adb", &cfg.adb, "--stdout"])
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::inherit())
        .spawn()
        .map_err(|error| format!("cannot start rustmic: {error}"))?;

    let mut den = Denoiser::new(live.clone());
    den.stats = stats;
    let result = {
        let out = mic.stdout.take().expect("rustmic stdout");
        pump(out, sink.as_mut(), &mut den)
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
        "starting (adb={}, device={}, denoise={}, gate={}, volume={})",
        cfg.adb, cfg.device, cfg.denoise, cfg.gate, cfg.volume
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
        if last_devices_check.elapsed() >= Duration::from_secs(3) {
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
    match pump(io::stdin().lock(), &mut StdoutSink, &mut den) {
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

    let mut cfg = Settings::load();
    let mut stats = false;
    let mut it = rest.iter().peekable();
    while let Some(arg) = it.next() {
        match arg.as_str() {
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
                "womic-mic {}\n\
                 \n\
                 usage:\n\
                 \x20 womic-mic run [--adb PATH] [--device NAME] [--gate 0.0..1.0] [--no-denoise] [--stats]\n\
                 \x20 womic-mic status | settings\n\
                 \x20 womic-mic filter [--gate N] [--no-denoise]   (stdin -> stdout, for testing)\n\
                 \x20 womic-mic install | install-no-enable | uninstall\n\
                 \x20 womic-mic start | stop | restart\n\
                 \x20 womic-mic set-default | unset-default   (Linux only)\n\
                 \n\
                 Linux:   rustmic --stdout | rnnoise | pacat -> womic_sink -> womic_mic\n\
                 Windows: rustmic --stdout | rnnoise | WASAPI -> virtual cable (VB-CABLE)",
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

//! womic-mic — WO Mic (Android) as a system microphone on Linux.
//!
//! Pipeline:  rustmic (adb/USB transport, Opus -> PCM s16le 48k mono)
//!            -> RNNoise denoise (nnnoiseless)
//!            -> pacat -> womic_sink -> (remap) -> source womic_mic
//!
//! The supervisor restarts every stage on failure, re-checks adb, recreates the
//! PipeWire nodes if they disappear and keeps the virtual sink awake, so the
//! microphone survives cable pulls, phone reboots and PipeWire restarts.

use std::env;
use std::fs;
use std::io::{self, Read, Write};
use std::path::PathBuf;
use std::process::{Child, Command, Stdio};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, RwLock};
use std::thread;
use std::time::{Duration, Instant};

use nnnoiseless::DenoiseState;

const FRAME: usize = 480; // 10 ms @ 48 kHz, RNNoise frame size
const SINK: &str = "womic_sink";
const SRC: &str = "womic_mic";
const BUILTIN_MIC: &str = "alsa_input.pci-0000_05_00.6.analog-stereo";

static STOP: AtomicBool = AtomicBool::new(false);

extern "C" fn on_signal(_sig: libc::c_int) {
    STOP.store(true, Ordering::SeqCst);
}

fn install_signals() {
    unsafe {
        libc::signal(libc::SIGTERM, on_signal as *const () as libc::sighandler_t);
        libc::signal(libc::SIGINT, on_signal as *const () as libc::sighandler_t);
        libc::signal(libc::SIGPIPE, libc::SIG_IGN);
    }
}

fn log(msg: &str) {
    eprintln!("womic-mic: {msg}");
}

fn home() -> PathBuf {
    PathBuf::from(env::var("HOME").unwrap_or_else(|_| "/home/oleg".to_string()))
}

fn cache_path(name: &str) -> PathBuf {
    let dir = home().join(".cache");
    let _ = fs::create_dir_all(&dir);
    dir.join(name)
}

// ---------------------------------------------------------------- config

struct Config {
    adb: String,
    denoise: bool,
    gate: f32,
    stats: bool,
}

fn settings_path() -> PathBuf {
    home().join(".config/womic-mic/config.toml")
}

/// Read `denoise = true|false` and `gate = 0.0..1.0` from the settings file.
/// Unknown keys and comments are ignored; a missing file keeps the defaults.
fn load_settings() -> (bool, f32) {
    let mut denoise = true;
    let mut gate = 0.0f32;
    if let Ok(text) = fs::read_to_string(settings_path()) {
        for line in text.lines() {
            let line = line.split('#').next().unwrap_or("").trim();
            let Some((k, v)) = line.split_once('=') else {
                continue;
            };
            let v = v.trim().trim_matches('"');
            match k.trim() {
                "denoise" => denoise = v != "false" && v != "0",
                "gate" => gate = v.parse::<f32>().unwrap_or(0.0).clamp(0.0, 1.0),
                _ => {}
            }
        }
    }
    (denoise, gate)
}

fn load_volume() -> u32 {
    let text = fs::read_to_string(settings_path()).unwrap_or_default();
    parse_volume(&text)
}

fn parse_volume(text: &str) -> u32 {
    text.lines()
        .find_map(|line| {
            let line = line.split('#').next().unwrap_or("").trim();
            let (key, value) = line.split_once('=')?;
            (key.trim() == "volume")
                .then(|| value.trim().parse::<u32>().ok())
                .flatten()
        })
        .unwrap_or(100)
        .clamp(0, 150)
}

impl Default for Config {
    fn default() -> Self {
        let candidate = home().join("Android/Sdk/platform-tools/adb");
        let adb = if candidate.exists() {
            candidate.to_string_lossy().into_owned()
        } else {
            "adb".to_string()
        };
        let (denoise, gate) = load_settings();
        Self {
            adb,
            denoise,
            gate,
            stats: false,
        }
    }
}

impl Config {
    fn from_args(args: &[String]) -> Self {
        let mut cfg = Config::default();
        let mut it = args.iter().peekable();
        while let Some(a) = it.next() {
            match a.as_str() {
                "--adb" => {
                    if let Some(v) = it.next() {
                        cfg.adb = v.clone();
                    }
                }
                "--no-denoise" => cfg.denoise = false,
                "--stats" => cfg.stats = true,
                "--gate" => {
                    if let Some(v) = it.next() {
                        cfg.gate = v.parse().unwrap_or(0.0);
                    }
                }
                other if other.starts_with("--gate=") => {
                    cfg.gate = other[7..].parse().unwrap_or(0.0);
                }
                other if other.starts_with("--adb=") => cfg.adb = other[6..].to_string(),
                _ => {}
            }
        }
        cfg
    }

    #[allow(dead_code)]
    fn unit_args(&self) -> String {
        let mut s = String::new();
        if !self.denoise {
            s.push_str(" --no-denoise");
        }
        if self.gate > 0.0 {
            s.push_str(&format!(" --gate {}", self.gate));
        }
        s
    }
}

// ---------------------------------------------------------------- pipewire helpers

fn pactl(args: &[&str]) -> bool {
    Command::new("pactl")
        .args(args)
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .status()
        .map(|s| s.success())
        .unwrap_or(false)
}

fn pactl_out(args: &[&str]) -> String {
    match Command::new("pactl").args(args).output() {
        Ok(o) => String::from_utf8_lossy(&o.stdout).into_owned(),
        Err(_) => String::new(),
    }
}

fn node_exists(kind: &str, name: &str) -> bool {
    pactl_out(&["list", "short", kind])
        .lines()
        .any(|l| l.split('\t').nth(1) == Some(name))
}

/// Idempotently make sure the virtual sink + remapped source exist.
fn ensure_devices() {
    if !node_exists("sinks", SINK) {
        if pactl(&[
            "load-module",
            "module-null-sink",
            &format!("sink_name={SINK}"),
            "sink_properties=device.description=WO_Mic_Sink",
        ]) {
            log("created null sink womic_sink");
        }
    }
    if !node_exists("sources", SRC) {
        if pactl(&[
            "load-module",
            "module-remap-source",
            &format!("master={SINK}.monitor"),
            &format!("source_name={SRC}"),
            "source_properties=device.description=WO_Mic_Phone",
        ]) {
            log("created remap source womic_mic");
        }
    }
    pactl(&["set-sink-volume", SINK, "100%"]);
    pactl(&["set-sink-mute", SINK, "0"]);
    pactl(&["set-source-volume", SRC, &format!("{}%", load_volume())]);
    pactl(&["set-source-mute", SRC, "0"]);
}

fn phone_ready(adb: &str) -> bool {
    match Command::new(adb).arg("devices").output() {
        Ok(o) => String::from_utf8_lossy(&o.stdout)
            .lines()
            .skip(1)
            .any(|l| l.split('\t').nth(1) == Some("device")),
        Err(_) => false,
    }
}

// ---------------------------------------------------------------- denoiser

/// Settings the GUI can change without restarting the service.
#[derive(Clone, Copy)]
struct Live {
    denoise: bool,
    gate: f32,
}

struct Denoiser {
    live: Arc<RwLock<Live>>,
    state: Box<DenoiseState<'static>>,
    pending: Vec<i16>,
    fin: [f32; FRAME],
    fout: [f32; FRAME],
    gain: f32,
    stats: bool,
    frames: u32,
    acc_in: f64,
    acc_out: f64,
    acc_vad: f64,
}

impl Denoiser {
    fn new(live: Arc<RwLock<Live>>) -> Self {
        Self {
            live,
            state: DenoiseState::new(),
            pending: Vec::with_capacity(FRAME),
            fin: [0.0; FRAME],
            fout: [0.0; FRAME],
            gain: 1.0,
            stats: false,
            frames: 0,
            acc_in: 0.0,
            acc_out: 0.0,
            acc_vad: 0.0,
        }
    }

    fn settings(&self) -> Live {
        self.live.read().map(|l| *l).unwrap_or(Live {
            denoise: true,
            gate: 0.0,
        })
    }

    /// Push mono s16 samples, append denoised s16 LE bytes to `out`.
    fn process(&mut self, samples: &[i16], out: &mut Vec<u8>) {
        let Live { denoise, gate } = self.settings();
        if !denoise {
            out.reserve(samples.len() * 2);
            for &s in samples {
                out.extend_from_slice(&s.to_le_bytes());
            }
            return;
        }
        out.reserve(samples.len() * 2);
        for &s in samples {
            self.pending.push(s);
            if self.pending.len() < FRAME {
                continue;
            }
            for i in 0..FRAME {
                self.fin[i] = self.pending[i] as f32;
            }
            let vad = self.state.process_frame(&mut self.fout, &self.fin);
            // optional soft gate driven by RNNoise's voice activity probability
            let target = if gate > 0.0 && vad < gate { 0.0 } else { 1.0 };
            self.gain += (target - self.gain) * 0.3;
            for i in 0..FRAME {
                let v = (self.fout[i] * self.gain).clamp(-32768.0, 32767.0) as i16;
                out.extend_from_slice(&v.to_le_bytes());
            }
            if self.stats {
                let mut si = 0.0f64;
                let mut so = 0.0f64;
                for i in 0..FRAME {
                    si += (self.fin[i] as f64) * (self.fin[i] as f64);
                    let o = (self.fout[i] * self.gain) as f64;
                    so += o * o;
                }
                self.acc_in += (si / FRAME as f64).sqrt();
                self.acc_out += (so / FRAME as f64).sqrt();
                self.acc_vad += vad as f64;
                self.frames += 1;
                if self.frames >= 50 {
                    log(&format!(
                        "stats: in_rms={:.1} out_rms={:.1} vad={:.2}",
                        self.acc_in / 50.0,
                        self.acc_out / 50.0,
                        self.acc_vad / 50.0
                    ));
                    self.frames = 0;
                    self.acc_in = 0.0;
                    self.acc_out = 0.0;
                    self.acc_vad = 0.0;
                }
            }
            self.pending.clear();
        }
    }
}

// ---------------------------------------------------------------- pump

fn pump(mut src: impl Read, mut dst: impl Write, den: &mut Denoiser) -> io::Result<()> {
    let mut raw = vec![0u8; 16 * 1024];
    let mut carry: Option<u8> = None;
    let mut samples: Vec<i16> = Vec::with_capacity(8192);
    let mut out: Vec<u8> = Vec::with_capacity(16 * 1024);
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
                .map(|c| i16::from_le_bytes([c[0], c[1]])),
        );
        if samples.is_empty() {
            continue;
        }
        den.process(&samples, &mut out);
        dst.write_all(&out)?;
        dst.flush()?;
    }
}

// ---------------------------------------------------------------- session

fn session(cfg: &Config, live: &Arc<RwLock<Live>>) -> io::Result<()> {
    // rustmic sets this up itself, but doing it here survives a stale forward
    let _ = Command::new(&cfg.adb)
        .args(["forward", "tcp:8125", "tcp:8125"])
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .status();

    let mut mic = Command::new("rustmic")
        .args(["--transport", "usb", "--adb", &cfg.adb, "--stdout"])
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::inherit())
        .spawn()?;

    let mut player = match Command::new("pacat")
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
    {
        Ok(p) => p,
        Err(e) => {
            let _ = mic.kill();
            let _ = mic.wait();
            return Err(e);
        }
    };

    let mut den = Denoiser::new(live.clone());
    den.stats = cfg.stats;
    let result = {
        let out = mic.stdout.take().expect("rustmic stdout");
        let inp = player.stdin.take().expect("pacat stdin");
        pump(out, inp, &mut den)
    };

    let _ = player.kill();
    let _ = player.wait();
    let _ = mic.kill();
    let _ = mic.wait();
    result
}

// ---------------------------------------------------------------- run

fn run(cfg: Config) -> i32 {
    install_signals();
    ensure_devices();
    log(&format!(
        "starting (adb={}, denoise={}, gate={})",
        cfg.adb, cfg.denoise, cfg.gate
    ));

    let pidfile = cache_path("womic-mic.pid");
    let _ = fs::write(&pidfile, std::process::id().to_string());

    // Settings hot-reload: the GUI writes config.toml, the daemon picks it up
    // without a restart (a restart storm used to trip systemd's start limit).
    let live = Arc::new(RwLock::new(Live {
        denoise: cfg.denoise,
        gate: cfg.gate,
    }));
    {
        let live = live.clone();
        thread::spawn(move || {
            let mut stamp = fs::metadata(settings_path())
                .and_then(|m| m.modified())
                .ok();
            while !STOP.load(Ordering::SeqCst) {
                thread::sleep(Duration::from_secs(1));
                let now = fs::metadata(settings_path())
                    .and_then(|m| m.modified())
                    .ok();
                if now == stamp {
                    continue;
                }
                stamp = now;
                let (denoise, gate) = load_settings();
                if let Ok(mut l) = live.write() {
                    l.denoise = denoise;
                    l.gate = gate;
                }
                log(&format!(
                    "settings reloaded: denoise={denoise}, gate={gate}"
                ));
            }
        });
    }

    // watchdog: recreate the PipeWire nodes if PipeWire/WirePlumber dropped them
    thread::spawn(|| {
        while !STOP.load(Ordering::SeqCst) {
            ensure_devices();
            thread::sleep(Duration::from_secs(3));
        }
    });

    let mut keepalive: Option<Child> = None;
    let mut backoff = 1u64;
    let mut last_devices_check = Instant::now() - Duration::from_secs(60);

    while !STOP.load(Ordering::SeqCst) {
        // keep the virtual sink running so pacat never blocks on a suspended node
        let dead = match keepalive.as_mut() {
            Some(c) => !matches!(c.try_wait(), Ok(None)),
            None => true,
        };
        if dead {
            keepalive = Command::new("parec")
                .args([&format!("--device={SINK}.monitor"), "--raw"])
                .stdin(Stdio::null())
                .stdout(Stdio::null())
                .stderr(Stdio::null())
                .spawn()
                .ok();
        }

        if last_devices_check.elapsed() >= Duration::from_secs(3) {
            last_devices_check = Instant::now();
            if !phone_ready(&cfg.adb) {
                log("phone not connected over adb, waiting");
                thread::sleep(Duration::from_secs(3));
                continue;
            }
        }

        let started = Instant::now();
        match session(&cfg, &live) {
            Ok(()) => log("session ended, reconnecting"),
            Err(e) => log(&format!("session error: {e}")),
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

    if let Some(mut c) = keepalive {
        let _ = c.kill();
        let _ = c.wait();
    }
    let _ = fs::remove_file(&pidfile);
    log("stopped");
    0
}

// ---------------------------------------------------------------- status

fn status() -> i32 {
    let pidfile = cache_path("womic-mic.pid");
    let pid = fs::read_to_string(&pidfile)
        .ok()
        .and_then(|s| s.trim().parse::<u32>().ok());
    let alive = pid
        .map(|p| PathBuf::from(format!("/proc/{p}")).exists())
        .unwrap_or(false);

    println!(
        "supervisor: {}",
        if alive {
            format!("running (pid {})", pid.unwrap())
        } else {
            "stopped".into()
        }
    );
    println!(
        "sink womic_sink: {}",
        if node_exists("sinks", SINK) {
            "present"
        } else {
            "absent"
        }
    );
    println!(
        "source womic_mic: {}",
        if node_exists("sources", SRC) {
            "present"
        } else {
            "absent"
        }
    );
    println!("configured source volume: {}%", load_volume());
    let def = pactl_out(&["get-default-source"]).trim().to_string();
    println!("default source: {def}");
    let phone = Config::default();
    println!(
        "phone (adb): {}",
        if phone_ready(&phone.adb) {
            "connected"
        } else {
            "not connected"
        }
    );
    let rustmic_running = Command::new("pgrep")
        .args(["-f", "rustmic --transport usb"])
        .stdout(Stdio::null())
        .status()
        .map(|s| s.success())
        .unwrap_or(false);
    println!(
        "client: {}",
        if rustmic_running {
            "running"
        } else {
            "stopped"
        }
    );
    if alive {
        0
    } else {
        1
    }
}

// ---------------------------------------------------------------- filter (stdin -> stdout)

fn filter(cfg: &Config) -> i32 {
    let stdin = io::stdin();
    let stdout = io::stdout();
    let live = Arc::new(RwLock::new(Live {
        denoise: cfg.denoise,
        gate: cfg.gate,
    }));
    let mut den = Denoiser::new(live);
    den.stats = cfg.stats;
    match pump(stdin.lock(), stdout.lock(), &mut den) {
        Ok(()) => 0,
        Err(e) => {
            log(&format!("filter error: {e}"));
            1
        }
    }
}

// ---------------------------------------------------------------- systemd

fn unit_text(exe: &str) -> String {
    format!(
        "[Unit]\n\
         Description=WO Mic phone microphone (womic-mic)\n\
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

fn install(_cfg: &Config, enable: bool) -> i32 {
    let exe = env::current_exe().unwrap_or_else(|_| PathBuf::from("womic-mic"));
    let unit_dir = home().join(".config/systemd/user");
    if let Err(e) = fs::create_dir_all(&unit_dir) {
        log(&format!("cannot create {unit_dir:?}: {e}"));
        return 1;
    }
    let unit = unit_dir.join("womic-mic.service");
    if let Err(e) = fs::write(&unit, unit_text(&exe.to_string_lossy())) {
        log(&format!("cannot write {unit:?}: {e}"));
        return 1;
    }
    println!("wrote {unit:?} (ExecStart={} run)", exe.to_string_lossy());
    let _ = Command::new("systemctl")
        .args(["--user", "daemon-reload"])
        .status();
    if enable {
        let _ = Command::new("systemctl")
            .args(["--user", "enable", "--now", "womic-mic"])
            .status();
        println!("enabled and started womic-mic.service");
    }
    0
}

fn uninstall() -> i32 {
    let _ = Command::new("systemctl")
        .args(["--user", "disable", "--now", "womic-mic"])
        .status();
    let unit = home().join(".config/systemd/user/womic-mic.service");
    let _ = fs::remove_file(&unit);
    let _ = Command::new("systemctl")
        .args(["--user", "daemon-reload"])
        .status();
    println!("removed womic-mic.service");
    0
}

fn set_default(enable: bool) -> i32 {
    let target = if enable { SRC } else { BUILTIN_MIC };
    if pactl(&["set-default-source", target]) {
        println!("default source -> {target}");
        0
    } else {
        log(&format!("failed to set default source to {target}"));
        1
    }
}

// ---------------------------------------------------------------- main

fn main() {
    install_signals();
    let args: Vec<String> = env::args().skip(1).collect();
    let cmd = args.first().map(String::as_str).unwrap_or("run");
    let rest = if args.is_empty() {
        &args[..]
    } else {
        &args[1..]
    };
    let cfg = Config::from_args(rest);

    let code = match cmd {
        "run" => run(cfg),
        "status" => status(),
        "settings" => {
            let (denoise, gate) = load_settings();
            println!("denoise: {denoise}");
            println!("gate: {gate}");
            println!("configured volume: {}%", load_volume());
            0
        }
        "filter" => filter(&cfg),
        "install" => install(&cfg, true),
        "install-no-enable" => install(&cfg, false),
        "uninstall" => uninstall(),
        "set-default" => set_default(true),
        "unset-default" => set_default(false),
        "-h" | "--help" | "help" => {
            println!(
                "womic-mic {}\n\
                 \n\
                 usage:\n\
                 \x20 womic-mic run [--adb PATH] [--gate 0.0..1.0] [--no-denoise]\n\
                 \x20 womic-mic status | settings\n\
                 \x20 womic-mic filter [--gate N] [--no-denoise]   (stdin -> stdout, for testing)\n\
                 \x20 womic-mic install | install-no-enable | uninstall\n\
                 \x20 womic-mic set-default | unset-default\n\
                 \n\
                 chain: rustmic --stdout | rnnoise | pacat -> womic_sink -> womic_mic",
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

#[cfg(test)]
mod tests {
    use super::parse_volume;

    #[test]
    fn volume_defaults_to_100_for_missing_or_invalid_values() {
        assert_eq!(parse_volume("denoise = true\ngate = 0.2\n"), 100);
        assert_eq!(parse_volume("volume = loud\n"), 100);
    }

    #[test]
    fn volume_is_clamped_to_supported_range() {
        assert_eq!(parse_volume("volume = 0\n"), 0);
        assert_eq!(parse_volume("volume = 125\n"), 125);
        assert_eq!(parse_volume("volume = 200\n"), 150);
    }
}

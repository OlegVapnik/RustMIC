//! womic-mic-gui — tray icon + settings window for the womic-mic daemon.
//!
//! The GUI never touches the audio path itself: it controls the systemd unit
//! (start/stop), writes the settings file the daemon reads on start, and shows
//! a live level meter by listening to the monitor of the virtual sink.

use gtk::prelude::*;
use gtk::{glib, Application, ApplicationWindow, Label, Orientation, ProgressBar, Scale, Switch};
use gtk4 as gtk;

use std::cell::{Cell, RefCell};
use std::fs;
use std::path::PathBuf;
use std::process::{Command, Stdio};
use std::rc::Rc;
use std::sync::mpsc::{channel, Sender};
use std::thread;
use std::time::Duration;
use std::{env, os::unix::fs::PermissionsExt};

use ksni::blocking::TrayMethods;

const SERVICE: &str = "womic-mic";
const SRC: &str = "womic_mic";
const BUILTIN_MIC: &str = "alsa_input.pci-0000_05_00.6.analog-stereo";
const SINK_MONITOR: &str = "womic_sink.monitor";

// ---------------------------------------------------------------- helpers

fn home() -> PathBuf {
    PathBuf::from(std::env::var("HOME").unwrap_or_else(|_| "/home/oleg".into()))
}

fn settings_path() -> PathBuf {
    home().join(".config/womic-mic/config.toml")
}

fn run(cmd: &str, args: &[&str]) -> bool {
    Command::new(cmd)
        .args(args)
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .status()
        .map(|s| s.success())
        .unwrap_or(false)
}

fn out(cmd: &str, args: &[&str]) -> String {
    Command::new(cmd)
        .args(args)
        .output()
        .map(|o| String::from_utf8_lossy(&o.stdout).into_owned())
        .unwrap_or_default()
}

fn executable_available(command: &str) -> bool {
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

fn service_active() -> bool {
    out("systemctl", &["--user", "is-active", SERVICE]).trim() == "active"
}

fn adb_path() -> String {
    let adb = home().join("Android/Sdk/platform-tools/adb");
    if adb.exists() {
        adb.to_string_lossy().into_owned()
    } else {
        "adb".to_string()
    }
}

/// Only a device attached over USB counts: adb also lists Wi-Fi devices, and a
/// stale TCP entry used to be reported as "connected" long after unplugging.
fn usb_phone() -> Option<String> {
    let listing = out(&adb_path(), &["devices", "-l"]);
    for line in listing.lines().skip(1) {
        let mut fields = line.split_whitespace();
        if fields.next().is_none() {
            continue;
        }
        let rest: Vec<&str> = fields.collect();
        if rest.first() != Some(&"device") || !rest.iter().any(|f| f.starts_with("usb:")) {
            continue;
        }
        let model = rest
            .iter()
            .find_map(|f| f.strip_prefix("model:"))
            .unwrap_or("телефон")
            .replace('_', " ");
        return Some(model);
    }
    None
}

fn phone_connected() -> bool {
    usb_phone().is_some()
}

/// Is the rustmic client alive right now? Checked through /proc so the poller
/// does not spawn a process every 1.5 s.
fn client_running() -> bool {
    let Ok(entries) = fs::read_dir("/proc") else {
        return false;
    };
    for entry in entries.flatten() {
        let name = entry.file_name();
        let name = name.to_string_lossy();
        if !name.starts_with(|c: char| c.is_ascii_digit()) {
            continue;
        }
        let Ok(cmdline) = fs::read(entry.path().join("cmdline")) else {
            continue;
        };
        if cmdline.split(|b| *b == 0).next() == Some(b"rustmic") {
            return true;
        }
    }
    false
}

fn default_is_womic() -> bool {
    out("pactl", &["get-default-source"]).trim() == SRC
}

fn set_default(on: bool) {
    let target = if on { SRC } else { BUILTIN_MIC };
    run("pactl", &["set-default-source", target]);
}

fn read_settings() -> (bool, f32) {
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

fn read_volume() -> u32 {
    fs::read_to_string(settings_path())
        .ok()
        .and_then(|text| {
            text.lines().find_map(|line| {
                let line = line.split('#').next().unwrap_or("").trim();
                let (key, value) = line.split_once('=')?;
                (key.trim() == "volume")
                    .then(|| value.trim().parse::<u32>().ok())
                    .flatten()
            })
        })
        .unwrap_or(100)
        .clamp(0, 150)
}

fn write_settings(denoise: bool, gate: f32, volume: u32) -> std::io::Result<()> {
    let path = settings_path();
    if let Some(dir) = path.parent() {
        fs::create_dir_all(dir)?;
    }
    let text = format!(
        "# womic-mic settings (managed by womic-mic-gui)\n\
         denoise = {denoise}\n\
         gate = {gate:.2}\n\
         volume = {volume}\n"
    );
    fs::write(path, text)
}

fn fmt_gate(v: f32) -> String {
    if v <= 0.005 {
        "выкл".into()
    } else {
        format!("{v:.2}")
    }
}

fn restart_service() {
    // systemctl blocks for ~1 s: never do that on the GTK main thread.
    thread::spawn(|| {
        run("systemctl", &["--user", "restart", SERVICE]);
    });
}

fn open_journal() {
    let _ = Command::new("kitty")
        .args(["-e", "journalctl", "--user", "-u", SERVICE, "-f"])
        .spawn();
}

/// Record 2 s from the virtual source and play it back so the user can hear
/// whether the phone actually captures anything.
fn test_mic() {
    thread::spawn(|| {
        let _ = Command::new("sh")
            .arg("-c")
            .arg(
                "parec --device=womic_mic --format=s16le --rate=48000 --channels=1 \
                 --raw --latency-msec=50 | head -c 192000 | \
                 paplay --raw --format=s16le --rate=48000 --channels=1",
            )
            .status();
    });
}

fn diagnostics() -> String {
    let phone = usb_phone().unwrap_or_else(|| "нет".into());
    let (denoise, gate) = read_settings();
    let volume = read_volume();
    let mut s = String::from("womic-mic diagnostics\n");
    s.push_str("--- dependencies ---\n");
    for (name, command) in [
        ("rustmic", "rustmic"),
        ("pacat", "pacat"),
        ("parec", "parec"),
        ("pactl", "pactl"),
        ("adb", &adb_path()),
        ("systemctl", "systemctl"),
        ("journalctl", "journalctl"),
    ] {
        let found = executable_available(command);
        s.push_str(&format!(
            "{name}: {}\n",
            if found { "available" } else { "missing" }
        ));
    }
    let audio_connected = Command::new("pactl")
        .arg("info")
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .status()
        .map(|status| status.success())
        .unwrap_or(false);
    s.push_str(&format!(
        "PipeWire/PulseAudio: {}\n",
        if audio_connected {
            "available"
        } else {
            "unavailable"
        }
    ));
    s.push_str(&format!(
        "service: {}\n",
        if service_active() {
            "active"
        } else {
            "inactive"
        }
    ));
    s.push_str(&format!("phone(usb): {phone}\n"));
    s.push_str(&format!(
        "client: {}\n",
        if client_running() {
            "running"
        } else {
            "stopped"
        }
    ));
    s.push_str(&format!(
        "default source: {}\n",
        out("pactl", &["get-default-source"]).trim()
    ));
    s.push_str(&format!("denoise: {denoise} gate: {gate:.2}\n"));
    s.push_str(&format!("configured source volume: {volume}%\n"));
    s.push_str(&format!(
        "sink/source nodes: {}/{}\n",
        if out("pactl", &["list", "short", "sinks"])
            .lines()
            .any(|line| line.split('\t').nth(1) == Some("womic_sink"))
        {
            "present"
        } else {
            "absent"
        },
        if out("pactl", &["list", "short", "sources"])
            .lines()
            .any(|line| line.split('\t').nth(1) == Some(SRC))
        {
            "present"
        } else {
            "absent"
        }
    ));
    s.push_str("--- journal ---\n");
    s.push_str(&out(
        "journalctl",
        &[
            "--user",
            "-u",
            SERVICE,
            "-n",
            "15",
            "--no-pager",
            "-o",
            "cat",
        ],
    ));
    s
}

// ---------------------------------------------------------------- tray

enum Cmd {
    Show,
    ToggleMic,
    ToggleDenoise,
    ToggleDefault,
    Restart,
    Test,
    Quit,
}

struct State {
    service: bool,
    phone: Option<String>,
    client: bool,
    default_mic: bool,
}

struct Tray {
    tx: Sender<Cmd>,
    service: bool,
    phone: bool,
    client: bool,
    default_mic: bool,
}

impl ksni::Tray for Tray {
    fn id(&self) -> String {
        "womic-mic".into()
    }

    fn title(&self) -> String {
        "WO Mic".into()
    }

    fn icon_name(&self) -> String {
        if self.service && self.client {
            "womic-mic".into()
        } else {
            "womic-mic-off".into()
        }
    }

    fn tool_tip(&self) -> ksni::ToolTip {
        let state = match (self.service, self.phone, self.client) {
            (false, _, _) => "выключен",
            (true, false, _) => "нет телефона",
            (true, true, false) => "телефон есть, потока нет",
            (true, true, true) => "микрофон работает",
        };
        ksni::ToolTip {
            title: "WO Mic".into(),
            description: state.into(),
            ..Default::default()
        }
    }

    fn activate(&mut self, _x: i32, _y: i32) {
        let _ = self.tx.send(Cmd::Show);
    }

    fn menu(&self) -> Vec<ksni::MenuItem<Self>> {
        use ksni::menu::*;
        vec![
            StandardItem {
                label: "Открыть настройки".into(),
                icon_name: "preferences-system".into(),
                activate: Box::new(|t: &mut Self| {
                    let _ = t.tx.send(Cmd::Show);
                }),
                ..Default::default()
            }
            .into(),
            MenuItem::Separator,
            StandardItem {
                label: "Перезапустить".into(),
                icon_name: "view-refresh".into(),
                activate: Box::new(|t: &mut Self| {
                    let _ = t.tx.send(Cmd::Restart);
                }),
                ..Default::default()
            }
            .into(),
            StandardItem {
                label: "Тест микрофона".into(),
                icon_name: "media-playback-start".into(),
                activate: Box::new(|t: &mut Self| {
                    let _ = t.tx.send(Cmd::Test);
                }),
                ..Default::default()
            }
            .into(),
            MenuItem::Separator,
            CheckmarkItem {
                label: "Микрофон включён".into(),
                checked: self.service,
                activate: Box::new(|t: &mut Self| {
                    let _ = t.tx.send(Cmd::ToggleMic);
                }),
                ..Default::default()
            }
            .into(),
            CheckmarkItem {
                label: "Шумодав".into(),
                checked: read_settings().0,
                activate: Box::new(|t: &mut Self| {
                    let _ = t.tx.send(Cmd::ToggleDenoise);
                }),
                ..Default::default()
            }
            .into(),
            CheckmarkItem {
                label: "Микрофон по умолчанию".into(),
                checked: self.default_mic,
                activate: Box::new(|t: &mut Self| {
                    let _ = t.tx.send(Cmd::ToggleDefault);
                }),
                ..Default::default()
            }
            .into(),
            MenuItem::Separator,
            StandardItem {
                label: "Выход".into(),
                icon_name: "application-exit".into(),
                activate: Box::new(|t: &mut Self| {
                    let _ = t.tx.send(Cmd::Quit);
                }),
                ..Default::default()
            }
            .into(),
        ]
    }
}

// ---------------------------------------------------------------- level meter

fn spawn_level_meter(tx: Sender<f32>) {
    thread::spawn(move || loop {
        let mut child = match Command::new("parec")
            .args([
                &format!("--device={SINK_MONITOR}"),
                "--raw",
                "--format=s16le",
                "--rate=48000",
                "--channels=1",
            ])
            .stdout(Stdio::piped())
            .stderr(Stdio::null())
            .spawn()
        {
            Ok(c) => c,
            Err(_) => {
                thread::sleep(Duration::from_secs(2));
                continue;
            }
        };
        let mut stdout = child.stdout.take().expect("stdout");
        let mut buf = vec![0u8; 1920]; // 20 ms
        loop {
            use std::io::Read;
            match stdout.read_exact(&mut buf) {
                Ok(()) => {
                    let mut sum = 0f64;
                    for pair in buf.chunks_exact(2) {
                        let s = i16::from_le_bytes([pair[0], pair[1]]) as f64;
                        sum += s * s;
                    }
                    let rms = (sum / (buf.len() / 2) as f64).sqrt() / 32768.0;
                    if tx.send(rms as f32).is_err() {
                        return;
                    }
                }
                Err(_) => break,
            }
        }
        let _ = child.kill();
        let _ = child.wait();
        thread::sleep(Duration::from_secs(1));
    });
}

// ---------------------------------------------------------------- ui

struct Ui {
    window: ApplicationWindow,
    status_phone: Label,
    status_client: Label,
    status_service: Label,
    level: ProgressBar,
    sw_mic: Switch,
    #[allow(dead_code)]
    sw_denoise: Switch,
    #[allow(dead_code)]
    scale_gate: Scale,
    sw_default: Switch,
    guard: Rc<Cell<bool>>,
}

fn build_ui(app: &Application) -> Rc<Ui> {
    let style = gtk::CssProvider::new();
    style.load_from_data(
        "frame.card { \
             border: 1px solid alpha(@theme_fg_color, 0.12); \
             border-radius: 12px; \
             padding: 6px; \
         } \
         frame.card > label { font-weight: 600; } \
         progressbar trough { min-height: 8px; border-radius: 8px; } \
         progressbar progress { border-radius: 8px; }",
    );
    if let Some(display) = gtk::gdk::Display::default() {
        gtk::style_context_add_provider_for_display(
            &display,
            &style,
            gtk::STYLE_PROVIDER_PRIORITY_APPLICATION,
        );
    }

    let window = ApplicationWindow::builder()
        .application(app)
        .title("WO Mic")
        .default_width(480)
        .default_height(760)
        .build();

    let root = gtk::Box::new(Orientation::Vertical, 14);
    root.set_margin_top(20);
    root.set_margin_bottom(20);
    root.set_margin_start(20);
    root.set_margin_end(20);
    let guard = Rc::new(Cell::new(false));

    let hero = gtk::Box::new(Orientation::Horizontal, 12);
    let brand = gtk::Box::new(Orientation::Vertical, 3);
    let title = Label::new(Some("WO Mic"));
    title.add_css_class("title-1");
    title.set_xalign(0.0);
    let subtitle = Label::new(Some("Android-телефон как микрофон Linux"));
    subtitle.add_css_class("dim-label");
    subtitle.set_xalign(0.0);
    brand.append(&title);
    brand.append(&subtitle);
    brand.set_hexpand(true);
    hero.append(&brand);

    let sw_mic = Switch::new();
    sw_mic.set_active(service_active());
    sw_mic.set_valign(gtk::Align::Center);
    hero.append(&sw_mic);
    root.append(&hero);

    {
        let g = guard.clone();
        sw_mic.connect_state_set(move |_, on| {
            if !g.get() {
                let action = if on { "start" } else { "stop" };
                thread::spawn(move || {
                    run("systemctl", &["--user", action, SERVICE]);
                });
            }
            glib::Propagation::Proceed
        });
    }

    // --- status
    let status_frame = gtk::Frame::builder().label("Статус").build();
    status_frame.add_css_class("card");
    let grid = gtk::Grid::builder()
        .row_spacing(8)
        .column_spacing(16)
        .margin_top(10)
        .margin_bottom(10)
        .margin_start(12)
        .margin_end(12)
        .build();

    let status_phone = Label::new(Some("—"));
    let status_client = Label::new(Some("—"));
    let status_service = Label::new(Some("—"));
    let level = ProgressBar::new();
    level.set_show_text(false);
    level.set_size_request(-1, 8);

    grid.attach(&Label::new(Some("Телефон (USB)")), 0, 0, 1, 1);
    grid.attach(&status_phone, 1, 0, 1, 1);
    grid.attach(&Label::new(Some("Поток")), 0, 1, 1, 1);
    grid.attach(&status_client, 1, 1, 1, 1);
    grid.attach(&Label::new(Some("Сервис")), 0, 2, 1, 1);
    grid.attach(&status_service, 1, 2, 1, 1);
    grid.attach(&Label::new(Some("Уровень")), 0, 3, 1, 1);
    grid.attach(&level, 1, 3, 1, 1);
    for l in [&status_phone, &status_client, &status_service] {
        l.set_xalign(0.0);
    }
    status_frame.set_child(Some(&grid));
    root.append(&status_frame);

    // --- settings
    let cfg_frame = gtk::Frame::builder().label("Настройки").build();
    let cfg_box = gtk::Box::new(Orientation::Vertical, 10);
    cfg_box.set_margin_top(10);
    cfg_box.set_margin_bottom(10);
    cfg_box.set_margin_start(12);
    cfg_box.set_margin_end(12);
    let settings_notice = Label::new(None);
    settings_notice.set_xalign(0.0);
    settings_notice.set_wrap(true);
    settings_notice.add_css_class("error");
    settings_notice.set_visible(false);
    let (denoise, gate) = read_settings();

    let sw_default = Switch::new();
    sw_default.set_active(default_is_womic());
    {
        let g = guard.clone();
        sw_default.connect_state_set(move |_, on| {
            if !g.get() {
                set_default(on);
            }
            glib::Propagation::Proceed
        });
    }
    cfg_box.append(&row("Микрофон по умолчанию", &sw_default));

    let sw_denoise = Switch::new();
    sw_denoise.set_active(denoise);
    {
        let g = guard.clone();
        let notice = settings_notice.clone();
        sw_denoise.connect_state_set(move |_, on| {
            if !g.get() {
                let (_, gate) = read_settings();
                match write_settings(on, gate, read_volume()) {
                    Ok(()) => {
                        notice.set_visible(false);
                        restart_service();
                    }
                    Err(error) => {
                        notice.set_text(&format!("Не удалось сохранить настройки: {error}"));
                        notice.set_visible(true);
                        eprintln!("womic-mic-gui: cannot save settings: {error}");
                    }
                }
            }
            glib::Propagation::Proceed
        });
    }
    cfg_box.append(&row("Шумодав (RNNoise)", &sw_denoise));

    let scale_gate = Scale::with_range(Orientation::Horizontal, 0.0, 1.0, 0.05);
    scale_gate.set_value(gate as f64);
    scale_gate.set_draw_value(false);
    scale_gate.set_hexpand(true);
    scale_gate.set_size_request(160, -1);

    let gate_value = Label::new(Some(&fmt_gate(gate)));
    gate_value.set_width_chars(5);
    gate_value.set_xalign(1.0);
    gate_value.add_css_class("dim-label");

    {
        // Debounce: one restart per drag, never two, and never remove a source
        // id that has already fired (that used to panic and kill the app).
        let armed = Rc::new(Cell::new(false));
        let a = armed.clone();
        let gv = gate_value.clone();
        let notice = settings_notice.clone();
        scale_gate.connect_value_changed(move |s| {
            let v = s.value() as f32;
            gv.set_text(&fmt_gate(v));
            let (denoise, _) = read_settings();
            if let Err(error) = write_settings(denoise, v, read_volume()) {
                notice.set_text(&format!("Не удалось сохранить настройки: {error}"));
                notice.set_visible(true);
                eprintln!("womic-mic-gui: cannot save settings: {error}");
                return;
            }
            notice.set_visible(false);
            if a.get() {
                return;
            }
            a.set(true);
            let a2 = a.clone();
            glib::timeout_add_local_once(Duration::from_millis(700), move || {
                a2.set(false);
                restart_service();
            });
        });
    }

    let gate_box = gtk::Box::new(Orientation::Horizontal, 12);
    let gate_label = Label::new(Some("Гейт"));
    gate_label.set_xalign(0.0);
    gate_label.set_tooltip_text(Some("Порог VAD: 0 — выключен, 1 — режет всё тише порога"));
    gate_box.append(&gate_label);
    gate_box.append(&scale_gate);
    gate_box.append(&gate_value);
    cfg_box.append(&gate_box);
    cfg_box.append(&settings_notice);

    cfg_frame.set_child(Some(&cfg_box));
    cfg_frame.add_css_class("card");
    root.append(&cfg_frame);

    // --- output level
    let volume_frame = gtk::Frame::builder().label("Громкость микрофона").build();
    volume_frame.add_css_class("card");
    let volume_box = gtk::Box::new(Orientation::Vertical, 8);
    volume_box.set_margin_top(10);
    volume_box.set_margin_bottom(10);
    volume_box.set_margin_start(12);
    volume_box.set_margin_end(12);
    let volume = read_volume();
    let volume_scale = Scale::with_range(Orientation::Horizontal, 0.0, 150.0, 1.0);
    volume_scale.set_value(volume as f64);
    volume_scale.set_draw_value(false);
    volume_scale.set_hexpand(true);
    let volume_value = Label::new(Some(&format!("{volume}%")));
    volume_value.set_xalign(1.0);
    volume_value.add_css_class("dim-label");
    {
        let label = volume_value.clone();
        let notice = settings_notice.clone();
        let armed = Rc::new(Cell::new(false));
        let slider = volume_scale.clone();
        volume_scale.connect_value_changed(move |scale| {
            let value = scale.value().round().clamp(0.0, 150.0) as u32;
            label.set_text(&format!("{value}%"));
            let (denoise, gate) = read_settings();
            if let Err(error) = write_settings(denoise, gate, value) {
                label.set_tooltip_text(Some(&format!("Не удалось сохранить: {error}")));
                notice.set_text(&format!("Не удалось сохранить настройки: {error}"));
                notice.set_visible(true);
                eprintln!("womic-mic-gui: cannot save settings: {error}");
                return;
            }
            notice.set_visible(false);
            if armed.get() {
                return;
            }
            armed.set(true);
            let armed = armed.clone();
            let label = label.clone();
            let notice = notice.clone();
            let slider = slider.clone();
            glib::timeout_add_local_once(Duration::from_millis(180), move || {
                armed.set(false);
                let value = slider.value().round().clamp(0.0, 150.0) as u32;
                let applied = Command::new("pactl")
                    .args(["set-source-volume", SRC, &format!("{value}%")])
                    .status();
                if !matches!(applied, Ok(status) if status.success()) {
                    label.set_tooltip_text(Some(
                        "Значение сохранено; PipeWire применит его, когда источник будет доступен",
                    ));
                    notice.set_text(
                        "Громкость сохранена; PipeWire применит её, когда источник появится",
                    );
                    notice.set_visible(true);
                } else {
                    label.set_tooltip_text(None);
                    notice.set_visible(false);
                }
            });
        });
    }
    let volume_row = gtk::Box::new(Orientation::Horizontal, 12);
    let volume_description = Label::new(Some("Уровень сигнала на входе приложений"));
    volume_description.set_xalign(0.0);
    volume_description.set_hexpand(true);
    volume_row.append(&volume_description);
    volume_row.append(&volume_value);
    volume_box.append(&volume_row);
    volume_box.append(&volume_scale);
    volume_frame.set_child(Some(&volume_box));
    root.append(&volume_frame);

    // --- actions
    let actions = gtk::Grid::builder()
        .column_spacing(8)
        .row_spacing(8)
        .column_homogeneous(true)
        .build();

    let btn_restart = gtk::Button::with_label("Перезапустить");
    btn_restart.set_tooltip_text(Some("Перезапустить службу womic-mic"));
    btn_restart.connect_clicked(|_| restart_service());
    actions.attach(&btn_restart, 0, 0, 1, 1);

    let btn_test = gtk::Button::with_label("Тест микрофона");
    btn_test.set_tooltip_text(Some("Записать 2 секунды и проиграть в колонки"));
    btn_test.connect_clicked(|_| test_mic());
    actions.attach(&btn_test, 1, 0, 1, 1);

    let btn_log = gtk::Button::with_label("Журнал");
    btn_log.set_tooltip_text(Some("Открыть journalctl в терминале"));
    btn_log.connect_clicked(|_| open_journal());
    actions.attach(&btn_log, 0, 1, 1, 1);

    let btn_diag = gtk::Button::with_label("Диагностика");
    btn_diag.set_tooltip_text(Some("Скопировать состояние и последние строки журнала"));
    {
        let b = btn_diag.clone();
        btn_diag.connect_clicked(move |_| {
            let text = diagnostics();
            if let Some(display) = gtk::gdk::Display::default() {
                display.clipboard().set_text(&text);
            }
            b.set_label("Скопировано");
            let b2 = b.clone();
            glib::timeout_add_local_once(Duration::from_millis(1500), move || {
                b2.set_label("Диагностика");
            });
        });
    }
    actions.attach(&btn_diag, 1, 1, 1, 1);

    root.append(&actions);

    let scroller = gtk::ScrolledWindow::builder()
        .hscrollbar_policy(gtk::PolicyType::Never)
        .child(&root)
        .build();
    window.set_child(Some(&scroller));

    let ui = Rc::new(Ui {
        window,
        status_phone,
        status_client,
        status_service,
        level,
        sw_mic,
        sw_denoise,
        scale_gate,
        sw_default,
        guard,
    });

    // hide instead of quitting
    let w = ui.window.clone();
    ui.window.connect_close_request(move |_| {
        w.set_visible(false);
        glib::Propagation::Stop
    });

    ui
}

fn row(label: &str, widget: &impl IsA<gtk::Widget>) -> gtk::Box {
    let b = gtk::Box::new(Orientation::Horizontal, 12);
    let l = Label::new(Some(label));
    l.set_xalign(0.0);
    l.set_hexpand(true);
    b.append(&l);
    b.append(widget);
    b
}

fn main() {
    let (cmd_tx, cmd_rx) = channel::<Cmd>();
    let (state_tx, state_rx) = channel::<State>();
    let (level_tx, level_rx) = channel::<f32>();

    // tray
    let tray_tx = cmd_tx.clone();
    let tray = Tray {
        tx: tray_tx,
        service: service_active(),
        phone: phone_connected(),
        client: client_running(),
        default_mic: default_is_womic(),
    };
    let handle = match tray.spawn() {
        Ok(h) => Some(h),
        Err(e) => {
            eprintln!("womic-mic-gui: tray unavailable: {e}");
            None
        }
    };

    // background state poller
    let poll_tx = state_tx.clone();
    thread::spawn(move || loop {
        let st = State {
            service: service_active(),
            phone: usb_phone(),
            client: client_running(),
            default_mic: default_is_womic(),
        };
        if poll_tx.send(st).is_err() {
            return;
        }
        thread::sleep(Duration::from_millis(1500));
    });

    spawn_level_meter(level_tx);

    let app = Application::builder()
        .application_id("io.github.womic.WomicMic")
        .build();

    let ui_cell: Rc<RefCell<Option<Rc<Ui>>>> = Rc::new(RefCell::new(None));

    app.connect_activate({
        let ui_cell = ui_cell.clone();
        move |app| {
            if ui_cell.borrow().is_none() {
                *ui_cell.borrow_mut() = Some(build_ui(app));
            }
            if let Some(ui) = ui_cell.borrow().as_ref() {
                ui.window.present();
            }
        }
    });

    // command pump + ui refresh
    let ui_for_tick = ui_cell.clone();
    let handle_for_tick = handle.clone();
    let cmd_rx = Rc::new(RefCell::new(cmd_rx));
    let state_rx = Rc::new(RefCell::new(state_rx));
    let level_rx = Rc::new(RefCell::new(level_rx));
    let smooth = Cell::new(0f32);

    glib::timeout_add_local(Duration::from_millis(120), {
        let cmd_rx = cmd_rx.clone();
        let state_rx = state_rx.clone();
        let level_rx = level_rx.clone();
        let app = app.clone();
        move || {
            // commands from tray
            while let Ok(cmd) = cmd_rx.borrow().try_recv() {
                match cmd {
                    Cmd::Show => {
                        if let Some(ui) = ui_for_tick.borrow().as_ref() {
                            ui.window.present();
                        }
                    }
                    Cmd::ToggleMic => {
                        if service_active() {
                            run("systemctl", &["--user", "stop", SERVICE]);
                        } else {
                            run("systemctl", &["--user", "start", SERVICE]);
                        }
                    }
                    Cmd::ToggleDenoise => {
                        let (denoise, gate) = read_settings();
                        match write_settings(!denoise, gate, read_volume()) {
                            Ok(()) => restart_service(),
                            Err(error) => {
                                eprintln!("womic-mic-gui: cannot save settings: {error}")
                            }
                        }
                    }
                    Cmd::ToggleDefault => {
                        set_default(!default_is_womic());
                    }
                    Cmd::Restart => restart_service(),
                    Cmd::Test => test_mic(),
                    Cmd::Quit => {
                        app.quit();
                    }
                }
            }

            // live level
            let mut peak = 0f32;
            while let Ok(v) = level_rx.borrow().try_recv() {
                peak = peak.max(v);
            }
            let prev = smooth.get();
            let next = if peak > prev { peak } else { prev * 0.75 };
            smooth.set(next);
            if let Some(ui) = ui_for_tick.borrow().as_ref() {
                ui.level.set_fraction((next * 3.0).min(1.0) as f64);
            }

            // state refresh
            let mut latest: Option<State> = None;
            while let Ok(st) = state_rx.borrow().try_recv() {
                latest = Some(st);
            }
            if let Some(st) = latest {
                if let Some(ui) = ui_for_tick.borrow().as_ref() {
                    ui.status_phone.set_text(match &st.phone {
                        Some(model) => model,
                        None => "не найден",
                    });
                    ui.status_client
                        .set_text(if st.client { "идёт" } else { "нет" });
                    ui.status_service.set_text(if st.service {
                        "работает"
                    } else {
                        "остановлен"
                    });
                    ui.guard.set(true);
                    ui.sw_mic.set_active(st.service);
                    ui.sw_default.set_active(st.default_mic);
                    ui.guard.set(false);
                }
                if let Some(h) = &handle_for_tick {
                    let (s, p, c, d) = (st.service, st.phone.is_some(), st.client, st.default_mic);
                    h.update(move |t: &mut Tray| {
                        t.service = s;
                        t.phone = p;
                        t.client = c;
                        t.default_mic = d;
                    });
                }
            }

            glib::ControlFlow::Continue
        }
    });

    app.connect_shutdown(move |_| {
        if let Some(h) = &handle {
            let _ = h.shutdown();
        }
    });

    app.run();
    std::process::exit(0);
}

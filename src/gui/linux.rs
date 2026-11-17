//! RustMIC-gui — tray icon + settings window for the RustMIC daemon.
//!
//! The GUI never touches the audio path itself: it controls the systemd unit
//! (start/stop), writes the settings file the daemon reads live, and shows a
//! level meter by listening to the monitor of the virtual sink.

use gtk::prelude::*;
use gtk::{
    glib, Application, ApplicationWindow, Button, DropDown, Entry, Label, Orientation, ProgressBar,
    Scale, Switch,
};
use gtk4 as gtk;

use std::cell::{Cell, RefCell};
use std::process::{Command, Stdio};
use std::rc::Rc;
use std::sync::mpsc::{channel, Sender};
use std::thread;
use std::time::Duration;

use ksni::blocking::TrayMethods;

use rustmic_mic::config::Settings;
use rustmic_mic::{adb, audio, config, platform};

// ---------------------------------------------------------------- helpers

fn settings() -> Settings {
    Settings::load()
}

fn save(settings: &Settings) -> std::io::Result<()> {
    settings.save()
}

fn fmt_gate(value: f32) -> String {
    if value <= 0.005 {
        "выкл".into()
    } else {
        format!("{value:.2}")
    }
}

fn restart_service() {
    // systemctl blocks for ~1 s: never do that on the GTK main thread.
    thread::spawn(|| {
        let _ = platform::restart();
    });
}

fn service_active() -> bool {
    platform::service_active()
}

/// Run our own CLI in a thread so the GTK loop never blocks on mpv startup.
fn run_cli(args: &[&str]) {
    let exe = config::program_dir().join(if cfg!(windows) { "RustMIC.exe" } else { "RustMIC" });
    let args = args.iter().map(|arg| arg.to_string()).collect::<Vec<_>>();
    thread::spawn(move || {
        let _ = rustmic_mic::command(&exe)
            .args(&args)
            .stdin(Stdio::null())
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .status();
    });
}

/// Connection details for the currently selected transport.
fn link_info(cfg: &Settings) -> Option<String> {
    match cfg.transport.as_str() {
        "wifi" | "wifi-direct" => {
            if cfg.host.trim().is_empty() {
                None
            } else {
                Some(cfg.host.clone())
            }
        }
        "bluetooth" => {
            if cfg.bt_address.trim().is_empty() {
                None
            } else {
                Some(cfg.bt_address.clone())
            }
        }
        _ => adb::usb_phone(&cfg.adb)
            .map(|phone| phone.model.unwrap_or_else(|| "телефон".into())),
    }
}

fn link_label(cfg: &Settings) -> &'static str {
    match cfg.transport.as_str() {
        "wifi" => "Телефон (Wi-Fi)",
        "wifi-direct" => "Телефон (Wi-Fi Direct)",
        "bluetooth" => "Телефон (Bluetooth)",
        _ => "Телефон (USB)",
    }
}

fn phone_connected() -> bool {
    link_info(&settings()).is_some()
}

/// Is the rustmic client alive right now? Checked through /proc so the poller
/// does not spawn a process every 1.5 s.
fn client_running() -> bool {
    let Ok(entries) = std::fs::read_dir("/proc") else {
        return false;
    };
    for entry in entries.flatten() {
        let name = entry.file_name();
        let name = name.to_string_lossy();
        if !name.starts_with(|c: char| c.is_ascii_digit()) {
            continue;
        }
        let Ok(cmdline) = std::fs::read(entry.path().join("cmdline")) else {
            continue;
        };
        if cmdline.split(|byte| *byte == 0).next() == Some(b"rustmic") {
            return true;
        }
    }
    false
}

fn default_is_rustmic() -> bool {
    audio::default_is_rustmic()
}

fn set_default(on: bool) {
    let _ = audio::set_default(on);
}

fn open_journal() {
    platform::open_log();
}

fn test_mic() {
    platform::test_mic();
}

fn diagnostics() -> String {
    platform::diagnostics(&settings())
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
        "RustMIC".into()
    }

    fn title(&self) -> String {
        "RustMIC".into()
    }

    fn icon_name(&self) -> String {
        if self.service && self.client {
            "RustMIC".into()
        } else {
            "RustMIC-off".into()
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
            title: "RustMIC".into(),
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
                activate: Box::new(|tray: &mut Self| {
                    let _ = tray.tx.send(Cmd::Show);
                }),
                ..Default::default()
            }
            .into(),
            MenuItem::Separator,
            StandardItem {
                label: "Перезапустить".into(),
                icon_name: "view-refresh".into(),
                activate: Box::new(|tray: &mut Self| {
                    let _ = tray.tx.send(Cmd::Restart);
                }),
                ..Default::default()
            }
            .into(),
            StandardItem {
                label: "Тест микрофона".into(),
                icon_name: "media-playback-start".into(),
                activate: Box::new(|tray: &mut Self| {
                    let _ = tray.tx.send(Cmd::Test);
                }),
                ..Default::default()
            }
            .into(),
            MenuItem::Separator,
            CheckmarkItem {
                label: "Микрофон включён".into(),
                checked: self.service,
                activate: Box::new(|tray: &mut Self| {
                    let _ = tray.tx.send(Cmd::ToggleMic);
                }),
                ..Default::default()
            }
            .into(),
            CheckmarkItem {
                label: "Шумодав".into(),
                checked: settings().denoise,
                activate: Box::new(|tray: &mut Self| {
                    let _ = tray.tx.send(Cmd::ToggleDenoise);
                }),
                ..Default::default()
            }
            .into(),
            CheckmarkItem {
                label: "Микрофон по умолчанию".into(),
                checked: self.default_mic,
                activate: Box::new(|tray: &mut Self| {
                    let _ = tray.tx.send(Cmd::ToggleDefault);
                }),
                ..Default::default()
            }
            .into(),
            MenuItem::Separator,
            StandardItem {
                label: "Выход".into(),
                icon_name: "application-exit".into(),
                activate: Box::new(|tray: &mut Self| {
                    let _ = tray.tx.send(Cmd::Quit);
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
                &format!("--device={}.monitor", config::SINK),
                "--raw",
                "--format=s16le",
                "--rate=48000",
                "--channels=1",
            ])
            .stdout(Stdio::piped())
            .stderr(Stdio::null())
            .spawn()
        {
            Ok(child) => child,
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
                        let sample = i16::from_le_bytes([pair[0], pair[1]]) as f64;
                        sum += sample * sample;
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
    status_link_label: Label,
    status_phone: Label,
    status_client: Label,
    status_service: Label,
    level: ProgressBar,
    sw_mic: Switch,
    sw_default: Switch,
    music_now: Label,
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
        .title("RustMIC")
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
    let title = Label::new(Some("RustMIC"));
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
        let guard = guard.clone();
        sw_mic.connect_state_set(move |_, on| {
            if !guard.get() {
                thread::spawn(move || {
                    let _ = if on {
                        platform::start()
                    } else {
                        platform::stop()
                    };
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

    let status_link_label = Label::new(Some(link_label(&settings())));
    let status_phone = Label::new(Some("—"));
    let status_client = Label::new(Some("—"));
    let status_service = Label::new(Some("—"));
    let level = ProgressBar::new();
    level.set_show_text(false);
    level.set_size_request(-1, 8);

    grid.attach(&status_link_label, 0, 0, 1, 1);
    grid.attach(&status_phone, 1, 0, 1, 1);
    grid.attach(&Label::new(Some("Поток")), 0, 1, 1, 1);
    grid.attach(&status_client, 1, 1, 1, 1);
    grid.attach(&Label::new(Some("Сервис")), 0, 2, 1, 1);
    grid.attach(&status_service, 1, 2, 1, 1);
    grid.attach(&Label::new(Some("Уровень")), 0, 3, 1, 1);
    grid.attach(&level, 1, 3, 1, 1);
    for label in [&status_phone, &status_client, &status_service] {
        label.set_xalign(0.0);
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
    let current = settings();

    // Connection: no cable needed for Wi-Fi / Wi-Fi Direct / Bluetooth.
    let transport_drop = DropDown::from_strings(config::TRANSPORTS);
    transport_drop.set_selected(
        config::TRANSPORTS
            .iter()
            .position(|transport| *transport == current.transport)
            .unwrap_or(0) as u32,
    );
    {
        let notice = settings_notice.clone();
        transport_drop.connect_selected_notify(move |drop| {
            let choice = config::TRANSPORTS
                .get(drop.selected() as usize)
                .copied()
                .unwrap_or("usb");
            let mut next = settings();
            next.transport = choice.to_string();
            match save(&next) {
                Ok(()) => restart_service(),
                Err(error) => {
                    notice.set_text(&format!("Не удалось сохранить настройки: {error}"));
                    notice.set_visible(true);
                }
            }
        });
    }
    cfg_box.append(&row("Подключение", &transport_drop));

    let host_entry = Entry::new();
    host_entry.set_text(&current.host);
    host_entry.set_placeholder_text(Some("IP телефона, например 192.168.1.50"));
    host_entry.set_tooltip_text(Some("Wi-Fi / Wi-Fi Direct: IP WO Mic на телефоне. Enter — перезапуск."));
    {
        let notice = settings_notice.clone();
        host_entry.connect_changed(move |entry| {
            let mut next = settings();
            let text = entry.text().to_string();
            if next.host == text {
                return;
            }
            next.host = text;
            if let Err(error) = save(&next) {
                notice.set_text(&format!("Не удалось сохранить настройки: {error}"));
                notice.set_visible(true);
            }
        });
        host_entry.connect_activate(|_| restart_service());
    }
    cfg_box.append(&row("Хост (Wi-Fi)", &host_entry));

    let bt_entry = Entry::new();
    bt_entry.set_text(&current.bt_address);
    bt_entry.set_placeholder_text(Some("AA:BB:CC:DD:EE:FF"));
    bt_entry.set_tooltip_text(Some("Bluetooth: MAC-адрес телефона. Enter — перезапуск."));
    {
        let notice = settings_notice.clone();
        bt_entry.connect_changed(move |entry| {
            let mut next = settings();
            let text = entry.text().to_string();
            if next.bt_address == text {
                return;
            }
            next.bt_address = text;
            if let Err(error) = save(&next) {
                notice.set_text(&format!("Не удалось сохранить настройки: {error}"));
                notice.set_visible(true);
            }
        });
        bt_entry.connect_activate(|_| restart_service());
    }
    cfg_box.append(&row("Bluetooth MAC", &bt_entry));

    let sw_default = Switch::new();
    sw_default.set_active(default_is_rustmic());
    {
        let guard = guard.clone();
        sw_default.connect_state_set(move |_, on| {
            if !guard.get() {
                set_default(on);
            }
            glib::Propagation::Proceed
        });
    }
    cfg_box.append(&row("Микрофон по умолчанию", &sw_default));

    let sw_denoise = Switch::new();
    sw_denoise.set_active(current.denoise);
    {
        let guard = guard.clone();
        let notice = settings_notice.clone();
        sw_denoise.connect_state_set(move |_, on| {
            if !guard.get() {
                let mut next = settings();
                next.denoise = on;
                match save(&next) {
                    Ok(()) => {
                        notice.set_visible(false);
                        restart_service();
                    }
                    Err(error) => {
                        notice.set_text(&format!("Не удалось сохранить настройки: {error}"));
                        notice.set_visible(true);
                        eprintln!("RustMIC-gui: cannot save settings: {error}");
                    }
                }
            }
            glib::Propagation::Proceed
        });
    }
    cfg_box.append(&row("Шумодав (RNNoise)", &sw_denoise));

    let scale_gate = Scale::with_range(Orientation::Horizontal, 0.0, 1.0, 0.05);
    scale_gate.set_value(current.gate as f64);
    scale_gate.set_draw_value(false);
    scale_gate.set_hexpand(true);
    scale_gate.set_size_request(160, -1);

    let gate_value = Label::new(Some(&fmt_gate(current.gate)));
    gate_value.set_width_chars(5);
    gate_value.set_xalign(1.0);
    gate_value.add_css_class("dim-label");

    {
        // Debounce: one restart per drag, never two, and never remove a source
        // id that has already fired (that used to panic and kill the app).
        let armed = Rc::new(Cell::new(false));
        let flag = armed.clone();
        let value_label = gate_value.clone();
        let notice = settings_notice.clone();
        scale_gate.connect_value_changed(move |scale| {
            let value = scale.value() as f32;
            value_label.set_text(&fmt_gate(value));
            let mut next = settings();
            next.gate = value;
            if let Err(error) = save(&next) {
                notice.set_text(&format!("Не удалось сохранить настройки: {error}"));
                notice.set_visible(true);
                eprintln!("RustMIC-gui: cannot save settings: {error}");
                return;
            }
            notice.set_visible(false);
            if flag.get() {
                return;
            }
            flag.set(true);
            let flag = flag.clone();
            glib::timeout_add_local_once(Duration::from_millis(700), move || {
                flag.set(false);
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
    let volume = current.volume;
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
            let mut next = settings();
            next.volume = value;
            if let Err(error) = save(&next) {
                label.set_tooltip_text(Some(&format!("Не удалось сохранить: {error}")));
                notice.set_text(&format!("Не удалось сохранить настройки: {error}"));
                notice.set_visible(true);
                eprintln!("RustMIC-gui: cannot save settings: {error}");
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
                if audio::linux::set_source_volume(value) {
                    label.set_tooltip_text(None);
                    notice.set_visible(false);
                } else {
                    label.set_tooltip_text(Some(
                        "Значение сохранено; PipeWire применит его, когда источник будет доступен",
                    ));
                    notice.set_text(
                        "Громкость сохранена; PipeWire применит её, когда источник появится",
                    );
                    notice.set_visible(true);
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

    // --- music straight into the microphone
    let music_frame = gtk::Frame::builder().label("Музыка в микрофон").build();
    music_frame.add_css_class("card");
    let music_box = gtk::Box::new(Orientation::Vertical, 8);
    music_box.set_margin_top(10);
    music_box.set_margin_bottom(10);
    music_box.set_margin_start(12);
    music_box.set_margin_end(12);
    let music_note = Label::new(Some("Название или ссылка YouTube / YouTube Music / SoundCloud  — играет сразу в микрофон, файл скачивать не надо"));
    music_note.set_xalign(0.0);
    music_note.set_wrap(true);
    music_note.add_css_class("dim-label");
    let music_entry = Entry::new();
    music_entry.set_placeholder_text(Some("например: Harvest Moon или https://youtu.be/..."));
    let music_now = Label::new(Some("ничего не играет"));
    music_now.set_xalign(0.0);
    music_now.add_css_class("dim-label");
    let music_buttons = gtk::Box::new(Orientation::Horizontal, 8);
    let btn_play = Button::with_label("Играть");
    let query_entry = music_entry.clone();
    btn_play.connect_clicked(move |_| {
        let query = query_entry.text().to_string();
        if query.trim().is_empty() {
            return;
        }
        run_cli(&["play", &query]);
    });
    let btn_stop = Button::with_label("Стоп");
    btn_stop.connect_clicked(|_| run_cli(&["stop-music"]));
    music_buttons.append(&btn_play);
    music_buttons.append(&btn_stop);
    music_box.append(&music_note);
    music_box.append(&music_entry);
    music_box.append(&music_buttons);
    music_box.append(&music_now);
    music_frame.set_child(Some(&music_box));
    root.append(&music_frame);

    // --- actions
    let actions = gtk::Grid::builder()
        .column_spacing(8)
        .row_spacing(8)
        .column_homogeneous(true)
        .build();

    let btn_restart = gtk::Button::with_label("Перезапустить");
    btn_restart.set_tooltip_text(Some("Перезапустить службу RustMIC"));
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
        let button = btn_diag.clone();
        btn_diag.connect_clicked(move |_| {
            let text = diagnostics();
            if let Some(display) = gtk::gdk::Display::default() {
                display.clipboard().set_text(&text);
            }
            button.set_label("Скопировано");
            let button = button.clone();
            glib::timeout_add_local_once(Duration::from_millis(1500), move || {
                button.set_label("Диагностика");
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
        status_link_label,
        status_phone,
        status_client,
        status_service,
        level,
        sw_mic,
        sw_default,
        music_now,
        guard,
    });

    // hide instead of quitting
    let window = ui.window.clone();
    ui.window.connect_close_request(move |_| {
        window.set_visible(false);
        glib::Propagation::Stop
    });

    ui
}

fn row(label: &str, widget: &impl IsA<gtk::Widget>) -> gtk::Box {
    let box_row = gtk::Box::new(Orientation::Horizontal, 12);
    let text = Label::new(Some(label));
    text.set_xalign(0.0);
    text.set_hexpand(true);
    box_row.append(&text);
    box_row.append(widget);
    box_row
}

pub fn run() {
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
        default_mic: default_is_rustmic(),
    };
    let handle = match tray.spawn() {
        Ok(handle) => Some(handle),
        Err(error) => {
            eprintln!("RustMIC-gui: tray unavailable: {error}");
            None
        }
    };

    // background state poller
    let poll_tx = state_tx.clone();
    thread::spawn(move || loop {
        let state = State {
            service: service_active(),
            phone: link_info(&settings()),
            client: client_running(),
            default_mic: default_is_rustmic(),
        };
        if poll_tx.send(state).is_err() {
            return;
        }
        thread::sleep(Duration::from_millis(1500));
    });

    spawn_level_meter(level_tx);

    let app = Application::builder()
        .application_id("io.github.OlegVapnik.RustMIC")
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
                        let _ = if service_active() {
                            platform::stop()
                        } else {
                            platform::start()
                        };
                    }
                    Cmd::ToggleDenoise => {
                        let mut next = settings();
                        next.denoise = !next.denoise;
                        match save(&next) {
                            Ok(()) => restart_service(),
                            Err(error) => {
                                eprintln!("RustMIC-gui: cannot save settings: {error}")
                            }
                        }
                    }
                    Cmd::ToggleDefault => {
                        set_default(!default_is_rustmic());
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
            while let Ok(value) = level_rx.borrow().try_recv() {
                peak = peak.max(value);
            }
            let previous = smooth.get();
            let next = if peak > previous { peak } else { previous * 0.75 };
            smooth.set(next);
            if let Some(ui) = ui_for_tick.borrow().as_ref() {
                ui.level.set_fraction((next * 3.0).min(1.0) as f64);
                let playing = rustmic_mic::music::now_playing()
                    .map(|query| format!("играет: {query}"))
                    .unwrap_or_else(|| "ничего не играет".to_string());
                ui.music_now.set_text(&playing);
            }

            // state refresh
            let mut latest: Option<State> = None;
            while let Ok(state) = state_rx.borrow().try_recv() {
                latest = Some(state);
            }
            if let Some(state) = latest {
                if let Some(ui) = ui_for_tick.borrow().as_ref() {
                    let cfg = settings();
                    ui.status_link_label.set_text(link_label(&cfg));
                    ui.status_phone.set_text(match &state.phone {
                        Some(info) => info,
                        None => "не найден",
                    });
                    ui.status_client
                        .set_text(if state.client { "идёт" } else { "нет" });
                    ui.status_service.set_text(if state.service {
                        "работает"
                    } else {
                        "остановлен"
                    });
                    ui.guard.set(true);
                    ui.sw_mic.set_active(state.service);
                    ui.sw_default.set_active(state.default_mic);
                    ui.guard.set(false);
                }
                if let Some(handle) = &handle_for_tick {
                    let (service, phone, client, default_mic) = (
                        state.service,
                        state.phone.is_some(),
                        state.client,
                        state.default_mic,
                    );
                    handle.update(move |tray: &mut Tray| {
                        tray.service = service;
                        tray.phone = phone;
                        tray.client = client;
                        tray.default_mic = default_mic;
                    });
                }
            }

            glib::ControlFlow::Continue
        }
    });

    app.connect_shutdown(move |_| {
        if let Some(handle) = &handle {
            let _ = handle.shutdown();
        }
    });

    app.run();
    std::process::exit(0);
}

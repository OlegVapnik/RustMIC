//! Shared egui front-end for the RustMIC daemon.
//!
//! The GUI is deliberately stateless about audio: it reads `AppState` from the
//! local control socket and sends `Command`s back. When the daemon is not
//! running it shows an explicit "служба не запущена" screen with a start button
//! instead of quietly falling back to stale files.
//!
//! One window, three tabs: microphone, soundpad and settings. The Linux build
//! adds a StatusNotifier tray; Windows uses its own tray module.

#[cfg(unix)]
#[path = "tray_linux.rs"]
mod tray;
#[cfg(windows)]
#[path = "tray_windows.rs"]
mod tray;

use std::sync::mpsc::{channel, Receiver, Sender};
use std::thread;
use std::time::{Duration, Instant};

use eframe::egui::{
    self, Align, Align2, Color32, FontData, FontDefinitions, FontFamily, FontId, Layout, Margin,
    RichText, Sense, Stroke, TextStyle, Theme, ThemePreference, Vec2,
};

use crate::config::{self, GateMode, Settings};
use crate::core::{AppState, Levels, Tile};
use crate::{audio, ipc, platform};

use tray::{TrayCommand, TrayHandle, TrayStatus};

// ---------------------------------------------------------------- palette

const BG: Color32 = Color32::from_rgb(10, 13, 17);
const PANEL: Color32 = Color32::from_rgb(15, 19, 25);
const CARD: Color32 = Color32::from_rgb(20, 25, 32);
const CARD_HOVER: Color32 = Color32::from_rgb(26, 32, 41);
const BORDER: Color32 = Color32::from_rgb(39, 47, 58);
const TEXT: Color32 = Color32::from_rgb(232, 238, 245);
const MUTED: Color32 = Color32::from_rgb(138, 150, 164);
const ACCENT: Color32 = Color32::from_rgb(64, 214, 181);
const ACCENT_DIM: Color32 = Color32::from_rgb(31, 108, 94);
const AMBER: Color32 = Color32::from_rgb(242, 190, 82);
const RED: Color32 = Color32::from_rgb(240, 103, 103);
const GREEN: Color32 = Color32::from_rgb(89, 214, 129);

fn semibold() -> FontFamily {
    FontFamily::Name("InterSemiBold".into())
}

fn heading(text: impl Into<String>) -> RichText {
    RichText::new(text).family(semibold()).size(18.0).color(TEXT)
}

fn small(text: impl Into<String>) -> RichText {
    RichText::new(text).size(11.0).color(MUTED)
}

// ---------------------------------------------------------------- events

enum UiEvent {
    State(Box<AppState>),
    Offline(String),
    DefaultMic(bool),
    Diagnostics(String),
    Notice(String),
}

#[derive(Clone, Copy, PartialEq, Eq)]
enum Tab {
    Mic,
    Pad,
    Settings,
}

struct GateChange {
    value: f32,
    commit: bool,
}

// ---------------------------------------------------------------- app

struct App {
    settings: Settings,
    state: AppState,
    online: bool,
    offline_error: Option<String>,
    tab: Tab,
    events: Receiver<UiEvent>,
    event_tx: Sender<UiEvent>,
    commands: Sender<crate::core::Command>,
    tray_commands: Receiver<TrayCommand>,
    tray: Option<TrayHandle>,
    tray_updated: Instant,
    default_mic: bool,
    smooth_db: f32,
    peak_db: f32,
    peak_at: Instant,
    last_frame: Instant,
    audio_edit_until: Instant,
    music_query: String,
    diagnostics: Option<String>,
    notice: Option<(String, Instant)>,
    busy_since: Option<Instant>,
    settings_dirty: bool,
    hidden: bool,
    quitting: bool,
}

impl Drop for App {
    fn drop(&mut self) {
        if let Some(tray) = &self.tray {
            tray.shutdown();
        }
    }
}

impl App {
    fn new(cc: &eframe::CreationContext<'_>) -> Self {
        install_theme(&cc.egui_ctx);
        egui_extras::install_image_loaders(&cc.egui_ctx);

        let settings = Settings::load();
        let state = AppState::from_settings(&settings);

        let (event_tx, events) = channel::<UiEvent>();
        let (command_tx, command_rx) = channel::<crate::core::Command>();

        spawn_state_reader(event_tx.clone());
        spawn_command_worker(command_rx, event_tx.clone());
        spawn_system_poller(event_tx.clone());

        let (tray_tx, tray_commands) = channel::<TrayCommand>();
        let tray = match tray::spawn(tray_tx) {
            Ok(handle) => Some(handle),
            Err(error) => {
                eprintln!("RustMIC-gui: tray unavailable: {error}");
                None
            }
        };

        Self {
            settings,
            state,
            online: false,
            offline_error: None,
            tab: Tab::Mic,
            events,
            event_tx,
            commands: command_tx,
            tray_commands,
            tray,
            tray_updated: Instant::now() - Duration::from_secs(5),
            default_mic: false,
            smooth_db: -60.0,
            peak_db: -60.0,
            peak_at: Instant::now(),
            last_frame: Instant::now(),
            audio_edit_until: Instant::now(),
            music_query: String::new(),
            diagnostics: None,
            notice: None,
            busy_since: None,
            settings_dirty: false,
            hidden: false,
            quitting: false,
        }
    }

    fn service_running(&self) -> bool {
        self.online && self.state.service_running
    }

    fn status(&self) -> (&'static str, Color32) {
        if !self.service_running() {
            ("Служба не запущена", RED)
        } else if self.state.connection.client_running {
            ("Микрофон работает", GREEN)
        } else if self.state.connection.phone_connected {
            ("Телефон подключён, поток не идёт", AMBER)
        } else {
            ("Ожидание телефона", AMBER)
        }
    }

    fn send(&mut self, command: crate::core::Command) {
        if self.online {
            let _ = self.commands.send(command);
        } else {
            self.persist_audio();
            if let Err(error) = self.settings.save() {
                self.notify(format!("Не удалось сохранить настройки: {error}"));
            } else {
                self.notify("Служба не запущена: настройка сохранена".into());
            }
        }
    }

    fn persist_audio(&mut self) {
        self.settings.denoise = self.state.audio.denoise;
        self.settings.gate_mode = self.state.audio.gate_mode;
        self.settings.gate_voice = self.state.audio.gate_voice;
        self.settings.gate_level_db = self.state.audio.gate_level_db;
        self.settings.mic_volume = self.state.audio.mic_volume;
        self.settings.music_volume = self.state.audio.music_volume;
        self.settings.loudnorm = self.state.audio.loudnorm;
        self.settings.ducking = self.state.audio.ducking;
        self.settings.ducking_db = self.state.audio.ducking_db;
        self.settings.monitor_music = self.state.audio.monitor_music;
    }

    fn notify(&mut self, text: String) {
        self.notice = Some((text, Instant::now()));
    }

    fn start_service(&mut self) {
        self.busy_since = Some(Instant::now());
        thread::spawn(|| {
            let _ = platform::start();
        });
    }

    fn stop_service(&mut self) {
        self.busy_since = Some(Instant::now());
        thread::spawn(|| {
            let _ = platform::stop();
        });
    }

    fn restart_service(&mut self) {
        self.busy_since = Some(Instant::now());
        thread::spawn(|| {
            let _ = platform::restart();
        });
    }

    fn play(&mut self, query: String) {
        if query.trim().is_empty() {
            return;
        }
        if !self.online {
            self.notify("Служба не запущена — сначала включите микрофон".into());
            return;
        }
        let _ = self
            .commands
            .send(crate::core::Command::Play { query: query.clone() });
        self.music_query.clear();
    }

    fn pump_events(&mut self, ctx: &egui::Context) {
        while let Ok(event) = self.events.try_recv() {
            match event {
                UiEvent::State(next) => {
                    let mut next = *next;
                    let editing = Instant::now() < self.audio_edit_until;
                    if editing {
                        next.audio = self.state.audio.clone();
                    }
                    self.state = next;
                    self.online = true;
                    self.offline_error = None;
                    self.busy_since = None;
                }
                UiEvent::Offline(error) => {
                    if self.online {
                        self.state.service_running = false;
                        self.state.connection.client_running = false;
                    }
                    self.online = false;
                    self.state.service_running = false;
                    self.offline_error = Some(error);
                }
                UiEvent::DefaultMic(value) => self.default_mic = value,
                UiEvent::Diagnostics(text) => self.diagnostics = Some(text),
                UiEvent::Notice(text) => self.notify(text),
            }
        }

        while let Ok(command) = self.tray_commands.try_recv() {
            match command {
                TrayCommand::Show => {
                    self.hidden = false;
                    ctx.send_viewport_cmd(egui::ViewportCommand::Visible(true));
                    ctx.send_viewport_cmd(egui::ViewportCommand::Focus);
                }
                TrayCommand::ToggleMic => {
                    if self.service_running() {
                        self.stop_service();
                    } else {
                        self.start_service();
                    }
                }
                TrayCommand::ToggleDenoise => {
                    self.state.audio.denoise = !self.state.audio.denoise;
                    self.audio_edit_until = Instant::now() + Duration::from_millis(700);
                    self.send(crate::core::Command::SetDenoise {
                        enabled: self.state.audio.denoise,
                    });
                }
                TrayCommand::ToggleDefault => {
                    let on = !self.default_mic;
                    self.default_mic = on;
                    thread::spawn(move || {
                        let _ = audio::set_default(on);
                    });
                }
                TrayCommand::Restart => self.restart_service(),
                TrayCommand::Test => platform::test_mic(),
                TrayCommand::Quit => {
                    self.quitting = true;
                    ctx.send_viewport_cmd(egui::ViewportCommand::Close);
                }
            }
        }

        if !self.quitting && self.tray.is_some() {
            if let Some(status) = self.tray_status() {
                let show = self.tray_updated.elapsed() >= Duration::from_millis(700);
                if show {
                    if let Some(tray) = &self.tray {
                        tray.update(status);
                    }
                    self.tray_updated = Instant::now();
                }
            }
        }

        if let Some((_, at)) = &self.notice {
            if at.elapsed() > Duration::from_secs(6) {
                self.notice = None;
            }
        }
        if let Some(at) = self.busy_since {
            if at.elapsed() > Duration::from_secs(20) {
                self.busy_since = None;
            }
        }

        if !self.quitting && self.tray.is_some() && self.hidden {
            // Keep the app alive while the window is in the tray.
            ctx.request_repaint_after(Duration::from_millis(250));
        }
    }

    fn tray_status(&self) -> Option<TrayStatus> {
        Some(TrayStatus {
            service: self.service_running(),
            phone: self.state.connection.phone_connected,
            client: self.state.connection.client_running,
            default_mic: self.default_mic,
            denoise: self.state.audio.denoise,
        })
    }

    // ------------------------------------------------------------ top bars

    fn top_bar(&mut self, ui: &mut egui::Ui) {
        egui::Panel::top("RustMIC-top")
            .exact_size(58.0)
            .resizable(false)
            .show_separator_line(false)
            .frame(
                egui::Frame::new()
                    .fill(PANEL)
                    .inner_margin(Margin::symmetric(18, 10)),
            )
            .show(ui, |ui| {
                ui.horizontal_centered(|ui| {
                    ui.label(
                        RichText::new("RustMIC")
                            .family(semibold())
                            .size(20.0)
                            .color(TEXT),
                    );
                    ui.add_space(8.0);
                    let (text, color) = self.status();
                    status_pill(ui, text, color);
                    ui.with_layout(Layout::right_to_left(Align::Center), |ui| {
                        ui.menu_button("Меню", |ui| {
                            if ui.button("Открыть настройки").clicked() {
                                self.tab = Tab::Settings;
                                ui.close();
                            }
                            if ui.button("Папка настроек").clicked() {
                                platform::open_settings_folder();
                                ui.close();
                            }
                            if ui.button("Журнал").clicked() {
                                platform::open_log();
                                ui.close();
                            }
                            if ui.button("Диагностика").clicked() {
                                self.request_diagnostics();
                                ui.close();
                            }
                            ui.separator();
                            if ui.button("Перезапустить службу").clicked() {
                                self.restart_service();
                                ui.close();
                            }
                            if ui.button("Выход").clicked() {
                                self.quitting = true;
                                ui.ctx()
                                    .send_viewport_cmd(egui::ViewportCommand::Close);
                            }
                        });
                    });
                });
            });
    }

    fn tabs_bar(&mut self, ui: &mut egui::Ui) {
        egui::Panel::top("RustMIC-tabs")
            .resizable(false)
            .show_separator_line(false)
            .frame(
                egui::Frame::new()
                    .fill(PANEL)
                    .inner_margin(Margin::symmetric(18, 0)),
            )
            .show(ui, |ui| {
                ui.horizontal(|ui| {
                    for (tab, label) in [
                        (Tab::Mic, "Микрофон"),
                        (Tab::Pad, "Саундпад"),
                        (Tab::Settings, "Настройки"),
                    ] {
                        let selected = self.tab == tab;
                        let text = RichText::new(label)
                            .size(14.0)
                            .family(if selected { semibold() } else { FontFamily::Proportional })
                            .color(if selected { ACCENT } else { MUTED });
                        if ui.selectable_label(selected, text).clicked() {
                            self.tab = tab;
                        }
                    }
                });
                ui.add_space(2.0);
            });
    }

    // ------------------------------------------------------------ microphone

    fn tab_microphone(&mut self, ui: &mut egui::Ui) {
        if !self.service_running() {
            self.offline_card(ui);
            ui.add_space(12.0);
        }

        card(ui, "Соединение", |ui| {
            let connection = self.state.connection.clone();
            let (title, state_text, state_color) = connection_summary(&connection);
            ui.horizontal(|ui| {
                ui.label(RichText::new(title).size(16.0).family(semibold()).color(TEXT));
                ui.with_layout(Layout::right_to_left(Align::Center), |ui| {
                    status_pill(ui, state_text, state_color);
                });
            });
            ui.add_space(8.0);
            dot_row(
                ui,
                "Поток",
                if connection.client_running {
                    "идёт"
                } else if !self.service_running() {
                    "служба остановлена"
                } else {
                    "нет"
                },
                if connection.client_running { GREEN } else { AMBER },
            );
            dot_row(
                ui,
                "Виртуальный микрофон",
                if connection.sink_present && connection.source_present {
                    "готов"
                } else {
                    "не найден"
                },
                if connection.sink_present && connection.source_present {
                    GREEN
                } else {
                    RED
                },
            );
            if !connection.phone_connected {
                ui.add_space(4.0);
                ui.label(small("Проверьте кабель, Wi-Fi или Bluetooth и запустите WO Mic на телефоне."));
            }
        });

        ui.add_space(12.0);
        card(ui, "Уровень", |ui| {
            if let Some(change) =
                level_meter(ui, &self.state.levels, &self.state.audio, self.smooth_db, self.peak_db)
            {
                let now = Instant::now();
                match self.state.audio.gate_mode {
                    GateMode::Level => {
                        self.state.audio.gate_level_db = change.value;
                        self.settings.gate_level_db = change.value;
                    }
                    GateMode::Voice => {
                        self.state.audio.gate_voice = change.value.round() as u32;
                        self.settings.gate_voice = self.state.audio.gate_voice;
                    }
                }
                self.audio_edit_until = now + Duration::from_millis(700);
                if change.commit {
                    match self.state.audio.gate_mode {
                        GateMode::Level => {
                            let _ = self.commands.send(crate::core::Command::SetGateLevel {
                                value: change.value,
                            });
                        }
                        GateMode::Voice => {
                            let _ = self.commands.send(crate::core::Command::SetGateVoice {
                                value: change.value.round() as u32,
                            });
                        }
                    }
                }
            }
            ui.add_space(8.0);
            ui.horizontal(|ui| {
                let muted = self.state.audio.muted;
                let button = if muted {
                    egui::Button::new(RichText::new("Заглушить микрофон").color(RED))
                        .fill(Color32::from_rgb(55, 23, 27))
                        .stroke(Stroke::new(1.0, RED))
                } else {
                    egui::Button::new("Заглушить микрофон")
                };
                if ui
                    .add(button)
                    .on_hover_text(if muted {
                        "Микрофон заглушён — нажмите, чтобы включить"
                    } else {
                        "Нажмите, чтобы заглушить микрофон"
                    })
                    .clicked()
                {
                    let muted = !muted;
                    self.state.audio.muted = muted;
                    let _ = self
                        .commands
                        .send(crate::core::Command::SetMuted { muted });
                }
                if muted {
                    ui.label(RichText::new("заглушён").size(12.0).color(RED));
                }
                if ui.button("Тест микрофона").clicked() {
                    platform::test_mic();
                }
                ui.with_layout(Layout::right_to_left(Align::Center), |ui| {
                    ui.label(small(format!(
                        "гейт: {}",
                        if self.state.levels.gate_open {
                            "открыт"
                        } else {
                            "закрыт"
                        }
                    )));
                });
            });
        });

        ui.add_space(12.0);
        card(ui, "Обработка", |ui| {
            let mut denoise = self.state.audio.denoise;
            if toggle_row(
                ui,
                "Шумодав RNNoise",
                &mut denoise,
                true,
                "Убирает постоянный шум до гейта",
            ) {
                self.state.audio.denoise = denoise;
                self.audio_edit_until = Instant::now() + Duration::from_millis(700);
                self.send(crate::core::Command::SetDenoise { enabled: denoise });
            }

            ui.add_space(8.0);
            let mut mode = self.state.audio.gate_mode;
            ui.horizontal(|ui| {
                ui.label(RichText::new("Режим гейта").size(13.0).color(TEXT));
                ui.with_layout(Layout::right_to_left(Align::Center), |ui| {
                    egui::ComboBox::from_id_salt("gate-mode")
                        .selected_text(match mode {
                            GateMode::Voice => "по голосу (VAD)",
                            GateMode::Level => "по уровню (dBFS)",
                        })
                        .show_ui(ui, |ui| {
                            ui.selectable_value(&mut mode, GateMode::Voice, "по голосу (VAD)");
                            ui.selectable_value(&mut mode, GateMode::Level, "по уровню (dBFS)");
                        });
                });
            });
            if mode != self.state.audio.gate_mode {
                self.state.audio.gate_mode = mode;
                self.settings.gate_mode = mode;
                self.audio_edit_until = Instant::now() + Duration::from_millis(700);
                let _ = self
                    .commands
                    .send(crate::core::Command::SetGateMode { mode });
            }

            ui.add_space(6.0);
            match self.state.audio.gate_mode {
                GateMode::Voice => {
                    let mut gate = self.state.audio.gate_voice as f32;
                    let row = slider_row(ui, "Порог VAD", &mut gate, 0.0..=100.0, "%", 0);
                    if row.changed {
                        let gate = gate.round() as u32;
                        self.state.audio.gate_voice = gate;
                        self.settings.gate_voice = gate;
                        self.audio_edit_until = Instant::now() + Duration::from_millis(700);
                    }
                    if row.committed {
                        let _ = self.commands.send(crate::core::Command::SetGateVoice {
                            value: gate.round() as u32,
                        });
                    }
                    ui.label(small(
                        "0 % — гейт выключен. Порог можно тянуть и по полоске вероятности голоса.",
                    ));
                }
                GateMode::Level => {
                    let mut gate = self.state.audio.gate_level_db;
                    let row = slider_row(ui, "Порог", &mut gate, -60.0..=0.0, " dB", 0);
                    if row.changed {
                        self.state.audio.gate_level_db = gate;
                        self.settings.gate_level_db = gate;
                        self.audio_edit_until = Instant::now() + Duration::from_millis(700);
                    }
                    if row.committed {
                        let _ = self
                            .commands
                            .send(crate::core::Command::SetGateLevel { value: gate });
                    }
                    ui.label(small(
                        "Гистерезис 3 дБ: сигнал ниже порога закрывает гейт.",
                    ));
                }
            }

            ui.add_space(10.0);
            let mut volume = self.state.audio.mic_volume as f32;
            let row = slider_row(ui, "Громкость микрофона", &mut volume, 0.0..=100.0, "%", 0);
            if row.changed {
                let volume = volume.round() as u32;
                self.state.audio.mic_volume = volume;
                self.settings.mic_volume = volume;
                self.audio_edit_until = Instant::now() + Duration::from_millis(700);
            }
            if row.committed {
                let _ = self
                    .commands
                    .send(crate::core::Command::SetMicVolume { value: volume.round() as u32 });
            }
        });
    }

    fn offline_card(&mut self, ui: &mut egui::Ui) {
        egui::Frame::new()
            .fill(Color32::from_rgb(38, 24, 26))
            .stroke(Stroke::new(1.0, Color32::from_rgb(92, 49, 53)))
            .corner_radius(12.0)
            .inner_margin(Margin::same(16))
            .show(ui, |ui| {
                ui.horizontal(|ui| {
                    ui.vertical(|ui| {
                        ui.label(
                            RichText::new("Служба RustMIC не запущена")
                                .family(semibold())
                                .size(16.0)
                                .color(TEXT),
                        );
                        ui.label(small(
                            "GUI работает отдельно от демона. Запустите службу, чтобы включить микрофон.",
                        ));
                        if let Some(error) = &self.offline_error {
                            ui.label(small(format!("сокет: {error}")));
                        }
                    });
                    ui.with_layout(Layout::right_to_left(Align::Center), |ui| {
                        let busy = self.busy_since.is_some();
                        if ui
                            .add_enabled(!busy, egui::Button::new("Запустить службу"))
                            .clicked()
                        {
                            self.start_service();
                        }
                        if busy {
                            ui.spinner();
                        }
                    });
                });
            });
    }

    // ------------------------------------------------------------ soundpad

    fn tab_soundpad(&mut self, ui: &mut egui::Ui) {
        if !self.service_running() {
            self.offline_card(ui);
            ui.add_space(12.0);
        }

        card(ui, "Сейчас играет", |ui| {
            let track = self.state.track.clone();
            if let Some(track) = track {
                ui.horizontal(|ui| {
                    if let Some(cover) = track.cover.clone() {
                        ui.add(
                            egui::Image::from_uri(cover)
                                .fit_to_exact_size(Vec2::splat(56.0))
                                .corner_radius(6.0),
                        );
                    } else {
                        ui.add_sized(
                            [56.0, 56.0],
                            egui::Label::new(
                                RichText::new("♪").size(26.0).color(ACCENT).family(semibold()),
                            )
                            .truncate(),
                        );
                    }
                    ui.vertical(|ui| {
                        ui.add(
                            egui::Label::new(
                                RichText::new(if track.title.is_empty() {
                                    track.query.clone()
                                } else {
                                    track.title.clone()
                                })
                                .size(16.0)
                                .family(semibold())
                                .color(TEXT),
                            )
                            .truncate(),
                        );
                        ui.add(egui::Label::new(small(track.query.clone())).truncate());
                    });
                });
                ui.add_space(8.0);
                let fraction = if track.duration > 0.0 {
                    (track.position / track.duration).clamp(0.0, 1.0) as f32
                } else {
                    0.0
                };
                ui.add(egui::ProgressBar::new(fraction).desired_width(ui.available_width()));
                ui.horizontal(|ui| {
                    ui.label(small(format!(
                        "{} / {}",
                        clock(track.position),
                        if track.duration > 0.0 {
                            clock(track.duration)
                        } else {
                            "—".into()
                        }
                    )));
                    ui.with_layout(Layout::right_to_left(Align::Center), |ui| {
                        if ui
                            .add(
                                egui::Button::new(RichText::new("■").size(16.0))
                                    .min_size(Vec2::new(34.0, 30.0)),
                            )
                            .on_hover_text("Стоп")
                            .clicked()
                        {
                            let _ = self.commands.send(crate::core::Command::StopMusic);
                        }
                        if ui
                            .add(
                                egui::Button::new(RichText::new("»").size(18.0))
                                    .min_size(Vec2::new(34.0, 30.0)),
                            )
                            .on_hover_text("+10 секунд")
                            .clicked()
                        {
                            let _ = self.commands.send(crate::core::Command::Seek { seconds: 10.0 });
                        }
                        if ui
                            .add(
                                egui::Button::new(
                                    RichText::new(if track.paused { "▶" } else { "Ⅱ" }).size(22.0),
                                )
                                .min_size(Vec2::new(44.0, 38.0)),
                            )
                            .on_hover_text(if track.paused { "Продолжить" } else { "Пауза" })
                            .clicked()
                        {
                            let _ = self.commands.send(crate::core::Command::Pause {
                                paused: !track.paused,
                            });
                        }
                        if ui
                            .add(
                                egui::Button::new(RichText::new("«").size(18.0))
                                    .min_size(Vec2::new(34.0, 30.0)),
                            )
                            .on_hover_text("−10 секунд")
                            .clicked()
                        {
                            let _ = self.commands.send(crate::core::Command::Seek { seconds: -10.0 });
                        }
                    });
                });
            } else {
                ui.label(RichText::new("Ничего не играет").size(16.0).color(MUTED));
                ui.label(small(
                    "Введите ссылку YouTube / YouTube Music / SoundCloud или название трека ниже.",
                ));
            }

            ui.add_space(10.0);
            let mut music_volume = self.state.audio.music_volume as f32;
            let row = slider_row(ui, "Громкость в микрофон", &mut music_volume, 0.0..=100.0, "%", 0);
            if row.changed {
                let volume = music_volume.round() as u32;
                self.state.audio.music_volume = volume;
                self.settings.music_volume = volume;
                self.audio_edit_until = Instant::now() + Duration::from_millis(700);
            }
            if row.committed {
                let _ = self.commands.send(crate::core::Command::SetMusicVolume {
                    value: music_volume.round() as u32,
                });
            }
        });

        ui.add_space(12.0);
        card(ui, "Звук музыки", |ui| {
            let mut loudnorm = self.state.audio.loudnorm;
            if toggle_row(
                ui,
                "Выравнивание громкости (loudnorm)",
                &mut loudnorm,
                true,
                "Подтягивает тихие и громкие треки к одному уровню",
            ) {
                self.state.audio.loudnorm = loudnorm;
                self.settings.loudnorm = loudnorm;
                self.audio_edit_until = Instant::now() + Duration::from_millis(700);
                let _ = self
                    .commands
                    .send(crate::core::Command::SetLoudnorm { enabled: loudnorm });
            }

            let mut ducking = self.state.audio.ducking;
            if toggle_row(
                ui,
                "Приглушать музыку, когда гейт открыт",
                &mut ducking,
                true,
                "Музыка тише, пока вы говорите",
            ) {
                self.state.audio.ducking = ducking;
                self.settings.ducking = ducking;
                self.audio_edit_until = Instant::now() + Duration::from_millis(700);
                let _ = self
                    .commands
                    .send(crate::core::Command::SetDucking { enabled: ducking });
            }
            if self.state.audio.ducking {
                let mut ducking_db = self.state.audio.ducking_db;
                let row = slider_row(ui, "Затухание", &mut ducking_db, 0.0..=30.0, " dB", 0);
                if row.changed {
                    self.state.audio.ducking_db = ducking_db;
                    self.settings.ducking_db = ducking_db;
                    self.audio_edit_until = Instant::now() + Duration::from_millis(700);
                }
                if row.committed {
                    let _ = self.commands.send(crate::core::Command::SetDuckingDb {
                        value: ducking_db,
                    });
                }
            }

            let mut monitor = self.state.audio.monitor_music;
            let monitor_enabled = !cfg!(windows);
            if toggle_row(
                ui,
                "Слышать музыку в колонках",
                &mut monitor,
                monitor_enabled,
                "Мониторинг доступен только на Linux",
            ) && monitor_enabled
            {
                self.state.audio.monitor_music = monitor;
                self.settings.monitor_music = monitor;
                self.audio_edit_until = Instant::now() + Duration::from_millis(700);
                let _ = self
                    .commands
                    .send(crate::core::Command::SetMonitor { enabled: monitor });
            }
            if monitor_enabled {
                let mut volume = self.state.audio.monitor_volume as f32;
                let row = slider_row(
                    ui,
                    "Громкость в колонках",
                    &mut volume,
                    0.0..=100.0,
                    "%",
                    0,
                );
                if row.changed {
                    let volume = volume.round() as u32;
                    self.state.audio.monitor_volume = volume;
                    self.settings.monitor_volume = volume;
                    self.audio_edit_until = Instant::now() + Duration::from_millis(700);
                }
                if row.committed {
                    let _ = self.commands.send(crate::core::Command::SetMonitorVolume {
                        value: volume.round() as u32,
                    });
                }
                ui.label(small(
                    "Только для вас: 0 % — тишина в колонках, в микрофон трек идёт как обычно.",
                ));
            } else {
                ui.label(small("На Windows мониторинг недоступен: музыка и голос идут в CABLE Input."));
            }
        });

        ui.add_space(12.0);
        card(ui, "Поиск и запуск", |ui| {
            let response = ui.add(
                egui::TextEdit::singleline(&mut self.music_query)
                    .hint_text("Название трека или ссылка YouTube / SoundCloud")
                    .desired_width(ui.available_width()),
            );
            let enter = response.lost_focus() && ui.input(|i| i.key_pressed(egui::Key::Enter));
            if enter {
                self.play(self.music_query.clone());
            }

            let query = self.music_query.trim().to_string();
            if !query.is_empty() {
                ui.add_space(8.0);
                ui.label(small("Результат"));
                egui::Frame::new()
                    .fill(CARD_HOVER)
                    .stroke(Stroke::new(1.0, BORDER))
                    .corner_radius(10.0)
                    .inner_margin(Margin::same(10))
                    .show(ui, |ui| {
                        ui.set_width(ui.available_width());
                        ui.horizontal(|ui| {
                            ui.label(RichText::new("♪").size(24.0).color(ACCENT));
                            ui.vertical(|ui| {
                                ui.add(
                                    egui::Label::new(
                                        RichText::new(query.clone()).size(13.0).color(TEXT),
                                    )
                                    .truncate(),
                                );
                                ui.label(small("нажмите ▶, чтобы пустить в микрофон"));
                            });
                            ui.with_layout(Layout::right_to_left(Align::Center), |ui| {
                                if ui
                                    .add(
                                        egui::Button::new(RichText::new("+").size(18.0))
                                            .min_size(Vec2::new(32.0, 30.0)),
                                    )
                                    .on_hover_text("Добавить в плитки")
                                    .clicked()
                                {
                                    let title = query.clone();
                                    let _ = self.commands.send(crate::core::Command::AddTile {
                                        title,
                                        query: query.clone(),
                                    });
                                    self.music_query.clear();
                                }
                                if ui
                                    .add(
                                        egui::Button::new(RichText::new("▶").size(18.0))
                                            .min_size(Vec2::new(36.0, 30.0)),
                                    )
                                    .on_hover_text("Играть в микрофон")
                                    .clicked()
                                {
                                    self.play(query.clone());
                                }
                            });
                        });
                    });
            }
            ui.label(small(
                "Поток идёт через yt-dlp и mpv прямо в виртуальный микрофон, без скачивания.",
            ));
        });

        ui.add_space(12.0);
        card(ui, "Плитки", |ui| {
            let tiles = self.state.tiles.clone();
            if tiles.is_empty() {
                ui.label(RichText::new("Плиток пока нет").size(14.0).color(TEXT));
                ui.label(small(
                    "Введите трек в поиске выше и нажмите «+», чтобы добавить его сюда.",
                ));
            } else {
                let mut play: Option<String> = None;
                let mut remove: Option<String> = None;
                for chunk in tiles.chunks(2) {
                    ui.columns(2, |columns| {
                        for (index, tile) in chunk.iter().enumerate() {
                            let (play_tile, remove_tile) = tile_card(&mut columns[index], tile);
                            if play_tile {
                                play = Some(tile.query.clone());
                            }
                            if remove_tile {
                                remove = Some(tile.id.clone());
                            }
                        }
                    });
                    ui.add_space(8.0);
                }
                if let Some(id) = remove {
                    let _ = self.commands.send(crate::core::Command::RemoveTile { tile_id: id });
                }
                if let Some(query) = play {
                    self.play(query);
                }
            }
        });
    }

    // ------------------------------------------------------------ settings

    fn tab_settings(&mut self, ui: &mut egui::Ui) {
        card(ui, "Подключение", |ui| {
            let mut changed = false;

            let before = self.settings.transport.clone();
            ui.horizontal(|ui| {
                ui.label(RichText::new("Канал").size(13.0).color(TEXT));
                ui.with_layout(Layout::right_to_left(Align::Center), |ui| {
                    egui::ComboBox::from_id_salt("transport")
                        .selected_text(transport_label(&self.settings.transport))
                        .show_ui(ui, |ui| {
                            for transport in config::TRANSPORTS {
                                if ui
                                    .selectable_value(
                                        &mut self.settings.transport,
                                        transport.to_string(),
                                        transport_label(transport),
                                    )
                                    .clicked()
                                {
                                    changed = true;
                                }
                            }
                        });
                });
            });
            if before != self.settings.transport {
                changed = true;
            }

            if self.settings.transport == "wifi" || self.settings.transport == "wifi-direct" {
                ui.add_space(6.0);
                ui.label("IP телефона (WO Mic, TCP 7800)");
                if ui
                    .add(
                        egui::TextEdit::singleline(&mut self.settings.host)
                            .hint_text("192.168.1.50")
                            .desired_width(ui.available_width()),
                    )
                    .changed()
                {
                    changed = true;
                }
                ui.label(small(
                    "Телефон и компьютер должны быть в одной Wi-Fi сети. Кабель не нужен.",
                ));
            } else if self.settings.transport == "bluetooth" {
                ui.add_space(6.0);
                ui.label("Bluetooth MAC-адрес телефона");
                if ui
                    .add(
                        egui::TextEdit::singleline(&mut self.settings.bt_address)
                            .hint_text("AA:BB:CC:DD:EE:FF")
                            .desired_width(ui.available_width()),
                    )
                    .changed()
                {
                    changed = true;
                }
            } else {
                ui.label(small("USB: телефон подключён кабелем, нужен adb."));
            }

            if cfg!(windows) {
                ui.add_space(10.0);
                ui.label("Выход в систему");
                if ui
                    .add(
                        egui::TextEdit::singleline(&mut self.settings.device)
                            .desired_width(ui.available_width()),
                    )
                    .changed()
                {
                    changed = true;
                }
                ui.label(small(
                    "Windows: приложения должны записывать с CABLE Output (VB-CABLE).",
                ));
            }

            if changed {
                self.settings_dirty = true;
            }

            if self.settings_dirty {
                ui.add_space(10.0);
                ui.horizontal(|ui| {
                    let save = egui::Button::new(RichText::new("Сохранить и перезапустить службу")
                        .color(BG))
                        .fill(ACCENT);
                    if ui.add(save).clicked() {
                        match self.settings.save() {
                            Ok(()) => {
                                self.settings_dirty = false;
                                self.notify("Настройки подключения сохранены".into());
                                self.restart_service();
                            }
                            Err(error) => {
                                self.notify(format!("Не удалось сохранить настройки: {error}"))
                            }
                        }
                    }
                    ui.label(small("есть несохранённые изменения"));
                });
            }
        });

        ui.add_space(12.0);
        card(ui, "Система", |ui| {
            if cfg!(unix) {
                let mut default_mic = self.default_mic;
                if toggle_row(
                    ui,
                    "Сделать RustMIC микрофоном по умолчанию",
                    &mut default_mic,
                    true,
                    "Системные приложения будут сразу выбирать rustmic_mic",
                ) {
                    self.default_mic = default_mic;
                    thread::spawn(move || {
                        let _ = audio::set_default(default_mic);
                    });
                }
            }

            ui.add_space(10.0);
            ui.horizontal(|ui| {
                if ui.button("Перезапустить службу").clicked() {
                    self.restart_service();
                }
                if ui.button("Журнал").clicked() {
                    platform::open_log();
                }
                if ui.button("Папка настроек").clicked() {
                    platform::open_settings_folder();
                }
            });
            ui.horizontal(|ui| {
                if ui.button("Диагностика").clicked() {
                    self.request_diagnostics();
                }
                if self.diagnostics.is_some() && ui.button("Скопировать").clicked() {
                    if let Some(text) = &self.diagnostics {
                        ui.ctx().copy_text(text.clone());
                    }
                }
                if self.diagnostics.is_some() && ui.button("Закрыть").clicked() {
                    self.diagnostics = None;
                }
            });
            if let Some(text) = self.diagnostics.clone() {
                egui::ScrollArea::vertical()
                    .max_height(220.0)
                    .show(ui, |ui| {
                        ui.add(
                            egui::TextEdit::multiline(&mut text.as_str())
                                .desired_width(f32::INFINITY)
                                .interactive(false),
                        );
                    });
            }

            ui.add_space(10.0);
            egui::CollapsingHeader::new("О программе")
                .default_open(false)
                .show(ui, |ui| {
                    egui::Grid::new("about-info")
                        .num_columns(2)
                        .spacing([20.0, 8.0])
                        .show(ui, |ui| {
                            kv(ui, "Версия", env!("CARGO_PKG_VERSION"));
                            kv(ui, "Протокол", "v1");
                            kv(ui, "Сокет", &ipc::socket_display());
                            kv(
                                ui,
                                "PID демона",
                                &if self.state.daemon_pid == 0 {
                                    "—".into()
                                } else {
                                    self.state.daemon_pid.to_string()
                                },
                            );
                        });
                });
        });

        if let Some(notice) = &self.notice {
            ui.add_space(10.0);
            ui.colored_label(AMBER, notice.0.clone());
        }
    }

    fn request_diagnostics(&mut self) {
        let settings = self.settings.clone();
        let tx = self.event_tx.clone();
        thread::spawn(move || {
            let text = platform::diagnostics(&settings);
            let _ = tx.send(UiEvent::Diagnostics(text));
        });
    }

    // ------------------------------------------------------------ meter animation

    fn animate_meter(&mut self, ctx: &egui::Context) {
        let now = Instant::now();
        let dt = (now - self.last_frame).as_secs_f32().clamp(0.0, 0.1);
        self.last_frame = now;
        let db = amplitude_db(self.state.levels.rms);
        if db > self.smooth_db {
            self.smooth_db = db;
        } else {
            self.smooth_db = (self.smooth_db - dt * 38.0).max(db).max(-60.0);
        }
        if db > self.peak_db {
            self.peak_db = db;
            self.peak_at = now;
        } else if now.duration_since(self.peak_at) > Duration::from_millis(900) {
            self.peak_db = (self.peak_db - dt * 22.0).max(self.smooth_db).max(-60.0);
        }
        ctx.request_repaint_after(Duration::from_millis(33));
    }
}

impl eframe::App for App {
    fn ui(&mut self, ui: &mut egui::Ui, _frame: &mut eframe::Frame) {
        let ctx = ui.ctx().clone();
        self.pump_events(&ctx);
        self.animate_meter(&ctx);

        if !self.quitting && self.tray.is_some() && ui.input(|i| i.viewport().close_requested()) {
            ctx.send_viewport_cmd(egui::ViewportCommand::CancelClose);
            ctx.send_viewport_cmd(egui::ViewportCommand::Visible(false));
            self.hidden = true;
        }

        self.top_bar(ui);
        self.tabs_bar(ui);

        egui::CentralPanel::default()
            .frame(
                egui::Frame::new()
                    .fill(BG)
                    .inner_margin(Margin::symmetric(20, 16)),
            )
            .show(ui, |ui| {
                egui::ScrollArea::vertical()
                    .auto_shrink([false, false])
                    .show(ui, |ui| {
                        ui.set_width(ui.available_width());
                        match self.tab {
                            Tab::Mic => self.tab_microphone(ui),
                            Tab::Pad => self.tab_soundpad(ui),
                            Tab::Settings => self.tab_settings(ui),
                        }
                        ui.add_space(20.0);
                    });
            });
    }
}

// ---------------------------------------------------------------- workers

fn spawn_state_reader(tx: Sender<UiEvent>) {
    thread::Builder::new()
        .name("RustMIC-gui-state".into())
        .spawn(move || loop {
            match ipc::Client::connect() {
                Ok(mut client) => loop {
                    match client.recv() {
                        Ok(state) => {
                            if tx.send(UiEvent::State(Box::new(state))).is_err() {
                                return;
                            }
                        }
                        Err(error) => {
                            let _ = tx.send(UiEvent::Offline(error.to_string()));
                            break;
                        }
                    }
                },
                Err(error) => {
                    let _ = tx.send(UiEvent::Offline(error.to_string()));
                }
            }
            thread::sleep(Duration::from_secs(1));
        })
        .ok();
}

fn spawn_command_worker(rx: Receiver<crate::core::Command>, tx: Sender<UiEvent>) {
    thread::Builder::new()
        .name("RustMIC-gui-commands".into())
        .spawn(move || {
            let mut client: Option<ipc::Client> = None;
            while let Ok(command) = rx.recv() {
                let result = (|| {
                    if client.is_none() {
                        client = Some(ipc::Client::connect()?);
                    }
                    client
                        .as_mut()
                        .expect("client")
                        .request(command)
                        .map(|_| ())
                })();
                if let Err(error) = result {
                    client = None;
                    let _ = tx.send(UiEvent::Notice(format!("Команда не выполнена: {error}")));
                }
            }
        })
        .ok();
}

fn spawn_system_poller(tx: Sender<UiEvent>) {
    thread::Builder::new()
        .name("RustMIC-gui-system".into())
        .spawn(move || loop {
            let _ = tx.send(UiEvent::DefaultMic(audio::default_is_rustmic()));
            thread::sleep(Duration::from_secs(2));
        })
        .ok();
}

// ---------------------------------------------------------------- widgets

fn install_theme(ctx: &egui::Context) {
    let mut fonts = FontDefinitions::default();
    fonts.font_data.insert(
        "Inter".to_owned(),
        std::sync::Arc::new(FontData::from_static(include_bytes!(
            "../../assets/fonts/Inter-Regular.ttf"
        ))),
    );
    fonts.font_data.insert(
        "InterSemiBold".to_owned(),
        std::sync::Arc::new(FontData::from_static(include_bytes!(
            "../../assets/fonts/Inter-SemiBold.ttf"
        ))),
    );
    fonts
        .families
        .entry(FontFamily::Proportional)
        .or_default()
        .insert(0, "Inter".to_owned());
    fonts
        .families
        .entry(FontFamily::Monospace)
        .or_default()
        .push("Inter".to_owned());
    fonts.families.insert(
        FontFamily::Name("InterSemiBold".into()),
        vec!["InterSemiBold".to_owned(), "Inter".to_owned()],
    );
    ctx.set_fonts(fonts);

    ctx.set_theme(ThemePreference::Dark);
    let mut style = (*ctx.style_of(Theme::Dark)).clone();
    style.spacing.item_spacing = Vec2::new(8.0, 8.0);
    style.spacing.button_padding = Vec2::new(12.0, 7.0);
    style.spacing.slider_width = 150.0;
    style.text_styles = [
        (
            TextStyle::Heading,
            FontId::new(24.0, FontFamily::Name("InterSemiBold".into())),
        ),
        (TextStyle::Body, FontId::new(14.0, FontFamily::Proportional)),
        (
            TextStyle::Button,
            FontId::new(14.0, FontFamily::Proportional),
        ),
        (TextStyle::Small, FontId::new(11.0, FontFamily::Proportional)),
        (
            TextStyle::Monospace,
            FontId::new(12.0, FontFamily::Monospace),
        ),
    ]
    .into();

    let mut visuals = egui::Visuals::dark();
    visuals.panel_fill = BG;
    visuals.window_fill = PANEL;
    visuals.extreme_bg_color = Color32::from_rgb(8, 11, 15);
    visuals.faint_bg_color = CARD;
    visuals.selection.bg_fill = ACCENT_DIM;
    visuals.selection.stroke = Stroke::new(1.0, ACCENT);
    visuals.hyperlink_color = ACCENT;
    visuals.slider_trailing_fill = true;
    visuals.widgets.noninteractive.bg_fill = CARD;
    visuals.widgets.noninteractive.bg_stroke = Stroke::new(1.0, BORDER);
    visuals.widgets.noninteractive.fg_stroke = Stroke::new(1.0, TEXT);
    // egui uses `inactive.bg_fill` for slider rails; keep it a little lighter
    // than the cards so the rail is visible against the dark theme.
    visuals.widgets.inactive.bg_fill = CARD_HOVER;
    visuals.widgets.inactive.bg_stroke = Stroke::new(1.0, BORDER);
    visuals.widgets.inactive.fg_stroke = Stroke::new(1.0, TEXT);
    visuals.widgets.hovered.bg_fill = CARD_HOVER;
    visuals.widgets.hovered.bg_stroke = Stroke::new(1.0, ACCENT_DIM);
    visuals.widgets.hovered.fg_stroke = Stroke::new(1.0, TEXT);
    visuals.widgets.active.bg_fill = ACCENT_DIM;
    visuals.widgets.active.bg_stroke = Stroke::new(1.0, ACCENT);
    visuals.widgets.active.fg_stroke = Stroke::new(1.0, TEXT);
    for widget in [
        &mut visuals.widgets.inactive,
        &mut visuals.widgets.hovered,
        &mut visuals.widgets.active,
        &mut visuals.widgets.noninteractive,
        &mut visuals.widgets.open,
    ] {
        widget.corner_radius = 9.0.into();
    }
    style.visuals = visuals;
    ctx.set_style_of(Theme::Dark, style);
}

fn status_pill(ui: &mut egui::Ui, text: &str, color: Color32) {
    egui::Frame::new()
        .fill(color.gamma_multiply(0.16))
        .stroke(Stroke::new(1.0, color.gamma_multiply(0.55)))
        .corner_radius(999.0)
        .inner_margin(Margin::symmetric(10, 4))
        .show(ui, |ui| {
            ui.horizontal(|ui| {
                let (rect, _) = ui.allocate_exact_size(Vec2::splat(8.0), Sense::hover());
                ui.painter().circle_filled(rect.center(), 4.0, color);
                ui.label(RichText::new(text).size(12.0).color(color));
            });
        });
}

fn card(ui: &mut egui::Ui, title: &str, add: impl FnOnce(&mut egui::Ui)) {
    egui::Frame::new()
        .fill(CARD)
        .stroke(Stroke::new(1.0, BORDER))
        .corner_radius(14.0)
        .inner_margin(Margin::same(16))
        .show(ui, |ui| {
            ui.set_width(ui.available_width());
            ui.label(heading(title));
            ui.add_space(8.0);
            add(ui);
        });
}

fn kv(ui: &mut egui::Ui, key: &str, value: &str) {
    ui.label(small(key));
    ui.label(RichText::new(value).size(13.0).color(TEXT));
    ui.end_row();
}

fn transport_label(transport: &str) -> &'static str {
    match transport {
        "wifi" => "Wi-Fi",
        "wifi-direct" => "Wi-Fi Direct",
        "bluetooth" => "Bluetooth",
        _ => "USB",
    }
}

fn clock(seconds: f64) -> String {
    let total = seconds.max(0.0) as u64;
    format!("{:02}:{:02}", total / 60, total % 60)
}

fn amplitude_db(rms: f32) -> f32 {
    if rms <= 0.000_001 {
        -60.0
    } else {
        (20.0 * rms.log10()).clamp(-60.0, 0.0)
    }
}

fn db_fraction(db: f32) -> f32 {
    ((db + 60.0) / 60.0).clamp(0.0, 1.0)
}

fn db_color(db: f32) -> Color32 {
    if db <= -18.0 {
        lerp_color(ACCENT, AMBER, ((db + 60.0) / 42.0).clamp(0.0, 1.0))
    } else {
        lerp_color(AMBER, RED, ((db + 18.0) / 18.0).clamp(0.0, 1.0))
    }
}

fn lerp_color(a: Color32, b: Color32, t: f32) -> Color32 {
    let t = t.clamp(0.0, 1.0);
    Color32::from_rgb(
        (a.r() as f32 + (b.r() as f32 - a.r() as f32) * t) as u8,
        (a.g() as f32 + (b.g() as f32 - a.g() as f32) * t) as u8,
        (a.b() as f32 + (b.b() as f32 - a.b() as f32) * t) as u8,
    )
}

// ---------------------------------------------------------------- widgets

struct SliderRow {
    changed: bool,
    committed: bool,
}

/// One line: label on the left, full-width slider in the middle, value on the
/// right. Every numeric setting in the app uses this layout.
fn slider_row(
    ui: &mut egui::Ui,
    label: &str,
    value: &mut f32,
    range: std::ops::RangeInclusive<f32>,
    suffix: &str,
    decimals: usize,
) -> SliderRow {
    let mut changed = false;
    let mut committed = false;

    ui.horizontal(|ui| {
        let full = ui.available_width();
        let label_width = (full * 0.34).clamp(96.0, 168.0);
        ui.allocate_ui_with_layout(
            Vec2::new(label_width, 20.0),
            Layout::left_to_right(Align::Center),
            |ui| {
                ui.add(egui::Label::new(RichText::new(label).size(13.0).color(TEXT)).truncate());
            },
        );

        let value_width = 58.0;
        let slider_width = (ui.available_width() - value_width - 8.0).max(60.0);
        let response = ui.add_sized(
            Vec2::new(slider_width, 20.0),
            egui::Slider::new(value, range)
                .show_value(false)
                .trailing_fill(false),
        );
        changed = response.changed();
        committed = response.drag_stopped()
            || (response.changed() && !ui.input(|i| i.pointer.any_down()));

        ui.with_layout(Layout::right_to_left(Align::Center), |ui| {
            ui.add(
                egui::Label::new(
                    RichText::new(format!("{value:.*}{suffix}", decimals))
                        .size(13.0)
                        .color(TEXT),
                )
                .truncate(),
            );
        });
    });

    SliderRow { changed, committed }
}

/// Theme-styled switch used instead of checkboxes.
fn switch(ui: &mut egui::Ui, on: &mut bool) -> bool {
    let (rect, response) =
        ui.allocate_exact_size(Vec2::new(38.0, 20.0), Sense::click());
    let mut response = response.on_hover_cursor(egui::CursorIcon::PointingHand);
    if response.clicked() {
        *on = !*on;
        response.mark_changed();
    }

    let t = ui.ctx().animate_bool_responsive(response.id, *on);
    let radius = rect.height() / 2.0;
    let bg = lerp_color(Color32::from_rgb(45, 54, 66), ACCENT, t);
    ui.painter().rect_filled(rect, radius, bg);
    ui.painter().rect_stroke(
        rect,
        radius,
        Stroke::new(1.0, lerp_color(BORDER, ACCENT, t)),
        egui::StrokeKind::Inside,
    );
    let pad = 2.5;
    let knob_x = egui::lerp(rect.left() + radius..=rect.right() - radius, t);
    ui.painter().circle_filled(
        egui::pos2(knob_x, rect.center().y),
        radius - pad,
        if *on { Color32::from_rgb(8, 30, 25) } else { MUTED },
    );

    response.changed()
}

/// Label on the left, switch on the right.
fn toggle_row(ui: &mut egui::Ui, label: &str, value: &mut bool, enabled: bool, hint: &str) -> bool {
    let mut changed = false;
    let response = ui
        .add_enabled_ui(enabled, |ui| {
            ui.horizontal(|ui| {
                ui.add(
                    egui::Label::new(
                        RichText::new(label)
                            .size(13.0)
                            .color(if enabled { TEXT } else { MUTED }),
                    )
                    .truncate(),
                );
                ui.with_layout(Layout::right_to_left(Align::Center), |ui| {
                    changed = switch(ui, value);
                });
            });
        })
        .response;
    response.on_hover_text(hint);
    changed
}

/// Colored dot + label on the left, value on the right.
fn dot_row(ui: &mut egui::Ui, label: &str, value: &str, color: Color32) {
    ui.horizontal(|ui| {
        let (rect, _) = ui.allocate_exact_size(Vec2::splat(10.0), Sense::hover());
        ui.painter().circle_filled(rect.center(), 3.5, color);
        ui.label(RichText::new(label).size(13.0).color(MUTED));
        ui.with_layout(Layout::right_to_left(Align::Center), |ui| {
            ui.label(RichText::new(value).size(13.0).color(color));
        });
    });
}

/// "Infinix X6836 · USB" style header for the microphone tab.
fn connection_summary(
    connection: &crate::core::ConnectionState,
) -> (String, &'static str, Color32) {
    let transport = transport_label(&connection.transport);
    let title = match phone_model(&connection.phone) {
        Some(model) => format!("{model} · {transport}"),
        None => format!("Телефон · {transport}"),
    };
    let (text, color) = if connection.phone_connected {
        ("подключён", GREEN)
    } else if connection.client_running {
        ("нет связи", AMBER)
    } else {
        ("не подключён", RED)
    };
    (title, text, color)
}

fn phone_model(phone: &str) -> Option<String> {
    let open = phone.rfind('(')?;
    let close = open + phone[open..].find(')')?;
    let model = phone[open + 1..close].trim();
    if model.is_empty() || model.eq_ignore_ascii_case("usb") {
        None
    } else {
        Some(model.to_string())
    }
}

/// Tile card used inside the two-column soundpad grid.
fn tile_card(ui: &mut egui::Ui, tile: &Tile) -> (bool, bool) {
    let mut play = false;
    let mut remove = false;

    egui::Frame::new()
        .fill(CARD_HOVER)
        .stroke(Stroke::new(1.0, BORDER))
        .corner_radius(10.0)
        .inner_margin(Margin::same(8))
        .show(ui, |ui| {
            ui.set_width(ui.available_width());

            ui.horizontal(|ui| {
                if let Some(cover) = tile.cover.clone() {
                    ui.add(
                        egui::Image::from_uri(cover)
                            .fit_to_exact_size(Vec2::splat(40.0))
                            .corner_radius(6.0),
                    );
                } else {
                    let (rect, _) = ui.allocate_exact_size(Vec2::splat(40.0), Sense::hover());
                    ui.painter().rect_filled(rect, 6.0, CARD);
                    ui.painter().text(
                        rect.center(),
                        Align2::CENTER_CENTER,
                        "♪",
                        FontId::proportional(18.0),
                        ACCENT,
                    );
                }
                ui.vertical(|ui| {
                    ui.add(
                        egui::Label::new(
                            RichText::new(if tile.title.is_empty() {
                                tile.query.clone()
                            } else {
                                tile.title.clone()
                            })
                            .size(13.0)
                            .color(TEXT),
                        )
                        .truncate(),
                    );
                    ui.label(small(if tile.duration > 0.0 {
                        clock(tile.duration)
                    } else {
                        "трек".into()
                    }));
                });
            });

            ui.add_space(6.0);
            ui.horizontal(|ui| {
                if ui
                    .add(
                        egui::Button::new(RichText::new("▶ Играть").size(12.0))
                            .min_size(Vec2::new(0.0, 24.0)),
                    )
                    .clicked()
                {
                    play = true;
                }
                ui.with_layout(Layout::right_to_left(Align::Center), |ui| {
                    if ui
                        .add(
                            egui::Button::new(RichText::new("×").size(15.0))
                                .min_size(Vec2::new(26.0, 24.0)),
                        )
                        .on_hover_text("Удалить плитку")
                        .clicked()
                    {
                        remove = true;
                    }
                });
            });
        });

    (play, remove)
}

// ---------------------------------------------------------------- meter

/// Input level. In VAD mode the scale is voice probability in percent, in
/// level mode it is dBFS. The amber marker is the gate threshold and can be
/// dragged right on the scale.
fn level_meter(
    ui: &mut egui::Ui,
    levels: &Levels,
    audio: &crate::core::AudioState,
    smooth_db: f32,
    peak_db: f32,
) -> Option<GateChange> {
    let voice = audio.gate_mode == GateMode::Voice;
    let width = ui.available_width();
    let height = if voice { 140.0 } else { 132.0 };
    let (rect, response) = ui.allocate_exact_size(Vec2::new(width, height), Sense::click_and_drag());
    let painter = ui.painter_at(rect);

    let mut change = None;

    if voice {
        let track = egui::Rect::from_min_size(rect.min, Vec2::new(rect.width(), 48.0));
        painter.rect_filled(track, 10.0, Color32::from_rgb(8, 12, 16));
        painter.rect_stroke(
            track,
            10.0,
            Stroke::new(1.0, BORDER),
            egui::StrokeKind::Inside,
        );

        let inner = track.shrink2(Vec2::new(8.0, 9.0));
        let vad = levels.vad.clamp(0.0, 1.0);
        if vad > 0.0 {
            let steps = 40;
            for index in 0..steps {
                let t0 = index as f32 / steps as f32;
                let t1 = (index + 1) as f32 / steps as f32;
                if t0 > vad {
                    break;
                }
                let segment = egui::Rect::from_min_max(
                    egui::pos2(inner.left() + inner.width() * t0, inner.top()),
                    egui::pos2(inner.left() + inner.width() * t1.min(vad), inner.bottom()),
                );
                painter.rect_filled(
                    segment,
                    0.0,
                    lerp_color(ACCENT_DIM, ACCENT, t0),
                );
            }
        }

        for percent in [0.0_f32, 25.0, 50.0, 75.0, 100.0] {
            let x = inner.left() + inner.width() * percent / 100.0;
            painter.line_segment(
                [egui::pos2(x, inner.top()), egui::pos2(x, inner.bottom())],
                Stroke::new(1.0, Color32::from_black_alpha(70)),
            );
            let (anchor, x) = if percent <= 0.5 {
                (Align2::LEFT_TOP, inner.left())
            } else if percent >= 99.5 {
                (Align2::RIGHT_TOP, inner.right())
            } else {
                (Align2::CENTER_TOP, x)
            };
            painter.text(
                egui::pos2(x, track.bottom() + 3.0),
                anchor,
                format!("{percent:.0}%"),
                FontId::proportional(9.0),
                MUTED,
            );
        }

        let threshold = audio.gate_voice as f32 / 100.0;
        let x = inner.left() + inner.width() * threshold;
        painter.line_segment(
            [
                egui::pos2(x, track.top() - 3.0),
                egui::pos2(x, track.bottom() + 3.0),
            ],
            Stroke::new(2.0, AMBER),
        );
        painter.circle_filled(egui::pos2(x, track.bottom()), 5.0, AMBER);

        painter.text(
            egui::pos2(rect.left(), track.bottom() + 18.0),
            Align2::LEFT_TOP,
            format!("вероятность голоса {:.0}% · порог {:.0}%", vad * 100.0, threshold * 100.0),
            FontId::proportional(11.0),
            if levels.gate_open { GREEN } else { MUTED },
        );
        painter.text(
            egui::pos2(rect.left(), track.bottom() + 36.0),
            Align2::LEFT_TOP,
            "Жёлтая метка — порог гейта. Тяните её по шкале:",
            FontId::proportional(10.0),
            MUTED,
        );
        painter.text(
            egui::pos2(rect.left(), track.bottom() + 50.0),
            Align2::LEFT_TOP,
            "выше порога микрофон открыт, ниже — закрыт.",
            FontId::proportional(10.0),
            MUTED,
        );

        if response.dragged() || response.clicked() {
            if let Some(pos) = response.interact_pointer_pos() {
                let value =
                    ((pos.x - inner.left()) / inner.width()).clamp(0.0, 1.0) * 100.0;
                change = Some(GateChange {
                    value: value.round(),
                    commit: response.drag_stopped() || response.clicked(),
                });
            }
        }
    } else {
        let meter = egui::Rect::from_min_size(rect.min, Vec2::new(rect.width(), 64.0));
        painter.rect_filled(meter, 10.0, Color32::from_rgb(8, 12, 16));
        painter.rect_stroke(
            meter,
            10.0,
            Stroke::new(1.0, BORDER),
            egui::StrokeKind::Inside,
        );

        let inner = meter.shrink2(Vec2::new(8.0, 9.0));
        let fraction = db_fraction(smooth_db);
        if fraction > 0.0 {
            let steps = 48;
            for index in 0..steps {
                let t0 = index as f32 / steps as f32;
                let t1 = (index + 1) as f32 / steps as f32;
                if t0 > fraction {
                    break;
                }
                let segment = egui::Rect::from_min_max(
                    egui::pos2(inner.left() + inner.width() * t0, inner.top()),
                    egui::pos2(
                        inner.left() + inner.width() * t1.min(fraction),
                        inner.bottom(),
                    ),
                );
                painter.rect_filled(segment, 0.0, db_color(-60.0 + t0 * 60.0));
            }
        }

        if peak_db > -59.5 {
            let x = inner.left() + inner.width() * db_fraction(peak_db);
            painter.line_segment(
                [egui::pos2(x, inner.top()), egui::pos2(x, inner.bottom())],
                Stroke::new(1.5, Color32::WHITE.gamma_multiply(0.85)),
            );
        }

        for db in [-48.0_f32, -36.0, -24.0, -12.0, 0.0] {
            let x = inner.left() + inner.width() * db_fraction(db);
            painter.line_segment(
                [egui::pos2(x, inner.top()), egui::pos2(x, inner.bottom())],
                Stroke::new(1.0, Color32::from_black_alpha(60)),
            );
            painter.text(
                egui::pos2(x, meter.bottom() + 3.0),
                Align2::CENTER_TOP,
                format!("{db:.0}"),
                FontId::proportional(9.0),
                MUTED,
            );
        }
        painter.text(
            egui::pos2(inner.right(), meter.bottom() + 3.0),
            Align2::RIGHT_TOP,
            "dB",
            FontId::proportional(9.0),
            MUTED,
        );

        let x = inner.left() + inner.width() * db_fraction(audio.gate_level_db);
        painter.line_segment(
            [
                egui::pos2(x, meter.top() - 3.0),
                egui::pos2(x, meter.bottom() + 3.0),
            ],
            Stroke::new(2.0, AMBER),
        );
        painter.circle_filled(egui::pos2(x, meter.bottom()), 5.0, AMBER);

        painter.text(
            egui::pos2(rect.left(), meter.bottom() + 18.0),
            Align2::LEFT_TOP,
            format!(
                "уровень {:.0} dB · порог {:.0} dB · гейт {}",
                smooth_db,
                audio.gate_level_db,
                if levels.gate_open { "открыт" } else { "закрыт" }
            ),
            FontId::proportional(11.0),
            if levels.gate_open { GREEN } else { MUTED },
        );
        painter.text(
            egui::pos2(rect.left(), meter.bottom() + 36.0),
            Align2::LEFT_TOP,
            "Жёлтая метка — порог гейта. Тяните её по шкале:",
            FontId::proportional(10.0),
            MUTED,
        );
        painter.text(
            egui::pos2(rect.left(), meter.bottom() + 50.0),
            Align2::LEFT_TOP,
            "сигнал громче порога открывает микрофон.",
            FontId::proportional(10.0),
            MUTED,
        );

        if response.dragged() || response.clicked() {
            if let Some(pos) = response.interact_pointer_pos() {
                let t = ((pos.x - inner.left()) / inner.width()).clamp(0.0, 1.0);
                change = Some(GateChange {
                    value: (-60.0 + t * 60.0).round(),
                    commit: response.drag_stopped() || response.clicked(),
                });
            }
        }
    }

    if response.hovered() {
        ui.ctx().set_cursor_icon(egui::CursorIcon::ResizeHorizontal);
    }

    change
}

// ---------------------------------------------------------------- entry

pub fn run() {
    let options = eframe::NativeOptions {
        viewport: egui::ViewportBuilder::default()
            .with_inner_size([560.0, 790.0])
            .with_min_inner_size([460.0, 620.0])
            .with_title("RustMIC")
            .with_app_id("RustMIC"),
        ..Default::default()
    };
    if let Err(error) = eframe::run_native(
        "RustMIC",
        options,
        Box::new(|cc| Ok(Box::new(App::new(cc)))),
    ) {
        eprintln!("RustMIC-gui: {error}");
    }
}

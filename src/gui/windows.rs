//! Windows GUI: an egui window instead of the GTK tray app.
//!
//! Windows has no StatusNotifier tray without extra shell plumbing, so this is
//! a plain window with the same controls: mic on/off, denoise, gate, volume,
//! diagnostics. It drives the same background process as the CLI.

use std::sync::mpsc::{channel, Receiver};
use std::thread;
use std::time::Duration;

use eframe::egui;

use womic_mic::config::Settings;
use womic_mic::{audio, level, platform};

#[derive(Clone, Default)]
struct Status {
    service: bool,
    phone: Option<String>,
    client: bool,
    backend: bool,
    capture: Option<String>,
    adb: String,
}

struct App {
    settings: Settings,
    status: Status,
    rx: Receiver<Status>,
    smooth: f32,
    diagnostics: Option<String>,
    notice: Option<String>,
    busy: bool,
}

impl App {
    fn new() -> Self {
        let settings = Settings::load();
        let (tx, rx) = channel();
        let adb = settings.adb.clone();
        let device = settings.device.clone();
        thread::spawn(move || loop {
            let phone = womic_mic::adb::usb_phone(&adb).map(|phone| {
                phone.model.unwrap_or_else(|| "телефон".to_string())
            });
            let capture = womic_mic::audio::windows::input_names()
                .into_iter()
                .find(|name| name.to_lowercase().contains("cable output"));
            let status = Status {
                service: platform::service_active(),
                phone,
                client: platform::rustmic_running(),
                backend: audio::windows::device_present(&device),
                capture,
                adb: adb.clone(),
            };
            if tx.send(status).is_err() {
                return;
            }
            thread::sleep(Duration::from_millis(1200));
        });
        Self {
            settings,
            status: Status::default(),
            rx,
            smooth: 0.0,
            diagnostics: None,
            notice: None,
            busy: false,
        }
    }

    fn save(&mut self) {
        match self.settings.save() {
            Ok(()) => self.notice = None,
            Err(error) => self.notice = Some(format!("Не удалось сохранить настройки: {error}")),
        }
    }

    fn ui_status(&mut self, ui: &mut egui::Ui) {
        egui::Grid::new("status")
            .num_columns(2)
            .spacing([18.0, 6.0])
            .show(ui, |ui| {
                ui.label("Телефон (USB)");
                ui.label(self.status.phone.clone().unwrap_or_else(|| "не найден".into()));
                ui.end_row();
                ui.label("Поток");
                ui.label(if self.status.client { "идёт" } else { "нет" });
                ui.end_row();
                ui.label("Служба");
                ui.label(if self.status.service {
                    "работает"
                } else {
                    "остановлена"
                });
                ui.end_row();
                ui.label("Выход в кабель");
                ui.label(if self.status.backend {
                    "доступен"
                } else {
                    "не найден"
                });
                ui.end_row();
                ui.label("Виртуальный микрофон");
                ui.label(
                    self.status
                        .capture
                        .clone()
                        .unwrap_or_else(|| "не найден (нужен виртуальный кабель)".into()),
                );
                ui.end_row();
                ui.label("Уровень");
                ui.add(
                    egui::ProgressBar::new((self.smooth * 3.0).min(1.0)).desired_width(220.0),
                );
                ui.end_row();
            });
    }

    fn ui_actions(&mut self, ui: &mut egui::Ui) {
        ui.horizontal(|ui| {
            if ui.button("Перезапустить").clicked() {
                self.busy = true;
                thread::spawn(platform::restart);
            }
            if ui.button("Тест микрофона").clicked() {
                platform::test_mic();
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
                self.diagnostics = Some(platform::diagnostics(&self.settings));
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
    }
}

impl eframe::App for App {
    fn ui(&mut self, ui: &mut egui::Ui, _frame: &mut eframe::Frame) {
        ui.ctx().request_repaint_after(Duration::from_millis(120));

        while let Ok(status) = self.rx.try_recv() {
            if self.busy && status.service {
                self.busy = false;
            }
            self.status = status;
        }
        let rms = level::read();
        self.smooth = if rms > self.smooth {
            rms
        } else {
            self.smooth * 0.75
        };

        egui::Frame::central_panel(ui.style()).show(ui, |ui| {
            ui.heading("WO Mic");
            ui.label("Телефон Android как микрофон Windows");
            ui.separator();

            self.ui_status(ui);
            ui.separator();

            let mut on = self.status.service;
            if ui.checkbox(&mut on, "Микрофон включён").changed() {
                self.busy = true;
                if on {
                    thread::spawn(platform::start);
                } else {
                    thread::spawn(platform::stop);
                }
            }
            if ui
                .checkbox(&mut self.settings.denoise, "Шумодав (RNNoise)")
                .changed()
            {
                self.save();
            }
            if ui
                .add(
                    egui::Slider::new(&mut self.settings.gate, 0.0..=1.0)
                        .text("Гейт")
                        .fixed_decimals(2),
                )
                .changed()
            {
                self.save();
            }
            if ui
                .add(
                    egui::Slider::new(&mut self.settings.volume, 0..=150)
                        .text("Громкость, %"),
                )
                .changed()
            {
                self.save();
            }
            ui.horizontal(|ui| {
                ui.label("Выходное устройство");
                if ui
                    .text_edit_singleline(&mut self.settings.device)
                    .changed()
                {
                    self.save();
                }
            });
            ui.label(
                egui::RichText::new(
                    "В приложениях выбирайте запись с \"CABLE Output\" \
                     (виртуальный кабель). По умолчанию Windows его не переключает.",
                )
                .small()
                .weak(),
            );
            if let Some(notice) = &self.notice {
                ui.colored_label(egui::Color32::LIGHT_RED, notice);
            }
            ui.separator();
            self.ui_actions(ui);
            ui.separator();
            ui.label(
                egui::RichText::new(format!("adb: {}", self.status.adb))
                    .small()
                    .weak(),
            );
        });
    }
}

pub fn run() {
    let options = eframe::NativeOptions {
        viewport: egui::ViewportBuilder::default()
            .with_inner_size([520.0, 640.0])
            .with_min_inner_size([420.0, 520.0])
            .with_title("WO Mic"),
        ..Default::default()
    };
    if let Err(error) = eframe::run_native(
        "WO Mic",
        options,
        Box::new(|_cc| Ok(Box::new(App::new()))),
    ) {
        eprintln!("womic-mic-gui: {error}");
    }
}

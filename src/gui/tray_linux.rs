//! Linux StatusNotifier tray (ksni).

use std::sync::mpsc::Sender;

use ksni::blocking::TrayMethods;

#[derive(Clone, Copy)]
pub enum TrayCommand {
    Show,
    ToggleMic,
    ToggleDenoise,
    ToggleDefault,
    Restart,
    Test,
    Quit,
}

#[derive(Clone, Copy, Default)]
pub struct TrayStatus {
    pub service: bool,
    pub phone: bool,
    pub client: bool,
    pub default_mic: bool,
    pub denoise: bool,
}

struct Tray {
    tx: Sender<TrayCommand>,
    status: TrayStatus,
}

impl ksni::Tray for Tray {
    fn id(&self) -> String {
        "RustMIC".into()
    }

    fn title(&self) -> String {
        "RustMIC".into()
    }

    fn icon_name(&self) -> String {
        if self.status.service && self.status.client {
            "audio-input-microphone".into()
        } else {
            "microphone-sensitivity-muted".into()
        }
    }

    fn tool_tip(&self) -> ksni::ToolTip {
        let state = match (self.status.service, self.status.phone, self.status.client) {
            (false, _, _) => "служба остановлена",
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
        let _ = self.tx.send(TrayCommand::Show);
    }

    fn menu(&self) -> Vec<ksni::MenuItem<Self>> {
        use ksni::menu::*;
        vec![
            StandardItem {
                label: "Открыть RustMIC".into(),
                icon_name: "preferences-system".into(),
                activate: Box::new(|tray: &mut Self| {
                    let _ = tray.tx.send(TrayCommand::Show);
                }),
                ..Default::default()
            }
            .into(),
            MenuItem::Separator,
            CheckmarkItem {
                label: "Микрофон включён".into(),
                checked: self.status.service,
                activate: Box::new(|tray: &mut Self| {
                    let _ = tray.tx.send(TrayCommand::ToggleMic);
                }),
                ..Default::default()
            }
            .into(),
            CheckmarkItem {
                label: "Шумодав".into(),
                checked: self.status.denoise,
                activate: Box::new(|tray: &mut Self| {
                    let _ = tray.tx.send(TrayCommand::ToggleDenoise);
                }),
                ..Default::default()
            }
            .into(),
            CheckmarkItem {
                label: "Микрофон по умолчанию".into(),
                checked: self.status.default_mic,
                activate: Box::new(|tray: &mut Self| {
                    let _ = tray.tx.send(TrayCommand::ToggleDefault);
                }),
                ..Default::default()
            }
            .into(),
            MenuItem::Separator,
            StandardItem {
                label: "Перезапустить".into(),
                icon_name: "view-refresh".into(),
                activate: Box::new(|tray: &mut Self| {
                    let _ = tray.tx.send(TrayCommand::Restart);
                }),
                ..Default::default()
            }
            .into(),
            StandardItem {
                label: "Тест микрофона".into(),
                icon_name: "media-playback-start".into(),
                activate: Box::new(|tray: &mut Self| {
                    let _ = tray.tx.send(TrayCommand::Test);
                }),
                ..Default::default()
            }
            .into(),
            MenuItem::Separator,
            StandardItem {
                label: "Выход".into(),
                icon_name: "application-exit".into(),
                activate: Box::new(|tray: &mut Self| {
                    let _ = tray.tx.send(TrayCommand::Quit);
                }),
                ..Default::default()
            }
            .into(),
        ]
    }
}

pub struct TrayHandle {
    handle: ksni::blocking::Handle<Tray>,
}

impl TrayHandle {
    pub fn update(&self, status: TrayStatus) {
        self.handle.update(move |tray: &mut Tray| {
            tray.status = status;
        });
    }

    pub fn shutdown(&self) {
        let _ = self.handle.shutdown();
    }
}

pub fn spawn(tx: Sender<TrayCommand>) -> Result<TrayHandle, String> {
    let tray = Tray {
        tx,
        status: TrayStatus::default(),
    };
    tray.spawn()
        .map(|handle| TrayHandle { handle })
        .map_err(|error| error.to_string())
}

//! Windows tray backend (tray-icon).
//!
//! The window continues to live in the taskbar; the tray is an extra shortcut
//! for quick actions and for bringing the window back after it was minimised.

use std::sync::mpsc::Sender;

use tray_icon::menu::{CheckMenuItem, Menu, MenuEvent, MenuItem, PredefinedMenuItem};
use tray_icon::{Icon, MouseButton, TrayIcon, TrayIconBuilder, TrayIconEvent};

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

pub struct TrayHandle {
    icon: TrayIcon,
    mic_item: CheckMenuItem,
    denoise_item: CheckMenuItem,
    default_item: CheckMenuItem,
}

impl TrayHandle {
    pub fn update(&self, status: TrayStatus) {
        self.mic_item.set_checked(status.service);
        self.denoise_item.set_checked(status.denoise);
        self.default_item.set_checked(status.default_mic);
        let state = match (status.service, status.phone, status.client) {
            (false, _, _) => "служба остановлена",
            (true, false, _) => "нет телефона",
            (true, true, false) => "телефон есть, потока нет",
            (true, true, true) => "микрофон работает",
        };
        let _ = self
            .icon
            .set_tooltip(Some(format!("RustMIC — {state}")));
        let _ = self.icon.set_icon(Some(icon(status.service && status.client)));
    }

    pub fn shutdown(&self) {
        let _ = self.icon.set_visible(false);
    }
}

pub fn spawn(tx: Sender<TrayCommand>) -> Result<TrayHandle, String> {
    let menu = Menu::new();

    let open = MenuItem::with_id("open", "Открыть RustMIC", true, None);
    let mic_item = CheckMenuItem::with_id("mic", "Микрофон включён", true, false, None);
    let denoise_item = CheckMenuItem::with_id("denoise", "Шумодав", true, true, None);
    let default_item = CheckMenuItem::with_id("default", "Микрофон по умолчанию", true, false, None);
    let restart = MenuItem::with_id("restart", "Перезапустить", true, None);
    let test = MenuItem::with_id("test", "Тест микрофона", true, None);
    let quit = MenuItem::with_id("quit", "Выход", true, None);
    menu.append_items(&[
        &open,
        &PredefinedMenuItem::separator(),
        &mic_item,
        &denoise_item,
        &default_item,
        &PredefinedMenuItem::separator(),
        &restart,
        &test,
        &PredefinedMenuItem::separator(),
        &quit,
    ])
    .map_err(|e| format!("меню трея: {e}"))?;

    let menu_tx = tx.clone();
    MenuEvent::set_event_handler(Some(move |event: MenuEvent| {
        let command = match event.id.as_ref() {
            "open" => TrayCommand::Show,
            "mic" => TrayCommand::ToggleMic,
            "denoise" => TrayCommand::ToggleDenoise,
            "default" => TrayCommand::ToggleDefault,
            "restart" => TrayCommand::Restart,
            "test" => TrayCommand::Test,
            "quit" => TrayCommand::Quit,
            _ => return,
        };
        let _ = menu_tx.send(command);
    }));

    let click_tx = tx;
    TrayIconEvent::set_event_handler(Some(move |event: TrayIconEvent| {
        if let TrayIconEvent::Click {
            button: MouseButton::Left,
            ..
        } = event
        {
            let _ = click_tx.send(TrayCommand::Show);
        }
    }));

    let icon = TrayIconBuilder::new()
        .with_id("RustMIC")
        .with_menu(Box::new(menu))
        .with_tooltip("RustMIC")
        .with_icon(icon(false))
        .with_menu_on_left_click(false)
        .build()
        .map_err(|e| format!("иконка трея: {e}"))?;

    Ok(TrayHandle {
        icon,
        mic_item,
        denoise_item,
        default_item,
    })
}

/// A tiny vector microphone icon, so the binary has no extra image dependencies.
fn icon(active: bool) -> Icon {
    const SIZE: u32 = 32;
    let mut rgba = vec![0u8; (SIZE * SIZE * 4) as usize];
    let accent = if active {
        [0x3b, 0x82, 0xf6, 0xff]
    } else {
        [0x64, 0x74, 0x8b, 0xff]
    };

    let mut put = |x: i32, y: i32, color: [u8; 4]| {
        if (0..SIZE as i32).contains(&x) && (0..SIZE as i32).contains(&y) {
            let index = ((y as u32 * SIZE + x as u32) * 4) as usize;
            rgba[index..index + 4].copy_from_slice(&color);
        }
    };

    // Rounded background.
    let center = SIZE as f32 / 2.0;
    for y in 0..SIZE as i32 {
        for x in 0..SIZE as i32 {
            let dx = x as f32 + 0.5 - center;
            let dy = y as f32 + 0.5 - center;
            if dx * dx + dy * dy <= 15.0 * 15.0 {
                put(x, y, accent);
            }
        }
    }
    // Microphone capsule.
    for y in 9..21 {
        for x in 13..19 {
            let dx = x as f32 + 0.5 - 16.0;
            let dy = y as f32 + 0.5 - 15.0;
            if dx * dx + (dy / 1.35) * (dy / 1.35) <= 10.0 {
                put(x, y, [0xff, 0xff, 0xff, 0xff]);
            }
        }
    }
    // Stand.
    for y in 21..24 {
        for x in 14..18 {
            put(x, y, [0xff, 0xff, 0xff, 0xff]);
        }
    }
    for x in 11..21 {
        put(x, 24, [0xff, 0xff, 0xff, 0xff]);
    }
    for y in 20..23 {
        for x in 10..12 {
            put(x, y, [0xff, 0xff, 0xff, 0xff]);
        }
        for x in 20..22 {
            put(x, y, [0xff, 0xff, 0xff, 0xff]);
        }
    }

    Icon::from_rgba(rgba, SIZE, SIZE).expect("valid tray icon")
}

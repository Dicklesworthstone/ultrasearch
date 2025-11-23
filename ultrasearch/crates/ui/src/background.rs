use anyhow::Result;
use global_hotkey::{
    hotkey::{Code, HotKey, Modifiers},
    GlobalHotKeyEvent, GlobalHotKeyManager,
};
use muda::{CheckMenuItem, Menu, MenuItem, PredefinedMenuItem};
use once_cell::sync::OnceCell;
use std::sync::mpsc::{self, Receiver, Sender};
use std::thread;
use std::time::Duration;
use sysinfo::System;
use tray_icon::{Icon, TrayIconBuilder, TrayIconEvent};

#[derive(Debug, Clone, Copy, Default)]
pub struct TrayState {
    pub indexing: bool,
    pub offline: bool,
    pub update_available: bool,
    pub restart_ready: bool,
}

static TRAY_STATE_TX: OnceCell<Sender<TrayState>> = OnceCell::new();

/// Non-blocking setter; drops updates if background thread isn't ready yet.
pub fn set_tray_status(state: TrayState) {
    if let Some(tx) = TRAY_STATE_TX.get() {
        let _ = tx.send(state);
    }
}

pub enum UserAction {
    Show,
    ShowHelp,
    Quit,
    ToggleQuickSearch,
    HotkeyConflict { powertoys: bool },
    CheckUpdates,
    RestartUpdate,
    ToggleOptIn,
}

pub fn spawn() -> Result<Receiver<UserAction>> {
    let (tx, rx) = mpsc::channel();
    let (status_tx, status_rx) = mpsc::channel();
    let _ = TRAY_STATE_TX.set(status_tx);

    thread::spawn(move || {
        // --- Hotkeys ---
        let hotkey_manager = GlobalHotKeyManager::new().unwrap();
        // Alt + Space
        let hotkey = HotKey::new(Some(Modifiers::ALT), Code::Space);
        if let Err(e) = hotkey_manager.register(hotkey) {
            eprintln!("Failed to register hotkey: {}", e);
            let powertoys = detect_powertoys_run();
            let _ = tx.send(UserAction::HotkeyConflict { powertoys });
        }

        // Spawn Event Poller + Tray
        let tx_clone = tx.clone();
        let hotkey_id = hotkey.id();
        thread::spawn(move || {
            // --- Tray ---
            let menu = Menu::new();
            let show_item = MenuItem::new("Show UltraSearch", true, None);
            let status_indexing = CheckMenuItem::new("🔄 Indexing", true, false, None);
            let status_offline = CheckMenuItem::new("⚠ Offline", true, false, None);
            let status_update = CheckMenuItem::new("⬆ Update available", true, false, None);
            let help_item = MenuItem::new("❓ Help / Shortcuts", true, None);
            let check_updates_item = MenuItem::new("🔍 Check for Updates", true, None);
            let restart_item = MenuItem::new("↻ Restart to Update", true, None);
            let quit_item = MenuItem::new("✕ Quit", true, None);
            let _ = menu.append_items(&[
                &show_item,
                &status_indexing,
                &status_offline,
                &status_update,
                &help_item,
                &check_updates_item,
                &restart_item,
                &PredefinedMenuItem::separator(),
                &quit_item,
            ]);
            // Status indicators are informational only
            status_indexing.set_enabled(false);
            status_offline.set_enabled(false);
            status_update.set_enabled(false);
            restart_item.set_enabled(false);

            let width = 32u32;
            let height = 32u32;
            let mut rgba = Vec::with_capacity((width * height * 4) as usize);
            for _ in 0..(width * height) {
                rgba.extend_from_slice(&[0, 120, 255, 255]);
            }
            let icon = Icon::from_rgba(rgba, width, height).unwrap();

            let tray_icon = TrayIconBuilder::new()
                .with_menu(Box::new(menu))
                .with_tooltip("UltraSearch")
                .with_icon(icon)
                .build()
                .unwrap();

            let show_id = show_item.id().clone();
            let help_id = help_item.id().clone();
            let check_id = check_updates_item.id().clone();
            let restart_id = restart_item.id().clone();
            let quit_id = quit_item.id().clone();
            let menu_rx = muda::MenuEvent::receiver();
            let tray_rx = TrayIconEvent::receiver();
            let hotkey_rx = GlobalHotKeyEvent::receiver();

            loop {
                // Tray status updates
                if let Ok(state) = status_rx.try_recv() {
                    let tooltip = if state.offline {
                        "UltraSearch - Offline"
                    } else if state.update_available {
                        "UltraSearch - Update available"
                    } else if state.indexing {
                        "UltraSearch — Indexing"
                    } else {
                        "UltraSearch - Idle"
                    };
                    let _ = tray_icon.set_tooltip(Some(tooltip));
                    status_indexing.set_checked(state.indexing);
                    status_offline.set_checked(state.offline);
                    status_update.set_checked(state.update_available);
                    restart_item.set_enabled(state.restart_ready);
                }

                // Menu
                if let Ok(event) = menu_rx.try_recv() {
                    if event.id == show_id {
                        let _ = tx_clone.send(UserAction::Show);
                    } else if event.id == help_id {
                        let _ = tx_clone.send(UserAction::ShowHelp);
                    } else if event.id == check_id {
                        let _ = tx_clone.send(UserAction::CheckUpdates);
                    } else if event.id == restart_id {
                        let _ = tx_clone.send(UserAction::RestartUpdate);
                    } else if event.id == quit_id {
                        let _ = tx_clone.send(UserAction::Quit);
                    }
                }

                // Tray
                if let Ok(_event) = tray_rx.try_recv() {
                    let _ = tx_clone.send(UserAction::Show);
                }

                // Hotkey
                if let Ok(event) = hotkey_rx.try_recv() {
                    if event.id == hotkey_id && event.state == global_hotkey::HotKeyState::Released
                    {
                        let _ = tx_clone.send(UserAction::ToggleQuickSearch);
                    }
                }

                thread::sleep(Duration::from_millis(50));
            }
        });

        // 5. Run Platform Message Loop (Windows)
        #[cfg(target_os = "windows")]
        unsafe {
            use windows::Win32::UI::WindowsAndMessaging::{
                DispatchMessageW, GetMessageW, TranslateMessage, MSG,
            };
            let mut msg = MSG::default();
            while GetMessageW(&mut msg, None, 0, 0).as_bool() {
                let _ = TranslateMessage(&msg);
                DispatchMessageW(&msg);
            }
        }

        #[cfg(not(target_os = "windows"))]
        loop {
            thread::sleep(Duration::from_secs(1));
        }
    });

    Ok(rx)
}

fn detect_powertoys_run() -> bool {
    let sys = System::new_all();
    sys.processes().values().any(|p| {
        p.name()
            .to_string_lossy()
            .to_ascii_lowercase()
            .contains("powertoys")
    })
}

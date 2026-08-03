//! System tray: icon, tooltip, and right-click menu.
//!
//! Left-click shows the main window. The right-click menu offers the same
//! actions the dashboard exposes (pause/resume, voice on/off, quit) so the
//! user can drive Continuum without opening the window.

use tauri::menu::{Menu, MenuItem, PredefinedMenuItem};
use tauri::tray::{MouseButton, MouseButtonState, TrayIconBuilder, TrayIconEvent};
use tauri::{App, Emitter, Manager};

const TRAY_ID: &str = "continuum-tray";
const IDLE_TOOLTIP: &str = "Continuum · Idle";

/// Registers Continuum's single application tray icon and its controls.
pub fn init(app: &mut App) -> tauri::Result<()> {
    let open = MenuItem::with_id(app, "open", "Open dashboard", true, None::<&str>)?;
    let pause = MenuItem::with_id(app, "pause", "Pause", true, None::<&str>)?;
    let resume = MenuItem::with_id(app, "resume", "Resume", true, None::<&str>)?;
    let voice_on = MenuItem::with_id(app, "voice-on", "Voice on", true, None::<&str>)?;
    let voice_off = MenuItem::with_id(app, "voice-off", "Voice off", true, None::<&str>)?;
    let separator = PredefinedMenuItem::separator(app)?;
    let quit = MenuItem::with_id(app, "quit", "Quit Continuum", true, None::<&str>)?;

    let menu = Menu::with_items(
        app,
        &[
            &open, &separator, &pause, &resume, &voice_on, &voice_off, &separator, &quit,
        ],
    )?;

    let mut tray = TrayIconBuilder::with_id(TRAY_ID)
        .menu(&menu)
        .show_menu_on_left_click(false)
        .tooltip(IDLE_TOOLTIP);

    if let Some(icon) = app.default_window_icon() {
        tray = tray.icon(icon.clone());
    }

    let _tray = tray
        .on_menu_event(|app, event| match event.id.as_ref() {
            "open" => show_main_window(app),
            "pause" => {
                let _ = app.emit("continuum:control", serde_json::json!({"action": "pause"}));
            }
            "resume" => {
                let _ = app.emit("continuum:control", serde_json::json!({"action": "resume"}));
            }
            "voice-on" => {
                let _ = app.emit(
                    "continuum:control",
                    serde_json::json!({"action": "voice-on"}),
                );
            }
            "voice-off" => {
                let _ = app.emit(
                    "continuum:control",
                    serde_json::json!({"action": "voice-off"}),
                );
            }
            "quit" => {
                app.exit(0);
            }
            _ => {}
        })
        .on_tray_icon_event(|tray, event| {
            if let TrayIconEvent::Click {
                button: MouseButton::Left,
                button_state: MouseButtonState::Up,
                ..
            } = event
            {
                show_main_window(tray.app_handle());
            }
        })
        .build(app)?;

    Ok(())
}

fn show_main_window(app: &tauri::AppHandle) {
    if let Some(window) = app.get_webview_window("main") {
        let _ = window.show();
        let _ = window.unminimize();
        let _ = window.set_focus();
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn tray_identity_is_quiet_and_readable() {
        assert_eq!(TRAY_ID, "continuum-tray");
        assert_eq!(IDLE_TOOLTIP, "Continuum · Idle");
        assert!(!IDLE_TOOLTIP.contains('-'));
        assert!(!IDLE_TOOLTIP.contains('—'));
    }

    #[test]
    fn tauri_config_does_not_register_a_second_tray_icon() {
        let config: serde_json::Value = serde_json::from_str(include_str!("../tauri.conf.json"))
            .expect("tauri.conf.json should contain valid JSON");

        assert_eq!(
            config
                .get("productName")
                .and_then(serde_json::Value::as_str),
            Some("Continuum")
        );
        assert!(
            config.pointer("/app/trayIcon").is_none(),
            "the tray must be registered only by tray::init so its actions are attached"
        );
    }
}

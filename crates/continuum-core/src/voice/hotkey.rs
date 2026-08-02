//! # Global hotkey for toggle-listen (Windows only)
//!
//! Registers a system-wide hotkey via `RegisterHotKey` so the user can
//! trigger voice capture without Continuum's window having focus. The registered
//! hotkey runs on a dedicated OS thread with its own message loop — we use
//! thread-only `PostThreadMessage` / `GetMessageW` because we don't want to
//! create a hidden window.
//!
//! Pressing the configured chord sends a unit value on a
//! [`tokio::sync::mpsc`] channel, which the main runtime polls to open a
//! push-to-talk voice session.
//!
//! Format: `"Ctrl+Shift+K"`, `"Alt+F12"`, `"Win+Space"`, `"Ctrl+K"`. Case-
//! insensitive, whitespace around `+` is tolerated. Pass `""` to disable.

use std::sync::atomic::{AtomicU32, Ordering};

use anyhow::{anyhow, Context, Result};
use tokio::sync::mpsc;
use windows::Win32::Foundation::{HWND, LPARAM, WPARAM};
use windows::Win32::UI::Input::KeyboardAndMouse::{
    RegisterHotKey, UnregisterHotKey, HOT_KEY_MODIFIERS, MOD_ALT, MOD_CONTROL, MOD_NOREPEAT,
    MOD_SHIFT, MOD_WIN, VK_SPACE, VK_TAB,
};
use windows::Win32::UI::WindowsAndMessaging::{GetMessageW, MSG, WM_HOTKEY};

/// Handle to a running hotkey listener. Dropping this value stops the
/// listener thread and unregisters the hotkey.
pub struct HotkeyHandle {
    stop_id: u32,
    thread: Option<std::thread::JoinHandle<()>>,
    thread_id: u32,
    hotkey_spec: String,
}

impl HotkeyHandle {
    /// Human-readable description of the registered chord.
    pub fn spec(&self) -> &str {
        &self.hotkey_spec
    }
}

impl Drop for HotkeyHandle {
    fn drop(&mut self) {
        // Post a quit message so the thread exits cleanly. Use our stop_id
        // as the wParam so the thread knows to unregister + exit.
        unsafe {
            use windows::Win32::UI::WindowsAndMessaging::PostThreadMessageW;
            let _ = PostThreadMessageW(
                self.thread_id,
                CONTINUUM_HOTKEY_STOP_MSG,
                WPARAM(self.stop_id as usize),
                LPARAM(0),
            );
        }
        if let Some(handle) = self.thread.take() {
            let _ = handle.join();
        }
    }
}

/// Private message id we post to the listener thread to shut it down.
/// Anything in `[WM_USER, 0x7FFF]` is safe for application-private use.
const CONTINUUM_HOTKEY_STOP_MSG: u32 = 0x0400 + 42;

/// Each registered hotkey gets a unique integer id; 1 is reserved for the
/// listener's own "press" event so the shutdown message cannot collide.
const HOTKEY_ID: i32 = 1;

/// Monotonic counter used to give each listener a distinct stop-id,
/// reducing the chance of a late PostThreadMessage reaching a reused
/// thread id and confusing a fresh listener.
static NEXT_STOP_ID: AtomicU32 = AtomicU32::new(1);

/// Parse a chord spec like `"Ctrl+Shift+K"` into
/// `(HOT_KEY_MODIFIERS, virtual-key)`. Whitespace around `+` is tolerated,
/// keys are case-insensitive.
pub fn parse_chord(spec: &str) -> Result<(HOT_KEY_MODIFIERS, u32)> {
    let normalized = spec.trim();
    if normalized.is_empty() {
        anyhow::bail!("hotkey spec is empty");
    }

    let mut modifiers = HOT_KEY_MODIFIERS(0);
    let mut vk: Option<u32> = None;

    for part in normalized.split('+') {
        let part = part.trim().to_ascii_lowercase();
        match part.as_str() {
            "ctrl" | "control" => modifiers |= MOD_CONTROL,
            "shift" => modifiers |= MOD_SHIFT,
            "alt" => modifiers |= MOD_ALT,
            "win" | "super" | "meta" => modifiers |= MOD_WIN,
            key if !key.is_empty() => {
                if vk.is_some() {
                    anyhow::bail!("hotkey spec has more than one non-modifier key: {spec}");
                }
                vk = Some(
                    virtual_key_code(key)
                        .ok_or_else(|| anyhow!("unknown key '{key}' in hotkey spec '{spec}'"))?,
                );
            }
            _ => {}
        }
    }

    let vk = vk.ok_or_else(|| anyhow!("hotkey spec '{spec}' has no target key"))?;
    if modifiers.0 == 0 {
        anyhow::bail!(
            "hotkey spec '{spec}' must include at least one modifier (Ctrl/Shift/Alt/Win)"
        );
    }

    Ok((modifiers | MOD_NOREPEAT, vk))
}

fn virtual_key_code(key: &str) -> Option<u32> {
    // Single character keys: letters and digits map 1:1 to their ASCII
    // uppercase value in the Win32 VK_ space.
    if key.len() == 1 {
        let ch = key.chars().next().unwrap();
        if ch.is_ascii_alphabetic() {
            return Some(ch.to_ascii_uppercase() as u32);
        }
        if ch.is_ascii_digit() {
            return Some(ch as u32);
        }
    }
    // Function keys F1–F24.
    if let Some(rest) = key.strip_prefix('f') {
        if let Ok(n) = rest.parse::<u32>() {
            if (1..=24).contains(&n) {
                // VK_F1 = 0x70, F2 = 0x71, ...
                return Some(0x70 + (n - 1));
            }
        }
    }
    // Named keys the user is likely to pick.
    Some(match key {
        "space" => VK_SPACE.0 as u32,
        "tab" => VK_TAB.0 as u32,
        _ => return None,
    })
}

/// Start listening for the configured chord. When pressed, a `()` is sent
/// on the returned channel receiver. Dropping the returned [`HotkeyHandle`]
/// stops the listener.
///
/// Errors:
/// - spec parse errors
/// - `RegisterHotKey` rejection (another app owns the chord, bad combo)
pub fn spawn_hotkey_listener(spec: &str) -> Result<(HotkeyHandle, mpsc::UnboundedReceiver<()>)> {
    let (modifiers, vk) = parse_chord(spec).with_context(|| format!("parse_chord({spec})"))?;
    let (tx, rx) = mpsc::unbounded_channel();
    let stop_id = NEXT_STOP_ID.fetch_add(1, Ordering::Relaxed);
    let spec_owned = spec.to_string();

    let (started_tx, started_rx) = std::sync::mpsc::channel();

    let thread = std::thread::Builder::new()
        .name("continuum-hotkey".into())
        .spawn(move || {
            let tid = unsafe { windows::Win32::System::Threading::GetCurrentThreadId() };

            let register_ok = unsafe {
                RegisterHotKey(Some(HWND::default()), HOTKEY_ID, modifiers, vk).is_ok()
            };
            if !register_ok {
                let _ = started_tx.send(Err(anyhow!(
                    "RegisterHotKey failed for '{spec_owned}' — probably already owned by another app"
                )));
                return;
            }
            let _ = started_tx.send(Ok(tid));

            tracing::info!(
                layer = "voice",
                component = "hotkey",
                spec = %spec_owned,
                "Global hotkey registered"
            );

            loop {
                let mut msg = MSG::default();
                let result = unsafe { GetMessageW(&mut msg, None, 0, 0) };
                if result.0 <= 0 {
                    // Quit or error — exit the loop.
                    break;
                }
                if msg.message == WM_HOTKEY && msg.wParam.0 as i32 == HOTKEY_ID {
                    if tx.send(()).is_err() {
                        break;
                    }
                } else if msg.message == CONTINUUM_HOTKEY_STOP_MSG
                    && msg.wParam.0 as u32 == stop_id
                {
                    break;
                }
            }

            unsafe {
                let _ = UnregisterHotKey(Some(HWND::default()), HOTKEY_ID);
            }
            tracing::info!(
                layer = "voice",
                component = "hotkey",
                "Global hotkey listener exited"
            );
        })
        .context("failed to spawn hotkey thread")?;

    let thread_id = started_rx
        .recv()
        .context("hotkey thread crashed during init")??;

    Ok((
        HotkeyHandle {
            stop_id,
            thread: Some(thread),
            thread_id,
            hotkey_spec: spec.to_string(),
        },
        rx,
    ))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parse_ctrl_shift_k() {
        let (mods, vk) = parse_chord("Ctrl+Shift+K").unwrap();
        assert_eq!(vk, b'K' as u32);
        assert!(mods.0 & MOD_CONTROL.0 != 0);
        assert!(mods.0 & MOD_SHIFT.0 != 0);
        assert!(mods.0 & MOD_NOREPEAT.0 != 0);
    }

    #[test]
    fn parse_whitespace_and_case_tolerant() {
        let (mods, vk) = parse_chord("  alt + SHIFT + f12 ").unwrap();
        assert_eq!(vk, 0x7B); // VK_F12
        assert!(mods.0 & MOD_ALT.0 != 0);
        assert!(mods.0 & MOD_SHIFT.0 != 0);
    }

    #[test]
    fn parse_win_space() {
        let (mods, vk) = parse_chord("Win+Space").unwrap();
        assert_eq!(vk, VK_SPACE.0 as u32);
        assert!(mods.0 & MOD_WIN.0 != 0);
    }

    #[test]
    fn parse_rejects_empty_spec() {
        assert!(parse_chord("").is_err());
        assert!(parse_chord("   ").is_err());
    }

    #[test]
    fn parse_rejects_no_modifier() {
        assert!(parse_chord("K").is_err());
        assert!(parse_chord("F7").is_err());
    }

    #[test]
    fn parse_rejects_unknown_key() {
        assert!(parse_chord("Ctrl+Muffin").is_err());
    }

    #[test]
    fn parse_rejects_two_keys() {
        assert!(parse_chord("Ctrl+K+L").is_err());
    }

    #[test]
    fn parse_digits_and_letters() {
        let (_, vk) = parse_chord("Ctrl+1").unwrap();
        assert_eq!(vk, b'1' as u32);
        let (_, vk) = parse_chord("Alt+a").unwrap();
        assert_eq!(vk, b'A' as u32);
    }

    #[test]
    fn parse_function_keys_in_range() {
        let (_, vk) = parse_chord("Ctrl+F1").unwrap();
        assert_eq!(vk, 0x70);
        let (_, vk) = parse_chord("Ctrl+F12").unwrap();
        assert_eq!(vk, 0x7B);
    }

    #[test]
    fn parse_rejects_out_of_range_function_key() {
        assert!(parse_chord("Ctrl+F25").is_err());
    }

    #[test]
    fn parse_all_modifier_aliases() {
        let (mods, _) = parse_chord("control+super+shift+alt+K").unwrap();
        assert!(mods.0 & MOD_CONTROL.0 != 0);
        assert!(mods.0 & MOD_WIN.0 != 0);
        assert!(mods.0 & MOD_SHIFT.0 != 0);
        assert!(mods.0 & MOD_ALT.0 != 0);
    }
}

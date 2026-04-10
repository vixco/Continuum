//! # Kairo Desktop
//!
//! Tauri 2 backend for the Kairo dashboard. Serves the Next.js frontend
//! and bridges between the web UI and the Kairo Core runtime.

// Prevents additional console window on Windows in release
#![cfg_attr(not(debug_assertions), windows_subsystem = "windows")]

#[tauri::command]
fn greet() -> String {
    "hello from kairo core".to_string()
}

fn main() {
    tauri::Builder::default()
        .plugin(tauri_plugin_shell::init())
        .invoke_handler(tauri::generate_handler![greet])
        .run(tauri::generate_context!())
        .expect("error while running tauri application");
}

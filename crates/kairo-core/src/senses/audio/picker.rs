//! # Interactive audio device picker
//!
//! On first run (or when the saved device can no longer be found), Kairo asks
//! the user to pick their microphone. We probe each cpal input device for 1
//! second to show live RMS levels — the user sees which device is actually
//! hearing them and selects it by number.
//!
//! The choice is saved to `~/.kairo-dev/config.toml` under
//! `[audio].device_name` + `[audio].device_index`. On subsequent runs Kairo
//! verifies the device at the saved index still has the saved name and
//! silently reuses it. If the name no longer matches (devices reordered
//! across reboots, hardware plugged/unplugged), the picker runs again.

use std::io::{self, Write};
use std::path::Path;
use std::sync::{Arc, Mutex};
use std::time::Duration;

use anyhow::{Context, Result};
use cpal::traits::{DeviceTrait, HostTrait, StreamTrait};
use cpal::SampleFormat;
use toml::Value;

/// Duration each device is probed for RMS. Keep short to bound picker startup
/// time — at 16 devices × 1s = 16s, which is already on the edge of tolerable.
const PROBE_DURATION: Duration = Duration::from_millis(1000);

/// Threshold above which a device is considered "active" (real audio present)
/// during the picker probe. Matches the VAD threshold the main pipeline uses.
const ACTIVE_RMS_THRESHOLD: f32 = 0.01;

/// The user's chosen device. Stored to config and used to find the device
/// again on subsequent runs.
#[derive(Debug, Clone)]
pub struct DevicePick {
    /// cpal enumeration index at picker time.
    pub index: usize,
    /// Display name from `device.description()`. Used to verify on next run
    /// that the device at this index is still the one the user picked.
    pub name: String,
}

/// Interactive picker. Lists input devices with a live RMS probe, prompts the
/// user for a number on stdin, returns the pick. Does NOT write to config —
/// the caller is responsible for calling [`save_config`] if it wants to
/// persist the choice.
pub fn pick_interactive() -> Result<DevicePick> {
    let host = cpal::default_host();
    let devices: Vec<cpal::Device> = host
        .input_devices()
        .context("Failed to enumerate audio input devices")?
        .collect();

    if devices.is_empty() {
        anyhow::bail!("No audio input devices found. Plug in a microphone and try again.");
    }

    let approx_secs = devices.len() as u64;
    println!(
        "Probing {} audio input devices (this takes about {} seconds).",
        devices.len(),
        approx_secs
    );
    println!("Speak into your real microphone now — keep talking for the full probe.\n");

    let mut rows: Vec<Row> = Vec::with_capacity(devices.len());
    for (idx, d) in devices.iter().enumerate() {
        let name = display_name(d);
        let result = probe_device(d);
        rows.push(Row {
            index: idx,
            name,
            result,
        });
    }

    println!("Select your microphone:");
    for row in &rows {
        let status = match &row.result {
            Ok(rms) if *rms > ACTIVE_RMS_THRESHOLD => "*** ACTIVE ***".to_string(),
            Ok(_) => "(silent)".to_string(),
            Err(e) => format!("ERROR: {e}"),
        };
        println!("  [{:>2}] {:<42}  {}", row.index + 1, row.name, status);
    }
    println!();

    loop {
        print!("Enter device number (1-{}): ", rows.len());
        io::stdout().flush().ok();
        let mut input = String::new();
        io::stdin()
            .read_line(&mut input)
            .context("Failed to read stdin")?;
        match input.trim().parse::<usize>() {
            Ok(n) if n >= 1 && n <= rows.len() => {
                let chosen = &rows[n - 1];
                return Ok(DevicePick {
                    index: chosen.index,
                    name: chosen.name.clone(),
                });
            }
            _ => {
                println!("Please enter a number between 1 and {}.", rows.len());
            }
        }
    }
}

/// Returns true if the cpal input device at `index` has a display name equal
/// to `expected_name`. Used on startup to verify a previously saved choice is
/// still valid before trusting it.
pub fn verify_saved(index: usize, expected_name: &str) -> bool {
    let host = cpal::default_host();
    let Ok(iter) = host.input_devices() else {
        return false;
    };
    let Some(device) = iter.into_iter().nth(index) else {
        return false;
    };
    display_name(&device) == expected_name
}

/// Persists a `DevicePick` into the `[audio]` section of `config_path`.
/// Reads the existing TOML, updates only the two fields, writes back.
/// Creates the file if it doesn't exist.
pub fn save_config(config_path: &Path, pick: &DevicePick) -> Result<()> {
    let mut root = read_config_root(config_path)?;
    let audio = root
        .entry("audio".to_string())
        .or_insert_with(|| Value::Table(toml::Table::new()));
    let audio_table = audio
        .as_table_mut()
        .ok_or_else(|| anyhow::anyhow!("[audio] in config.toml is not a table"))?;
    audio_table.insert("device_name".to_string(), Value::String(pick.name.clone()));
    audio_table.insert(
        "device_index".to_string(),
        Value::Integer(pick.index as i64),
    );
    write_config_root(config_path, &root)
}

/// Removes the `device_name` and `device_index` entries from `[audio]` in
/// `config_path` (if present). Called by `--reset-audio` so the next run
/// re-invokes the picker.
pub fn clear_config(config_path: &Path) -> Result<()> {
    let mut root = match read_config_root(config_path) {
        Ok(r) => r,
        Err(_) => return Ok(()),
    };
    if let Some(audio) = root.get_mut("audio").and_then(|v| v.as_table_mut()) {
        audio.remove("device_name");
        audio.remove("device_index");
        if audio.is_empty() {
            root.remove("audio");
        }
    }
    write_config_root(config_path, &root)
}

// ---------------------------------------------------------------------------
// Internals
// ---------------------------------------------------------------------------

struct Row {
    index: usize,
    name: String,
    result: Result<f32, String>,
}

fn display_name(device: &cpal::Device) -> String {
    device
        .description()
        .map(|d| d.name().to_string())
        .unwrap_or_else(|_| "<unnamed>".to_string())
}

/// Opens a short input stream on the given device and returns the peak RMS
/// observed. Errors are caught and returned as strings so the picker can
/// continue rendering the list even if one device fails to open.
fn probe_device(device: &cpal::Device) -> Result<f32, String> {
    let default_cfg = device
        .default_input_config()
        .map_err(|e| format!("default_input_config: {e}"))?;

    let sample_format = default_cfg.sample_format();
    let stream_config: cpal::StreamConfig = default_cfg.into();

    let peak = Arc::new(Mutex::new(0.0f32));
    let err_flag = Arc::new(Mutex::new(None::<String>));

    let err_clone = Arc::clone(&err_flag);
    let err_cb = move |e: cpal::StreamError| {
        *err_clone.lock().unwrap() = Some(e.to_string());
    };

    let stream = match sample_format {
        SampleFormat::F32 => {
            let peak_cb = Arc::clone(&peak);
            device.build_input_stream(
                &stream_config,
                move |data: &[f32], _: &cpal::InputCallbackInfo| {
                    let rms = rms_f32(data);
                    let mut p = peak_cb.lock().unwrap();
                    if rms > *p {
                        *p = rms;
                    }
                },
                err_cb,
                None,
            )
        }
        SampleFormat::I16 => {
            let peak_cb = Arc::clone(&peak);
            device.build_input_stream(
                &stream_config,
                move |data: &[i16], _: &cpal::InputCallbackInfo| {
                    let rms = rms_i16(data);
                    let mut p = peak_cb.lock().unwrap();
                    if rms > *p {
                        *p = rms;
                    }
                },
                err_cb,
                None,
            )
        }
        SampleFormat::U16 => {
            let peak_cb = Arc::clone(&peak);
            device.build_input_stream(
                &stream_config,
                move |data: &[u16], _: &cpal::InputCallbackInfo| {
                    let rms = rms_u16(data);
                    let mut p = peak_cb.lock().unwrap();
                    if rms > *p {
                        *p = rms;
                    }
                },
                err_cb,
                None,
            )
        }
        other => return Err(format!("unsupported sample format: {other:?}")),
    }
    .map_err(|e| format!("build_input_stream: {e}"))?;

    stream.play().map_err(|e| format!("play: {e}"))?;
    std::thread::sleep(PROBE_DURATION);
    drop(stream);

    if let Some(err) = err_flag.lock().unwrap().take() {
        return Err(format!("stream error: {err}"));
    }

    let peak_val = *peak.lock().unwrap();
    Ok(peak_val)
}

fn rms_f32(data: &[f32]) -> f32 {
    if data.is_empty() {
        return 0.0;
    }
    let sum_sq: f32 = data.iter().map(|&s| s * s).sum();
    (sum_sq / data.len() as f32).sqrt()
}

fn rms_i16(data: &[i16]) -> f32 {
    if data.is_empty() {
        return 0.0;
    }
    let scale = 1.0 / i16::MAX as f32;
    let sum_sq: f32 = data
        .iter()
        .map(|&s| {
            let f = s as f32 * scale;
            f * f
        })
        .sum();
    (sum_sq / data.len() as f32).sqrt()
}

fn rms_u16(data: &[u16]) -> f32 {
    if data.is_empty() {
        return 0.0;
    }
    let sum_sq: f32 = data
        .iter()
        .map(|&s| {
            let f = (s as f32 - 32768.0) / 32768.0;
            f * f
        })
        .sum();
    (sum_sq / data.len() as f32).sqrt()
}

fn read_config_root(config_path: &Path) -> Result<toml::Table> {
    match std::fs::read_to_string(config_path) {
        Ok(text) if text.trim().is_empty() => Ok(toml::Table::new()),
        Ok(text) => text.parse::<toml::Table>().with_context(|| {
            format!(
                "Failed to parse existing {} — refusing to overwrite",
                config_path.display()
            )
        }),
        Err(_) => Ok(toml::Table::new()),
    }
}

fn write_config_root(config_path: &Path, root: &toml::Table) -> Result<()> {
    if let Some(parent) = config_path.parent() {
        std::fs::create_dir_all(parent)
            .with_context(|| format!("Failed to create config parent dir {}", parent.display()))?;
    }
    let text = toml::to_string_pretty(root).context("Failed to serialize config.toml")?;
    std::fs::write(config_path, text)
        .with_context(|| format!("Failed to write {}", config_path.display()))?;
    Ok(())
}

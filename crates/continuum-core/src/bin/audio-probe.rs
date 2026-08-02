//! # audio-probe
//!
//! Thin wrapper around the interactive audio picker. Lets the user re-pick
//! their microphone without starting the full Continuum runtime. The pick is
//! saved to `~/.continuum-dev/config.toml` under `[audio].device_name` +
//! `[audio].device_index`, so the next `continuum` run uses it silently.
//!
//! Same effect as `cargo run --release --bin continuum -- --reset-audio`, but
//! doesn't boot perception/triage/orchestrator afterwards.

use anyhow::{Context, Result};

use continuum_core::config::continuum_dev_dir;
use continuum_core::senses::audio::{pick_interactive, save_audio_config};

fn main() -> Result<()> {
    let dev_dir = continuum_dev_dir();
    std::fs::create_dir_all(&dev_dir).context("Failed to create ~/.continuum-dev/")?;
    let config_path = dev_dir.join("config.toml");

    let pick = pick_interactive()?;
    save_audio_config(&config_path, &pick).context("Failed to save audio device choice")?;

    println!(
        "\nSaved audio device: [{}] {} -> {}",
        pick.index + 1,
        pick.name,
        config_path.display()
    );
    println!("Continuum will use this device automatically on next start.");
    Ok(())
}

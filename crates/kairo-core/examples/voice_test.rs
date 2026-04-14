//! # voice_test — Phase 5A acceptance gate
//!
//! Smoke test that proves Kairo can speak end-to-end. Loads the TTS config,
//! builds a Piper voice bank (English + Dutch when both are installed),
//! synthesises a short phrase in each language, and plays them through the
//! default audio output device with timing info.
//!
//! Running this example is the acceptance test for Phase 5A:
//!
//! ```bash
//! cargo run --example voice_test -p kairo-core
//! ```
//!
//! Required setup (run once):
//!
//! ```powershell
//! powershell scripts/download-models.ps1
//! ```
//!
//! The script installs Piper voices, the Piper Windows binary, and the
//! espeak-ng-data phoneme dictionary under `~/.kairo-dev/`. This example
//! reads all paths from the same config that the main `kairo` binary uses,
//! so it will pick up overrides in `~/.kairo-dev/config.toml` if present.

use std::collections::HashMap;
use std::path::PathBuf;
use std::sync::Arc;
use std::time::{Duration, Instant};

use anyhow::{Context, Result};
use tracing_subscriber::EnvFilter;

use kairo_core::config::{kairo_dev_dir, load_config, TtsVoiceConfig};
use kairo_core::voice::playback::PlaybackStream;
use kairo_core::voice::sounds::{FeedbackCue, FeedbackPlayer};
use kairo_core::voice::tts::{set_espeak_data_dir, PiperEngine, PiperVoiceBank, TtsEngine};

struct TestPhrase {
    language: &'static str,
    label: &'static str,
    text: &'static str,
}

const PHRASES: &[TestPhrase] = &[
    TestPhrase {
        language: "nl",
        label: "Dutch",
        text: "Hallo, ik ben Kairo. Alles werkt zoals het hoort.",
    },
    TestPhrase {
        language: "en",
        label: "English",
        text: "Hello, I am Kairo. Everything is working.",
    },
];

fn init_tracing() {
    let filter = EnvFilter::try_from_default_env()
        .unwrap_or_else(|_| EnvFilter::new("info,kairo_core=debug"));
    let _ = tracing_subscriber::fmt()
        .with_env_filter(filter)
        .with_target(false)
        .compact()
        .try_init();
}

fn expand_home(raw: &str) -> PathBuf {
    if let Some(stripped) = raw.strip_prefix("~/") {
        if let Some(home) = dirs::home_dir() {
            return home.join(stripped);
        }
    }
    if raw == "~" {
        if let Some(home) = dirs::home_dir() {
            return home;
        }
    }
    PathBuf::from(raw)
}

fn build_voice_bank(
    voice_entries: &HashMap<String, TtsVoiceConfig>,
    primary: &str,
    length_scale: Option<f32>,
) -> Result<PiperVoiceBank> {
    let mut loaded = HashMap::new();
    let mut missing: Vec<String> = Vec::new();

    for (lang, entry) in voice_entries {
        let model = expand_home(&entry.model_path);
        let config = expand_home(&entry.config_path);
        if !model.exists() || !config.exists() {
            missing.push(format!(
                "  {lang}: model={} config={}",
                model.display(),
                config.display()
            ));
            continue;
        }
        let engine = PiperEngine::new(&model, &config, length_scale, entry.speaker_id)
            .with_context(|| format!("Failed to initialise Piper voice for {lang}"))?;
        loaded.insert(lang.clone(), engine);
    }

    if !missing.is_empty() {
        eprintln!(
            "[voice_test] Missing voice files (skipped):\n{}",
            missing.join("\n")
        );
    }

    if loaded.is_empty() {
        anyhow::bail!(
            "No Piper voices are installed. Run: powershell scripts/download-models.ps1"
        );
    }

    // Fall back to the first available language if the configured primary is missing.
    let primary = if loaded.contains_key(primary) {
        primary.to_string()
    } else {
        loaded
            .keys()
            .next()
            .cloned()
            .expect("at least one voice loaded")
    };

    PiperVoiceBank::new(primary, loaded)
}

fn main() -> Result<()> {
    init_tracing();

    println!("[voice_test] Loading config...");
    let dev_dir = kairo_dev_dir();
    let config_path = dev_dir.join("config.toml");
    let config = load_config(&config_path).context("Failed to load Kairo config")?;

    if !config.tts.enabled {
        eprintln!("[voice_test] TTS is disabled in config. Set [tts].enabled = true.");
        std::process::exit(2);
    }

    let espeak_dir = expand_home(&config.tts.espeak_data_dir);
    println!("[voice_test] espeak-ng-data: {}", espeak_dir.display());
    if !espeak_dir.exists() {
        eprintln!(
            "[voice_test] espeak-ng-data directory missing. Run the download script first."
        );
        std::process::exit(2);
    }
    set_espeak_data_dir(&espeak_dir);

    println!("[voice_test] Loading Piper voice bank...");
    let bank = build_voice_bank(
        &config.tts.voices,
        &config.tts.primary,
        config.tts.length_scale,
    )?;
    println!("[voice_test] Loaded {} voice(s)", bank.voice_count());

    println!("[voice_test] Opening audio output...");
    let playback = Arc::new(
        PlaybackStream::open_default_with_volume(config.voice.volume)
            .context("Failed to open default audio output device")?,
    );
    println!(
        "[voice_test] Output: {} Hz × {} channel(s), volume={:.2}",
        playback.device_sample_rate(),
        playback.channels(),
        playback.volume()
    );

    let feedback = FeedbackPlayer::new(playback.clone(), config.voice.feedback_sounds);
    feedback.play(FeedbackCue::Listen);
    std::thread::sleep(Duration::from_millis(200));
    playback.wait_drain();

    let mut timings: Vec<(String, u128, u128)> = Vec::new();

    for phrase in PHRASES {
        println!(
            "\n[voice_test] ── {} ({}): {:?}",
            phrase.label, phrase.language, phrase.text
        );

        let synth_start = Instant::now();
        let audio = bank
            .synthesize_for_language(phrase.text, Some(phrase.language))
            .with_context(|| format!("synth failed for {}", phrase.label))?;
        let synth_ms = synth_start.elapsed().as_millis();

        let audio_ms = if audio.sample_rate > 0 {
            (audio.samples.len() as u128 * 1000) / audio.sample_rate as u128
        } else {
            0
        };

        println!(
            "[voice_test]   synth: {synth_ms} ms for {} samples ({audio_ms} ms of audio, {} Hz)",
            audio.samples.len(),
            audio.sample_rate
        );

        playback.push_mono(&audio.samples, audio.sample_rate);
        playback.wait_drain();

        timings.push((phrase.label.to_string(), synth_ms, audio_ms));
    }

    println!("\n[voice_test] Done — summary:");
    for (label, synth_ms, audio_ms) in &timings {
        println!(
            "  {label:<10} synth={synth_ms} ms  audio={audio_ms} ms  ratio={:.2}×",
            if *synth_ms > 0 {
                *audio_ms as f64 / *synth_ms as f64
            } else {
                0.0
            }
        );
    }

    let any_slow = timings.iter().any(|(_, synth_ms, _)| *synth_ms > 1500);
    if any_slow {
        println!(
            "[voice_test] warning: at least one synthesis exceeded 1500 ms — check CPU load \
             or try a smaller voice model"
        );
    }

    feedback.play(FeedbackCue::Done);
    playback.wait_drain();

    Ok(())
}

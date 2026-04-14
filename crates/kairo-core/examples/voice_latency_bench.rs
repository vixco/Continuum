//! # voice_latency_bench — Phase 5C latency targets
//!
//! Measures per-stage voice latency over `N` iterations and reports
//! P50/P95/MAX against the ARCHITECTURE.md targets:
//!
//! - Wake detection (transcript → match): target under 10 ms
//! - Endpoint detection (transcript → decision): target under 20 ms
//! - TTS synthesis (text → PCM): target under 400 ms for short phrases
//! - Playback start (PCM ready → active): target under 50 ms
//! - Full pipeline (wake → first audio queued): target under 500 ms
//!
//! These targets are achievable on CPU-only hardware with a medium Piper
//! voice. Slower synth times are usually explained by background CPU load
//! or a large voice model (e.g. large-pro instead of medium).
//!
//! Run:
//!
//! ```bash
//! cargo run --example voice_latency_bench -p kairo-core
//! ```
//!
//! Optional env:
//! - `KAIRO_BENCH_N` — number of iterations (default 10)
//! - `KAIRO_BENCH_LANG` — language to benchmark: `en` (default) or `nl`

use std::collections::HashMap;
use std::path::PathBuf;
use std::sync::Arc;
use std::time::{Duration, Instant};

use anyhow::{Context, Result};
use tracing_subscriber::EnvFilter;

use kairo_core::config::{kairo_dev_dir, load_config, TtsVoiceConfig};
use kairo_core::voice::playback::PlaybackStream;
use kairo_core::voice::stt::{EndpointDecision, SemanticEndpointDetector, VoiceSession};
use kairo_core::voice::tts::{set_espeak_data_dir, PiperEngine, PiperVoiceBank, TtsEngine};
use kairo_core::voice::wake::TranscriptWakeDetector;

fn init_tracing() {
    let filter = EnvFilter::try_from_default_env().unwrap_or_else(|_| EnvFilter::new("warn"));
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
    PathBuf::from(raw)
}

fn load_voice_bank(
    voices: &HashMap<String, TtsVoiceConfig>,
    primary: &str,
    length_scale: Option<f32>,
) -> Result<PiperVoiceBank> {
    let mut loaded = HashMap::new();
    for (lang, entry) in voices {
        let model = expand_home(&entry.model_path);
        let cfg = expand_home(&entry.config_path);
        if !model.exists() || !cfg.exists() {
            continue;
        }
        let engine = PiperEngine::new(&model, &cfg, length_scale, entry.speaker_id)?;
        loaded.insert(lang.clone(), engine);
    }
    if loaded.is_empty() {
        anyhow::bail!(
            "No Piper voices installed. Run: powershell scripts/download-models.ps1"
        );
    }
    PiperVoiceBank::new(primary.to_string(), loaded)
}

fn percentile(samples: &mut [u128], p: f64) -> u128 {
    if samples.is_empty() {
        return 0;
    }
    samples.sort_unstable();
    let idx = ((samples.len() as f64 - 1.0) * p).round() as usize;
    samples[idx.min(samples.len() - 1)]
}

fn print_stage(name: &str, samples: &[u128], target_ms: u128) {
    let mut s = samples.to_vec();
    let p50 = percentile(&mut s, 0.5);
    let p95 = percentile(&mut s, 0.95);
    let max = samples.iter().copied().max().unwrap_or(0);
    let verdict = if p95 <= target_ms {
        "[OK]"
    } else if p95 <= target_ms * 2 {
        "[WARN]"
    } else {
        "[FAIL]"
    };
    println!(
        "  {verdict:<7} {name:<24}  P50 {p50:>5} ms  P95 {p95:>5} ms  MAX {max:>5} ms  (target P95 ≤ {target_ms} ms)"
    );
}

fn main() -> Result<()> {
    init_tracing();
    println!("Kairo voice latency benchmark");
    println!("──────────────────────────────");

    let n: usize = std::env::var("KAIRO_BENCH_N")
        .ok()
        .and_then(|s| s.parse().ok())
        .unwrap_or(10);
    let lang = std::env::var("KAIRO_BENCH_LANG").unwrap_or_else(|_| "en".to_string());
    println!("iterations: {n}   language: {lang}");

    let dev_dir = kairo_dev_dir();
    let config = load_config(&dev_dir.join("config.toml")).context("Failed to load config")?;

    let espeak = expand_home(&config.tts.espeak_data_dir);
    if !espeak.exists() {
        eprintln!("espeak-ng-data missing. Run the download script first.");
        std::process::exit(2);
    }
    set_espeak_data_dir(&espeak);

    let bank = load_voice_bank(&config.tts.voices, &lang, config.tts.length_scale)?;
    let playback = Arc::new(
        PlaybackStream::open_default_with_volume(config.voice.volume)
            .context("No audio output device")?,
    );

    let wake = TranscriptWakeDetector::new(config.voice.wake_keyword.clone());
    let endpoint = SemanticEndpointDetector::new(
        Duration::from_millis(config.voice.endpoint_silence_ms),
        Duration::from_millis(config.voice.listen_timeout_ms),
        config.voice.min_utterance_chars,
    );

    let phrase_en = "Hello. This is a short benchmark line.";
    let phrase_nl = "Hallo. Dit is een korte testzin.";
    let phrase = if lang == "nl" { phrase_nl } else { phrase_en };

    let mut wake_ms: Vec<u128> = Vec::with_capacity(n);
    let mut endpoint_ms: Vec<u128> = Vec::with_capacity(n);
    let mut synth_ms: Vec<u128> = Vec::with_capacity(n);
    let mut playback_start_ms: Vec<u128> = Vec::with_capacity(n);
    let mut full_ms: Vec<u128> = Vec::with_capacity(n);

    // Warmup synthesis so the first iteration doesn't include Piper process
    // startup cost.
    let _ = bank
        .synthesize_for_language("warmup", Some(&lang))
        .context("TTS warmup failed")?;

    for i in 0..n {
        let full_start = Instant::now();

        // Wake detection.
        let wake_start = Instant::now();
        let detected = wake.detect(&format!("{} {}", config.voice.wake_keyword, phrase));
        wake_ms.push(wake_start.elapsed().as_micros() / 1000);
        assert!(detected.is_some(), "wake phrase should match");
        let utterance = detected.unwrap().utterance_after_wake;

        // Endpoint detection.
        let session = VoiceSession::new(&utterance, &lang);
        let ep_start = Instant::now();
        let decision = endpoint.decide(&session);
        endpoint_ms.push(ep_start.elapsed().as_micros() / 1000);
        assert!(
            matches!(decision, EndpointDecision::Complete | EndpointDecision::Continue),
            "unexpected endpoint decision: {decision:?}"
        );

        // TTS synthesis.
        let synth_start = Instant::now();
        let audio = bank
            .synthesize_for_language(phrase, Some(&lang))
            .context("synth failed")?;
        synth_ms.push(synth_start.elapsed().as_millis());

        // Playback start — time from push to is_active flipping true. This
        // is driven by the cpal callback, so we poll with a short sleep.
        let play_start = Instant::now();
        playback.push_mono(&audio.samples, audio.sample_rate);
        while !playback.is_active() {
            std::thread::sleep(Duration::from_millis(1));
            if play_start.elapsed() > Duration::from_millis(500) {
                break;
            }
        }
        playback_start_ms.push(play_start.elapsed().as_millis());

        full_ms.push(full_start.elapsed().as_millis());

        // Drain so iterations don't pile up.
        playback.wait_drain();

        println!(
            "  iter {i:>2}/{n}   wake {} ms  endpoint {} ms  synth {} ms  playback_start {} ms  full {} ms",
            wake_ms[i], endpoint_ms[i], synth_ms[i], playback_start_ms[i], full_ms[i]
        );
    }

    println!("\nResults over {n} iterations:");
    print_stage("wake detect", &wake_ms, 10);
    print_stage("endpoint decision", &endpoint_ms, 20);
    print_stage("TTS synthesis", &synth_ms, 400);
    print_stage("playback start", &playback_start_ms, 50);
    print_stage("full pipeline", &full_ms, 500);

    println!("\nReminder: ARCHITECTURE.md budget is P95 for interactive voice loops.");
    println!("          Slower numbers usually indicate background CPU contention.");

    Ok(())
}

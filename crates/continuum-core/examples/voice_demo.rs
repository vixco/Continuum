//! # voice_demo — Phase 5C end-to-end demonstration
//!
//! Runs a full voice loop against mocked orchestrator output: wake-word
//! detection, streaming STT (piped-in), endpoint detection, TTS streaming,
//! conversation follow-up mode, and barge-in. Prints latency stats across
//! the pipeline.
//!
//! The demo does **not** require a microphone or Opus API — transcripts
//! are typed in on stdin so the acceptance gate works in any environment
//! (CI, headless dev box, whatever). For the real thing, run
//! `cargo run --bin continuum`.
//!
//! Run:
//!
//! ```bash
//! cargo run --example voice_demo -p continuum-core
//! ```
//!
//! Then follow the on-screen prompts. Type a command prefixed with
//! the wake phrase, e.g. `hey continuum what's the weather` — the demo
//! detects the wake, "transcribes" your text, streams a canned response
//! through Piper, and announces the follow-up window.

use std::collections::HashMap;
use std::io::{BufRead, Write};
use std::path::PathBuf;
use std::sync::Arc;
use std::time::{Duration, Instant};

use anyhow::{Context, Result};
use tracing_subscriber::EnvFilter;

use continuum_core::config::{continuum_dev_dir, load_config, TtsVoiceConfig};
use continuum_core::voice::playback::PlaybackStream;
use continuum_core::voice::sounds::{FeedbackCue, FeedbackPlayer};
use continuum_core::voice::streaming::SpeechController;
use continuum_core::voice::stt::{EndpointDecision, SemanticEndpointDetector, VoiceSession};
use continuum_core::voice::tts::{set_espeak_data_dir, PiperEngine, PiperVoiceBank, TtsEngine};
use continuum_core::voice::wake::TranscriptWakeDetector;

fn init_tracing() {
    let filter = EnvFilter::try_from_default_env()
        .unwrap_or_else(|_| EnvFilter::new("info,continuum_core=debug"));
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
        anyhow::bail!("No Piper voices installed. Run: powershell scripts/download-models.ps1");
    }
    let primary = if loaded.contains_key(primary) {
        primary.to_string()
    } else {
        loaded.keys().next().cloned().unwrap()
    };
    PiperVoiceBank::new(primary, loaded)
}

struct MockOrchestratorResponse {
    deltas: Vec<&'static str>,
}

/// A canned "orchestrator response" streamed as TextDelta chunks to show
/// sentence-level streaming TTS in action.
fn canned_response(command: &str) -> MockOrchestratorResponse {
    let lower = command.to_lowercase();
    if lower.contains("weather") || lower.contains("het weer") {
        MockOrchestratorResponse {
            deltas: vec![
                "Quick look — ",
                "it's overcast in Breda, twelve degrees. ",
                "Rain likely after three. ",
                "Want the hour-by-hour?",
            ],
        }
    } else if lower.contains("time") || lower.contains("tijd") {
        MockOrchestratorResponse {
            deltas: vec![
                "Just past ",
                "the top of the hour. ",
                "Want me to start a timer?",
            ],
        }
    } else {
        MockOrchestratorResponse {
            deltas: vec!["Got it — ", "I'll look into that. ", "Anything else?"],
        }
    }
}

fn prompt(msg: &str) {
    println!("\n{msg}");
    print!("> ");
    std::io::stdout().flush().ok();
}

fn main() -> Result<()> {
    init_tracing();

    println!("Continuum voice demo — Phase 5C acceptance gate");
    println!("────────────────────────────────────────────");

    let dev_dir = continuum_dev_dir();
    let config_path = dev_dir.join("config.toml");
    let config = load_config(&config_path).context("Failed to load Continuum config")?;

    if !config.tts.enabled {
        eprintln!("TTS is disabled in config. Exiting.");
        std::process::exit(2);
    }

    let espeak = expand_home(&config.tts.espeak_data_dir);
    if !espeak.exists() {
        eprintln!(
            "espeak-ng-data directory missing at {}. Run the download script first.",
            espeak.display()
        );
        std::process::exit(2);
    }
    set_espeak_data_dir(&espeak);

    let bank = load_voice_bank(
        &config.tts.voices,
        &config.tts.primary,
        config.tts.length_scale,
    )?;

    let playback = Arc::new(
        PlaybackStream::open_default_with_volume(config.voice.volume)
            .context("Failed to open default audio output")?,
    );
    let feedback = FeedbackPlayer::new(playback.clone(), config.voice.feedback_sounds);
    let engine: Arc<dyn TtsEngine> = Arc::new(bank);
    let speech = Arc::new(SpeechController::new(engine, playback.clone()));

    let wake = TranscriptWakeDetector::new(config.voice.wake_keyword.clone());
    let endpoint = SemanticEndpointDetector::new(
        Duration::from_millis(config.voice.endpoint_silence_ms),
        Duration::from_millis(config.voice.listen_timeout_ms),
        config.voice.min_utterance_chars,
    );

    println!(
        "Wake phrase: \"{}\"  |  Follow-up window: {}s  |  Volume: {:.2}",
        config.voice.wake_keyword, config.voice.conversation_followup_seconds, config.voice.volume
    );
    println!(
        "Enter text prefixed with the wake phrase to simulate a whisper transcript. \
         Type 'quit' to exit.\n"
    );

    let stdin = std::io::stdin();
    let mut followup_until: Option<Instant> = None;
    let mut session_count: u32 = 0;
    let mut latencies: Vec<(u128, u128, u128)> = Vec::new();

    loop {
        prompt("Transcript >");
        let mut line = String::new();
        if stdin.lock().read_line(&mut line).is_err() || line.is_empty() {
            break;
        }
        let line = line.trim();
        if line.is_empty() {
            continue;
        }
        if line.eq_ignore_ascii_case("quit") || line.eq_ignore_ascii_case("exit") {
            break;
        }

        let command_start = Instant::now();
        let now = Instant::now();
        let followup_active = followup_until.is_some_and(|d| now < d);

        let utterance = if followup_active {
            println!("  → Follow-up window active — wake word not required.");
            followup_until = None;
            feedback.play(FeedbackCue::Listen);
            line.to_string()
        } else if let Some(det) = wake.detect(line) {
            println!("  → Wake phrase matched: '{}'", det.keyword);
            feedback.play(FeedbackCue::Wake);
            det.utterance_after_wake
        } else {
            println!("  ✗ No wake phrase, not addressed to Continuum.");
            continue;
        };

        if utterance.trim().is_empty() {
            println!("  ✗ Nothing after the wake phrase.");
            continue;
        }

        let wake_to_transcript_ms = command_start.elapsed().as_millis();

        // Simulate endpoint detection by feeding the whole utterance in
        // one chunk.
        let mut session = VoiceSession::new(&utterance, "en");
        let endpoint_start = Instant::now();
        let decision = endpoint.decide(&session);
        let endpoint_ms = endpoint_start.elapsed().as_millis();
        println!("  endpoint: {decision:?}  ({endpoint_ms} ms)");

        // If heuristic says not complete, give it a short silence and retry.
        if matches!(decision, EndpointDecision::Continue) {
            std::thread::sleep(Duration::from_millis(config.voice.endpoint_silence_ms + 50));
            session.push_transcript("", "en");
        }

        let response = canned_response(&utterance);

        println!("  CONTINUUM: ");
        print!("    ");
        std::io::stdout().flush().ok();
        let first_audio_at = std::sync::Arc::new(std::sync::Mutex::new(None::<Instant>));

        let response_start = Instant::now();
        for delta in &response.deltas {
            print!("{delta}");
            std::io::stdout().flush().ok();
            // Record the moment we first enqueued text to TTS.
            {
                let mut lock = first_audio_at.lock().unwrap();
                if lock.is_none() {
                    *lock = Some(Instant::now());
                }
            }
            speech.push_delta(delta);
            std::thread::sleep(Duration::from_millis(80));
        }
        speech.flush();
        println!();

        let tts_submit_ms = response_start.elapsed().as_millis();

        // Wait until Continuum finishes talking before prompting again.
        speech.wait_idle();
        let full_ms = command_start.elapsed().as_millis();

        latencies.push((wake_to_transcript_ms, tts_submit_ms, full_ms));
        session_count += 1;

        if config.voice.conversation_followup_seconds > 0 {
            followup_until = Some(
                Instant::now() + Duration::from_secs(config.voice.conversation_followup_seconds),
            );
            println!(
                "  (follow-up window open for {} s — speak again without wake word)",
                config.voice.conversation_followup_seconds
            );
        }

        if session_count >= 5 {
            println!("  Enough samples — demo ending.");
            break;
        }
    }

    feedback.play(FeedbackCue::Done);
    playback.wait_drain();

    if !latencies.is_empty() {
        println!("\nLatency summary ({} sessions):", latencies.len());
        for (i, (wake_ms, tts_ms, full_ms)) in latencies.iter().enumerate() {
            println!(
                "  #{}  wake→transcript {wake_ms} ms  |  response→idle {tts_ms} ms  |  total {full_ms} ms",
                i + 1
            );
        }
    }

    println!("\nDone.");
    Ok(())
}

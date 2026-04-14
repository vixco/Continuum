//! # Text-to-speech
//!
//! Local TTS via [Piper](https://github.com/rhasspy/piper) invoked as a
//! subprocess. Piper reads UTF-8 text on stdin, emits 16-bit PCM on stdout,
//! and handles phonemization (espeak-ng) + neural VITS inference internally.
//! Running Piper as a subprocess keeps the heavy ONNX Runtime dependency out
//! of the Rust dependency graph (it would otherwise fight `ort` via
//! `kairo-vision`).
//!
//! The [`TtsEngine`] trait is object-safe so additional backends (e.g.
//! [`ElevenLabsEngine`] placeholder, future Kokoro) can plug in at runtime
//! via `Box<dyn TtsEngine>`.
//!
//! Streaming is achieved at the *sentence* level: the
//! [`crate::voice::streaming`] module buffers orchestrator `TextDelta`
//! tokens and hands a complete sentence to the engine as soon as one
//! terminates. First-audio latency is therefore bounded by the first-sentence
//! synthesis time (typically 150–400 ms on CPU for a short greeting).

use std::collections::HashMap;
use std::io::Write as _;
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};

use anyhow::{Context, Result};
use serde::Deserialize;

/// A single synthesised utterance: mono f32 PCM at a known sample rate.
#[derive(Debug, Clone)]
pub struct SynthesizedAudio {
    /// Mono f32 samples in [-1.0, 1.0].
    pub samples: Vec<f32>,
    /// Sample rate in Hz (22050 for Piper medium voices).
    pub sample_rate: u32,
}

/// Abstraction over a local TTS backend.
///
/// The trait is object-safe so we can swap engines at runtime via
/// `Box<dyn TtsEngine>`. Implementations must be `Send + Sync` because the
/// engine is shared across the playback task and the orchestrator stream
/// handler.
pub trait TtsEngine: Send + Sync {
    /// Synthesise `text` into mono f32 PCM audio.
    ///
    /// Blocking: this runs the ONNX inference and espeak phonemization
    /// on the calling thread. Wrap calls in [`tokio::task::spawn_blocking`]
    /// from async contexts.
    fn synthesize(&self, text: &str) -> Result<SynthesizedAudio>;

    /// Synthesise with an optional language hint. Engines with only one voice
    /// can ignore the hint.
    fn synthesize_for_language(
        &self,
        text: &str,
        _language: Option<&str>,
    ) -> Result<SynthesizedAudio> {
        self.synthesize(text)
    }

    /// Native sample rate of the current voice. Callers use this to decide
    /// whether to resample before feeding cpal.
    fn sample_rate(&self) -> u32;

    /// Human-readable engine identifier for logging (e.g. `"piper"`).
    fn name(&self) -> &'static str;
}

/// Multi-language Piper voice bank.
///
/// Each configured language owns its own Piper engine. The speech controller
/// passes the STT language hint through; unsupported or unknown languages
/// fall back to `primary`.
pub struct PiperVoiceBank {
    primary: String,
    voices: HashMap<String, PiperEngine>,
}

impl PiperVoiceBank {
    /// Creates a voice bank from a primary language and preloaded voices.
    pub fn new(primary: impl Into<String>, voices: HashMap<String, PiperEngine>) -> Result<Self> {
        let primary = primary.into();
        if !voices.contains_key(&primary) {
            anyhow::bail!("Primary Piper voice '{primary}' is not loaded");
        }
        Ok(Self { primary, voices })
    }

    fn choose_voice(&self, language: Option<&str>) -> (String, &PiperEngine) {
        let requested = language
            .map(crate::voice::stt::normalize_language)
            .unwrap_or_else(|| self.primary.clone());
        let key = if self.voices.contains_key(&requested) {
            requested
        } else {
            self.primary.clone()
        };

        let engine = self
            .voices
            .get(&key)
            .expect("primary voice is validated at construction");
        (key, engine)
    }

    /// Number of loaded voices.
    pub fn voice_count(&self) -> usize {
        self.voices.len()
    }
}

impl TtsEngine for PiperVoiceBank {
    fn synthesize(&self, text: &str) -> Result<SynthesizedAudio> {
        self.synthesize_for_language(text, Some(&self.primary))
    }

    fn synthesize_for_language(
        &self,
        text: &str,
        language: Option<&str>,
    ) -> Result<SynthesizedAudio> {
        let (voice, engine) = self.choose_voice(language);
        tracing::debug!(
            layer = "voice",
            component = "tts",
            voice = %voice,
            requested_language = ?language,
            "Routing utterance to Piper voice"
        );
        engine.synthesize(text)
    }

    fn sample_rate(&self) -> u32 {
        let (_, engine) = self.choose_voice(Some(&self.primary));
        engine.sample_rate()
    }

    fn name(&self) -> &'static str {
        "piper-bank"
    }
}

/// Piper TTS engine wrapping [`piper_rs::Piper`] with interior mutability.
///
/// `Piper::create` requires `&mut self`, so the inner handle is guarded by
/// a `Mutex`. Callers are expected to serialise synthesis anyway — playback
/// is sequential, and overlapping synthesis would compete for the same
/// ONNX Runtime session.
pub struct PiperEngine {
    model_path: PathBuf,
    config_path: PathBuf,
    sample_rate: u32,
    length_scale: Option<f32>,
    speaker_id: Option<i64>,
    piper_bin: PathBuf,
}

/// Minimal subset of the Piper `.onnx.json` sidecar we actually read.
///
/// We extract only `audio.sample_rate` so we can size the playback
/// resampler at engine init without first running a warmup synthesis.
/// Extra fields are ignored by serde.
#[derive(Debug, Deserialize)]
struct PiperVoiceConfig {
    audio: AudioSection,
}

#[derive(Debug, Deserialize)]
struct AudioSection {
    sample_rate: u32,
}

impl PiperEngine {
    /// Load a Piper voice from its ONNX model + JSON config paths.
    ///
    /// * `model_path` — `.onnx` file, e.g. `en_US-lessac-medium.onnx`.
    /// * `config_path` — matching `.onnx.json` sidecar. Sample rate is read
    ///   from this file so the playback resampler can be configured before
    ///   the first synthesis.
    /// * `length_scale` — speech rate multiplier; `None` uses the voice's
    ///   default. Values `<1.0` speed up, `>1.0` slow down.
    /// * `speaker_id` — for multi-speaker models; `None` uses the voice's
    ///   default speaker or the only speaker for single-speaker models.
    pub fn new(
        model_path: &Path,
        config_path: &Path,
        length_scale: Option<f32>,
        speaker_id: Option<i64>,
    ) -> Result<Self> {
        let sample_rate = read_sample_rate(config_path).with_context(|| {
            format!("Failed to read sample_rate from {}", config_path.display())
        })?;

        let piper_bin = resolve_piper_binary();

        tracing::info!(
            layer = "voice",
            component = "tts",
            model = %model_path.display(),
            sample_rate,
            length_scale = ?length_scale,
            speaker_id = ?speaker_id,
            "Piper engine loaded"
        );

        Ok(Self {
            model_path: model_path.to_path_buf(),
            config_path: config_path.to_path_buf(),
            sample_rate,
            length_scale,
            speaker_id,
            piper_bin,
        })
    }
}

impl TtsEngine for PiperEngine {
    fn synthesize(&self, text: &str) -> Result<SynthesizedAudio> {
        let start = std::time::Instant::now();
        let mut cmd = Command::new(&self.piper_bin);
        cmd.arg("--model")
            .arg(&self.model_path)
            .arg("--config")
            .arg(&self.config_path)
            .arg("--output-raw")
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped());

        if let Some(length_scale) = self.length_scale {
            cmd.arg("--length-scale").arg(length_scale.to_string());
        }
        if let Some(speaker_id) = self.speaker_id {
            cmd.arg("--speaker").arg(speaker_id.to_string());
        }

        let mut child = cmd.spawn().with_context(|| {
            format!(
                "Failed to spawn Piper binary '{}'",
                self.piper_bin.display()
            )
        })?;

        {
            let stdin = child.stdin.as_mut().context("Failed to open Piper stdin")?;
            stdin
                .write_all(text.as_bytes())
                .context("Failed to write text to Piper stdin")?;
        }

        let output = child
            .wait_with_output()
            .context("Failed to wait for Piper process")?;
        if !output.status.success() {
            let stderr = String::from_utf8_lossy(&output.stderr);
            anyhow::bail!("Piper synthesis failed: {stderr}");
        }

        let samples = pcm_i16le_to_f32(&output.stdout);
        let sr = self.sample_rate;

        let duration_ms = start.elapsed().as_millis() as u64;
        let audio_ms = if sr > 0 {
            (samples.len() as u64 * 1000) / sr as u64
        } else {
            0
        };

        tracing::debug!(
            layer = "voice",
            component = "tts",
            text_len = text.len(),
            samples = samples.len(),
            sample_rate = sr,
            synth_ms = duration_ms,
            audio_ms,
            "Synthesized utterance"
        );

        Ok(SynthesizedAudio {
            samples,
            sample_rate: sr,
        })
    }

    fn sample_rate(&self) -> u32 {
        self.sample_rate
    }

    fn name(&self) -> &'static str {
        "piper"
    }
}

fn pcm_i16le_to_f32(bytes: &[u8]) -> Vec<f32> {
    bytes
        .chunks_exact(2)
        .map(|chunk| {
            let sample = i16::from_le_bytes([chunk[0], chunk[1]]);
            sample as f32 / i16::MAX as f32
        })
        .collect()
}

/// ElevenLabs cloud TTS backend — **stub for Phase 5**.
///
/// The ROADMAP keeps ElevenLabs out of the local-first Phase 5 scope; this
/// type exists so the config surface (`[tts.elevenlabs]`) and the
/// [`TtsEngine`] trait both cover the cloud backend, letting a future
/// point-release wire the actual HTTP/WebSocket client without breaking the
/// public API.
///
/// Attempting to synthesize always returns an explanatory error so the
/// runtime falls back to Piper (or log-only mode) gracefully when the user
/// sets `engine = "elevenlabs"` before the plugin is implemented.
pub struct ElevenLabsEngine {
    voice_id: String,
    model_id: String,
}

impl ElevenLabsEngine {
    /// Construct an ElevenLabs engine from credentials. The current build
    /// does not perform any network I/O; the real HTTP client lands in a
    /// later plugin release.
    pub fn new(voice_id: impl Into<String>, model_id: impl Into<String>) -> Self {
        Self {
            voice_id: voice_id.into(),
            model_id: model_id.into(),
        }
    }
}

impl TtsEngine for ElevenLabsEngine {
    fn synthesize(&self, _text: &str) -> Result<SynthesizedAudio> {
        anyhow::bail!(
            "ElevenLabs TTS is a Phase 5 extension point — cloud backend not wired up yet. \
             Set `tts.engine = \"piper\"` in the config or wait for the cloud plugin release. \
             Configured voice: {voice_id}, model: {model_id}",
            voice_id = self.voice_id,
            model_id = self.model_id,
        )
    }

    fn sample_rate(&self) -> u32 {
        // ElevenLabs streams 22050 Hz PCM by default on the turbo model.
        22_050
    }

    fn name(&self) -> &'static str {
        "elevenlabs"
    }
}

/// Resolve the Piper binary path.
///
/// Resolution order:
/// 1. `KAIRO_PIPER_BIN` environment variable (explicit override)
/// 2. `~/.kairo-dev/bin/piper/piper.exe` on Windows (installed by `download-models.ps1`)
/// 3. `~/.kairo-dev/bin/piper/piper` on Unix (same install tree)
/// 4. Fallback `"piper"` — rely on PATH lookup.
///
/// The fallbacks let `voice_test` / `voice_demo` run out of the box once the
/// download script has populated the dev tree, without requiring the user to
/// manually set PATH or the env var.
fn resolve_piper_binary() -> PathBuf {
    if let Some(override_bin) = std::env::var_os("KAIRO_PIPER_BIN") {
        return PathBuf::from(override_bin);
    }

    if let Some(home) = dirs::home_dir() {
        let bundled = if cfg!(windows) {
            home.join(".kairo-dev")
                .join("bin")
                .join("piper")
                .join("piper.exe")
        } else {
            home.join(".kairo-dev")
                .join("bin")
                .join("piper")
                .join("piper")
        };
        if bundled.exists() {
            return bundled;
        }
    }

    PathBuf::from("piper")
}

/// Set the `PIPER_ESPEAKNG_DATA_DIRECTORY` environment variable so
/// espeak-rs can find its dictionary files. Must be called before the
/// first [`PiperEngine::new`].
///
/// Idempotent: overwrites any previously-set value with the caller's path.
/// If the caller passes a path that does not exist, logs a warning — the
/// actual synthesis call will fail later with a clearer phonemization error.
pub fn set_espeak_data_dir(dir: &Path) {
    if !dir.exists() {
        tracing::warn!(
            layer = "voice",
            component = "tts",
            path = %dir.display(),
            "espeak-ng data directory does not exist — Piper phonemization will fail"
        );
    }
    // SAFETY: env-var mutation is process-global; we set this once at startup
    // before any Piper engine is constructed.
    std::env::set_var("PIPER_ESPEAKNG_DATA_DIRECTORY", dir);
}

/// Read `audio.sample_rate` from a Piper `.onnx.json` config file.
fn read_sample_rate(config_path: &Path) -> Result<u32> {
    let raw = std::fs::read_to_string(config_path).context("Failed to read config file")?;
    let cfg: PiperVoiceConfig =
        serde_json::from_str(&raw).context("Failed to parse Piper config JSON")?;
    if cfg.audio.sample_rate == 0 {
        anyhow::bail!("Piper config reports sample_rate = 0");
    }
    Ok(cfg.audio.sample_rate)
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::Write;
    use tempfile::NamedTempFile;

    fn write_config(json: &str) -> NamedTempFile {
        let mut f = NamedTempFile::new().unwrap();
        f.write_all(json.as_bytes()).unwrap();
        f.flush().unwrap();
        f
    }

    #[test]
    fn test_read_sample_rate_happy_path() {
        let f = write_config(r#"{"audio":{"sample_rate":22050},"other":"ignored"}"#);
        let sr = read_sample_rate(f.path()).unwrap();
        assert_eq!(sr, 22050);
    }

    #[test]
    fn test_read_sample_rate_extra_audio_fields() {
        let f = write_config(r#"{"audio":{"sample_rate":16000,"quality":"low","channels":1}}"#);
        assert_eq!(read_sample_rate(f.path()).unwrap(), 16000);
    }

    #[test]
    fn test_read_sample_rate_rejects_zero() {
        let f = write_config(r#"{"audio":{"sample_rate":0}}"#);
        assert!(read_sample_rate(f.path()).is_err());
    }

    #[test]
    fn test_read_sample_rate_missing_audio() {
        let f = write_config(r#"{"not_audio":{"sample_rate":22050}}"#);
        assert!(read_sample_rate(f.path()).is_err());
    }

    #[test]
    fn test_read_sample_rate_missing_file() {
        assert!(read_sample_rate(Path::new("/nope/does/not/exist.json")).is_err());
    }

    #[test]
    fn test_read_sample_rate_malformed_json() {
        let f = write_config(r#"{not json at all"#);
        assert!(read_sample_rate(f.path()).is_err());
    }

    #[test]
    fn test_set_espeak_data_dir_sets_env() {
        let tmp = std::env::temp_dir();
        set_espeak_data_dir(&tmp);
        let got = std::env::var("PIPER_ESPEAKNG_DATA_DIRECTORY").unwrap();
        assert_eq!(got, tmp.to_string_lossy());
    }

    #[test]
    fn test_voice_bank_requires_primary() {
        let voices = HashMap::new();
        assert!(PiperVoiceBank::new("en", voices).is_err());
    }

    #[test]
    fn test_pcm_i16le_to_f32() {
        let bytes = [0x00, 0x00, 0xff, 0x7f, 0x00, 0x80];
        let samples = pcm_i16le_to_f32(&bytes);
        assert_eq!(samples.len(), 3);
        assert_eq!(samples[0], 0.0);
        assert!(samples[1] > 0.99);
        assert!(samples[2] < -0.99);
    }

    #[test]
    fn elevenlabs_engine_reports_clear_error() {
        let engine = ElevenLabsEngine::new("voice-xyz", "eleven_turbo_v2_5");
        let err = engine.synthesize("hi").unwrap_err().to_string();
        assert!(
            err.contains("Phase 5 extension point"),
            "error should explain extension point, got: {err}"
        );
        assert!(err.contains("voice-xyz"), "error should include voice id");
        assert_eq!(engine.name(), "elevenlabs");
        assert_eq!(engine.sample_rate(), 22_050);
    }

    #[test]
    fn resolve_piper_binary_uses_env_override() {
        let sentinel = "kairo-test-piper-override";
        std::env::set_var("KAIRO_PIPER_BIN", sentinel);
        let got = resolve_piper_binary();
        assert_eq!(got, std::path::PathBuf::from(sentinel));
        std::env::remove_var("KAIRO_PIPER_BIN");
    }
}

//! # Text-to-speech
//!
//! Local TTS via [Piper](https://github.com/rhasspy/piper) invoked as a
//! subprocess. Piper reads UTF-8 text on stdin, emits 16-bit PCM on stdout,
//! and handles phonemization (espeak-ng) + neural VITS inference internally.
//! Running Piper as a subprocess keeps the heavy ONNX Runtime dependency out
//! of the Rust dependency graph (it would otherwise fight `ort` via
//! `continuum-vision`).
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
use std::io::{Read as _, Write as _};
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::time::{Duration, Instant};

use anyhow::{Context, Result};
use serde::Deserialize;

use crate::config::{continuum_dev_dir, env_or_legacy};

/// Hard wall-clock ceiling for a single Piper synthesis call. Normal spoken
/// sentences take 150–800 ms on CPU; anything past this is a hung phonemizer
/// or a stuck ONNX session and the child should be killed rather than
/// permanently blocking the TTS worker thread.
const PIPER_SYNTH_TIMEOUT: Duration = Duration::from_secs(30);

/// How often to poll for child exit while waiting inside the timeout window.
const PIPER_POLL_INTERVAL: Duration = Duration::from_millis(25);

/// Hard wall-clock ceiling for a single Kokoros synthesis call. Kokoro-82M
/// inference is fast once the ONNX session is loaded, but the per-call
/// subprocess pays a model-load + phonemization cost; 30 s is the same safety
/// ceiling Piper uses and is well outside any normal utterance.
const KOKOROS_SYNTH_TIMEOUT: Duration = Duration::from_secs(30);

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
    /// * `model_path` — `.onnx` file, e.g. `en_US-norman-medium.onnx`.
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

        // Write the text; if that fails, reap the orphaned child before
        // returning — otherwise a stuck Piper would leave zombies.
        if let Some(mut stdin) = child.stdin.take() {
            if let Err(e) = stdin.write_all(text.as_bytes()) {
                let _ = child.kill();
                let _ = child.wait();
                return Err(anyhow::anyhow!("Failed to write text to Piper stdin: {e}"));
            }
            // Closing stdin signals end-of-input to Piper.
            drop(stdin);
        }

        let output = wait_child_with_timeout(&mut child, PIPER_SYNTH_TIMEOUT, "Piper")
            .inspect_err(|_| {
                let _ = child.kill();
                let _ = child.wait();
            })?;
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

/// Poll the child until it exits or `timeout` elapses, then return its full
/// stdout/stderr the same shape `wait_with_output` would have. On timeout
/// the child is killed and `Err` is returned so the caller can propagate
/// an "engine stuck" diagnostic without sacrificing the TTS worker thread.
///
/// `label` names the engine for diagnostics (e.g. `"Piper"`, `"Kokoros"`).
fn wait_child_with_timeout(
    child: &mut std::process::Child,
    timeout: Duration,
    label: &str,
) -> Result<std::process::Output> {
    let deadline = Instant::now() + timeout;
    let mut stdout = child.stdout.take();
    let mut stderr = child.stderr.take();
    let mut stdout_buf = Vec::new();
    let mut stderr_buf = Vec::new();

    loop {
        match child
            .try_wait()
            .with_context(|| format!("Failed to poll {label} process"))?
        {
            Some(status) => {
                // Child exited; drain any remaining pipe data.
                if let Some(mut s) = stdout.take() {
                    let _ = s.read_to_end(&mut stdout_buf);
                }
                if let Some(mut s) = stderr.take() {
                    let _ = s.read_to_end(&mut stderr_buf);
                }
                return Ok(std::process::Output {
                    status,
                    stdout: stdout_buf,
                    stderr: stderr_buf,
                });
            }
            None => {
                if Instant::now() >= deadline {
                    let _ = child.kill();
                    let _ = child.wait();
                    anyhow::bail!(
                        "{label} synthesis exceeded {} s timeout — killed",
                        timeout.as_secs()
                    );
                }
                std::thread::sleep(PIPER_POLL_INTERVAL);
            }
        }
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

/// Kokoros local TTS backend — [Kokoro-82M](https://github.com/lucasjinreal/Kokoros)
/// invoked as a subprocess via the `koko` CLI.
///
/// Mirrors [`PiperEngine`]: each `synthesize` call spawns `koko stream`, feeds
/// one utterance as a single stdin line, and reads 32-bit IEEE-float WAV
/// (24 kHz, mono) from stdout. Running Kokoros as a subprocess keeps its ONNX
/// Runtime dependency out of Continuum's Rust dependency graph — the same
/// reason Piper is a subprocess (see the module docs and the `ort` conflict
/// note). The `koko` binary must be installed separately (see
/// `scripts/download-models.ps1`); it is resolved via `resolve_koko_binary`.
///
/// Kokoros is the intended TTS for orchestrator/reasoning turns (see the
/// realtime voice plan): those turns are already multi-second, so the
/// per-call model-load cost (~150–300 ms) is acceptable. A long-running
/// `koko openai` server mode is a future optimisation if Kokoros is ever
/// needed on the sub-second conversational path.
pub struct KokorosEngine {
    model_path: PathBuf,
    voices_path: PathBuf,
    voice_name: String,
    speed: f32,
    sample_rate: u32,
    koko_bin: PathBuf,
}

impl KokorosEngine {
    /// Load a Kokoros voice.
    ///
    /// * `model_path` — `kokoro-v1.0.onnx`.
    /// * `voices_path` — `voices-v1.0.bin` (the bundled voice-style catalog).
    /// * `voice_name` — Kokoros style spec, e.g. `af_sky` or a blend like
    ///   `af_sarah.4+af_nicole.6`. Passed to `koko --style`.
    /// * `speed` — speech-rate multiplier (1.0 = native).
    ///
    /// The sample rate is fixed at 24 kHz for Kokoro-82M; the playback
    /// resampler is sized from this value.
    pub fn new(
        model_path: &Path,
        voices_path: &Path,
        voice_name: impl Into<String>,
        speed: f32,
    ) -> Result<Self> {
        let koko_bin = resolve_koko_binary();
        let sample_rate = 24_000;
        let voice_name = voice_name.into();

        tracing::info!(
            layer = "voice",
            component = "tts",
            model = %model_path.display(),
            voices = %voices_path.display(),
            voice = %voice_name,
            speed,
            sample_rate,
            "Kokoros engine loaded"
        );

        Ok(Self {
            model_path: model_path.to_path_buf(),
            voices_path: voices_path.to_path_buf(),
            voice_name,
            speed,
            sample_rate,
            koko_bin,
        })
    }
}

impl TtsEngine for KokorosEngine {
    fn synthesize(&self, text: &str) -> Result<SynthesizedAudio> {
        let start = std::time::Instant::now();
        // `koko stream` reads stdin line-by-line; collapse internal newlines so
        // a multi-line utterance is treated as one synthesis unit.
        let sanitized = text.replace(['\n', '\r'], " ");

        let mut cmd = Command::new(&self.koko_bin);
        cmd.arg("stream")
            .arg("--model")
            .arg(&self.model_path)
            .arg("--data")
            .arg(&self.voices_path)
            .arg("--style")
            .arg(&self.voice_name)
            .arg("--speed")
            .arg(self.speed.to_string())
            .arg("--mono")
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped());

        let mut child = cmd.spawn().with_context(|| {
            format!(
                "Failed to spawn Kokoros binary '{}'",
                self.koko_bin.display()
            )
        })?;

        // Feed the utterance as a single stdin line, then close stdin to
        // signal EOF so `koko stream` flushes and exits.
        if let Some(mut stdin) = child.stdin.take() {
            if let Err(e) = stdin.write_all(sanitized.as_bytes()) {
                let _ = child.kill();
                let _ = child.wait();
                return Err(anyhow::anyhow!(
                    "Failed to write text to Kokoros stdin: {e}"
                ));
            }
            if let Err(e) = stdin.write_all(b"\n") {
                let _ = child.kill();
                let _ = child.wait();
                return Err(anyhow::anyhow!(
                    "Failed to write newline to Kokoros stdin: {e}"
                ));
            }
            drop(stdin);
        }

        let output = wait_child_with_timeout(&mut child, KOKOROS_SYNTH_TIMEOUT, "Kokoros")
            .inspect_err(|_| {
                let _ = child.kill();
                let _ = child.wait();
            })?;
        if !output.status.success() {
            let stderr = String::from_utf8_lossy(&output.stderr);
            anyhow::bail!("Kokoros synthesis failed: {stderr}");
        }

        let samples =
            parse_kokoros_wav(&output.stdout).context("Failed to parse Kokoros WAV output")?;
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
            "Synthesized utterance (Kokoros)"
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
        "kokoros"
    }
}

/// Parse the 32-bit IEEE-float WAV (RIFF/WAVE, format code 3) that `koko
/// stream` writes to stdout. Returns the audio body as mono f32 samples.
///
/// Only the structural container is parsed — we scan for the `data` chunk and
/// copy its bytes as f32. The `fmt` chunk is inspected only to assert the
/// format is float (code 3) and to record the channel count so a stereo body
/// can be de-interleaved to channel 0. Kokoros with `--mono` emits one
/// channel, so the stereo path is a defensive fallback.
fn parse_kokoros_wav(bytes: &[u8]) -> Result<Vec<f32>> {
    if bytes.len() < 12 {
        anyhow::bail!("Kokoros WAV output too short ({} bytes)", bytes.len());
    }
    if &bytes[0..4] != b"RIFF" || &bytes[8..12] != b"WAVE" {
        anyhow::bail!("Kokoros stdout is not a RIFF/WAVE container");
    }

    let mut pos = 12;
    let mut channels: u16 = 1;
    let mut fmt_seen = false;
    let mut data: Option<&[u8]> = None;

    while pos + 8 <= bytes.len() {
        let chunk_id = &bytes[pos..pos + 4];
        let chunk_len = u32::from_le_bytes([
            bytes[pos + 4],
            bytes[pos + 5],
            bytes[pos + 6],
            bytes[pos + 7],
        ]) as usize;
        let body_start = pos + 8;
        let body_end = body_start + chunk_len;
        if body_end > bytes.len() {
            // Truncated chunk — take what remains as the data body.
            if chunk_id == b"data" {
                data = Some(&bytes[body_start..]);
            }
            break;
        }

        if chunk_id == b"fmt " {
            if chunk_len >= 4 {
                let format_code = u16::from_le_bytes([bytes[body_start], bytes[body_start + 1]]);
                if format_code != 3 {
                    anyhow::bail!("Kokoros WAV fmt code is {format_code}, expected 3 (IEEE float)");
                }
            }
            if chunk_len >= 6 {
                channels = u16::from_le_bytes([bytes[body_start + 2], bytes[body_start + 3]]);
            }
            fmt_seen = true;
        } else if chunk_id == b"data" {
            data = Some(&bytes[body_start..body_end]);
        }

        // Chunks are word-aligned; skip the pad byte if the length is odd.
        let next = body_end + (chunk_len & 1);
        pos = next;
    }

    let data = data.context("Kokoros WAV has no data chunk")?;
    if !fmt_seen {
        tracing::warn!(
            layer = "voice",
            component = "tts",
            "Kokoros WAV had no fmt chunk; assuming mono float32"
        );
    }

    // 32-bit IEEE float samples.
    let floats: Vec<f32> = data
        .chunks_exact(4)
        .map(|c| f32::from_le_bytes([c[0], c[1], c[2], c[3]]))
        .collect();

    if channels > 1 {
        // De-interleave to channel 0.
        Ok(floats.into_iter().step_by(channels as usize).collect())
    } else {
        Ok(floats)
    }
}

/// Resolve the Kokoros (`koko`) binary path.
///
/// Resolution order:
/// 1. `CONTINUUM_KOKO_BIN` environment variable (explicit override)
/// 2. `~/.continuum-dev/bin/kokoros/koko.exe` on Windows (installed by
///    `download-models.ps1`)
/// 3. `~/.continuum-dev/bin/kokoros/koko` on Unix (same install tree)
/// 4. Fallback `"koko"` — rely on PATH lookup.
fn resolve_koko_binary() -> PathBuf {
    if let Some(override_bin) = env_or_legacy("CONTINUUM_KOKO_BIN", "KAIRO_KOKO_BIN") {
        return PathBuf::from(override_bin);
    }

    let dev = continuum_dev_dir();
    let bundled = if cfg!(windows) {
        dev.join("bin").join("kokoros").join("koko.exe")
    } else {
        dev.join("bin").join("kokoros").join("koko")
    };
    if bundled.exists() {
        return bundled;
    }

    PathBuf::from("koko")
}

/// Resolve the Piper binary path.
///
/// Resolution order:
/// 1. `CONTINUUM_PIPER_BIN` environment variable (explicit override)
/// 2. `~/.continuum-dev/bin/piper/piper.exe` on Windows (installed by `download-models.ps1`)
/// 3. `~/.continuum-dev/bin/piper/piper` on Unix (same install tree)
/// 4. Fallback `"piper"` — rely on PATH lookup.
///
/// The fallbacks let `voice_test` / `voice_demo` run out of the box once the
/// download script has populated the dev tree, without requiring the user to
/// manually set PATH or the env var.
fn resolve_piper_binary() -> PathBuf {
    if let Some(override_bin) = env_or_legacy("CONTINUUM_PIPER_BIN", "KAIRO_PIPER_BIN") {
        return PathBuf::from(override_bin);
    }

    let dev = continuum_dev_dir();
    let bundled = if cfg!(windows) {
        dev.join("bin").join("piper").join("piper.exe")
    } else {
        dev.join("bin").join("piper").join("piper")
    };
    if bundled.exists() {
        return bundled;
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

    /// Build a minimal 32-bit IEEE-float WAV (format code 3) in memory for
    /// parser tests. `channels` controls the interleaving.
    fn build_float_wav(samples: &[f32], channels: u16, sample_rate: u32) -> Vec<u8> {
        let data_bytes: Vec<u8> = samples.iter().flat_map(|s| s.to_le_bytes()).collect();
        let data_len = data_bytes.len() as u32;

        let mut fmt_body: Vec<u8> = Vec::new();
        fmt_body.extend_from_slice(&3u16.to_le_bytes()); // format code 3 = IEEE float
        fmt_body.extend_from_slice(&channels.to_le_bytes());
        fmt_body.extend_from_slice(&sample_rate.to_le_bytes());
        let byte_rate = sample_rate * channels as u32 * 4;
        fmt_body.extend_from_slice(&byte_rate.to_le_bytes());
        let block_align = (channels as u32 * 4) as u16;
        fmt_body.extend_from_slice(&block_align.to_le_bytes());
        fmt_body.extend_from_slice(&32u16.to_le_bytes()); // bits per sample

        let fmt_len = fmt_body.len() as u32;
        let mut out = Vec::new();
        out.extend_from_slice(b"RIFF");
        out.extend_from_slice(&(36 + data_len).to_le_bytes());
        out.extend_from_slice(b"WAVE");
        out.extend_from_slice(b"fmt ");
        out.extend_from_slice(&fmt_len.to_le_bytes());
        out.extend_from_slice(&fmt_body);
        out.extend_from_slice(b"data");
        out.extend_from_slice(&data_len.to_le_bytes());
        out.extend_from_slice(&data_bytes);
        out
    }

    #[test]
    fn test_parse_kokoros_wav_mono_float() {
        let samples = vec![0.0_f32, 0.5, -0.5, 1.0];
        let wav = build_float_wav(&samples, 1, 24_000);
        let got = parse_kokoros_wav(&wav).unwrap();
        assert_eq!(got.len(), 4);
        assert_eq!(got[0], 0.0);
        assert!((got[1] - 0.5).abs() < 1e-5);
        assert!((got[2] - (-0.5)).abs() < 1e-5);
        assert!((got[3] - 1.0).abs() < 1e-5);
    }

    #[test]
    fn test_parse_kokoros_wav_stereo_deinterleaves() {
        // Interleaved stereo: [L0, R0, L1, R1]
        let samples = vec![0.1_f32, 0.9, 0.2, 0.8];
        let wav = build_float_wav(&samples, 2, 24_000);
        let got = parse_kokoros_wav(&wav).unwrap();
        // Channel 0 only.
        assert_eq!(got.len(), 2);
        assert!((got[0] - 0.1).abs() < 1e-5);
        assert!((got[1] - 0.2).abs() < 1e-5);
    }

    #[test]
    fn test_parse_kokoros_wav_rejects_non_wav() {
        assert!(parse_kokoros_wav(b"not a wav at all").is_err());
        assert!(parse_kokoros_wav(b"RIFF\x00\x00\x00\x00XXXX").is_err());
    }

    #[test]
    fn test_parse_kokoros_wav_rejects_pcm_format() {
        // Format code 1 (PCM) instead of 3 (float) — should be rejected.
        let mut wav = build_float_wav(&[0.0_f32, 0.5], 1, 24_000);
        // Patch the format code at the start of the fmt body (offset 20).
        wav[20] = 1;
        wav[21] = 0;
        assert!(parse_kokoros_wav(&wav).is_err());
    }

    #[test]
    fn kokoros_engine_metadata() {
        // Construction does not require the binary to exist; only synthesis
        // spawns it. We exercise the metadata accessors here.
        let engine = KokorosEngine::new(
            Path::new("/nonexistent/kokoro-v1.0.onnx"),
            Path::new("/nonexistent/voices-v1.0.bin"),
            "af_sky",
            1.0,
        )
        .unwrap();
        assert_eq!(engine.name(), "kokoros");
        assert_eq!(engine.sample_rate(), 24_000);
    }

    #[test]
    fn resolve_piper_binary_uses_env_override() {
        let sentinel = "continuum-test-piper-override";
        std::env::set_var("CONTINUUM_PIPER_BIN", sentinel);
        let got = resolve_piper_binary();
        assert_eq!(got, std::path::PathBuf::from(sentinel));
        std::env::remove_var("CONTINUUM_PIPER_BIN");
    }
}

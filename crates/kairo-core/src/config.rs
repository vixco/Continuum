//! # Configuration
//!
//! Loads and manages Kairo's runtime configuration. Every model, interval,
//! threshold, and prompt is readable from config and overridable via the
//! dashboard.
//!
//! Configuration is stored at `~/.kairo-dev/config.toml` with defaults loaded
//! from the bundled `config/` directory in the repository.

use anyhow::{Context, Result};
use serde::{Deserialize, Serialize};
use std::path::{Path, PathBuf};

/// Root configuration for the Kairo runtime.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(default)]
pub struct KairoConfig {
    /// Vision model configuration.
    pub vision: VisionConfig,
    /// Screen capture configuration.
    pub screen: ScreenConfig,
    /// Audio pipeline configuration.
    pub audio: AudioConfig,
    /// Context poller configuration.
    pub context: ContextConfig,
    /// Perception frame builder configuration.
    pub frame: FrameConfig,
    /// Raw log storage configuration.
    pub storage: StorageConfig,
    /// Memory distillation configuration.
    pub memory: MemoryConfig,
    /// Voice input and routing configuration.
    pub voice: VoiceConfig,
    /// Text-to-speech configuration (Phase 5).
    pub tts: TtsConfig,
    /// Worker pool configuration (Phase 8).
    pub workers: WorkersConfig,
    /// Skills system configuration (Phase 8).
    pub skills: SkillsConfig,
    /// Orchestrator (Claude Opus) subprocess configuration.
    pub orchestrator: OrchestratorSection,
    /// Triage (local LLM) runtime configuration.
    pub triage: TriageSection,
}

/// Configuration for the orchestrator (Claude Opus via CLI subprocess).
///
/// Every field is user-overridable via `config.toml` — per non-negotiable #3,
/// there are no hardcoded model IDs or timeouts anywhere else in the runtime.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(default)]
pub struct OrchestratorSection {
    /// Model ID passed to `claude --model`. Must be a currently-supported
    /// Claude Code model (e.g. `claude-opus-4-6`, `claude-sonnet-4-6`).
    pub model_id: String,
    /// Wall-clock timeout for a single wake cycle, in seconds. If the
    /// orchestrator doesn't emit a `result` event within this window the
    /// child process is killed and the wake is marked failed.
    pub wake_timeout_secs: u64,
    /// If true, pass `--bare` to Claude Code (skip hooks / plugins).
    /// Defaults to `false` so the user's normal Claude Code configuration
    /// applies; set to `true` for deterministic, reproducible wakes.
    pub bare_mode: bool,
}

/// Runtime knobs for the local triage LLM.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(default)]
pub struct TriageSection {
    /// Path to the `.gguf` triage model. Empty string means "use default
    /// location" (`<dev_dir>/models/triage/<default_file>`).
    pub model_path: String,
    /// llama.cpp context size in tokens. 2048 is generous for frame
    /// descriptions; keep low to minimise KV cache pressure.
    pub context_size: u32,
    /// Maximum tokens per triage response.
    pub max_tokens: u32,
    /// Sampling temperature. 0.0 is deterministic and fine for triage —
    /// we want a stable yes/no on each frame.
    pub temperature: f32,
    /// Layers to offload to GPU. `999` means "all available"; 0 is CPU-only.
    pub gpu_layers: u32,
    /// Log a warning when a triage decision exceeds this latency, in ms.
    pub latency_warn_ms: u64,
}

/// Configuration for the local vision model.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(default)]
pub struct VisionConfig {
    /// Name of the vision model (for display).
    pub name: String,
    /// Path to the ONNX model file.
    pub model_path: String,
    /// Whether GPU acceleration is enabled (Phase 1: always false).
    pub gpu_enabled: bool,
    /// Input image width for the model.
    pub input_width: u32,
    /// Input image height for the model.
    pub input_height: u32,
}

/// Configuration for screen capture.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(default)]
pub struct ScreenConfig {
    /// Interval between captures in seconds (1-10).
    pub interval_secs: u64,
    /// Width to downscale captured images to.
    pub capture_width: u32,
    /// Height to downscale captured images to.
    pub capture_height: u32,
    /// Whether to save screenshots to disk.
    pub save_screenshots: bool,
}

/// Configuration for the audio pipeline.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(default)]
pub struct AudioConfig {
    /// Whether audio capture is enabled.
    pub enabled: bool,
    /// Path to the whisper model file.
    pub whisper_model_path: String,
    /// Whisper language code (e.g. "nl", "en", "de"). Forcing a language
    /// avoids the auto-detector's low-confidence failures on short clips,
    /// where whisper-small tends to emit `[BLANK_AUDIO]` rather than risk a
    /// wrong transcript. Set to "auto" to let whisper guess per segment.
    pub whisper_language: String,
    /// Floor for the adaptive VAD threshold. The actual threshold used is
    /// `max(vad_threshold, noise_floor × vad_noise_floor_multiplier)`, so
    /// this value serves as an absolute minimum in very quiet environments.
    /// Default 0.005 catches genuine speech without tripping on a typical
    /// noise floor of 0.0001–0.001.
    pub vad_threshold: f32,
    /// Multiplier applied to the rolling noise floor when computing the
    /// effective VAD threshold. Default 5.0 — speech must be at least 5×
    /// the ambient level to trigger. Raise if your room is noisy, lower if
    /// you speak quietly.
    pub vad_noise_floor_multiplier: f32,
    /// Silence duration in ms before a speech segment ends.
    pub silence_duration_ms: u64,
    /// Maximum speech segment length in seconds before forced split.
    pub max_segment_secs: u64,
    /// Display name of the chosen input device. Saved by the interactive
    /// picker on first run. Paired with `device_index` — both are checked on
    /// startup; if the name at that index no longer matches, the picker is
    /// re-invoked.
    pub device_name: String,
    /// cpal enumeration index of the chosen input device. `None` means
    /// "not yet picked, run the interactive picker on startup".
    pub device_index: Option<usize>,
}

/// Configuration for the context poller.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(default)]
pub struct ContextConfig {
    /// Polling interval in seconds.
    pub poll_interval_secs: u64,
}

/// Configuration for the perception frame builder.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(default)]
pub struct FrameConfig {
    /// Interval between frames in seconds (2-10).
    pub interval_secs: u64,
    /// Minimum salience score for a frame to reach triage (0.0-1.0).
    pub salience_threshold: f32,
}

/// Configuration for raw log storage.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(default)]
pub struct StorageConfig {
    /// Path to the SQLite database file.
    pub db_path: String,
    /// Directory for screenshot JPEG files.
    pub screenshots_dir: String,
    /// Number of days to retain frames before rotation.
    pub retention_days: u32,
}

/// Configuration for background memory distillation.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(default)]
pub struct MemoryConfig {
    /// Whether raw-log to episodic distillation is enabled.
    pub distillation_enabled: bool,
    /// Interval between distillation passes in minutes.
    pub distillation_interval_minutes: u64,
    /// How far back each pass looks for undistilled frames.
    pub distillation_lookback_minutes: u64,
    /// Minimum salience for a frame to be distilled without audio/error signals.
    pub distillation_min_salience: f32,
    /// Maximum frames to distill in one pass.
    pub distillation_batch_size: usize,
}

/// Configuration for the voice input loop.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(default)]
pub struct VoiceConfig {
    /// Whether voice input routing is enabled.
    pub enabled: bool,
    /// Whether a wake phrase is required before voice commands wake the orchestrator.
    pub wake_word_enabled: bool,
    /// Wake phrase to detect in local STT transcripts.
    pub wake_keyword: String,
    /// Porcupine sensitivity placeholder, kept configurable for the native detector path.
    pub wake_sensitivity: f32,
    /// Optional Porcupine .ppn path. Empty means transcript wake detection is used.
    pub custom_keyword_path: String,
    /// Maximum time to keep a voice session open after wake.
    pub listen_timeout_ms: u64,
    /// Silence/endpointer timeout after the last transcript update.
    pub endpoint_silence_ms: u64,
    /// Minimum non-wake transcript length before a command is considered complete.
    pub min_utterance_chars: usize,
    /// Stop playback when fresh user speech arrives while Kairo is speaking.
    pub barge_in_enabled: bool,
    /// Suppress spoken output while call detection reports a live call.
    pub ambient_mute_enabled: bool,
    /// Route TTS voice by detected language when a matching voice is configured.
    pub language_detection_enabled: bool,
    /// Language used when STT language is unknown or unsupported.
    pub default_language: String,
    /// Master playback gain [0.0, 1.0]. Applied in the cpal fill callback so
    /// the config change takes effect on the next audio buffer.
    pub volume: f32,
    /// Play short audio cues (chime on wake, click on active-listen, double-beep
    /// on error) when the voice state transitions. Disable for pure silence.
    pub feedback_sounds: bool,
    /// Global hotkey for toggle-listen (empty string disables the hotkey).
    /// Modifier chord in the form "Ctrl+Shift+K" / "Alt+F12" / "Win+Space".
    pub hotkey: String,
    /// After Kairo finishes speaking, keep the voice session alive this many
    /// seconds so the user can ask a follow-up without re-triggering the wake
    /// word. `0` disables conversation mode.
    pub conversation_followup_seconds: u64,
}

/// Configuration for the text-to-speech pipeline.
///
/// Kairo supports multiple Piper voices — typically one per language.
/// The language-routing logic picks a voice based on the detected speech
/// language. ElevenLabs is an optional cloud plugin, disabled by default
/// per the ROADMAP's local-first stance.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(default)]
pub struct TtsConfig {
    /// Whether TTS is enabled at all. When `false`, whisper triage
    /// decisions and orchestrator responses are logged but not spoken.
    pub enabled: bool,
    /// Which TTS engine to use. `"piper"` (default, local) or `"elevenlabs"`
    /// (cloud plugin, requires API key).
    pub engine: String,
    /// Directory holding the espeak-ng dictionary files required by
    /// Piper's phonemizer. Must exist at runtime.
    pub espeak_data_dir: String,
    /// Per-language voice catalog. Keys are BCP-47 short codes (`en`,
    /// `nl`). The `primary` field selects which one is used when language
    /// detection is unavailable or disabled.
    pub voices: std::collections::HashMap<String, TtsVoiceConfig>,
    /// BCP-47 language code of the default voice (must appear in `voices`).
    pub primary: String,
    /// Piper `length_scale` parameter; `None` uses the voice's native
    /// value. Values below 1.0 speed up speech, above 1.0 slow it down.
    pub length_scale: Option<f32>,
    /// ElevenLabs streaming cloud TTS (optional plugin backend).
    pub elevenlabs: ElevenLabsConfig,
}

/// A single Piper voice entry.
#[derive(Debug, Default, Clone, Serialize, Deserialize)]
#[serde(default)]
pub struct TtsVoiceConfig {
    /// Absolute path to the `.onnx` model file.
    pub model_path: String,
    /// Absolute path to the `.onnx.json` config sidecar.
    pub config_path: String,
    /// Optional speaker id for multi-speaker models. `None` for
    /// single-speaker voices like `en_US-lessac-medium`.
    pub speaker_id: Option<i64>,
}

/// ElevenLabs streaming TTS configuration (optional cloud plugin).
///
/// Phase 5 is local-first: Piper is the only supported production backend.
/// This struct defines the config surface so the cloud plugin can be wired
/// up in a later minor release without breaking user configs in the
/// interim.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(default)]
pub struct ElevenLabsConfig {
    /// User-provided ElevenLabs API key. Empty string means the backend
    /// is disabled regardless of `tts.engine`.
    pub api_key: String,
    /// ElevenLabs voice ID (see https://elevenlabs.io/app/voice-library).
    pub voice_id: String,
    /// Model ID — `eleven_turbo_v2_5` (fastest) is the default target.
    pub model_id: String,
    /// Voice stability [0.0, 1.0]. Higher = more predictable prosody.
    pub stability: f32,
    /// Similarity boost [0.0, 1.0]. Higher = closer to reference voice.
    pub similarity_boost: f32,
}

impl Default for ElevenLabsConfig {
    fn default() -> Self {
        Self {
            api_key: String::new(),
            voice_id: String::new(),
            model_id: "eleven_turbo_v2_5".to_string(),
            stability: 0.5,
            similarity_boost: 0.75,
        }
    }
}

/// Configuration for the worker pool (Phase 8).
///
/// Workers are independent Claude Code subprocesses spawned by the orchestrator
/// to do actual work. The pool enforces concurrency limits, selects models, and
/// tracks lifecycle.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(default)]
pub struct WorkersConfig {
    /// Global mode. `"auto"` lets the orchestrator's explicit choice win, and
    /// falls back to a keyword heuristic. `"budget"` forces Sonnet for every
    /// worker. `"power"` forces Opus.
    pub mode: String,
    /// Model id used when mode is `"budget"` or the heuristic picks Sonnet.
    pub budget_model: String,
    /// Model id used when mode is `"power"` or the heuristic picks Opus.
    pub power_model: String,
    /// Maximum workers running at once. Excess requests queue. Hard cap: 10.
    pub max_concurrent: usize,
    /// Default wall-clock timeout per worker (seconds).
    pub default_timeout_secs: u64,
    /// Default CSV of tool names the worker may use. `mcp__kairo__*` by default
    /// plus the standard Claude Code built-ins. Workers never get
    /// `mcp__kairo__workers__*` — that would allow spawning sub-workers
    /// directly from a worker, which we route through the orchestrator instead.
    pub default_allowed_tools: String,
    /// How often the dashboard and MCP server see worker state updates, in ms.
    pub status_refresh_ms: u64,
    /// If a single task pattern fails this many times within `failure_window_secs`,
    /// the pool refuses to run it again until restart and surfaces an escalation.
    pub failure_streak_limit: u32,
    /// Window over which `failure_streak_limit` is counted.
    pub failure_window_secs: u64,
}

/// Configuration for the skills system (Phase 8).
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(default)]
pub struct SkillsConfig {
    /// Whether the skills system is active at all.
    pub enabled: bool,
    /// Directory holding `<name>/SKILL.md` files. Relative paths resolve
    /// relative to the current working directory at load time.
    pub dir: String,
    /// Reload SKILL.md files when they change on disk.
    pub hot_reload: bool,
    /// Approximate token budget for injected skill content per wake. Matching
    /// skills are appended until the budget is reached, in match-score order.
    pub token_budget: usize,
    /// Skills explicitly disabled by name. Third-party or noisy skills can be
    /// silenced without deleting the directory.
    pub disabled: Vec<String>,
}

impl Default for WorkersConfig {
    fn default() -> Self {
        Self {
            mode: "auto".to_string(),
            budget_model: "claude-sonnet-4-6".to_string(),
            power_model: "claude-opus-4-6".to_string(),
            max_concurrent: 3,
            default_timeout_secs: 1800,
            default_allowed_tools:
                "Read,Write,Edit,Glob,Grep,Bash,mcp__kairo__memory_*,mcp__kairo__system_*,\
                 mcp__kairo__fs_*,mcp__kairo__web_fetch"
                    .to_string(),
            status_refresh_ms: 500,
            failure_streak_limit: 3,
            failure_window_secs: 600,
        }
    }
}

impl Default for SkillsConfig {
    fn default() -> Self {
        Self {
            enabled: true,
            dir: "skills".to_string(),
            hot_reload: true,
            token_budget: 2000,
            disabled: Vec::new(),
        }
    }
}

// --- Defaults ---

impl Default for KairoConfig {
    fn default() -> Self {
        let base = kairo_dev_dir();
        Self {
            vision: VisionConfig::default(),
            screen: ScreenConfig::default(),
            audio: AudioConfig::default(),
            context: ContextConfig::default(),
            frame: FrameConfig::default(),
            storage: StorageConfig {
                db_path: base.join("raw_log.sqlite").to_string_lossy().into_owned(),
                screenshots_dir: base.join("screenshots").to_string_lossy().into_owned(),
                retention_days: 30,
            },
            memory: MemoryConfig::default(),
            voice: VoiceConfig::default(),
            tts: TtsConfig::default(),
            workers: WorkersConfig::default(),
            skills: SkillsConfig::default(),
            orchestrator: OrchestratorSection::default(),
            triage: TriageSection::default(),
        }
    }
}

impl Default for OrchestratorSection {
    fn default() -> Self {
        Self {
            model_id: "claude-opus-4-6".to_string(),
            wake_timeout_secs: 60,
            bare_mode: false,
        }
    }
}

impl Default for TriageSection {
    fn default() -> Self {
        Self {
            // Empty means "derive from dev_dir" at load time.
            model_path: String::new(),
            context_size: 2048,
            max_tokens: 256,
            temperature: 0.0,
            gpu_layers: 999,
            latency_warn_ms: 2000,
        }
    }
}

impl TriageSection {
    /// Resolve the effective `.gguf` path, filling in the default under
    /// `dev_dir/models/triage/qwen3-8b-q4_k_m.gguf` when the config value
    /// is empty. Callers pass the current `dev_dir`; this avoids baking a
    /// user-specific path into the serialised defaults.
    pub fn resolve_model_path(&self, dev_dir: &Path) -> PathBuf {
        if !self.model_path.is_empty() {
            return PathBuf::from(&self.model_path);
        }
        dev_dir
            .join("models")
            .join("triage")
            .join("qwen3-8b-q4_k_m.gguf")
    }
}

impl Default for VisionConfig {
    fn default() -> Self {
        let models_dir = kairo_dev_dir().join("models").join("vision");
        Self {
            name: "SmolVLM-256M".to_string(),
            model_path: models_dir
                .join("smolvlm-256m")
                .to_string_lossy()
                .into_owned(),
            gpu_enabled: false,
            input_width: 384,
            input_height: 384,
        }
    }
}

impl Default for ScreenConfig {
    fn default() -> Self {
        Self {
            interval_secs: 3,
            capture_width: 1280,
            capture_height: 720,
            save_screenshots: true,
        }
    }
}

impl Default for AudioConfig {
    fn default() -> Self {
        let models_dir = kairo_dev_dir().join("models").join("stt");
        Self {
            enabled: true,
            // whisper-medium (1.5 GB) recognises proper nouns like "Kairo"
            // reliably where whisper-small (244 MB) hallucinates real-word
            // mistranscriptions. The 3x size cost is paid for by the wake
            // gate actually working. Swap to whisper-small.bin if memory
            // is constrained or you don't mind more mis-detections.
            whisper_model_path: models_dir
                .join("whisper-medium.bin")
                .to_string_lossy()
                .into_owned(),
            // Force English for the default wake path. Whisper-small's
            // language auto-detect on short 1-2 second clips is unreliable
            // (we've seen "hey kairo" mis-detected as Portuguese "Ei,
            // Cairo!" with p=0.55). Forcing "en" keeps the wake word
            // intelligible. Users who primarily speak a different language
            // override this in ~/.kairo-dev/config.toml.
            whisper_language: "en".to_string(),
            // Adaptive VAD: floor 0.005 catches quiet speech; the 5×
            // noise-floor multiplier raises the effective threshold on
            // noisy setups automatically. See `AdaptiveVad` for details.
            vad_threshold: 0.005,
            vad_noise_floor_multiplier: 5.0,
            // 800 ms of trailing silence before a segment closes. Natural
            // mid-sentence pauses (commas, thinking) stay inside one segment
            // so Whisper sees the whole utterance.
            silence_duration_ms: 800,
            max_segment_secs: 8,
            device_name: String::new(),
            device_index: None,
        }
    }
}

impl Default for ContextConfig {
    fn default() -> Self {
        Self {
            poll_interval_secs: 1,
        }
    }
}

impl Default for FrameConfig {
    fn default() -> Self {
        Self {
            interval_secs: 3,
            // Lowered from 0.15 to 0.10: most frames score 0.00 in steady state,
            // and the triage layer (Phase 2) is cheap enough to run on
            // window-change events (salience ~0.20). Tune up if triage call
            // volume is too high.
            salience_threshold: 0.10,
        }
    }
}

impl Default for StorageConfig {
    fn default() -> Self {
        let base = kairo_dev_dir();
        Self {
            db_path: base.join("raw_log.sqlite").to_string_lossy().into_owned(),
            screenshots_dir: base.join("screenshots").to_string_lossy().into_owned(),
            retention_days: 30,
        }
    }
}

impl Default for MemoryConfig {
    fn default() -> Self {
        Self {
            distillation_enabled: true,
            distillation_interval_minutes: 15,
            distillation_lookback_minutes: 20,
            distillation_min_salience: 0.35,
            distillation_batch_size: 100,
        }
    }
}

impl Default for VoiceConfig {
    fn default() -> Self {
        Self {
            enabled: true,
            wake_word_enabled: true,
            wake_keyword: "hey kairo".to_string(),
            wake_sensitivity: 0.5,
            custom_keyword_path: String::new(),
            listen_timeout_ms: 12_000,
            endpoint_silence_ms: 700,
            min_utterance_chars: 3,
            barge_in_enabled: true,
            ambient_mute_enabled: true,
            // Disabled by default: Kairo speaks English only. Whisper still
            // transcribes any spoken language (audio.whisper_language =
            // "auto"), so Kairo understands multilingual input but routes
            // all TTS through the English primary voice. Enable when every
            // target language has a quality Piper voice configured.
            language_detection_enabled: false,
            default_language: "en".to_string(),
            volume: 0.8,
            feedback_sounds: true,
            hotkey: "Ctrl+Shift+K".to_string(),
            conversation_followup_seconds: 5,
        }
    }
}

impl Default for TtsConfig {
    fn default() -> Self {
        let tts_dir = kairo_dev_dir().join("models").join("tts");
        // English-only by default. The Dutch Piper voices available as of
        // 2026-04 (nl_NL-mls-medium) produce barely-intelligible speech, so
        // we don't ship a second voice in the default bank. Users who want
        // multilingual TTS add a [tts.voices.<lang>] section and flip
        // voice.language_detection_enabled to true.
        let mut voices = std::collections::HashMap::new();
        voices.insert(
            "en".to_string(),
            TtsVoiceConfig {
                model_path: tts_dir
                    .join("en_US-lessac-medium.onnx")
                    .to_string_lossy()
                    .into_owned(),
                config_path: tts_dir
                    .join("en_US-lessac-medium.onnx.json")
                    .to_string_lossy()
                    .into_owned(),
                speaker_id: None,
            },
        );
        Self {
            enabled: true,
            engine: "piper".to_string(),
            espeak_data_dir: tts_dir
                .join("espeak-ng-data")
                .to_string_lossy()
                .into_owned(),
            voices,
            primary: "en".to_string(),
            length_scale: None,
            elevenlabs: ElevenLabsConfig::default(),
        }
    }
}

/// Returns the Kairo development directory (`~/.kairo-dev/`).
pub fn kairo_dev_dir() -> PathBuf {
    dirs::home_dir()
        .unwrap_or_else(|| PathBuf::from("."))
        .join(".kairo-dev")
}

/// Load configuration from a TOML file, falling back to defaults for missing keys.
pub fn load_config(path: &Path) -> Result<KairoConfig> {
    if path.exists() {
        let contents = std::fs::read_to_string(path).context("Failed to read config file")?;
        let config: KairoConfig =
            toml::from_str(&contents).context("Failed to parse config TOML")?;
        Ok(config)
    } else {
        tracing::info!(
            layer = "senses",
            component = "config",
            "No config file at {}, using defaults",
            path.display()
        );
        Ok(KairoConfig::default())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_default_config_is_valid() {
        let config = KairoConfig::default();
        assert_eq!(config.screen.interval_secs, 3);
        assert_eq!(config.frame.salience_threshold, 0.10);
        assert_eq!(config.storage.retention_days, 30);
        assert!(config.memory.distillation_enabled);
        assert_eq!(config.memory.distillation_interval_minutes, 15);
        assert!(config.voice.enabled);
        assert_eq!(config.voice.wake_keyword, "hey kairo");
        assert_eq!(config.audio.max_segment_secs, 8);
        assert!(!config.vision.gpu_enabled);
        assert!(config.tts.enabled);
        assert_eq!(config.tts.primary, "en");
        assert!(config.tts.voices.contains_key("en"));
        // Kairo is English-only by default; Dutch is opt-in (see
        // TtsConfig::default docs).
        assert!(!config.tts.voices.contains_key("nl"));
        assert!(!config.voice.language_detection_enabled);
        // Default is forced to "en" to make the wake word reliable on short
        // clips — see AudioConfig::default docs.
        assert_eq!(config.audio.whisper_language, "en");
    }

    #[test]
    fn test_tts_config_parses_from_toml() {
        let toml_str = r#"
[tts]
enabled = false
espeak_data_dir = "/tmp/espeak-ng-data"
primary = "nl"

[tts.voices.nl]
model_path = "/tmp/voice.onnx"
config_path = "/tmp/voice.onnx.json"
speaker_id = 2
"#;
        let config: KairoConfig = toml::from_str(toml_str).unwrap();
        assert!(!config.tts.enabled);
        assert_eq!(config.tts.primary, "nl");
        let nl = &config.tts.voices["nl"];
        assert_eq!(nl.model_path, "/tmp/voice.onnx");
        assert_eq!(nl.speaker_id, Some(2));
    }

    #[test]
    fn test_voice_config_parses_from_toml() {
        let toml_str = r#"
[voice]
enabled = true
wake_word_enabled = false
wake_keyword = "kairo"
listen_timeout_ms = 5000
barge_in_enabled = false
"#;
        let config: KairoConfig = toml::from_str(toml_str).unwrap();
        assert!(config.voice.enabled);
        assert!(!config.voice.wake_word_enabled);
        assert_eq!(config.voice.wake_keyword, "kairo");
        assert_eq!(config.voice.listen_timeout_ms, 5000);
        assert!(!config.voice.barge_in_enabled);
    }

    #[test]
    fn test_memory_config_parses_from_toml() {
        let toml_str = r#"
[memory]
distillation_enabled = false
distillation_interval_minutes = 5
distillation_min_salience = 0.25
distillation_batch_size = 12
"#;
        let config: KairoConfig = toml::from_str(toml_str).unwrap();
        assert!(!config.memory.distillation_enabled);
        assert_eq!(config.memory.distillation_interval_minutes, 5);
        assert_eq!(config.memory.distillation_min_salience, 0.25);
        assert_eq!(config.memory.distillation_batch_size, 12);
    }

    #[test]
    fn test_load_missing_config_returns_defaults() {
        let config = load_config(Path::new("/nonexistent/config.toml")).unwrap();
        assert_eq!(config.screen.interval_secs, 3);
    }

    #[test]
    fn test_partial_toml_fills_defaults() {
        let toml_str = r#"
[screen]
interval_secs = 5
"#;
        let config: KairoConfig = toml::from_str(toml_str).unwrap();
        assert_eq!(config.screen.interval_secs, 5);
        // Other fields should be defaults
        assert_eq!(config.frame.salience_threshold, 0.10);
    }
}

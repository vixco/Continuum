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
    /// VAD speech probability threshold (0.0-1.0).
    pub vad_threshold: f32,
    /// Silence duration in ms before a speech segment ends.
    pub silence_duration_ms: u64,
    /// Maximum speech segment length in seconds before forced split.
    pub max_segment_secs: u64,
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
        }
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
            whisper_model_path: models_dir
                .join("whisper-small.bin")
                .to_string_lossy()
                .into_owned(),
            vad_threshold: 0.5,
            silence_duration_ms: 500,
            max_segment_secs: 8,
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
            salience_threshold: 0.15,
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

/// Returns the Kairo development directory (`~/.kairo-dev/`).
pub fn kairo_dev_dir() -> PathBuf {
    dirs::home_dir()
        .unwrap_or_else(|| PathBuf::from("."))
        .join(".kairo-dev")
}

/// Load configuration from a TOML file, falling back to defaults for missing keys.
pub fn load_config(path: &Path) -> Result<KairoConfig> {
    if path.exists() {
        let contents =
            std::fs::read_to_string(path).context("Failed to read config file")?;
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
        assert_eq!(config.frame.salience_threshold, 0.15);
        assert_eq!(config.storage.retention_days, 30);
        assert_eq!(config.audio.max_segment_secs, 8);
        assert!(!config.vision.gpu_enabled);
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
        assert_eq!(config.frame.salience_threshold, 0.15);
    }
}

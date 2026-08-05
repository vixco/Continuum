//! # Voice health checks
//!
//! Each voice component exposes a coarse health status so the Phase 7
//! repair agent can decide whether to restart, reinstall, or rollback.
//! Health is intentionally conservative: a component is only "Unhealthy"
//! when we know it's broken. "Degraded" covers "partially working, look
//! here first" (e.g. no Dutch voice loaded — English still works).
//!
//! The data model matches what the `system_health` MCP tool in Phase 7
//! will surface, so the repair agent can reason about a voice outage
//! without a separate schema.

use std::path::Path;

/// Lifecycle state of a voice sub-component.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum HealthStatus {
    /// Component is running and last-known-good.
    Healthy,
    /// Component is functional but some non-critical feature is off. The
    /// report string describes the degradation.
    Degraded,
    /// Component is not functional. A restart or reinstall is warranted.
    Unhealthy,
    /// Component was never initialised (TTS disabled in config, no mic, etc.).
    Disabled,
}

impl HealthStatus {
    /// Returns `true` if this status warrants an automatic restart attempt.
    pub fn should_restart(self) -> bool {
        matches!(self, HealthStatus::Unhealthy)
    }
}

/// A snapshot of one voice sub-component's health.
#[derive(Debug, Clone)]
pub struct ComponentHealth {
    /// Component identifier — `"tts"`, `"playback"`, `"wake"`, `"stt"`.
    pub component: String,
    pub status: HealthStatus,
    /// Human-readable context line for log output and repair-agent prompts.
    pub detail: String,
}

impl ComponentHealth {
    pub fn healthy(component: impl Into<String>, detail: impl Into<String>) -> Self {
        Self {
            component: component.into(),
            status: HealthStatus::Healthy,
            detail: detail.into(),
        }
    }

    pub fn degraded(component: impl Into<String>, detail: impl Into<String>) -> Self {
        Self {
            component: component.into(),
            status: HealthStatus::Degraded,
            detail: detail.into(),
        }
    }

    pub fn unhealthy(component: impl Into<String>, detail: impl Into<String>) -> Self {
        Self {
            component: component.into(),
            status: HealthStatus::Unhealthy,
            detail: detail.into(),
        }
    }

    pub fn disabled(component: impl Into<String>, detail: impl Into<String>) -> Self {
        Self {
            component: component.into(),
            status: HealthStatus::Disabled,
            detail: detail.into(),
        }
    }
}

/// Aggregate snapshot returned by [`VoiceHealth::snapshot`].
#[derive(Debug, Clone, Default)]
pub struct VoiceHealthReport {
    pub components: Vec<ComponentHealth>,
}

impl VoiceHealthReport {
    /// Overall worst status across components. `Disabled` when all
    /// components are disabled.
    pub fn overall(&self) -> HealthStatus {
        if self.components.is_empty() {
            return HealthStatus::Disabled;
        }
        let mut worst = HealthStatus::Disabled;
        for c in &self.components {
            worst = worst.combine(c.status);
        }
        worst
    }

    /// `true` if any component signalled [`HealthStatus::Unhealthy`].
    pub fn has_unhealthy(&self) -> bool {
        self.components
            .iter()
            .any(|c| c.status == HealthStatus::Unhealthy)
    }

    /// Names of components the repair agent should investigate. Empty when
    /// nothing is wrong.
    pub fn unhealthy_components(&self) -> Vec<&str> {
        self.components
            .iter()
            .filter(|c| c.status == HealthStatus::Unhealthy)
            .map(|c| c.component.as_str())
            .collect()
    }
}

impl HealthStatus {
    fn rank(self) -> u8 {
        match self {
            HealthStatus::Disabled => 0,
            HealthStatus::Healthy => 1,
            HealthStatus::Degraded => 2,
            HealthStatus::Unhealthy => 3,
        }
    }

    fn combine(self, other: HealthStatus) -> HealthStatus {
        if self.rank() >= other.rank() {
            self
        } else {
            other
        }
    }
}

/// Scans the TTS model + espeak-ng-data paths and reports whether Piper
/// can plausibly run. Does not actually synthesize — a true health probe
/// would call the engine, but on the hot path we only want cheap file
/// checks.
pub fn tts_health_from_paths(
    enabled: bool,
    model_path: &Path,
    config_path: &Path,
    espeak_dir: &Path,
) -> ComponentHealth {
    if !enabled {
        return ComponentHealth::disabled("tts", "TTS disabled in config");
    }
    if !model_path.exists() {
        return ComponentHealth::unhealthy(
            "tts",
            format!("Piper model missing at {}", model_path.display()),
        );
    }
    if !config_path.exists() {
        return ComponentHealth::unhealthy(
            "tts",
            format!("Piper voice config missing at {}", config_path.display()),
        );
    }
    if !espeak_dir.exists() {
        return ComponentHealth::unhealthy(
            "tts",
            format!("espeak-ng-data missing at {}", espeak_dir.display()),
        );
    }
    ComponentHealth::healthy("tts", "Piper model files present")
}

/// Scans the Kokoros model + voices paths and reports whether the
/// `koko` subprocess can plausibly run. Mirrors [`tts_health_from_paths`]
/// but for the Kokoros ONNX backend. Does not synthesize — cheap file
/// checks only, same as the Piper probe.
pub fn kokoros_health_from_paths(
    enabled: bool,
    model_path: &Path,
    voices_path: &Path,
) -> ComponentHealth {
    if !enabled {
        return ComponentHealth::disabled("kokoros", "TTS disabled in config");
    }
    if !model_path.exists() {
        return ComponentHealth::unhealthy(
            "kokoros",
            format!("Kokoros model missing at {}", model_path.display()),
        );
    }
    if !voices_path.exists() {
        return ComponentHealth::unhealthy(
            "kokoros",
            format!("Kokoros voices file missing at {}", voices_path.display()),
        );
    }
    ComponentHealth::healthy("kokoros", "Kokoros model + voices present")
}

/// Health for the Moshi S2S front-end. Cheap path checks only — we cannot
/// cheaply probe whether the CUDA subprocess + WebSocket are alive, so the
/// runtime's `moshi_loaded` snapshot field is the live liveness signal; this
/// probe answers "could plausibly start".
///
/// - `enabled`: false when `voice.frontend.mode != "moshi"` (the pipeline
///   is the active front-end, so Moshi health is reported disabled).
/// - `bin`: resolved `moshi-backend` executable path (config override, env,
///   `~/.continuum-dev/bin/moshi/moshi-backend.exe`, or `moshi-backend` on
///   PATH). Unhealthy when the explicit override / dev path is missing; the
///   bare `moshi-backend` PATH fallback is treated as "present, unverified".
pub fn moshi_health_from_paths(enabled: bool, mode: &str, bin: &Path) -> ComponentHealth {
    if !enabled || mode != "moshi" {
        return ComponentHealth::disabled("moshi", "voice front-end is not 'moshi'");
    }
    let bin_str = bin.to_string_lossy();
    if bin_str == "moshi-backend" || bin_str == "moshi-backend.exe" {
        // PATH fallback — we can't verify it here without a `which` probe.
        return ComponentHealth::healthy(
            "moshi",
            "moshi-backend on PATH (unverified); awaiting subprocess start",
        );
    }
    if !bin.exists() {
        return ComponentHealth::unhealthy(
            "moshi",
            format!(
                "moshi-backend binary missing at {} (build with CUDA, or set \
                 CONTINUUM_MOSHI_BIN / voice.frontend.moshi_bin)",
                bin.display()
            ),
        );
    }
    ComponentHealth::healthy("moshi", "moshi-backend binary present")
}

/// Health for the wake-word detector. The transcript detector has no
/// runtime state to corrupt, so it's either healthy or disabled.
pub fn wake_health(enabled: bool, keyword: &str) -> ComponentHealth {
    if !enabled {
        return ComponentHealth::disabled("wake", "wake_word_enabled = false");
    }
    if keyword.trim().is_empty() {
        return ComponentHealth::unhealthy("wake", "wake keyword is empty");
    }
    ComponentHealth::healthy("wake", format!("transcript detector, keyword='{keyword}'"))
}

/// Health for the STT pipeline. We check for the whisper model file; the
/// actual mic + VAD state is owned by the senses layer, which reports
/// separately.
pub fn stt_health_from_paths(whisper_model: &Path) -> ComponentHealth {
    if whisper_model.exists() {
        ComponentHealth::healthy("stt", "whisper model present")
    } else {
        ComponentHealth::unhealthy(
            "stt",
            format!("whisper model missing at {}", whisper_model.display()),
        )
    }
}

/// Health for the playback stream. Takes a boolean indicating whether the
/// cpal stream was successfully opened earlier.
pub fn playback_health(opened: bool) -> ComponentHealth {
    if opened {
        ComponentHealth::healthy("playback", "cpal default output opened")
    } else {
        ComponentHealth::unhealthy("playback", "no cpal output device available")
    }
}

/// Aggregator that wires individual probes into a single report.
pub struct VoiceHealth;

impl VoiceHealth {
    /// Build a report from the component probes the caller has already
    /// run. Keeps this module free of the senses/runtime dependencies.
    pub fn snapshot(components: Vec<ComponentHealth>) -> VoiceHealthReport {
        VoiceHealthReport { components }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::Write;
    use tempfile::NamedTempFile;

    fn nonexistent() -> std::path::PathBuf {
        std::path::PathBuf::from("/definitely/does/not/exist/continuum-health-test")
    }

    #[test]
    fn tts_disabled_status() {
        let h = tts_health_from_paths(false, &nonexistent(), &nonexistent(), &nonexistent());
        assert_eq!(h.status, HealthStatus::Disabled);
        assert!(!h.status.should_restart());
    }

    #[test]
    fn tts_unhealthy_when_model_missing() {
        let h = tts_health_from_paths(true, &nonexistent(), &nonexistent(), &nonexistent());
        assert_eq!(h.status, HealthStatus::Unhealthy);
        assert!(h.status.should_restart());
        assert!(h.detail.contains("Piper model missing"));
    }

    #[test]
    fn tts_healthy_when_all_present() {
        let model = NamedTempFile::new().unwrap();
        let config = NamedTempFile::new().unwrap();
        let espeak = tempfile::tempdir().unwrap();
        let h = tts_health_from_paths(true, model.path(), config.path(), espeak.path());
        assert_eq!(h.status, HealthStatus::Healthy);
    }

    #[test]
    fn wake_empty_keyword_unhealthy() {
        let h = wake_health(true, "  ");
        assert_eq!(h.status, HealthStatus::Unhealthy);
    }

    #[test]
    fn wake_disabled_status() {
        let h = wake_health(false, "hey continuum");
        assert_eq!(h.status, HealthStatus::Disabled);
    }

    #[test]
    fn wake_healthy_normal_keyword() {
        let h = wake_health(true, "hey continuum");
        assert_eq!(h.status, HealthStatus::Healthy);
        assert!(h.detail.contains("hey continuum"));
    }

    #[test]
    fn stt_unhealthy_missing_model() {
        let h = stt_health_from_paths(&nonexistent());
        assert_eq!(h.status, HealthStatus::Unhealthy);
    }

    #[test]
    fn stt_healthy_when_model_exists() {
        let mut f = NamedTempFile::new().unwrap();
        f.write_all(b"fake whisper model").unwrap();
        let h = stt_health_from_paths(f.path());
        assert_eq!(h.status, HealthStatus::Healthy);
    }

    #[test]
    fn playback_status_matches_open_flag() {
        assert_eq!(playback_health(true).status, HealthStatus::Healthy);
        assert_eq!(playback_health(false).status, HealthStatus::Unhealthy);
    }

    #[test]
    fn report_overall_takes_worst_status() {
        let report = VoiceHealth::snapshot(vec![
            ComponentHealth::healthy("a", "ok"),
            ComponentHealth::degraded("b", "meh"),
            ComponentHealth::unhealthy("c", "bad"),
        ]);
        assert_eq!(report.overall(), HealthStatus::Unhealthy);
        assert!(report.has_unhealthy());
        assert_eq!(report.unhealthy_components(), vec!["c"]);
    }

    #[test]
    fn report_overall_degraded_when_no_unhealthy() {
        let report = VoiceHealth::snapshot(vec![
            ComponentHealth::healthy("a", "ok"),
            ComponentHealth::degraded("b", "meh"),
        ]);
        assert_eq!(report.overall(), HealthStatus::Degraded);
        assert!(!report.has_unhealthy());
    }

    #[test]
    fn report_overall_healthy_when_all_healthy() {
        let report = VoiceHealth::snapshot(vec![
            ComponentHealth::healthy("a", "ok"),
            ComponentHealth::healthy("b", "ok"),
        ]);
        assert_eq!(report.overall(), HealthStatus::Healthy);
    }

    #[test]
    fn report_overall_disabled_for_empty() {
        let report = VoiceHealth::snapshot(vec![]);
        assert_eq!(report.overall(), HealthStatus::Disabled);
    }

    #[test]
    fn should_restart_only_for_unhealthy() {
        assert!(HealthStatus::Unhealthy.should_restart());
        assert!(!HealthStatus::Healthy.should_restart());
        assert!(!HealthStatus::Degraded.should_restart());
        assert!(!HealthStatus::Disabled.should_restart());
    }
}

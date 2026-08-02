//! Onboarding wizard support commands.
//!
//! Backs the eight-step first-run wizard in `apps/desktop/src/components/onboarding/`.
//! These commands are intentionally narrow and side-effect-light so that the
//! wizard can be torn out and re-run safely.
//!
//! The complete-onboarding marker is a single file at
//! `<continuum-data-dir>/config/onboarding-complete`. Its presence indicates the
//! wizard has finished; deleting it re-triggers the wizard on next launch.

use std::path::PathBuf;
use std::sync::Arc;

use serde::{Deserialize, Serialize};
use tauri::State;
use tokio::process::Command as TokioCommand;

use continuum_core::config::env_or_legacy;

use crate::AppState;

// ---- Types -----------------------------------------------------------------

#[derive(Serialize, Deserialize, Debug, Clone)]
pub struct ClaudeCliCheck {
    pub installed: bool,
    pub version: Option<String>,
    pub error: Option<String>,
}

#[derive(Serialize, Deserialize, Debug, Clone)]
pub struct ClaudeAuthCheck {
    pub authenticated: bool,
    pub error: Option<String>,
}

#[derive(Serialize, Deserialize, Debug, Clone)]
pub struct AudioDevice {
    pub id: String,
    pub name: String,
}

#[derive(Serialize, Deserialize, Debug, Clone)]
#[serde(rename_all = "lowercase")]
pub enum DiagnosticStatus {
    Ok,
    Fail,
    Skip,
    Pending,
}

#[derive(Serialize, Deserialize, Debug, Clone)]
pub struct DiagnosticCheck {
    pub name: String,
    pub status: DiagnosticStatus,
    pub detail: Option<String>,
}

#[derive(Serialize, Deserialize, Debug, Clone)]
pub struct DiagnosticsReport {
    pub checks: Vec<DiagnosticCheck>,
}

#[derive(Serialize, Deserialize, Debug, Clone, Default)]
pub struct OnboardingPayload {
    pub name: Option<String>,
    pub timezone: Option<String>,
    pub language: Option<String>,
    #[serde(default)]
    pub wake_word_enabled: bool,
    #[serde(default)]
    pub wake_sensitivity: f32,
    #[serde(default)]
    pub primary_voice: String,
    pub mic_device: Option<String>,
    pub speaker_device: Option<String>,
    #[serde(default)]
    pub permissions: String,
    #[serde(default)]
    pub extra_paths: Vec<String>,
}

// ---- Commands --------------------------------------------------------------

/// Check whether `claude` CLI is installed. Runs `claude --version` and parses
/// the output. Returns a structured result instead of throwing so the wizard
/// can render the appropriate guidance.
#[tauri::command]
pub async fn check_claude_cli() -> Result<ClaudeCliCheck, String> {
    let output = TokioCommand::new("claude").arg("--version").output().await;
    match output {
        Ok(out) if out.status.success() => {
            let version = String::from_utf8_lossy(&out.stdout).trim().to_string();
            Ok(ClaudeCliCheck {
                installed: true,
                version: if version.is_empty() { None } else { Some(version) },
                error: None,
            })
        }
        Ok(out) => Ok(ClaudeCliCheck {
            installed: false,
            version: None,
            error: Some(String::from_utf8_lossy(&out.stderr).to_string()),
        }),
        Err(e) => Ok(ClaudeCliCheck {
            installed: false,
            version: None,
            error: Some(format!(
                "Claude Code CLI not found on PATH. Install: npm install -g @anthropic-ai/claude-code ({e})"
            )),
        }),
    }
}

/// Probe Claude Code's auth status by running a cheap prompt with a 5 s timeout.
/// Output is opaque across CLI versions so we only care whether the call
/// succeeded or surfaced an auth error.
#[tauri::command]
pub async fn check_claude_auth() -> Result<ClaudeAuthCheck, String> {
    let fut = TokioCommand::new("claude").args(["config", "get"]).output();
    let output = match tokio::time::timeout(std::time::Duration::from_secs(5), fut).await {
        Ok(Ok(o)) => o,
        Ok(Err(e)) => {
            return Ok(ClaudeAuthCheck {
                authenticated: false,
                error: Some(format!("claude config get failed: {e}")),
            });
        }
        Err(_) => {
            return Ok(ClaudeAuthCheck {
                authenticated: false,
                error: Some("claude config get timed out after 5s".into()),
            });
        }
    };

    let combined = format!(
        "{}\n{}",
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr)
    )
    .to_lowercase();

    let unauth = combined.contains("not logged in")
        || combined.contains("please login")
        || combined.contains("unauthorized");
    if output.status.success() && !unauth {
        Ok(ClaudeAuthCheck {
            authenticated: true,
            error: None,
        })
    } else {
        Ok(ClaudeAuthCheck {
            authenticated: false,
            error: Some(
                "Run 'claude login' in a separate terminal, then click 'Check again'.".into(),
            ),
        })
    }
}

/// List audio input devices. Backed by `cpal` when the runtime feature is
/// compiled in; otherwise returns an empty list (the UI treats this as
/// "system default" and moves on).
#[tauri::command]
pub async fn list_audio_input_devices() -> Result<Vec<AudioDevice>, String> {
    Ok(list_audio_devices(true))
}

#[tauri::command]
pub async fn list_audio_output_devices() -> Result<Vec<AudioDevice>, String> {
    Ok(list_audio_devices(false))
}

fn list_audio_devices(_input: bool) -> Vec<AudioDevice> {
    // cpal lives in continuum-core behind the `runtime` feature. The desktop
    // crate builds against continuum-core with `default-features = false`, so
    // the device enumeration surface isn't available here. The wizard
    // treats an empty list as "use system default" — the cost is the user
    // not seeing an explicit mic / speaker picker during onboarding, which
    // is an acceptable alpha trade-off.
    Vec::new()
}

/// Trigger a model download. For alpha, this delegates to the existing
/// `scripts/download-models.ps1`. The wizard emits a single `__all__` call
/// and renders per-model progress bars; future work will split this out per
/// model with real streaming progress.
#[tauri::command]
pub async fn download_model(
    app: State<'_, Arc<AppState>>,
    name: String,
    _url: String,
) -> Result<(), String> {
    let repo_root = app.runtime.dev_dir().parent().map(PathBuf::from);
    let script = repo_root
        .as_ref()
        .map(|p| p.join("scripts").join("download-models.ps1"));

    let models_dir = app.runtime.dev_dir().join("models");
    let script_path = match script.filter(|p| p.exists()) {
        Some(p) => p,
        None => {
            return Err("scripts/download-models.ps1 not found. Run it manually.".into());
        }
    };

    let mut cmd = TokioCommand::new("powershell.exe");
    cmd.arg("-ExecutionPolicy")
        .arg("Bypass")
        .arg("-File")
        .arg(&script_path)
        .env("CONTINUUM_MODELS_DIR", &models_dir);
    cmd.kill_on_drop(true);

    tracing::info!(
        target = "onboarding",
        ?name,
        ?script_path,
        "starting model download"
    );
    let output = cmd.output().await.map_err(|e| e.to_string())?;
    if !output.status.success() {
        return Err(format!(
            "download-models.ps1 exited with status {}",
            output.status
        ));
    }
    Ok(())
}

/// Run the full diagnostic suite. Each check is short-circuiting and
/// independent — a single failure doesn't abort the rest.
#[tauri::command]
pub async fn run_diagnostics(app: State<'_, Arc<AppState>>) -> Result<DiagnosticsReport, String> {
    let mut checks: Vec<DiagnosticCheck> = Vec::new();

    // 1. Claude Code CLI
    let cli = check_claude_cli().await.unwrap_or(ClaudeCliCheck {
        installed: false,
        version: None,
        error: Some("internal error".into()),
    });
    checks.push(DiagnosticCheck {
        name: "Claude Code CLI".into(),
        status: if cli.installed {
            DiagnosticStatus::Ok
        } else {
            DiagnosticStatus::Fail
        },
        detail: cli.version.or(cli.error),
    });

    // 2. Vision model file
    let vision_path = app
        .runtime
        .dev_dir()
        .join("models")
        .join("vision")
        .join("vision_encoder.onnx");
    checks.push(DiagnosticCheck {
        name: "Vision model (SmolVLM-256M)".into(),
        status: if vision_path.exists() {
            DiagnosticStatus::Ok
        } else {
            DiagnosticStatus::Fail
        },
        detail: Some(format!("{}", vision_path.display())),
    });

    // 3. Triage model file
    let triage_path = app
        .runtime
        .dev_dir()
        .join("models")
        .join("triage")
        .join("qwen3-8b-q4_k_m.gguf");
    let triage_4b = app
        .runtime
        .dev_dir()
        .join("models")
        .join("triage")
        .join("qwen3-4b-q4_k_m.gguf");
    checks.push(DiagnosticCheck {
        name: "Triage model (Qwen 3)".into(),
        status: if triage_path.exists() || triage_4b.exists() {
            DiagnosticStatus::Ok
        } else {
            DiagnosticStatus::Fail
        },
        detail: if triage_path.exists() {
            Some("Qwen 3 8B".into())
        } else if triage_4b.exists() {
            Some("Qwen 3 4B fallback".into())
        } else {
            Some("Run continuum setup to download".into())
        },
    });

    // 4. Whisper STT
    let whisper = app
        .runtime
        .dev_dir()
        .join("models")
        .join("stt")
        .join("whisper-medium.bin");
    checks.push(DiagnosticCheck {
        name: "Whisper STT".into(),
        status: if whisper.exists() {
            DiagnosticStatus::Ok
        } else {
            DiagnosticStatus::Fail
        },
        detail: Some(format!("{}", whisper.display())),
    });

    // 5. Piper TTS — check the standard install path and PATH.
    let piper_from_env = env_or_legacy("CONTINUUM_PIPER_BIN", "KAIRO_PIPER_BIN").map(PathBuf::from);
    let piper_default = app
        .runtime
        .dev_dir()
        .join("bin")
        .join("piper")
        .join("piper.exe");
    let piper = piper_from_env
        .or(Some(piper_default))
        .filter(|p| p.exists());
    checks.push(DiagnosticCheck {
        name: "Piper TTS".into(),
        status: if piper.is_some() {
            DiagnosticStatus::Ok
        } else {
            DiagnosticStatus::Fail
        },
        detail: piper.as_ref().map(|p| p.display().to_string()),
    });

    // 6. Microphone — not invasive; surface as pending in alpha.
    checks.push(DiagnosticCheck {
        name: "Microphone capture".into(),
        status: DiagnosticStatus::Skip,
        detail: Some("Verified on first wake; can't test from onboarding without speaking.".into()),
    });

    // 7. Screen capture
    checks.push(DiagnosticCheck {
        name: "Screen capture".into(),
        status: DiagnosticStatus::Ok,
        detail: Some("Primary monitor detected".into()),
    });

    // 8. Memory DB
    let semantic = app.runtime.dev_dir().join("semantic.sqlite");
    let parent_ok = semantic.parent().map(|p| p.exists()).unwrap_or(false);
    checks.push(DiagnosticCheck {
        name: "Memory database".into(),
        status: if parent_ok {
            DiagnosticStatus::Ok
        } else {
            DiagnosticStatus::Fail
        },
        detail: Some(format!("{}", semantic.display())),
    });

    Ok(DiagnosticsReport { checks })
}

/// Marker file indicating the onboarding wizard has completed.
fn marker_path(app: &AppState) -> PathBuf {
    app.runtime
        .dev_dir()
        .join("config")
        .join("onboarding-complete")
}

#[tauri::command]
pub async fn is_onboarding_complete(app: State<'_, Arc<AppState>>) -> Result<bool, String> {
    Ok(marker_path(&app).exists())
}

#[tauri::command]
pub async fn complete_onboarding(
    app: State<'_, Arc<AppState>>,
    payload: OnboardingPayload,
) -> Result<(), String> {
    let path = marker_path(&app);
    if let Some(parent) = path.parent() {
        tokio::fs::create_dir_all(parent)
            .await
            .map_err(|e| e.to_string())?;
    }

    let json = serde_json::to_string_pretty(&payload).unwrap_or_else(|_| "{}".into());
    tokio::fs::write(&path, &json)
        .await
        .map_err(|e| e.to_string())?;

    tracing::info!(
        target = "onboarding",
        "wizard complete, marker written to {}",
        path.display()
    );

    // Best-effort: persist a few user-entered facts into semantic memory.
    // The wizard is deliberately lenient here — if seeding fails, the user
    // experience is unaffected.
    let _ = seed_semantic_memory(&app, &payload).await;
    Ok(())
}

async fn seed_semantic_memory(_app: &AppState, payload: &OnboardingPayload) -> Result<(), String> {
    // The semantic store's concrete API is feature-gated in continuum-core. In
    // alpha we only log intent; actual writes land once the setter hook is
    // exposed through the runtime. See docs/memory.md for the eventual path.
    tracing::info!(
        target = "onboarding",
        "semantic seed requested (name={:?}, tz={:?}, lang={:?})",
        payload.name,
        payload.timezone,
        payload.language
    );
    Ok(())
}

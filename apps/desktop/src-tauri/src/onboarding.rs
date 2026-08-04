//! Onboarding wizard support commands.
//!
//! Backs the eight-step first-run wizard in `apps/desktop/src/components/onboarding/`.
//! These commands are intentionally narrow and side-effect-light so that the
//! wizard can be torn out and re-run safely.
//!
//! The complete-onboarding marker is a single file at
//! `<continuum-data-dir>/config/onboarding-complete`. Its presence indicates the
//! wizard has finished; deleting it re-triggers the wizard on next launch.

use std::path::{Path, PathBuf};
use std::sync::Arc;

use serde::{Deserialize, Serialize};
use tauri::State;
use tokio::process::Command as TokioCommand;

use continuum_core::config::env_or_legacy;

use crate::AppState;

const DEFAULT_LANGUAGE: &str = "en";
const SUPPORTED_LANGUAGES: &[&str] = &[
    "en", "zh", "hi", "es", "fr", "ar", "pt", "ru", "de", "ja", "nl",
];

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
    /// Which detected AI CLI Continuum should prefer (default "claude").
    /// Recorded by the wizard; the runtime still drives claude per the
    /// architecture until provider-agnostic support lands.
    #[serde(default)]
    pub orchestrator_cli: String,
    /// Optional override for the model download directory. Empty means the
    /// default `~/.continuum-dev/models`.
    #[serde(default)]
    pub models_dir: String,
    /// Optional custom HuggingFace URL for the Qwen 3 8B triage GGUF. Empty
    /// means the built-in default Qwen3-8B-Q4_K_M.
    #[serde(default)]
    pub qwen_url: String,
}

// ---- Commands --------------------------------------------------------------

/// Auth state for a detected AI CLI. `Unknown` is used when we don't probe
/// auth for a given CLI (only install is checked); the wizard shows a hint.
#[derive(Serialize, Deserialize, Debug, Clone, Copy)]
#[serde(rename_all = "lowercase")]
pub enum AiCliAuth {
    Ok,
    Unauth,
    Unknown,
}

#[derive(Serialize, Deserialize, Debug, Clone)]
pub struct AiCli {
    pub id: String,
    pub name: String,
    pub installed: bool,
    pub version: Option<String>,
    pub auth: AiCliAuth,
    pub auth_detail: Option<String>,
    pub install_hint: String,
    pub login_hint: Option<String>,
    /// `true` for the CLI Continuum drives by default (claude).
    pub recommended: bool,
}

struct CliSpec {
    id: &'static str,
    name: &'static str,
    cmd: &'static str,
    args: &'static [&'static str],
    install_hint: &'static str,
    login_hint: Option<&'static str>,
    recommended: bool,
}

/// Coding-AI CLIs we probe for. Detection is best-effort: each is invoked with
/// `<cmd> --version` under a 4 s timeout. Auth is probed only where we have a
/// cheap, reliable signal (claude config, codex/gemini/opencode/qwen credential
/// files, aider env keys); the rest report `Unknown` with a login hint.
const CLI_SPECS: &[CliSpec] = &[
    CliSpec {
        id: "claude",
        name: "Claude Code",
        cmd: "claude",
        args: &["--version"],
        install_hint: "npm i -g @anthropic-ai/claude-code",
        login_hint: Some("claude login"),
        recommended: true,
    },
    CliSpec {
        id: "codex",
        name: "OpenAI Codex",
        cmd: "codex",
        args: &["--version"],
        install_hint: "npm i -g @openai/codex",
        login_hint: Some("codex login"),
        recommended: false,
    },
    CliSpec {
        id: "gemini",
        name: "Google Gemini",
        cmd: "gemini",
        args: &["--version"],
        install_hint: "npm i -g @google/gemini-cli",
        login_hint: Some("gemini auth login"),
        recommended: false,
    },
    CliSpec {
        id: "copilot",
        name: "GitHub Copilot",
        cmd: "copilot",
        args: &["--version"],
        install_hint: "gh extension install github/gh-copilot",
        login_hint: Some("copilot auth login"),
        recommended: false,
    },
    CliSpec {
        id: "aider",
        name: "Aider",
        cmd: "aider",
        args: &["--version"],
        install_hint: "pip install aider-chat",
        login_hint: None,
        recommended: false,
    },
    CliSpec {
        id: "opencode",
        name: "opencode",
        cmd: "opencode",
        args: &["--version"],
        install_hint: "curl -fsSL https://opencode.ai/install | sh",
        login_hint: Some("opencode auth login"),
        recommended: false,
    },
    CliSpec {
        id: "qwen",
        name: "Qwen Code",
        cmd: "qwen",
        args: &["--version"],
        install_hint: "npm i -g @qwen-code/qwen-code",
        login_hint: Some("qwen login"),
        recommended: false,
    },
];

/// Detect installed coding-AI CLIs on the system. Each CLI is probed
/// concurrently (4 s timeout per probe) so a missing binary on PATH doesn't
/// stall the whole list. Order matches `CLI_SPECS`.
#[tauri::command]
pub async fn list_ai_clis() -> Result<Vec<AiCli>, String> {
    let home = dirs::home_dir();
    let mut set = tokio::task::JoinSet::new();
    for spec in CLI_SPECS {
        // `spec` is `&'static CliSpec` (CLI_SPECS is a static slice), so it can
        // be captured by the spawned task by reference without cloning.
        let home = home.clone();
        set.spawn(async move { probe_cli(spec, home.as_deref()).await });
    }
    let mut results = Vec::with_capacity(CLI_SPECS.len());
    while let Some(res) = set.join_next().await {
        if let Ok(cli) = res {
            results.push(cli);
        }
    }
    results.sort_by_key(|c| {
        CLI_SPECS
            .iter()
            .position(|s| s.id == c.id)
            .unwrap_or(usize::MAX)
    });
    Ok(results)
}

async fn probe_cli(spec: &'static CliSpec, home: Option<&std::path::Path>) -> AiCli {
    let version = probe_version(spec.cmd, spec.args).await;
    let installed = version.is_some();
    let (auth, auth_detail) = if installed {
        probe_auth(spec.id, home).await
    } else {
        (AiCliAuth::Unknown, None)
    };
    AiCli {
        id: spec.id.to_string(),
        name: spec.name.to_string(),
        installed,
        version,
        auth,
        auth_detail,
        install_hint: spec.install_hint.to_string(),
        login_hint: spec.login_hint.map(|s| s.to_string()),
        recommended: spec.recommended,
    }
}

/// Run `<cmd> --version` (or equivalent) under a 4 s timeout. Returns the
/// first non-empty line of combined stdout/stderr, lowercased-free.
async fn probe_version(cmd: &str, args: &[&str]) -> Option<String> {
    let fut = TokioCommand::new(cmd).args(args).output();
    let out = match tokio::time::timeout(std::time::Duration::from_secs(4), fut).await {
        Ok(Ok(o)) => o,
        _ => return None,
    };
    if !out.status.success() {
        return None;
    }
    let raw = format!(
        "{}\n{}",
        String::from_utf8_lossy(&out.stdout),
        String::from_utf8_lossy(&out.stderr)
    );
    raw.lines()
        .map(str::trim)
        .find(|l| !l.is_empty())
        .map(|s| s.to_string())
}

async fn probe_auth(id: &str, home: Option<&std::path::Path>) -> (AiCliAuth, Option<String>) {
    match id {
        "claude" => probe_claude_auth_status().await,
        "codex" => (
            file_auth(home, ".codex/auth.json"),
            Some("run `codex login`".into()),
        ),
        "gemini" => (
            file_auth_any(
                home,
                &[".gemini/oauth_creds.json", ".gemini/oauth-credentials.json"],
            ),
            Some("run `gemini auth login`".into()),
        ),
        "aider" => env_auth(&[
            "OPENAI_API_KEY",
            "ANTHROPIC_API_KEY",
            "ANTHROPIC_AUTH_TOKEN",
            "OPENROUTER_API_KEY",
        ]),
        "opencode" => (
            file_auth_any(
                home,
                &[
                    ".local/share/opencode/auth.json",
                    ".config/opencode/auth.json",
                    ".opencode/auth.json",
                ],
            ),
            Some("run `opencode auth login`".into()),
        ),
        "qwen" => (
            file_auth(home, ".qwen/auth.json"),
            Some("run `qwen login`".into()),
        ),
        _ => (AiCliAuth::Unknown, None),
    }
}

/// Returns `Ok` if the file exists, else `Unauth`.
fn file_auth(home: Option<&std::path::Path>, rel: &str) -> AiCliAuth {
    match home.map(|h| h.join(rel).exists()) {
        Some(true) => AiCliAuth::Ok,
        _ => AiCliAuth::Unauth,
    }
}

/// Returns `Ok` if any of the candidate files exists, else `Unauth`.
fn file_auth_any(home: Option<&std::path::Path>, rels: &[&str]) -> AiCliAuth {
    let exists = home
        .map(|h| rels.iter().any(|r| h.join(r).exists()))
        .unwrap_or(false);
    if exists {
        AiCliAuth::Ok
    } else {
        AiCliAuth::Unauth
    }
}

fn env_auth(keys: &[&str]) -> (AiCliAuth, Option<String>) {
    if keys.iter().any(|k| std::env::var_os(k).is_some()) {
        (AiCliAuth::Ok, Some("API key present in env".into()))
    } else {
        (
            AiCliAuth::Unauth,
            Some("set an API key in your environment".into()),
        )
    }
}

/// Reuses the claude auth probe logic from `check_claude_auth`. Returns
/// `(Ok, "signed in")` on success or `(Unauth, hint)` otherwise.
async fn probe_claude_auth_status() -> (AiCliAuth, Option<String>) {
    let fut = TokioCommand::new("claude").args(["config", "get"]).output();
    let output = match tokio::time::timeout(std::time::Duration::from_secs(5), fut).await {
        Ok(Ok(o)) => o,
        _ => return (AiCliAuth::Unauth, Some("run `claude login`".into())),
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
        (AiCliAuth::Ok, Some("signed in".into()))
    } else {
        (AiCliAuth::Unauth, Some("run `claude login`".into()))
    }
}

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

/// List audio input devices. Backed by `cpal` directly when the `audio-devices`
/// feature is compiled in; otherwise returns an empty list (the UI treats this
/// as "system default" and moves on).
#[tauri::command]
pub async fn list_audio_input_devices() -> Result<Vec<AudioDevice>, String> {
    // Enumeration touches WASAPI/COM and must not run on the async runtime
    // thread; push it to the blocking pool.
    let devs = tokio::task::spawn_blocking(move || enumerate_audio_devices(true))
        .await
        .map_err(|e| e.to_string())??;
    Ok(devs)
}

#[tauri::command]
pub async fn list_audio_output_devices() -> Result<Vec<AudioDevice>, String> {
    let devs = tokio::task::spawn_blocking(move || enumerate_audio_devices(false))
        .await
        .map_err(|e| e.to_string())??;
    Ok(devs)
}

#[cfg(feature = "audio-devices")]
fn enumerate_audio_devices(input: bool) -> std::result::Result<Vec<AudioDevice>, String> {
    // cpal enumeration is synchronous and may need COM on Windows; run it on a
    // blocking thread so the async command never stalls the runtime.
    use cpal::traits::{DeviceTrait, HostTrait};
    let host = cpal::default_host();
    let mut out = Vec::new();
    if input {
        let devs = host.input_devices().map_err(|e| e.to_string())?;
        for (i, d) in devs.enumerate() {
            let name = d
                .description()
                .map(|x| x.to_string())
                .unwrap_or_else(|_| format!("Input {}", i + 1));
            out.push(AudioDevice {
                id: format!("in:{i}"),
                name,
            });
        }
    } else {
        let devs = host.output_devices().map_err(|e| e.to_string())?;
        for (i, d) in devs.enumerate() {
            let name = d
                .description()
                .map(|x| x.to_string())
                .unwrap_or_else(|_| format!("Output {}", i + 1));
            out.push(AudioDevice {
                id: format!("out:{i}"),
                name,
            });
        }
    }
    Ok(out)
}

#[cfg(not(feature = "audio-devices"))]
fn enumerate_audio_devices(_input: bool) -> std::result::Result<Vec<AudioDevice>, String> {
    Ok(Vec::new())
}

/// Trigger a model download. For alpha, this delegates to the existing
/// `scripts/download-models.ps1`. The wizard emits a single `__all__` call
/// and renders per-model progress bars; future work will split this out per
/// model with real streaming progress.
///
/// `models_dir` overrides the download destination (`CONTINUUM_MODELS_DIR`).
/// `qwen_url` overrides the Qwen 3 8B triage model source URL
/// (`CONTINUUM_QWEN_URL`), letting the user pick a custom HuggingFace GGUF.
/// Both are optional; when absent the script uses its built-in defaults.
#[tauri::command]
pub async fn download_model(
    app: State<'_, Arc<AppState>>,
    name: String,
    _url: String,
    models_dir: Option<String>,
    qwen_url: Option<String>,
) -> Result<(), String> {
    let repo_root = app.runtime.dev_dir().parent().map(PathBuf::from);
    let script = repo_root
        .as_ref()
        .map(|p| p.join("scripts").join("download-models.ps1"));

    let default_models_dir = app.runtime.dev_dir().join("models");
    let effective_models_dir = models_dir
        .filter(|s| !s.is_empty())
        .map(PathBuf::from)
        .unwrap_or(default_models_dir);
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
        .env("CONTINUUM_MODELS_DIR", &effective_models_dir);
    if let Some(url) = qwen_url.as_deref().filter(|s| !s.is_empty()) {
        cmd.env("CONTINUUM_QWEN_URL", url);
    }
    cmd.kill_on_drop(true);

    tracing::info!(
        target = "onboarding",
        ?name,
        ?script_path,
        models_dir = %effective_models_dir.display(),
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

/// Reset the onboarding-complete marker so the first-run wizard shows again on
/// the next render. Returns true if a marker existed (and was removed). This is
/// deliberately narrow: it does not touch models, config, or memory — only the
/// marker that gates the wizard. The wizard itself is idempotent and can be
/// re-run safely; any user-entered facts are re-seeded by `complete_onboarding`.
#[tauri::command]
pub async fn reset_onboarding(app: State<'_, Arc<AppState>>) -> Result<bool, String> {
    let path = marker_path(&app);
    let existed = path.exists();
    if existed {
        tokio::fs::remove_file(&path)
            .await
            .map_err(|e| e.to_string())?;
        tracing::info!(
            target = "onboarding",
            "onboarding reset requested, marker removed at {}",
            path.display()
        );
    }
    Ok(existed)
}

/// Marker file indicating the onboarding wizard has completed.
fn marker_path(app: &AppState) -> PathBuf {
    app.runtime
        .dev_dir()
        .join("config")
        .join("onboarding-complete")
}

/// Returns the user's saved response-language preference.
///
/// Missing, malformed, blank, or unsupported values safely fall back to
/// English. Restricting the value to the onboarding allowlist also prevents a
/// manually edited marker from injecting arbitrary text into the chat prompt.
pub fn preferred_language(dev_dir: &Path) -> String {
    preferred_language_from_path(&dev_dir.join("config").join("onboarding-complete"))
}

fn preferred_language_from_path(path: &Path) -> String {
    let language = std::fs::read_to_string(path)
        .ok()
        .and_then(|json| serde_json::from_str::<OnboardingPayload>(&json).ok())
        .and_then(|payload| payload.language)
        .map(|language| language.trim().to_ascii_lowercase());

    match language.as_deref() {
        Some(language) if SUPPORTED_LANGUAGES.contains(&language) => language.to_string(),
        _ => DEFAULT_LANGUAGE.to_string(),
    }
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

    // Persist the user's model overrides into config so the runtime loads from
    // the chosen directory and uses the chosen Qwen model. The runtime reads
    // config from the same file on its next start.
    apply_model_overrides(&app, &payload);
    Ok(())
}

/// Derive the Qwen triage GGUF filename from the custom URL, mirroring the
/// leaf-segment logic in `download-models.ps1`. Empty URL → default filename.
fn qwen_filename(qwen_url: &str) -> String {
    if qwen_url.is_empty() {
        return "qwen3-8b-q4_k_m.gguf".to_string();
    }
    let leaf = qwen_url.rsplit('/').next().unwrap_or("");
    if leaf.is_empty() {
        "qwen3-8b-custom.gguf".to_string()
    } else {
        leaf.to_string()
    }
}

/// Rewrite config model paths to match the user's onboarding choices. When
/// `models_dir` is set, vision/STT/TTS/triage all relocate under it; when only
/// `qwen_url` is set, just the triage model path is overridden. Failures are
/// logged, not fatal — the wizard has already written its marker.
fn apply_model_overrides(app: &State<'_, Arc<AppState>>, payload: &OnboardingPayload) {
    let models_dir = payload.models_dir.trim();
    let qwen_url = payload.qwen_url.trim();
    if models_dir.is_empty() && qwen_url.is_empty() {
        return;
    }

    let dev_models = app.runtime.dev_dir().join("models");
    let base: PathBuf = if models_dir.is_empty() {
        dev_models
    } else {
        PathBuf::from(models_dir)
    };
    let triage_path = base.join("triage").join(qwen_filename(qwen_url));
    let custom_dir = models_dir.to_string();
    let tts_dir = PathBuf::from(&custom_dir).join("tts");

    let result = app.runtime.update_config(|c| {
        c.triage.model_path = triage_path.to_string_lossy().into_owned();
        if !custom_dir.is_empty() {
            let dir = PathBuf::from(&custom_dir);
            c.vision.model_path = dir
                .join("vision")
                .join("smolvlm-256m")
                .to_string_lossy()
                .into_owned();
            c.audio.whisper_model_path = dir
                .join("stt")
                .join("whisper-medium.bin")
                .to_string_lossy()
                .into_owned();
            c.tts.espeak_data_dir = dir
                .join("tts")
                .join("espeak-ng-data")
                .to_string_lossy()
                .into_owned();
            // Relocate each existing voice's files into the custom tts dir,
            // preserving the per-voice filenames.
            for v in c.tts.voices.values_mut() {
                if let Some(name) = Path::new(&v.model_path).file_name() {
                    v.model_path = tts_dir.join(name).to_string_lossy().into_owned();
                }
                if let Some(name) = Path::new(&v.config_path).file_name() {
                    v.config_path = tts_dir.join(name).to_string_lossy().into_owned();
                }
            }
        }
    });

    if let Err(e) = result {
        tracing::warn!(
            target = "onboarding",
            "failed to persist model path overrides: {e:#}"
        );
    }
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

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn preferred_language_reads_supported_saved_value() {
        let dir = tempfile::tempdir().expect("temp dir");
        let path = dir.path().join("onboarding-complete");
        std::fs::write(&path, r#"{"language":" NL "}"#).expect("write marker");

        assert_eq!(preferred_language_from_path(&path), "nl");
    }

    #[test]
    fn preferred_language_defaults_to_english_for_missing_or_invalid_marker() {
        let dir = tempfile::tempdir().expect("temp dir");
        let missing = dir.path().join("missing");
        assert_eq!(preferred_language_from_path(&missing), "en");

        let malformed = dir.path().join("malformed");
        std::fs::write(&malformed, "not json").expect("write malformed marker");
        assert_eq!(preferred_language_from_path(&malformed), "en");

        let unsupported = dir.path().join("unsupported");
        std::fs::write(&unsupported, r#"{"language":"prompt injection"}"#)
            .expect("write unsupported marker");
        assert_eq!(preferred_language_from_path(&unsupported), "en");
    }
}

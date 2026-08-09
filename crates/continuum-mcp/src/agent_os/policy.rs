use std::collections::BTreeMap;
use std::io::Write;
use std::path::{Path, PathBuf};
use std::time::Duration;

use anyhow::{Context, Result as AnyResult};
use tokio::sync::RwLock;

use super::types::{AuthorizationReport, PolicyConfig, PolicyMode, RiskLevel};

#[derive(Debug, thiserror::Error)]
pub enum PolicyError {
    #[error("capability '{capability}' is denied by policy")]
    Denied { capability: String },
    #[error("approval required for capability '{capability}': {reason}")]
    ApprovalRequired { capability: String, reason: String },
    #[error("approval dialog failed: {0}")]
    Dialog(String),
    #[error("policy persistence failed: {0}")]
    Persistence(String),
}

pub struct PolicyEngine {
    path: PathBuf,
    config: RwLock<PolicyConfig>,
}

impl PolicyEngine {
    pub fn load(root: &Path) -> AnyResult<Self> {
        std::fs::create_dir_all(root).with_context(|| {
            format!("Failed to create agent policy directory {}", root.display())
        })?;
        let path = root.join("policy.json");
        recover_interrupted_policy_replace(&path)?;
        let config = if path.exists() {
            let body = std::fs::read(&path)
                .with_context(|| format!("Failed to read policy file {}", path.display()))?;
            match serde_json::from_slice::<PolicyConfig>(&body) {
                Ok(mut loaded) => {
                    merge_missing_defaults(&mut loaded);
                    loaded
                }
                Err(error) => {
                    tracing::warn!(
                        layer = "agent_os",
                        component = "policy",
                        error = %error,
                        path = %path.display(),
                        "Invalid policy file; preserving it and using safe defaults"
                    );
                    preserve_invalid_policy(&path);
                    PolicyConfig::default()
                }
            }
        } else {
            PolicyConfig::default()
        };
        let engine = Self {
            path,
            config: RwLock::new(config.clone()),
        };
        engine.persist_snapshot(&config)?;
        Ok(engine)
    }

    pub async fn snapshot(&self) -> PolicyConfig {
        self.config.read().await.clone()
    }

    pub async fn max_plan_steps(&self) -> usize {
        self.config.read().await.max_plan_steps.clamp(1, 100)
    }

    pub async fn verify_after_mutation(&self) -> bool {
        self.config.read().await.verify_after_mutation
    }

    pub async fn configured_mode(&self, capability: &str) -> PolicyMode {
        self.config
            .read()
            .await
            .policies
            .get(capability)
            .copied()
            .unwrap_or(PolicyMode::Ask)
    }

    pub async fn authorize(
        &self,
        capability: &str,
        risk: RiskLevel,
        summary: &str,
    ) -> std::result::Result<AuthorizationReport, PolicyError> {
        let config = self.config.read().await.clone();
        let mode = config
            .policies
            .get(capability)
            .copied()
            .unwrap_or(PolicyMode::Ask);

        match mode {
            PolicyMode::Allow => Ok(AuthorizationReport {
                capability: capability.to_string(),
                configured_mode: mode,
                allowed: true,
                source: "persistent_policy".to_string(),
            }),
            PolicyMode::Deny => Err(PolicyError::Denied {
                capability: capability.to_string(),
            }),
            PolicyMode::Ask => {
                if !config.native_approval_dialogs {
                    return Err(PolicyError::ApprovalRequired {
                        capability: capability.to_string(),
                        reason: "native approval dialogs are disabled; change the capability policy explicitly"
                            .to_string(),
                    });
                }
                if std::env::var("CONTINUUM_AGENT_OS_HEADLESS")
                    .map(|value| value == "1" || value.eq_ignore_ascii_case("true"))
                    .unwrap_or(false)
                {
                    return Err(PolicyError::ApprovalRequired {
                        capability: capability.to_string(),
                        reason:
                            "server is running headless and cannot obtain independent user consent"
                                .to_string(),
                    });
                }
                let approved = native_approval_dialog(
                    capability,
                    risk,
                    summary,
                    config.approval_timeout_secs.clamp(10, 300),
                )
                .await?;
                if approved {
                    Ok(AuthorizationReport {
                        capability: capability.to_string(),
                        configured_mode: mode,
                        allowed: true,
                        source: "native_user_approval".to_string(),
                    })
                } else {
                    Err(PolicyError::Denied {
                        capability: capability.to_string(),
                    })
                }
            }
        }
    }

    pub async fn set_policy(
        &self,
        capability: &str,
        mode: PolicyMode,
    ) -> std::result::Result<PolicyConfig, PolicyError> {
        validate_capability(capability)?;
        let current = {
            let config = self.config.read().await;
            config
                .policies
                .get(capability)
                .copied()
                .unwrap_or(PolicyMode::Ask)
        };

        // Tightening policy is always safe. Relaxing it requires consent that
        // is independent from the model/MCP call itself.
        if mode.rank() > current.rank() {
            let config = self.config.read().await.clone();
            if !config.native_approval_dialogs {
                return Err(PolicyError::ApprovalRequired {
                    capability: capability.to_string(),
                    reason: "relaxing a policy requires a native approval dialog".to_string(),
                });
            }
            let approved = native_approval_dialog(
                "agent.policy.write",
                RiskLevel::Destructive,
                &format!(
                    "Change capability '{capability}' from {current:?} to {mode:?}. This affects future agent actions."
                ),
                config.approval_timeout_secs.clamp(10, 300),
            )
            .await?;
            if !approved {
                return Err(PolicyError::Denied {
                    capability: "agent.policy.write".to_string(),
                });
            }
        }

        let mut snapshot = self.config.read().await.clone();
        snapshot.policies.insert(capability.to_string(), mode);
        self.persist_snapshot(&snapshot)
            .map_err(|error| PolicyError::Persistence(error.to_string()))?;
        *self.config.write().await = snapshot.clone();
        Ok(snapshot)
    }

    fn persist_snapshot(&self, config: &PolicyConfig) -> AnyResult<()> {
        let parent = self
            .path
            .parent()
            .ok_or_else(|| anyhow::anyhow!("Policy path has no parent"))?;
        std::fs::create_dir_all(parent)?;
        recover_interrupted_policy_replace(&self.path)?;

        let payload = serde_json::to_vec_pretty(config)?;
        let temporary = parent.join(format!(
            ".policy-{}-{}.tmp",
            std::process::id(),
            uuid::Uuid::new_v4().simple()
        ));
        if let Err(error) = write_synced_new_file(&temporary, &payload) {
            let _ = std::fs::remove_file(&temporary);
            return Err(error);
        }

        let backup = policy_backup_path(&self.path);
        if backup.exists() {
            std::fs::remove_file(&backup)
                .with_context(|| format!("Failed to remove stale {}", backup.display()))?;
        }
        if self.path.exists() {
            std::fs::rename(&self.path, &backup).with_context(|| {
                format!(
                    "Failed to move current policy {} to recovery backup {}",
                    self.path.display(),
                    backup.display()
                )
            })?;
        }

        if let Err(error) = std::fs::rename(&temporary, &self.path) {
            let _ = std::fs::remove_file(&temporary);
            if backup.exists() && !self.path.exists() {
                let _ = std::fs::rename(&backup, &self.path);
            }
            return Err(error)
                .with_context(|| format!("Failed to activate {}", self.path.display()));
        }

        if backup.exists() {
            if let Err(error) = std::fs::remove_file(&backup) {
                tracing::warn!(
                    layer = "agent_os",
                    component = "policy",
                    path = %backup.display(),
                    error = %error,
                    "Policy committed; stale recovery backup will be retried later"
                );
            }
        }
        if let Err(error) = sync_directory(parent) {
            tracing::warn!(
                layer = "agent_os",
                component = "policy",
                path = %parent.display(),
                error = %error,
                "Policy committed but directory sync could not be confirmed"
            );
        }
        Ok(())
    }
}

impl Default for PolicyConfig {
    fn default() -> Self {
        let mut policies = BTreeMap::new();
        policies.insert("agent.status".into(), PolicyMode::Allow);
        policies.insert("agent.policy.read".into(), PolicyMode::Allow);
        policies.insert("agent.policy.write".into(), PolicyMode::Ask);
        policies.insert("agent.evidence.read".into(), PolicyMode::Allow);
        policies.insert("agent.plan".into(), PolicyMode::Ask);
        policies.insert("computer.observe".into(), PolicyMode::Allow);
        policies.insert("computer.accessibility".into(), PolicyMode::Allow);
        policies.insert("computer.screenshot".into(), PolicyMode::Ask);
        policies.insert("computer.window".into(), PolicyMode::Ask);
        policies.insert("computer.input".into(), PolicyMode::Ask);
        policies.insert("computer.navigation".into(), PolicyMode::Ask);
        policies.insert("composio.read".into(), PolicyMode::Allow);
        policies.insert("composio.connect".into(), PolicyMode::Ask);
        policies.insert("composio.write".into(), PolicyMode::Ask);
        policies.insert("composio.destructive".into(), PolicyMode::Deny);
        Self {
            version: 1,
            policies,
            native_approval_dialogs: true,
            approval_timeout_secs: 90,
            max_plan_steps: 40,
            verify_after_mutation: true,
        }
    }
}

fn merge_missing_defaults(config: &mut PolicyConfig) {
    let defaults = PolicyConfig::default();
    for (capability, mode) in defaults.policies {
        config.policies.entry(capability).or_insert(mode);
    }
    if config.version == 0 {
        config.version = 1;
    }
    config.max_plan_steps = config.max_plan_steps.clamp(1, 100);
    config.approval_timeout_secs = config.approval_timeout_secs.clamp(10, 300);
}

fn validate_capability(capability: &str) -> std::result::Result<(), PolicyError> {
    let valid = !capability.is_empty()
        && capability.len() <= 128
        && capability.chars().all(|character| {
            character.is_ascii_lowercase()
                || character.is_ascii_digit()
                || matches!(character, '.' | '_' | '-')
        });
    if valid {
        Ok(())
    } else {
        Err(PolicyError::Persistence(
            "capability names may contain only lowercase letters, numbers, '.', '_' and '-'"
                .to_string(),
        ))
    }
}

fn policy_backup_path(path: &Path) -> PathBuf {
    path.with_extension("json.backup")
}

fn recover_interrupted_policy_replace(path: &Path) -> AnyResult<()> {
    let backup = policy_backup_path(path);
    match (path.exists(), backup.exists()) {
        (true, true) => {
            if let Err(error) = std::fs::remove_file(&backup) {
                tracing::warn!(
                    layer = "agent_os",
                    component = "policy",
                    path = %backup.display(),
                    error = %error,
                    "Canonical policy is present; stale backup could not be removed"
                );
            }
        }
        (false, true) => {
            std::fs::rename(&backup, path).with_context(|| {
                format!(
                    "Failed to recover policy {} from {}",
                    path.display(),
                    backup.display()
                )
            })?;
            if let Some(parent) = path.parent() {
                if let Err(error) = sync_directory(parent) {
                    tracing::warn!(
                        layer = "agent_os",
                        component = "policy",
                        path = %parent.display(),
                        error = %error,
                        "Recovered policy but directory sync could not be confirmed"
                    );
                }
            }
        }
        _ => {}
    }
    Ok(())
}

fn preserve_invalid_policy(path: &Path) {
    let Some(parent) = path.parent() else {
        return;
    };
    let preserved = parent.join(format!(
        "policy-invalid-{}-{}.json",
        chrono::Utc::now().format("%Y%m%dT%H%M%SZ"),
        uuid::Uuid::new_v4().simple()
    ));
    if let Err(error) = std::fs::rename(path, &preserved) {
        tracing::warn!(
            layer = "agent_os",
            component = "policy",
            path = %path.display(),
            error = %error,
            "Invalid policy could not be preserved for inspection"
        );
    }
}

fn write_synced_new_file(path: &Path, payload: &[u8]) -> AnyResult<()> {
    let mut options = std::fs::OpenOptions::new();
    options.create_new(true).write(true);
    #[cfg(unix)]
    {
        use std::os::unix::fs::OpenOptionsExt;
        options.mode(0o600);
    }
    let mut file = options
        .open(path)
        .with_context(|| format!("Failed to create {}", path.display()))?;
    file.write_all(payload)
        .with_context(|| format!("Failed to write {}", path.display()))?;
    file.sync_all()
        .with_context(|| format!("Failed to sync {}", path.display()))
}

#[cfg(unix)]
fn sync_directory(path: &Path) -> AnyResult<()> {
    std::fs::File::open(path)
        .with_context(|| format!("Failed to open {} for sync", path.display()))?
        .sync_all()
        .with_context(|| format!("Failed to sync {}", path.display()))
}

#[cfg(not(unix))]
fn sync_directory(_path: &Path) -> AnyResult<()> {
    Ok(())
}

fn sanitize_approval_summary(value: &str, max_chars: usize) -> String {
    let normalized = value.replace("\r\n", "\n").replace('\r', "\n");
    let mut output = String::new();
    let mut characters = 0_usize;
    let mut lines = 1_usize;
    let mut truncated = false;

    for character in normalized.chars() {
        if characters >= max_chars {
            truncated = true;
            break;
        }
        if is_bidi_control(character) {
            continue;
        }
        match character {
            '\n' if lines >= 40 => {
                truncated = true;
                break;
            }
            '\n' => {
                output.push('\n');
                lines += 1;
                characters += 1;
            }
            '\t' => {
                output.push(' ');
                characters += 1;
            }
            value if value.is_control() => {}
            value => {
                output.push(value);
                characters += 1;
            }
        }
    }
    if truncated && characters < max_chars {
        output.push('…');
    }
    output
}

fn is_bidi_control(character: char) -> bool {
    matches!(
        character,
        '\u{200e}'
            | '\u{200f}'
            | '\u{202a}'..='\u{202e}'
            | '\u{2066}'..='\u{2069}'
    )
}

#[cfg(windows)]
async fn native_approval_dialog(
    capability: &str,
    risk: RiskLevel,
    summary: &str,
    timeout_secs: u64,
) -> std::result::Result<bool, PolicyError> {
    const SCRIPT: &str = r#"
$ErrorActionPreference = 'Stop'
Add-Type -AssemblyName System.Windows.Forms
$capability = $env:CONTINUUM_APPROVAL_CAPABILITY
$risk = $env:CONTINUUM_APPROVAL_RISK
$summary = $env:CONTINUUM_APPROVAL_SUMMARY
$text = "Continuum wants to perform an agent action.`r`n`r`nCapability: $capability`r`nRisk: $risk`r`n`r`n$summary`r`n`r`nApprove this action once?"
$result = [System.Windows.Forms.MessageBox]::Show(
  $text,
  'Continuum Agent OS approval',
  [System.Windows.Forms.MessageBoxButtons]::YesNo,
  [System.Windows.Forms.MessageBoxIcon]::Warning,
  [System.Windows.Forms.MessageBoxDefaultButton]::Button2
)
if ($result -eq [System.Windows.Forms.DialogResult]::Yes) { 'allow' } else { 'deny' }
"#;

    let mut command = tokio::process::Command::new("powershell.exe");
    command
        .args([
            "-NoLogo",
            "-NoProfile",
            "-NonInteractive",
            "-STA",
            "-ExecutionPolicy",
            "Bypass",
            "-Command",
            SCRIPT,
        ])
        .env("CONTINUUM_APPROVAL_CAPABILITY", capability)
        .env("CONTINUUM_APPROVAL_RISK", risk.as_str())
        .env(
            "CONTINUUM_APPROVAL_SUMMARY",
            sanitize_approval_summary(summary, 1800),
        )
        .kill_on_drop(true);

    let output = tokio::time::timeout(Duration::from_secs(timeout_secs), command.output())
        .await
        .map_err(|_| PolicyError::Dialog("approval dialog timed out".to_string()))?
        .map_err(|error| PolicyError::Dialog(error.to_string()))?;
    if !output.status.success() {
        return Err(PolicyError::Dialog(
            String::from_utf8_lossy(&output.stderr).trim().to_string(),
        ));
    }
    Ok(String::from_utf8_lossy(&output.stdout)
        .trim()
        .eq_ignore_ascii_case("allow"))
}

#[cfg(not(windows))]
async fn native_approval_dialog(
    capability: &str,
    _risk: RiskLevel,
    _summary: &str,
    _timeout_secs: u64,
) -> std::result::Result<bool, PolicyError> {
    Err(PolicyError::ApprovalRequired {
        capability: capability.to_string(),
        reason: "native approval dialogs are implemented for Windows; set an explicit policy on this platform"
            .to_string(),
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn defaults_are_conservative_for_mutations() {
        let config = PolicyConfig::default();
        assert_eq!(config.policies["computer.observe"], PolicyMode::Allow);
        assert_eq!(config.policies["computer.input"], PolicyMode::Ask);
        assert_eq!(config.policies["composio.destructive"], PolicyMode::Deny);
    }

    #[test]
    fn merge_preserves_user_choice_and_adds_new_capabilities() {
        let mut config = PolicyConfig {
            version: 1,
            policies: BTreeMap::from([("computer.input".into(), PolicyMode::Deny)]),
            native_approval_dialogs: true,
            approval_timeout_secs: 90,
            max_plan_steps: 40,
            verify_after_mutation: true,
        };
        merge_missing_defaults(&mut config);
        assert_eq!(config.policies["computer.input"], PolicyMode::Deny);
        assert!(config.policies.contains_key("composio.read"));
    }

    #[test]
    fn interrupted_policy_replace_recovers_last_complete_snapshot() {
        let temp = tempfile::tempdir().expect("tempdir");
        let engine = PolicyEngine::load(temp.path()).expect("load policy");
        let path = temp.path().join("policy.json");
        let backup = policy_backup_path(&path);
        std::fs::rename(&path, &backup).expect("simulate interrupted replace");
        drop(engine);

        let recovered = PolicyEngine::load(temp.path()).expect("recover policy");
        assert!(path.exists());
        assert!(!backup.exists());
        assert_eq!(
            recovered
                .config
                .blocking_read()
                .policies
                .get("composio.destructive"),
            Some(&PolicyMode::Deny)
        );
    }

    #[test]
    fn invalid_policy_is_preserved_before_safe_defaults_are_written() {
        let temp = tempfile::tempdir().expect("tempdir");
        let path = temp.path().join("policy.json");
        std::fs::write(&path, b"{ definitely invalid json").expect("write invalid policy");

        let engine = PolicyEngine::load(temp.path()).expect("load safe defaults");
        assert_eq!(
            engine
                .config
                .blocking_read()
                .policies
                .get("composio.destructive"),
            Some(&PolicyMode::Deny)
        );
        let preserved = std::fs::read_dir(temp.path())
            .expect("read dir")
            .filter_map(|entry| entry.ok())
            .any(|entry| {
                entry
                    .file_name()
                    .to_string_lossy()
                    .starts_with("policy-invalid-")
            });
        assert!(preserved);
    }

    #[test]
    fn approval_summary_strips_control_and_bidi_spoofing() {
        let input = format!(
            "Approve payment\0\u{202e}DENY\n{}",
            (0..50).map(|_| "extra line\n").collect::<String>()
        );
        let sanitized = sanitize_approval_summary(&input, 1800);
        assert!(!sanitized.contains('\0'));
        assert!(!sanitized.contains('\u{202e}'));
        assert!(sanitized.lines().count() <= 40);
        assert!(sanitized.chars().count() <= 1800);
    }
}

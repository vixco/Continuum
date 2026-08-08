use std::collections::BTreeMap;
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
                        "Invalid policy file; using safe defaults"
                    );
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
        let payload = serde_json::to_vec_pretty(config)?;
        let temporary = self
            .path
            .with_extension(format!("json.{}.tmp", uuid::Uuid::new_v4().simple()));
        std::fs::write(&temporary, payload)
            .with_context(|| format!("Failed to write temporary policy {}", temporary.display()))?;
        if self.path.exists() {
            std::fs::remove_file(&self.path).with_context(|| {
                format!("Failed to replace policy file {}", self.path.display())
            })?;
        }
        std::fs::rename(&temporary, &self.path)
            .with_context(|| format!("Failed to activate policy file {}", self.path.display()))?;
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
        .env("CONTINUUM_APPROVAL_SUMMARY", truncate(summary, 1800))
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

fn truncate(value: &str, max_chars: usize) -> String {
    let mut out: String = value.chars().take(max_chars).collect();
    if value.chars().count() > max_chars {
        out.push('…');
    }
    out
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
}

//! # Local action permission gateway
//!
//! This module is the single consent boundary for agent tools. Policies are
//! loaded from the bundled defaults and overlaid by `<data_dir>/permissions.toml`.
//! Approval requests and grants use one file per record so the desktop and MCP
//! processes can coordinate without a shared in-memory lock.

use std::collections::BTreeMap;
use std::fs;
use std::path::{Path, PathBuf};

use anyhow::{anyhow, Context, Result};
use chrono::{DateTime, Duration, Utc};
use serde::{Deserialize, Serialize};
use uuid::Uuid;

use crate::audit::{Actor, AuditLog};

const REQUESTS_DIR: &str = "permission-requests";
const GRANTS_DIR: &str = "permission-grants";
const OVERRIDE_FILE: &str = "permissions.toml";

/// User-configurable consent tier for one tool action.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum PermissionTier {
    /// Execute without an interactive prompt.
    Auto,
    /// Ask once, then allow matching calls for this session.
    SessionApproved,
    /// Ask for every invocation.
    AlwaysConfirm,
    /// Refuse the action.
    Blocked,
}

impl PermissionTier {
    /// Stable TOML token used by the policy files.
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Auto => "auto",
            Self::SessionApproved => "session-approved",
            Self::AlwaysConfirm => "always-confirm",
            Self::Blocked => "blocked",
        }
    }
}

impl std::str::FromStr for PermissionTier {
    type Err = anyhow::Error;

    fn from_str(value: &str) -> Result<Self> {
        match value {
            "auto" => Ok(Self::Auto),
            "session-approved" => Ok(Self::SessionApproved),
            "always-confirm" => Ok(Self::AlwaysConfirm),
            "blocked" => Ok(Self::Blocked),
            other => Err(anyhow!("unknown permission tier {other:?}")),
        }
    }
}

/// Effective policy row returned to the desktop.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct PermissionPolicyEntry {
    /// Exact public MCP tool name.
    pub action: String,
    /// Effective tier after applying user overrides.
    pub tier: PermissionTier,
    /// Whether the value came from `permissions.toml`.
    pub overridden: bool,
}

/// A durable request waiting for a user decision.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct PermissionRequest {
    /// Unique request identifier.
    pub id: String,
    /// Session that attempted the action.
    pub session_id: String,
    /// Exact public tool name.
    pub action: String,
    /// Optional canonical resource, such as a repository or file path.
    pub resource: Option<String>,
    /// Tier that caused the prompt.
    pub tier: PermissionTier,
    /// Sanitized summary suitable for display.
    pub summary: String,
    /// Creation time.
    pub created_at: DateTime<Utc>,
}

/// Scope selected by the user when approving a request.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum GrantScope {
    /// Permit one matching invocation.
    Once,
    /// Permit matching invocations until the session grant expires.
    Session,
}

/// A durable permission grant.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct PermissionGrant {
    /// Unique grant identifier.
    pub id: String,
    /// Session this grant belongs to.
    pub session_id: String,
    /// Exact public tool name.
    pub action: String,
    /// Optional exact resource scope.
    pub resource: Option<String>,
    /// Whether this is one-use or session-long.
    pub scope: GrantScope,
    /// Creation time.
    pub created_at: DateTime<Utc>,
    /// Expiry time.
    pub expires_at: DateTime<Utc>,
}

/// Lightweight health snapshot for the permission gateway.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct PermissionHealth {
    /// Whether bundled defaults and user overrides both parse.
    pub policy_valid: bool,
    /// Number of unreadable request records.
    pub corrupt_requests: usize,
    /// Number of unreadable grant records.
    pub corrupt_grants: usize,
}

impl PermissionHealth {
    /// Returns true when permission decisions can no longer be evaluated safely.
    pub fn should_restart(&self) -> bool {
        !self.policy_valid
    }
}

/// Result of checking an action against the gateway.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "decision", rename_all = "snake_case")]
pub enum PermissionDecision {
    /// The action may proceed.
    Allow,
    /// The action must stop until this request is approved.
    Ask {
        /// Durable request shown by the desktop.
        request: PermissionRequest,
    },
    /// The policy explicitly blocks the action.
    Deny {
        /// Safe reason suitable for model and UI output.
        reason: String,
    },
}

/// Filesystem-backed permission gateway shared by desktop and MCP processes.
#[derive(Debug, Clone)]
pub struct PermissionGateway {
    data_dir: PathBuf,
    session_id: String,
    defaults: &'static str,
    audit: AuditLog,
}

impl PermissionGateway {
    /// Creates a gateway. `defaults` must be the bundled default policy text.
    pub fn new(data_dir: PathBuf, session_id: impl Into<String>, defaults: &'static str) -> Self {
        Self {
            audit: AuditLog::new(&data_dir),
            data_dir,
            session_id: session_id.into(),
            defaults,
        }
    }

    /// Loads the effective policy, failing closed when either policy is invalid.
    pub fn list_policy(&self) -> Result<Vec<PermissionPolicyEntry>> {
        let defaults = parse_policy(self.defaults).context("invalid bundled permission policy")?;
        let overrides =
            if self.override_path().exists() {
                parse_policy(&fs::read_to_string(self.override_path()).with_context(|| {
                    format!("failed to read {}", self.override_path().display())
                })?)
                .context("invalid user permission policy")?
            } else {
                BTreeMap::new()
            };
        Ok(defaults
            .into_iter()
            .map(|(action, default)| PermissionPolicyEntry {
                tier: overrides.get(&action).copied().unwrap_or(default),
                overridden: overrides.contains_key(&action),
                action,
            })
            .collect())
    }

    /// Persists a user override for an existing action.
    pub fn set_policy(&self, action: &str, tier: PermissionTier) -> Result<()> {
        if !parse_policy(self.defaults)?.contains_key(action) {
            return Err(anyhow!("unknown permission action {action:?}"));
        }
        let mut overrides = if self.override_path().exists() {
            parse_policy(&fs::read_to_string(self.override_path())?)?
        } else {
            BTreeMap::new()
        };
        overrides.insert(action.to_string(), tier);
        atomic_write(&self.override_path(), render_policy(&overrides).as_bytes())?;
        self.audit.record(
            "permission_policy_changed",
            Actor::User,
            format!("{action} set to {}", tier.as_str()),
            Some(serde_json::json!({ "action": action, "tier": tier })),
        );
        Ok(())
    }

    /// Checks a call. Missing or malformed policies are denied, never allowed.
    pub fn check(&self, action: &str, resource: Option<&str>, summary: &str) -> PermissionDecision {
        let tier = match self
            .list_policy()
            .ok()
            .and_then(|rows| rows.into_iter().find(|row| row.action == action))
        {
            Some(row) => row.tier,
            None => return self.deny(action, "action has no valid permission policy"),
        };
        match tier {
            PermissionTier::Auto => PermissionDecision::Allow,
            PermissionTier::Blocked => self.deny(action, "action is blocked by user policy"),
            PermissionTier::SessionApproved | PermissionTier::AlwaysConfirm => {
                if self.consume_matching_grant(action, resource) {
                    self.audit.record(
                        "permission_grant_used",
                        Actor::Agent,
                        format!("permission used for {action}"),
                        Some(serde_json::json!({ "action": action, "resource": resource })),
                    );
                    PermissionDecision::Allow
                } else {
                    let request = self
                        .find_pending(action, resource)
                        .unwrap_or_else(|| self.create_request(action, resource, tier, summary));
                    PermissionDecision::Ask { request }
                }
            }
        }
    }

    /// Lists pending requests, oldest first. Corrupt records are skipped.
    pub fn list_requests(&self) -> Vec<PermissionRequest> {
        read_json_records(&self.data_dir.join(REQUESTS_DIR))
    }

    /// Lists non-expired grants. Corrupt and expired records are removed.
    pub fn list_grants(&self) -> Vec<PermissionGrant> {
        let now = Utc::now();
        let mut grants = read_json_records::<PermissionGrant>(&self.data_dir.join(GRANTS_DIR));
        grants.retain(|grant| {
            if grant.expires_at > now {
                true
            } else {
                let _ = fs::remove_file(self.grant_path(&grant.id));
                false
            }
        });
        grants
    }

    /// Returns a non-mutating health snapshot for the repair agent.
    pub fn health(&self) -> PermissionHealth {
        PermissionHealth {
            policy_valid: self.list_policy().is_ok(),
            corrupt_requests: corrupt_record_count(&self.data_dir.join(REQUESTS_DIR)),
            corrupt_grants: corrupt_record_count(&self.data_dir.join(GRANTS_DIR)),
        }
    }

    /// Approves a pending request with a bounded lifetime.
    pub fn approve(
        &self,
        request_id: &str,
        scope: GrantScope,
        ttl_secs: u64,
    ) -> Result<PermissionGrant> {
        let request = read_json::<PermissionRequest>(&self.request_path(request_id))
            .context("permission request does not exist")?;
        if request.tier == PermissionTier::AlwaysConfirm && scope != GrantScope::Once {
            return Err(anyhow!("always-confirm requests can only be approved once"));
        }
        let ttl = ttl_secs.clamp(30, 8 * 60 * 60);
        let grant = PermissionGrant {
            id: Uuid::new_v4().to_string(),
            session_id: request.session_id.clone(),
            action: request.action.clone(),
            resource: request.resource.clone(),
            scope,
            created_at: Utc::now(),
            expires_at: Utc::now() + Duration::seconds(ttl as i64),
        };
        write_json(&self.grant_path(&grant.id), &grant)?;
        fs::remove_file(self.request_path(request_id))?;
        self.audit.record(
            "permission_approved",
            Actor::User,
            format!("approved {} for {:?}", grant.action, scope),
            Some(serde_json::json!({ "request_id": request_id, "grant_id": grant.id, "scope": scope })),
        );
        Ok(grant)
    }

    /// Denies and removes a pending request.
    pub fn deny_request(&self, request_id: &str) -> Result<()> {
        let request = read_json::<PermissionRequest>(&self.request_path(request_id))
            .context("permission request does not exist")?;
        fs::remove_file(self.request_path(request_id))?;
        self.audit.record(
            "permission_denied",
            Actor::User,
            format!("denied {}", request.action),
            Some(serde_json::json!({ "request_id": request_id, "action": request.action })),
        );
        Ok(())
    }

    /// Revokes a previously issued grant.
    pub fn revoke(&self, grant_id: &str) -> Result<()> {
        let grant = read_json::<PermissionGrant>(&self.grant_path(grant_id))
            .context("permission grant does not exist")?;
        fs::remove_file(self.grant_path(grant_id))?;
        self.audit.record(
            "permission_revoked",
            Actor::User,
            format!("revoked {}", grant.action),
            Some(serde_json::json!({ "grant_id": grant_id, "action": grant.action })),
        );
        Ok(())
    }

    fn override_path(&self) -> PathBuf {
        self.data_dir.join(OVERRIDE_FILE)
    }

    fn request_path(&self, id: &str) -> PathBuf {
        self.data_dir.join(REQUESTS_DIR).join(format!("{id}.json"))
    }

    fn grant_path(&self, id: &str) -> PathBuf {
        self.data_dir.join(GRANTS_DIR).join(format!("{id}.json"))
    }

    fn deny(&self, action: &str, reason: &str) -> PermissionDecision {
        self.audit.record(
            "permission_blocked",
            Actor::Agent,
            format!("blocked {action}: {reason}"),
            Some(serde_json::json!({ "action": action, "reason": reason })),
        );
        PermissionDecision::Deny {
            reason: reason.to_string(),
        }
    }

    fn create_request(
        &self,
        action: &str,
        resource: Option<&str>,
        tier: PermissionTier,
        summary: &str,
    ) -> PermissionRequest {
        let request = PermissionRequest {
            id: Uuid::new_v4().to_string(),
            session_id: self.session_id.clone(),
            action: action.to_string(),
            resource: resource.map(str::to_string),
            tier,
            summary: summary.chars().take(400).collect(),
            created_at: Utc::now(),
        };
        if let Err(error) = write_json(&self.request_path(&request.id), &request) {
            tracing::warn!(
                layer = "system",
                component = "permissions",
                error = %error,
                "Failed to persist permission request"
            );
        }
        self.audit.record(
            "permission_requested",
            Actor::Agent,
            format!("approval requested for {action}"),
            Some(serde_json::json!({ "request_id": request.id, "action": action, "resource": resource })),
        );
        request
    }

    fn find_pending(&self, action: &str, resource: Option<&str>) -> Option<PermissionRequest> {
        self.list_requests().into_iter().find(|request| {
            request.session_id == self.session_id
                && request.action == action
                && request.resource.as_deref() == resource
        })
    }

    fn consume_matching_grant(&self, action: &str, resource: Option<&str>) -> bool {
        let now = Utc::now();
        for grant in self.list_grants() {
            if grant.session_id != self.session_id
                || grant.action != action
                || grant.resource.as_deref() != resource
                || grant.expires_at <= now
            {
                continue;
            }
            if grant.scope == GrantScope::Once {
                let source = self.grant_path(&grant.id);
                let claimed = source.with_extension("claimed");
                if fs::rename(&source, &claimed).is_err() {
                    continue;
                }
                let _ = fs::remove_file(claimed);
            }
            return true;
        }
        false
    }
}

fn parse_policy(input: &str) -> Result<BTreeMap<String, PermissionTier>> {
    let root: toml::Table = toml::from_str(input)?;
    let mut out = BTreeMap::new();
    for (_namespace, value) in root {
        let table = value
            .as_table()
            .ok_or_else(|| anyhow!("permission namespaces must be TOML tables"))?;
        for (action, value) in table {
            let raw = value
                .as_str()
                .ok_or_else(|| anyhow!("permission for {action} must be a string"))?;
            out.insert(action.clone(), raw.parse()?);
        }
    }
    Ok(out)
}

fn render_policy(policy: &BTreeMap<String, PermissionTier>) -> String {
    let mut grouped: BTreeMap<&str, Vec<(&str, PermissionTier)>> = BTreeMap::new();
    for (action, tier) in policy {
        let namespace = action.split('_').next().unwrap_or("tools");
        grouped.entry(namespace).or_default().push((action, *tier));
    }
    let mut output = String::from("# User overrides managed by Continuum.\n");
    for (namespace, entries) in grouped {
        output.push_str(&format!("\n[{namespace}]\n"));
        for (action, tier) in entries {
            output.push_str(&format!("{action} = {:?}\n", tier.as_str()));
        }
    }
    output
}

fn read_json<T: for<'de> Deserialize<'de>>(path: &Path) -> Result<T> {
    serde_json::from_slice(&fs::read(path)?).map_err(Into::into)
}

fn read_json_records<T: for<'de> Deserialize<'de> + Ord>(dir: &Path) -> Vec<T> {
    let Ok(entries) = fs::read_dir(dir) else {
        return Vec::new();
    };
    let mut records: Vec<T> = entries
        .flatten()
        .filter_map(|entry| read_json(&entry.path()).ok())
        .collect();
    records.sort();
    records
}

fn corrupt_record_count(dir: &Path) -> usize {
    let Ok(entries) = fs::read_dir(dir) else {
        return 0;
    };
    entries
        .flatten()
        .filter(|entry| read_json::<serde_json::Value>(&entry.path()).is_err())
        .count()
}

fn write_json(path: &Path, value: &impl Serialize) -> Result<()> {
    atomic_write(path, &serde_json::to_vec_pretty(value)?)
}

fn atomic_write(path: &Path, bytes: &[u8]) -> Result<()> {
    let parent = path.parent().ok_or_else(|| anyhow!("path has no parent"))?;
    fs::create_dir_all(parent)?;
    let tmp = parent.join(format!(
        ".{}.{}.tmp",
        path.file_name().unwrap_or_default().to_string_lossy(),
        Uuid::new_v4()
    ));
    fs::write(&tmp, bytes)?;
    fs::rename(&tmp, path).or_else(|error| {
        if path.exists() {
            fs::remove_file(path)?;
            fs::rename(&tmp, path)
        } else {
            Err(error)
        }
    })?;
    Ok(())
}

impl Ord for PermissionRequest {
    fn cmp(&self, other: &Self) -> std::cmp::Ordering {
        self.created_at
            .cmp(&other.created_at)
            .then_with(|| self.id.cmp(&other.id))
    }
}

impl PartialOrd for PermissionRequest {
    fn partial_cmp(&self, other: &Self) -> Option<std::cmp::Ordering> {
        Some(self.cmp(other))
    }
}

impl Ord for PermissionGrant {
    fn cmp(&self, other: &Self) -> std::cmp::Ordering {
        self.created_at
            .cmp(&other.created_at)
            .then_with(|| self.id.cmp(&other.id))
    }
}

impl PartialOrd for PermissionGrant {
    fn partial_cmp(&self, other: &Self) -> Option<std::cmp::Ordering> {
        Some(self.cmp(other))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use tempfile::TempDir;

    const DEFAULTS: &str = r#"
[fs]
fs_read_file = "auto"
fs_write_file = "session-approved"
fs_delete = "always-confirm"
[repair]
repair_restart = "blocked"
"#;

    fn gateway(tmp: &TempDir) -> PermissionGateway {
        PermissionGateway::new(tmp.path().to_path_buf(), "session-a", DEFAULTS)
    }

    #[test]
    fn policy_defaults_and_overrides_round_trip() {
        let tmp = TempDir::new().unwrap();
        let gate = gateway(&tmp);
        gate.set_policy("fs_read_file", PermissionTier::Blocked)
            .unwrap();
        let rows = gate.list_policy().unwrap();
        let row = rows
            .iter()
            .find(|row| row.action == "fs_read_file")
            .unwrap();
        assert_eq!(row.tier, PermissionTier::Blocked);
        assert!(row.overridden);
    }

    #[test]
    fn auto_allows_and_unknown_denies() {
        let tmp = TempDir::new().unwrap();
        let gate = gateway(&tmp);
        assert_eq!(
            gate.check("fs_read_file", None, "read"),
            PermissionDecision::Allow
        );
        assert!(matches!(
            gate.check("missing", None, "no"),
            PermissionDecision::Deny { .. }
        ));
    }

    #[test]
    fn session_request_can_be_approved_and_reused() {
        let tmp = TempDir::new().unwrap();
        let gate = gateway(&tmp);
        let PermissionDecision::Ask { request } = gate.check("fs_write_file", Some("a"), "write")
        else {
            panic!("expected approval request");
        };
        gate.approve(&request.id, GrantScope::Session, 300).unwrap();
        assert_eq!(
            gate.check("fs_write_file", Some("a"), "write"),
            PermissionDecision::Allow
        );
        assert_eq!(
            gate.check("fs_write_file", Some("a"), "write"),
            PermissionDecision::Allow
        );
    }

    #[test]
    fn once_grant_is_consumed() {
        let tmp = TempDir::new().unwrap();
        let gate = gateway(&tmp);
        let PermissionDecision::Ask { request } = gate.check("fs_delete", Some("a"), "delete")
        else {
            panic!("expected approval request");
        };
        gate.approve(&request.id, GrantScope::Once, 300).unwrap();
        assert_eq!(
            gate.check("fs_delete", Some("a"), "delete"),
            PermissionDecision::Allow
        );
        assert!(matches!(
            gate.check("fs_delete", Some("a"), "delete"),
            PermissionDecision::Ask { .. }
        ));
    }

    #[test]
    fn requests_can_be_denied_and_grants_revoked() {
        let tmp = TempDir::new().unwrap();
        let gate = gateway(&tmp);
        let PermissionDecision::Ask { request } = gate.check("fs_write_file", None, "write") else {
            panic!("expected request");
        };
        gate.deny_request(&request.id).unwrap();
        assert!(gate.list_requests().is_empty());
        let PermissionDecision::Ask { request } = gate.check("fs_write_file", None, "write") else {
            panic!("expected request");
        };
        let grant = gate.approve(&request.id, GrantScope::Session, 300).unwrap();
        gate.revoke(&grant.id).unwrap();
        assert!(gate.list_grants().is_empty());
    }

    #[test]
    fn always_confirm_rejects_session_grants() {
        let tmp = TempDir::new().unwrap();
        let gate = gateway(&tmp);
        let PermissionDecision::Ask { request } = gate.check("fs_delete", None, "delete") else {
            panic!("expected request");
        };
        assert!(gate.approve(&request.id, GrantScope::Session, 300).is_err());
    }

    #[test]
    fn health_fails_closed_on_invalid_override() {
        let tmp = TempDir::new().unwrap();
        let gate = gateway(&tmp);
        fs::write(tmp.path().join(OVERRIDE_FILE), "not toml = [").unwrap();
        let health = gate.health();
        assert!(!health.policy_valid);
        assert!(health.should_restart());
    }
}

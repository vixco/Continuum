//! # Applying Context-page intents (spec §4.13)
//!
//! The runtime half of [`crate::context::intents`]: it takes a drained
//! [`ContextIntent`] and performs its effect against the live stores. Every
//! handler here obeys three rules:
//!
//! 1. **Never panic, never kill the loop.** Each handler returns a
//!    `Result`; [`apply_intent`] logs failures and moves on to the next
//!    intent. One malformed project id must not stop a queued deletion.
//! 2. **Live *and* durable.** Anything the user asserts is applied to the
//!    in-memory objects (resolver, session-state hub, toggle control) *and*
//!    persisted (Projects table, `session_pins`, `project_rules`,
//!    `config.toml`), so it survives a restart. When the durable half
//!    fails, the live half still stands and the failure is logged: the
//!    Projects table is authoritative and `config.toml` is only the boot
//!    seed (spec §4.3).
//! 3. **Audited.** Every applied intent writes one
//!    [`crate::audit::AuditEntry`] with `actor: user`.
//!
//! ## The Forget cascade
//!
//! `forget` is keyed on `raw_reference` (spec §4.13) and removes, in order:
//!
//! | rung | store | how |
//! |---|---|---|
//! | `context_events` row + FTS entry | raw log | `DELETE FROM context_events`; the `_ad` trigger drops the FTS mirror |
//! | `perception_frames` row + screenshot | raw log | `RawLog::delete_frame` (file first, then row) |
//! | derived episodic memories | LanceDB | `EpisodicStore::delete_by_source_frames` |
//! | unconfirmed vault candidate | vault | the `Candidate` note whose `source_ref` is `triage:frame:<id>` |
//!
//! Rungs are independent: a failure on one is logged and the rest still
//! run. A deletion that half-succeeds is strictly better than one that
//! aborts on the first locked file.
//!
//! # Layer
//!
//! Cross-layer control plane, runtime-gated (it touches the raw log, the
//! vault and the episodic store).

use std::collections::HashSet;
use std::path::{Path, PathBuf};
use std::sync::Arc;

use anyhow::{anyhow, Result};
use chrono::Utc;
use serde_json::json;
use tokio::sync::Mutex;

use continuum_memory::{NodeStatus, NodeType, NoteDraft, Sensitivity, Source, Vault};

use crate::audit::{Actor, AuditLog};
use crate::config::ProjectConfigEntry;
use crate::config_edit;
use crate::context::intents::{ContextAction, ContextIntent, SessionField, ToggleName};
use crate::context::project::{
    unique_slug, OverrideRule, ProjectEntry, ProjectResolver, ProjectStatus, RuleAction,
};
use crate::context::session_state::SessionStateHub;
use crate::memory::episodic::EpisodicStore;
use crate::memory::raw_log::RawLog;
use crate::senses::privacy::emit_system_event;
use crate::senses::screenshots::ScreenshotPolicy;
use crate::senses::toggles::ToggleControl;

/// The stores the two *deletion* intents touch — `forget` and
/// `delete_range` (spec §4.13).
///
/// Extracted so those handlers can run either inline against an
/// [`IntentContext`] (tests, non-runtime callers) or from owned handles on
/// a spawned task ([`DeferredIntentContext`]). Nothing here borrows the
/// resolver, which is what made the deletion path main-loop-bound.
struct DeletionStores<'a> {
    raw_log: &'a RawLog,
    episodic: &'a Arc<Mutex<EpisodicStore>>,
    vault: &'a Vault,
    screenshot_policy: ScreenshotPolicy,
}

/// Owned handles for applying the deletion intents **off** the runtime's
/// select loop (I1).
///
/// `forget` and `delete_range` await LanceDB, two SQLite `DELETE`s, a
/// screenshot `remove_file` per row and a full `vault.pending()` scan. Run
/// inline on the 250 ms voice ticker, one "delete the last hour" click
/// froze push-to-talk and the hotkey for the duration. The runtime now
/// hands these two kinds to a single serial worker task holding this
/// context, which preserves file order between deletions while keeping the
/// loop responsive.
#[derive(Clone)]
pub struct DeferredIntentContext {
    /// The raw-log database (events, frames, screenshots).
    pub raw_log: RawLog,
    /// Episodic memories (the Forget/delete-range cascade).
    pub episodic: Arc<Mutex<EpisodicStore>>,
    /// The memory vault (candidate cleanup).
    pub vault: Arc<Vault>,
    /// Where audit lines go.
    pub audit: AuditLog,
    /// Screenshot deletion policy from `[storage]`.
    pub screenshot_policy: ScreenshotPolicy,
}

impl DeferredIntentContext {
    fn stores(&self) -> DeletionStores<'_> {
        DeletionStores {
            raw_log: &self.raw_log,
            episodic: &self.episodic,
            vault: &self.vault,
            screenshot_policy: self.screenshot_policy,
        }
    }
}

/// Whether an action must be applied by the deferred worker rather than
/// inline on the runtime's select loop (I1).
///
/// Exactly the two deletion kinds: everything else is a handful of small
/// SQLite statements plus in-memory mutation of the loop-owned resolver.
pub fn is_deferred(action: &ContextAction) -> bool {
    matches!(
        action,
        ContextAction::Forget { .. } | ContextAction::DeleteRange { .. }
    )
}

/// Everything a handler may touch. Borrowed per drain pass from the main
/// loop, which owns the resolver.
pub struct IntentContext<'a> {
    /// The raw-log database (Projects table, rules, pins, events, frames).
    pub raw_log: &'a RawLog,
    /// Episodic memories (the Forget/delete-range cascade).
    pub episodic: &'a Arc<Mutex<EpisodicStore>>,
    /// The memory vault (candidate cleanup, preference notes).
    pub vault: &'a Arc<Vault>,
    /// The live session state.
    pub session: &'a SessionStateHub,
    /// The live project resolver (main-loop owned).
    pub resolver: &'a mut ProjectResolver,
    /// The live observation toggles.
    pub toggles: &'a ToggleControl,
    /// Where audit lines go.
    pub audit: &'a AuditLog,
    /// `<data_dir>/config.toml`.
    pub config_path: PathBuf,
    /// Screenshot deletion policy from `[storage]`.
    pub screenshot_policy: ScreenshotPolicy,
}

impl IntentContext<'_> {
    fn stores(&self) -> DeletionStores<'_> {
        DeletionStores {
            raw_log: self.raw_log,
            episodic: self.episodic,
            vault: self.vault,
            screenshot_policy: self.screenshot_policy,
        }
    }
}

/// What one applied intent did, for the audit line and the caller's log.
#[derive(Debug, Clone, PartialEq)]
pub struct IntentOutcome {
    /// Whether anything actually changed.
    pub applied: bool,
    /// One human-readable line.
    pub summary: String,
    /// Structured detail for the audit JSONL.
    pub details: serde_json::Value,
}

impl IntentOutcome {
    fn new(applied: bool, summary: impl Into<String>, details: serde_json::Value) -> Self {
        Self {
            applied,
            summary: summary.into(),
            details,
        }
    }
}

/// Audit `kind` for one action, chosen so the audit log groups the way the
/// spec's four families do (wake / tool / toggle / correction / delete).
fn audit_kind(action: &ContextAction) -> &'static str {
    match action {
        ContextAction::SetToggle { .. } => "toggle_change",
        ContextAction::Forget { .. } | ContextAction::DeleteRange { .. } => "deletion",
        ContextAction::RecordUserCommand { .. } => "user_command",
        _ => "correction",
    }
}

/// Applies one intent, logging and swallowing any failure.
///
/// Returns the outcome for tests and the caller's tracing; the audit line
/// is written here so no call site can forget it.
pub async fn apply_intent(ctx: &mut IntentContext<'_>, intent: &ContextIntent) -> IntentOutcome {
    let result = match &intent.action {
        ContextAction::AddProject {
            name,
            root_path,
            project_id,
        } => add_project(ctx, name, root_path, project_id.as_deref()).await,
        ContextAction::ConfirmProject { project_id } => confirm_project(ctx, project_id).await,
        ContextAction::Correct {
            field,
            value,
            match_process,
            match_title_substring,
        } => {
            correct(
                ctx,
                *field,
                value,
                match_process.as_deref(),
                match_title_substring.as_deref(),
            )
            .await
        }
        ContextAction::NotThisProject {
            project_id,
            match_process,
            match_title_substring,
        } => {
            not_this_project(
                ctx,
                project_id,
                match_process.as_deref(),
                match_title_substring.as_deref(),
            )
            .await
        }
        ContextAction::Pin { field, value } => pin(ctx, *field, value.as_deref()).await,
        ContextAction::Forget {
            raw_reference,
            event_id,
        } => forget(&ctx.stores(), raw_reference.as_deref(), *event_id).await,
        ContextAction::DeleteRange { from, to } => delete_range(&ctx.stores(), *from, *to).await,
        ContextAction::SetToggle { name, value } => set_toggle(ctx, *name, *value).await,
        ContextAction::RecordUserCommand { text } => record_user_command(ctx, text),
    };

    finish_intent(ctx.audit, intent, result)
}

/// Applies one deletion intent from owned handles — the spawned-worker
/// half of [`apply_intent`] (I1).
///
/// Only [`is_deferred`] actions are accepted; anything else is a wiring
/// bug and is reported as a failed intent rather than silently dropped.
/// Logging and the audit line are identical to the inline path.
pub async fn apply_deferred_intent(
    ctx: &DeferredIntentContext,
    intent: &ContextIntent,
) -> IntentOutcome {
    let result = match &intent.action {
        ContextAction::Forget {
            raw_reference,
            event_id,
        } => forget(&ctx.stores(), raw_reference.as_deref(), *event_id).await,
        ContextAction::DeleteRange { from, to } => delete_range(&ctx.stores(), *from, *to).await,
        other => Err(anyhow!(
            "{} is not a deferred intent kind",
            other.kind_label()
        )),
    };
    finish_intent(&ctx.audit, intent, result)
}

/// Logs the handler's result and writes the intent's audit line. Shared by
/// both entry points so neither can forget it.
fn finish_intent(
    audit: &AuditLog,
    intent: &ContextIntent,
    result: Result<IntentOutcome>,
) -> IntentOutcome {
    let label = intent.action.kind_label();
    let outcome = match result {
        Ok(outcome) => {
            tracing::info!(
                layer = "context",
                component = "intent",
                intent = label,
                intent_id = %intent.id,
                applied = outcome.applied,
                summary = %outcome.summary,
                "Applied context intent"
            );
            outcome
        }
        Err(e) => {
            tracing::warn!(
                layer = "context",
                component = "intent",
                intent = label,
                intent_id = %intent.id,
                error = %e,
                "Context intent failed"
            );
            IntentOutcome::new(
                false,
                format!("{label} failed: {e}"),
                json!({ "error": e.to_string() }),
            )
        }
    };

    let mut details = outcome.details.clone();
    if let Some(obj) = details.as_object_mut() {
        obj.insert("intent".into(), json!(label));
        obj.insert("intent_id".into(), json!(intent.id));
        obj.insert("applied".into(), json!(outcome.applied));
    }
    audit.record(
        audit_kind(&intent.action),
        Actor::User,
        outcome.summary.clone(),
        Some(details),
    );
    outcome
}

// ---------------------------------------------------------------------------
// Projects
// ---------------------------------------------------------------------------

/// Mirrors a resolver entry into `config.toml`. Failure is logged, not
/// propagated: the Projects table is authoritative (spec §4.3), and boot
/// reconciliation re-seeds config from it anyway.
fn mirror_project_to_config(config_path: &Path, entry: &ProjectEntry) {
    let cfg_entry = ProjectConfigEntry {
        id: entry.id.clone(),
        name: entry.name.clone(),
        root_paths: entry.root_paths.clone(),
        repo: entry.repo.clone(),
        keywords: entry.keywords.clone(),
        zone: entry.zone,
    };
    if let Err(e) = config_edit::upsert_known_project(config_path, &cfg_entry) {
        tracing::warn!(
            layer = "context",
            component = "intent",
            project = %entry.id,
            path = %config_path.display(),
            error = %e,
            "Could not mirror the project into config.toml; the Projects table stays authoritative"
        );
    }
}

async fn add_project(
    ctx: &mut IntentContext<'_>,
    name: &str,
    root_path: &str,
    explicit_id: Option<&str>,
) -> Result<IntentOutcome> {
    let name = name.trim();
    let root_path = root_path.trim();
    if name.is_empty() {
        return Err(anyhow!("a project needs a name"));
    }
    if root_path.is_empty() {
        return Err(anyhow!("a project needs a root path"));
    }
    if !Path::new(root_path).is_absolute() {
        return Err(anyhow!("root path must be absolute: {root_path}"));
    }

    let existing: HashSet<String> = ctx
        .resolver
        .projects()
        .iter()
        .map(|p| p.id.clone())
        .collect();
    let id = match explicit_id {
        Some(id) if crate::context::project::is_valid_project_id(id) => id.to_string(),
        // An explicit-but-invalid id is slugified rather than rejected —
        // the user typed a name into a form, not a slug.
        Some(id) => unique_slug(id, &existing),
        None => unique_slug(name, &existing),
    };

    // A project the user typed in by hand is confirmed by definition: they
    // just told us it exists and where it lives. Only auto-discovery
    // produces unconfirmed candidates.
    let entry = ProjectEntry {
        id: id.clone(),
        name: name.to_string(),
        root_paths: vec![root_path.to_string()],
        repo: None,
        keywords: Vec::new(),
        zone: None,
        status: ProjectStatus::Confirmed,
    };

    ctx.raw_log.upsert_project(&entry).await?;
    ctx.resolver.upsert_project(entry.clone());
    mirror_project_to_config(&ctx.config_path, &entry);

    Ok(IntentOutcome::new(
        true,
        format!("Added project \"{name}\" ({id})"),
        json!({ "project_id": id, "root_path": root_path }),
    ))
}

async fn confirm_project(ctx: &mut IntentContext<'_>, project_id: &str) -> Result<IntentOutcome> {
    let confirmed = ctx.raw_log.confirm_project(project_id).await?;
    let live = ctx.resolver.confirm_project(project_id);
    if !confirmed && !live {
        return Err(anyhow!("no such project: {project_id}"));
    }
    if let Some(entry) = ctx
        .resolver
        .projects()
        .iter()
        .find(|p| p.id == project_id)
        .cloned()
    {
        mirror_project_to_config(&ctx.config_path, &entry);
    }
    Ok(IntentOutcome::new(
        true,
        format!("Confirmed project {project_id}"),
        json!({ "project_id": project_id }),
    ))
}

// ---------------------------------------------------------------------------
// Corrections, exclusions, pins
// ---------------------------------------------------------------------------

/// Persists a tier-0 rule live + durably.
async fn persist_rule(ctx: &mut IntentContext<'_>, rule: OverrideRule) -> Result<()> {
    ctx.raw_log.add_project_rule(&rule).await?;
    ctx.resolver.add_rule(rule);
    Ok(())
}

/// Builds the match criteria for a derived rule. When the caller gave
/// neither, the current foreground process is used — a rule with no
/// criteria never matches anything (`OverrideRule::matches`), so silently
/// writing one would look like it worked and do nothing.
fn rule_scope(
    ctx: &IntentContext<'_>,
    match_process: Option<&str>,
    match_title_substring: Option<&str>,
) -> Result<(Option<String>, Option<String>)> {
    let process = match_process
        .map(str::trim)
        .filter(|s| !s.is_empty())
        .map(str::to_string);
    let title = match_title_substring
        .map(str::trim)
        .filter(|s| !s.is_empty())
        .map(str::to_string);
    if process.is_some() || title.is_some() {
        return Ok((process, title));
    }
    let fallback = ctx.session.snapshot().active_app.filter(|app| {
        // The privacy sentinel is not an application name.
        app != crate::senses::privacy::EXCLUDED_PROCESS && !app.is_empty()
    });
    match fallback {
        Some(app) => Ok((Some(app), None)),
        None => Err(anyhow!(
            "no scope for the rule: pass match_process or match_title_substring"
        )),
    }
}

/// Writes the Confirmed preference note a correction leaves behind
/// (spec §4.13), zone-inherited per the §4.1 propagation rule: a
/// correction made while the session state was `local_only` becomes a
/// `Sensitive` note, so the cloud gate strips it from cloud-bound context.
async fn write_correction_note(
    vault: &Vault,
    field: SessionField,
    value: &str,
    project: Option<String>,
    local_only: bool,
) -> Result<String> {
    let draft = NoteDraft {
        node_type: NodeType::Preference,
        title: format!("User corrected {}", field.as_str()),
        body: format!(
            "The user corrected Continuum's belief about the current {} to \"{}\" \
             from the Context page.",
            field.as_str(),
            value
        ),
        project,
        status: NodeStatus::Confirmed,
        confidence: 1.0,
        importance: 0.6,
        source: Source::UserStatement,
        source_ref: Some(format!("context-page:correct:{}", field.as_str())),
        sensitivity: if local_only {
            Sensitivity::Sensitive
        } else {
            Sensitivity::default()
        },
        expires: None,
        relations: Vec::new(),
        tags: vec!["context-page".into(), "correction".into()],
    };
    let note = vault.create(draft).await?;
    Ok(note.frontmatter.id)
}

async fn correct(
    ctx: &mut IntentContext<'_>,
    field: SessionField,
    value: &str,
    match_process: Option<&str>,
    match_title_substring: Option<&str>,
) -> Result<IntentOutcome> {
    let value = value.trim();
    if value.is_empty() {
        return Err(anyhow!("a correction needs a value"));
    }
    if field == SessionField::Project && !crate::context::project::is_valid_project_id(value) {
        return Err(anyhow!(
            "\"{value}\" is not a project id; add the project first"
        ));
    }

    let before = ctx.session.snapshot();
    ctx.session.apply_correction(field, value, Utc::now());

    let mut rule_written = false;
    if field == SessionField::Project {
        let (process, title) = rule_scope(ctx, match_process, match_title_substring)?;
        persist_rule(
            ctx,
            OverrideRule {
                match_process: process,
                match_title_substring: title,
                action: RuleAction::ForceProject,
                project_id: value.to_string(),
            },
        )
        .await?;
        rule_written = true;
    }

    // The note is the last rung: a vault hiccup must not undo a correction
    // the user can see on screen.
    let note_id = match write_correction_note(
        ctx.vault,
        field,
        value,
        before.active_project.clone(),
        before.local_only,
    )
    .await
    {
        Ok(id) => Some(id),
        Err(e) => {
            tracing::warn!(
                layer = "context",
                component = "intent",
                error = %e,
                "Could not write the correction preference note"
            );
            None
        }
    };

    Ok(IntentOutcome::new(
        true,
        format!("Corrected {} to \"{}\"", field.as_str(), value),
        json!({
            "field": field.as_str(),
            "value": value,
            "rule_written": rule_written,
            "note_id": note_id,
        }),
    ))
}

async fn not_this_project(
    ctx: &mut IntentContext<'_>,
    project_id: &str,
    match_process: Option<&str>,
    match_title_substring: Option<&str>,
) -> Result<IntentOutcome> {
    if !crate::context::project::is_valid_project_id(project_id) {
        return Err(anyhow!("\"{project_id}\" is not a project id"));
    }
    let (process, title) = rule_scope(ctx, match_process, match_title_substring)?;
    persist_rule(
        ctx,
        OverrideRule {
            match_process: process.clone(),
            match_title_substring: title.clone(),
            action: RuleAction::ExcludeProject,
            project_id: project_id.to_string(),
        },
    )
    .await?;
    Ok(IntentOutcome::new(
        true,
        format!("This is not {project_id}"),
        json!({
            "project_id": project_id,
            "match_process": process,
            "match_title_substring": title,
        }),
    ))
}

async fn pin(
    ctx: &mut IntentContext<'_>,
    field: SessionField,
    value: Option<&str>,
) -> Result<IntentOutcome> {
    let value = value.map(str::trim).filter(|v| !v.is_empty());
    ctx.session.set_pin(field, value, Utc::now());
    match value {
        Some(v) => {
            ctx.raw_log
                .set_session_pin(field.as_str(), Some(v), None)
                .await?;
            Ok(IntentOutcome::new(
                true,
                format!("Pinned {} to \"{}\"", field.as_str(), v),
                json!({ "field": field.as_str(), "value": v }),
            ))
        }
        None => {
            let existed = ctx.raw_log.clear_session_pin(field.as_str()).await?;
            Ok(IntentOutcome::new(
                true,
                format!("Cleared the {} pin", field.as_str()),
                json!({ "field": field.as_str(), "existed": existed }),
            ))
        }
    }
}

/// Records `last_user_command` on behalf of an out-of-process producer
/// (fixwave 3b, I9 — the desktop chat).
///
/// Synchronous under the hood: session state is in-memory, so this costs
/// one lock. The audit detail deliberately carries only a length, never the
/// text — the user's own words already live in the session state and the
/// chat transcript, and the audit log is a plaintext JSONL file.
fn record_user_command(ctx: &mut IntentContext<'_>, text: &str) -> Result<IntentOutcome> {
    let text = text.trim();
    if text.is_empty() {
        return Ok(IntentOutcome::new(
            false,
            "Empty user command ignored",
            json!({ "source": "chat" }),
        ));
    }
    ctx.session.note_user_command(text, "chat", Utc::now());
    Ok(IntentOutcome::new(
        true,
        "Recorded the last user command from chat",
        json!({ "source": "chat", "chars": text.chars().count() }),
    ))
}

// ---------------------------------------------------------------------------
// Deletion
// ---------------------------------------------------------------------------

/// Deletes the unconfirmed vault candidate derived from a frame, if any.
///
/// Candidates carry `source_ref = "triage:frame:<uuid>"` (Task B3). The
/// vault index has no `source_ref` column to query on, so the pending list
/// is scanned — it is short by construction (candidates are resolved or
/// expire), and this runs once per user click.
async fn delete_candidate_for_frame(vault: &Vault, frame_id: &str) -> Result<u64> {
    delete_candidates_for_frames(vault, std::slice::from_ref(&frame_id.to_string())).await
}

/// The bulk form: one sweep of `vault.pending()` for a whole set of frame
/// ids (fixwave 2, M7 — `delete_range` purges frames, events, screenshots
/// and episodic memories, so the vault candidates *derived from* those
/// frames have to go with them, exactly like `forget` does for one frame).
///
/// Deliberately one pass over `pending()` rather than one pass per frame: a
/// range purge can cover thousands of frames.
async fn delete_candidates_for_frames(vault: &Vault, frame_ids: &[String]) -> Result<u64> {
    if frame_ids.is_empty() {
        return Ok(0);
    }
    let needles: std::collections::HashSet<String> = frame_ids
        .iter()
        .map(|id| format!("triage:frame:{id}"))
        .collect();
    let mut deleted = 0u64;
    for summary in vault.pending().await? {
        let note = match vault.get(&summary.id).await {
            Ok(n) => n,
            Err(_) => continue,
        };
        if note
            .frontmatter
            .source_ref
            .as_deref()
            .is_some_and(|r| needles.contains(r))
        {
            vault.delete(&summary.id).await?;
            deleted += 1;
        }
    }
    Ok(deleted)
}

async fn forget(
    ctx: &DeletionStores<'_>,
    raw_reference: Option<&str>,
    event_id: Option<i64>,
) -> Result<IntentOutcome> {
    // Resolve the cascade key. An explicit event id wins and supplies its
    // own raw_reference when the caller did not pass one.
    let mut events_deleted = 0u64;
    let mut reference = raw_reference
        .map(str::trim)
        .filter(|s| !s.is_empty())
        .map(str::to_string);

    if let Some(id) = event_id {
        if let Some(row) = ctx.raw_log.context_event(id).await? {
            if reference.is_none() {
                reference = row.raw_reference.clone();
            }
        }
        events_deleted += ctx.raw_log.delete_context_event(id).await?;
    }

    let Some(reference) = reference else {
        if events_deleted == 0 {
            return Err(anyhow!("forget needs a raw_reference or an event_id"));
        }
        return Ok(IntentOutcome::new(
            true,
            "Forgot 1 event (no raw reference to cascade from)".to_string(),
            json!({ "events": events_deleted, "frames": 0, "episodic": 0, "candidates": 0 }),
        ));
    };

    // Rung 1: every context_events row pointing at the same observation.
    events_deleted += ctx
        .raw_log
        .delete_context_events_by_raw_reference(&reference)
        .await?;

    // Rungs 2-4 only apply when the reference is a perception frame. A git
    // OID or a synthetic reference has no frame, no screenshot and no
    // derived memory — deleting the event row is the whole cascade.
    let frame_id = uuid::Uuid::parse_str(&reference).ok();
    let mut frames_deleted = 0u64;
    let mut screenshots_deleted = 0usize;
    let mut episodic_deleted = 0u64;
    let mut candidates_deleted = 0u64;

    if let Some(frame_id) = frame_id {
        match ctx
            .raw_log
            // I2: explicit deletion is never gated on the rotation knob.
            .delete_frame(frame_id, &ctx.screenshot_policy.for_explicit_delete())
            .await
        {
            Ok((rows, files)) => {
                frames_deleted = rows;
                screenshots_deleted = files;
            }
            Err(e) => tracing::warn!(
                layer = "context",
                component = "intent",
                error = %e,
                "Forget: frame deletion failed; continuing the cascade"
            ),
        }

        {
            let store = ctx.episodic.lock().await;
            match store
                .delete_by_source_frames(std::slice::from_ref(&reference))
                .await
            {
                Ok(n) => episodic_deleted = n,
                Err(e) => tracing::warn!(
                    layer = "context",
                    component = "intent",
                    error = %e,
                    "Forget: episodic deletion failed; continuing the cascade"
                ),
            }
        }

        match delete_candidate_for_frame(ctx.vault, &reference).await {
            Ok(n) => candidates_deleted = n,
            Err(e) => tracing::warn!(
                layer = "context",
                component = "intent",
                error = %e,
                "Forget: vault candidate cleanup failed"
            ),
        }
    }

    Ok(IntentOutcome::new(
        true,
        format!(
            "Forgot {events_deleted} event(s), {frames_deleted} frame(s), \
             {episodic_deleted} memory(ies), {candidates_deleted} candidate(s)"
        ),
        json!({
            "raw_reference": reference,
            "events": events_deleted,
            "frames": frames_deleted,
            "screenshots": screenshots_deleted,
            "episodic": episodic_deleted,
            "candidates": candidates_deleted,
        }),
    ))
}

async fn delete_range(
    ctx: &DeletionStores<'_>,
    from: chrono::DateTime<Utc>,
    to: chrono::DateTime<Utc>,
) -> Result<IntentOutcome> {
    if to < from {
        return Err(anyhow!("delete range ends before it starts"));
    }

    // Episodic first: once the frames are gone their ids cannot be
    // recovered, and the memories are keyed on their own timestamps
    // anyway.
    let mut episodic_deleted = 0u64;
    {
        let store = ctx.episodic.lock().await;
        match store.delete_ts_range(from, to).await {
            Ok(n) => episodic_deleted = n,
            Err(e) => tracing::warn!(
                layer = "context",
                component = "intent",
                error = %e,
                "Delete-range: episodic purge failed; continuing"
            ),
        }
    }

    // M7: the frame ids have to be read *before* the purge — vault
    // candidates are keyed on `triage:frame:<id>` and the ids die with the
    // rows. Best-effort: a failed id read must not block the purge.
    let frame_ids = match ctx.raw_log.frame_ids_in_range(from, to).await {
        Ok(ids) => ids,
        Err(e) => {
            tracing::warn!(
                layer = "context",
                component = "intent",
                error = %e,
                "Delete-range: frame id read failed; vault candidates will not cascade"
            );
            Vec::new()
        }
    };

    let stats = ctx
        .raw_log
        // I2: explicit deletion is never gated on the rotation knob.
        .delete_range(from, to, &ctx.screenshot_policy.for_explicit_delete())
        .await?;

    // M7: same cascade rung `forget` has — candidates derived from purged
    // frames go with them, so a "delete the last hour" never leaves a
    // pending decision quoting a frame the user just erased.
    let mut candidates_deleted = 0u64;
    match delete_candidates_for_frames(ctx.vault, &frame_ids).await {
        Ok(n) => candidates_deleted = n,
        Err(e) => tracing::warn!(
            layer = "context",
            component = "intent",
            error = %e,
            "Delete-range: vault candidate cleanup failed"
        ),
    }

    Ok(IntentOutcome::new(
        true,
        format!(
            "Purged {} frame(s), {} event(s), {} memory(ies) and {} candidate(s) between {} and {}",
            stats.frames_deleted,
            stats.events_deleted,
            episodic_deleted,
            candidates_deleted,
            from.to_rfc3339(),
            to.to_rfc3339()
        ),
        json!({
            "from": from.to_rfc3339(),
            "to": to.to_rfc3339(),
            "frames": stats.frames_deleted,
            "events": stats.events_deleted,
            "screenshots": stats.screenshots_deleted,
            "episodic": episodic_deleted,
            "candidates": candidates_deleted,
        }),
    ))
}

// ---------------------------------------------------------------------------
// Toggles
// ---------------------------------------------------------------------------

async fn set_toggle(
    ctx: &mut IntentContext<'_>,
    name: ToggleName,
    value: bool,
) -> Result<IntentOutcome> {
    let changed = ctx.toggles.set(name, value);
    if !changed {
        return Ok(IntentOutcome::new(
            false,
            format!(
                "{} observation was already {}",
                name.as_str(),
                if value { "on" } else { "off" }
            ),
            json!({ "name": name.as_str(), "value": value }),
        ));
    }

    // Every toggle change writes a `system`/`toggle_change` event
    // (spec §4.1). Log-only when no events channel is installed.
    emit_system_event(
        "toggle_change",
        &format!(
            "{} observation {} from the Context page",
            name.as_str(),
            if value { "enabled" } else { "disabled" }
        ),
    );

    if let Err(e) = config_edit::set_toggle(&ctx.config_path, name, value) {
        tracing::warn!(
            layer = "context",
            component = "intent",
            toggle = name.as_str(),
            path = %ctx.config_path.display(),
            error = %e,
            "Toggle applied live but could not be persisted to config.toml"
        );
    }

    Ok(IntentOutcome::new(
        true,
        format!(
            "{} observation {}",
            name.as_str(),
            if value { "enabled" } else { "disabled" }
        ),
        json!({ "name": name.as_str(), "value": value }),
    ))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::config::ProjectsConfig;
    use crate::context::intents::ContextAction;
    use crate::memory::events::{ContextEvent, EventSensitivity, EventSource, EventType};
    use crate::senses::types::{ContextObservation, PerceptionFrame, ScreenObservation};
    use chrono::{DateTime, Duration};
    use tempfile::TempDir;

    struct Harness {
        _dir: TempDir,
        data_dir: PathBuf,
        raw_log: RawLog,
        episodic: Arc<Mutex<EpisodicStore>>,
        vault: Arc<Vault>,
        session: SessionStateHub,
        resolver: ProjectResolver,
        toggles: ToggleControl,
        audit: AuditLog,
        screenshot_policy: ScreenshotPolicy,
    }

    impl Harness {
        async fn new() -> Self {
            let dir = TempDir::new().unwrap();
            let data_dir = dir.path().to_path_buf();
            let raw_log = RawLog::open(&data_dir.join("raw.sqlite").to_string_lossy())
                .await
                .unwrap();
            let episodic = Arc::new(Mutex::new(
                EpisodicStore::open_for_test(&data_dir.join("lance").to_string_lossy())
                    .await
                    .unwrap(),
            ));
            let vault = Arc::new(Vault::open(&data_dir.join("vault")).await.unwrap());
            Self {
                data_dir,
                raw_log,
                episodic,
                vault,
                session: SessionStateHub::new(),
                resolver: ProjectResolver::new(Vec::new(), Vec::new(), &ProjectsConfig::default()),
                toggles: ToggleControl::default(),
                audit: AuditLog::new(dir.path()),
                screenshot_policy: ScreenshotPolicy::default(),
                _dir: dir,
            }
        }

        fn ctx(&mut self) -> IntentContext<'_> {
            IntentContext {
                raw_log: &self.raw_log,
                episodic: &self.episodic,
                vault: &self.vault,
                session: &self.session,
                resolver: &mut self.resolver,
                toggles: &self.toggles,
                audit: &self.audit,
                config_path: self.data_dir.join("config.toml"),
                screenshot_policy: self.screenshot_policy,
            }
        }

        async fn apply(&mut self, action: ContextAction) -> IntentOutcome {
            let intent = ContextIntent::new(action);
            let mut ctx = self.ctx();
            apply_intent(&mut ctx, &intent).await
        }
    }

    fn frame(id: uuid::Uuid, ts: DateTime<Utc>, screenshot: Option<String>) -> PerceptionFrame {
        PerceptionFrame {
            id,
            ts,
            screen: ScreenObservation {
                description: "a screen".into(),
                world_compact: None,
                foreground_app: "Code.exe".into(),
                has_error_visible: false,
                confidence: 0.9,
                screenshot_path: screenshot,
                ts,
            },
            audio: None,
            context: ContextObservation {
                foreground_process_name: "Code.exe".into(),
                foreground_window_title: "main.rs".into(),
                ts,
                ..Default::default()
            },
            salience_hint: 0.5,
        }
    }

    fn event(ts: DateTime<Utc>, raw_reference: Option<String>) -> ContextEvent {
        ContextEvent {
            ts,
            source: EventSource::Screen,
            application: "Code.exe".into(),
            window_title: "main.rs".into(),
            project_id: None,
            event_type: EventType::Error,
            summary: "build failed".into(),
            importance: 0.8,
            confidence: 0.9,
            sensitivity: EventSensitivity::CloudAllowed,
            raw_reference,
        }
    }

    // --- projects ---

    #[tokio::test]
    async fn add_project_writes_table_resolver_and_config() {
        let mut h = Harness::new().await;
        let out = h
            .apply(ContextAction::AddProject {
                name: "Sim Charts".into(),
                root_path: "D:/work/simcharts".into(),
                project_id: None,
            })
            .await;
        assert!(out.applied, "{out:?}");

        let rows = h.raw_log.list_projects().await.unwrap();
        assert_eq!(rows.len(), 1);
        assert_eq!(rows[0].entry.id, "sim-charts");
        assert_eq!(rows[0].entry.status, ProjectStatus::Confirmed);
        assert!(h.resolver.projects().iter().any(|p| p.id == "sim-charts"));

        let cfg = crate::config::load_config(&h.data_dir.join("config.toml")).unwrap();
        assert_eq!(cfg.projects.known.len(), 1);
        assert_eq!(cfg.projects.known[0].id, "sim-charts");
    }

    #[tokio::test]
    async fn add_project_rejects_a_relative_root() {
        let mut h = Harness::new().await;
        let out = h
            .apply(ContextAction::AddProject {
                name: "x".into(),
                root_path: "relative/path".into(),
                project_id: None,
            })
            .await;
        assert!(!out.applied);
        assert!(h.raw_log.list_projects().await.unwrap().is_empty());
    }

    #[tokio::test]
    async fn confirm_project_promotes_a_discovered_candidate() {
        let mut h = Harness::new().await;
        let candidate = ProjectEntry {
            id: "discovered-one".into(),
            name: "Discovered One".into(),
            root_paths: vec!["D:/work/one".into()],
            repo: None,
            keywords: Vec::new(),
            zone: None,
            status: ProjectStatus::Discovered,
        };
        h.raw_log.upsert_project(&candidate).await.unwrap();
        h.resolver.upsert_project(candidate);

        let out = h
            .apply(ContextAction::ConfirmProject {
                project_id: "discovered-one".into(),
            })
            .await;
        assert!(out.applied, "{out:?}");

        let rows = h.raw_log.list_projects().await.unwrap();
        assert_eq!(rows[0].entry.status, ProjectStatus::Confirmed);
        assert!(h
            .resolver
            .projects()
            .iter()
            .find(|p| p.id == "discovered-one")
            .unwrap()
            .participates());
        let cfg = crate::config::load_config(&h.data_dir.join("config.toml")).unwrap();
        assert_eq!(cfg.projects.known.len(), 1);
    }

    #[tokio::test]
    async fn confirm_unknown_project_fails_without_touching_anything() {
        let mut h = Harness::new().await;
        let out = h
            .apply(ContextAction::ConfirmProject {
                project_id: "ghost".into(),
            })
            .await;
        assert!(!out.applied);
        assert!(h.raw_log.list_projects().await.unwrap().is_empty());
    }

    // --- corrections ---

    #[tokio::test]
    async fn correcting_the_project_writes_state_rule_and_note() {
        let mut h = Harness::new().await;
        h.session
            .apply_frame(&frame(uuid::Uuid::new_v4(), Utc::now(), None), None);

        let out = h
            .apply(ContextAction::Correct {
                field: SessionField::Project,
                value: "continuum".into(),
                match_process: None,
                match_title_substring: None,
            })
            .await;
        assert!(out.applied, "{out:?}");

        let state = h.session.snapshot();
        assert_eq!(state.active_project.as_deref(), Some("continuum"));
        assert!(state.is_user_confirmed(SessionField::Project));

        let rules = h.raw_log.list_project_rules().await.unwrap();
        assert_eq!(rules.len(), 1);
        assert_eq!(rules[0].action, RuleAction::ForceProject);
        assert_eq!(rules[0].project_id, "continuum");
        // Scope defaulted to the foreground process.
        assert_eq!(rules[0].match_process.as_deref(), Some("Code.exe"));

        let notes = h.vault.search("corrected", 10).await.unwrap();
        assert!(
            notes
                .iter()
                .any(|n| n.node_type == NodeType::Preference && n.status == NodeStatus::Confirmed),
            "a Confirmed preference note is written"
        );
    }

    #[tokio::test]
    async fn correcting_a_goal_marks_it_confident_and_confirmed() {
        let mut h = Harness::new().await;
        let out = h
            .apply(ContextAction::Correct {
                field: SessionField::Goal,
                value: "ship the context engine".into(),
                match_process: None,
                match_title_substring: None,
            })
            .await;
        assert!(out.applied, "{out:?}");
        let state = h.session.snapshot();
        assert_eq!(
            state.current_goal.as_deref(),
            Some("ship the context engine")
        );
        assert_eq!(state.confidence, 1.0);
        assert!(state.is_user_confirmed(SessionField::Goal));
        // No resolver rule for a goal correction.
        assert!(h.raw_log.list_project_rules().await.unwrap().is_empty());
    }

    #[tokio::test]
    async fn a_local_only_session_produces_a_sensitive_correction_note() {
        let mut h = Harness::new().await;
        {
            let mut state = h.session.snapshot();
            state.local_only = true;
            h.session.replace(state);
        }
        h.apply(ContextAction::Correct {
            field: SessionField::Task,
            value: "private work".into(),
            match_process: None,
            match_title_substring: None,
        })
        .await;

        let pending = h.vault.search("corrected", 10).await.unwrap();
        let note = pending.first().expect("note written");
        assert_eq!(note.sensitivity, Sensitivity::Sensitive);
    }

    #[tokio::test]
    async fn correcting_a_project_to_a_non_slug_is_refused() {
        let mut h = Harness::new().await;
        let out = h
            .apply(ContextAction::Correct {
                field: SessionField::Project,
                value: "Not A Slug".into(),
                match_process: Some("Code.exe".into()),
                match_title_substring: None,
            })
            .await;
        assert!(!out.applied);
        assert!(h.raw_log.list_project_rules().await.unwrap().is_empty());
    }

    #[tokio::test]
    async fn not_this_project_persists_an_exclude_rule() {
        let mut h = Harness::new().await;
        let out = h
            .apply(ContextAction::NotThisProject {
                project_id: "continuum".into(),
                match_process: None,
                match_title_substring: Some("scratch".into()),
            })
            .await;
        assert!(out.applied, "{out:?}");
        let rules = h.raw_log.list_project_rules().await.unwrap();
        assert_eq!(rules.len(), 1);
        assert_eq!(rules[0].action, RuleAction::ExcludeProject);
        assert_eq!(rules[0].match_title_substring.as_deref(), Some("scratch"));
        assert!(rules[0].match_process.is_none());
    }

    #[tokio::test]
    async fn a_rule_with_no_derivable_scope_is_refused() {
        let mut h = Harness::new().await;
        // No frame applied, so there is no foreground process to fall back on.
        let out = h
            .apply(ContextAction::NotThisProject {
                project_id: "continuum".into(),
                match_process: None,
                match_title_substring: None,
            })
            .await;
        assert!(!out.applied);
        assert!(h.raw_log.list_project_rules().await.unwrap().is_empty());
    }

    // --- pins ---

    #[tokio::test]
    async fn pin_persists_and_blocks_frame_overwrite_until_cleared() {
        let mut h = Harness::new().await;
        let out = h
            .apply(ContextAction::Pin {
                field: SessionField::Project,
                value: Some("continuum".into()),
            })
            .await;
        assert!(out.applied, "{out:?}");

        let pins = h.raw_log.list_session_pins().await.unwrap();
        assert_eq!(pins.len(), 1);
        assert_eq!(pins[0].0, "project");
        assert_eq!(pins[0].1.as_deref(), Some("continuum"));

        // A frame resolving another project must not move the field.
        let other = crate::context::project::CurrentProject {
            id: "other".into(),
            name: "Other".into(),
            root_path: None,
            confidence: 0.9,
            source_tier: 1,
            zone: None,
            status: ProjectStatus::Configured,
        };
        h.session
            .apply_frame(&frame(uuid::Uuid::new_v4(), Utc::now(), None), Some(&other));
        assert_eq!(
            h.session.snapshot().active_project.as_deref(),
            Some("continuum")
        );
        // ...but the mechanical fields still update.
        assert_eq!(h.session.snapshot().active_app.as_deref(), Some("Code.exe"));

        let out = h
            .apply(ContextAction::Pin {
                field: SessionField::Project,
                value: None,
            })
            .await;
        assert!(out.applied);
        assert!(h.raw_log.list_session_pins().await.unwrap().is_empty());
        h.session
            .apply_frame(&frame(uuid::Uuid::new_v4(), Utc::now(), None), Some(&other));
        assert_eq!(
            h.session.snapshot().active_project.as_deref(),
            Some("other")
        );
    }

    // --- forget cascade ---

    #[tokio::test]
    async fn forget_cascades_across_every_store() {
        let mut h = Harness::new().await;
        let ts = Utc::now();
        let frame_id = uuid::Uuid::new_v4();

        // A screenshot file on disk, referenced by the frame row.
        let shot = h.data_dir.join("shot.jpg");
        std::fs::write(&shot, b"jpeg").unwrap();
        h.raw_log
            .write_frame(&frame(
                frame_id,
                ts,
                Some(shot.to_string_lossy().to_string()),
            ))
            .await
            .unwrap();

        // An events row pointing at it, plus a second, unrelated row.
        h.raw_log
            .insert_event_for_test(
                &event(ts, Some(frame_id.to_string())),
                &uuid::Uuid::new_v4().to_string(),
            )
            .await;
        h.raw_log
            .insert_event_for_test(
                &event(ts, Some(uuid::Uuid::new_v4().to_string())),
                &uuid::Uuid::new_v4().to_string(),
            )
            .await;

        // An episodic memory derived from the frame + an unrelated one.
        {
            let mut store = h.episodic.lock().await;
            store
                .insert_event(&crate::memory::episodic::EpisodicEvent {
                    id: uuid::Uuid::new_v4().to_string(),
                    ts,
                    kind: crate::memory::episodic::EventKind::Remember,
                    summary: "derived".into(),
                    importance: 0.7,
                    tags: vec![],
                    source_frame_id: Some(frame_id.to_string()),
                    project: None,
                    sensitivity: crate::memory::events::EventSensitivity::CloudAllowed,
                })
                .await
                .unwrap();
            store
                .insert_event(&crate::memory::episodic::EpisodicEvent {
                    id: uuid::Uuid::new_v4().to_string(),
                    ts,
                    kind: crate::memory::episodic::EventKind::Remember,
                    summary: "unrelated".into(),
                    importance: 0.7,
                    tags: vec![],
                    source_frame_id: Some(uuid::Uuid::new_v4().to_string()),
                    project: None,
                    sensitivity: crate::memory::events::EventSensitivity::CloudAllowed,
                })
                .await
                .unwrap();
        }

        // An unconfirmed vault candidate referencing the frame.
        h.vault
            .create(NoteDraft {
                node_type: NodeType::Error,
                title: "build failed".into(),
                body: "…".into(),
                project: None,
                status: NodeStatus::Candidate,
                confidence: 0.8,
                importance: 0.8,
                source: Source::Observed,
                source_ref: Some(format!("triage:frame:{frame_id}")),
                sensitivity: Sensitivity::default(),
                expires: None,
                relations: Vec::new(),
                tags: Vec::new(),
            })
            .await
            .unwrap();

        let out = h
            .apply(ContextAction::Forget {
                raw_reference: Some(frame_id.to_string()),
                event_id: None,
            })
            .await;
        assert!(out.applied, "{out:?}");

        // Events row + FTS entry gone; the unrelated row survives.
        assert_eq!(h.raw_log.context_event_count().await.unwrap(), 1);
        assert!(h
            .raw_log
            .search_context_events("build failed", 10)
            .await
            .unwrap()
            .iter()
            .all(|r| r.raw_reference.as_deref() != Some(frame_id.to_string().as_str())));
        // Frame row + screenshot gone.
        assert_eq!(h.raw_log.frame_count().await.unwrap(), 0);
        assert!(!shot.exists());
        // Derived episodic memory gone, unrelated one kept.
        assert_eq!(h.episodic.lock().await.event_count().await.unwrap(), 1);
        // Candidate gone.
        assert!(h.vault.pending().await.unwrap().is_empty());
    }

    #[tokio::test]
    async fn forget_by_event_id_uses_the_rows_own_reference() {
        let mut h = Harness::new().await;
        let ts = Utc::now();
        let frame_id = uuid::Uuid::new_v4();
        h.raw_log
            .write_frame(&frame(frame_id, ts, None))
            .await
            .unwrap();
        h.raw_log
            .insert_event_for_test(
                &event(ts, Some(frame_id.to_string())),
                &uuid::Uuid::new_v4().to_string(),
            )
            .await;
        let rows = h.raw_log.list_context_events().await.unwrap();
        let id = rows[0].id;

        let out = h
            .apply(ContextAction::Forget {
                raw_reference: None,
                event_id: Some(id),
            })
            .await;
        assert!(out.applied, "{out:?}");
        assert_eq!(h.raw_log.context_event_count().await.unwrap(), 0);
        assert_eq!(h.raw_log.frame_count().await.unwrap(), 0);
    }

    #[tokio::test]
    async fn forget_with_a_non_frame_reference_only_drops_the_event() {
        let mut h = Harness::new().await;
        let ts = Utc::now();
        h.raw_log
            .insert_event_for_test(
                &event(ts, Some("deadbeefcafe".into())),
                &uuid::Uuid::new_v4().to_string(),
            )
            .await;
        let out = h
            .apply(ContextAction::Forget {
                raw_reference: Some("deadbeefcafe".into()),
                event_id: None,
            })
            .await;
        assert!(out.applied, "{out:?}");
        assert_eq!(h.raw_log.context_event_count().await.unwrap(), 0);
        assert_eq!(out.details["frames"], 0);
    }

    #[tokio::test]
    async fn forget_without_any_key_is_refused() {
        let mut h = Harness::new().await;
        let out = h
            .apply(ContextAction::Forget {
                raw_reference: None,
                event_id: None,
            })
            .await;
        assert!(!out.applied);
    }

    // --- delete range ---

    #[tokio::test]
    async fn delete_range_purges_frames_events_screenshots_and_memories() {
        let mut h = Harness::new().await;
        let now = Utc::now();
        let inside = now - Duration::minutes(30);
        let outside = now - Duration::days(3);

        for (ts, name) in [(inside, "in.jpg"), (outside, "out.jpg")] {
            let shot = h.data_dir.join(name);
            std::fs::write(&shot, b"jpeg").unwrap();
            h.raw_log
                .write_frame(&frame(
                    uuid::Uuid::new_v4(),
                    ts,
                    Some(shot.to_string_lossy().to_string()),
                ))
                .await
                .unwrap();
            h.raw_log
                .insert_event_for_test(&event(ts, None), &uuid::Uuid::new_v4().to_string())
                .await;
            let mut store = h.episodic.lock().await;
            store
                .insert_event(&crate::memory::episodic::EpisodicEvent {
                    id: uuid::Uuid::new_v4().to_string(),
                    ts,
                    kind: crate::memory::episodic::EventKind::Remember,
                    summary: format!("memory {name}"),
                    importance: 0.7,
                    tags: vec![],
                    source_frame_id: None,
                    project: None,
                    sensitivity: crate::memory::events::EventSensitivity::CloudAllowed,
                })
                .await
                .unwrap();
        }

        let out = h
            .apply(ContextAction::DeleteRange {
                from: now - Duration::hours(1),
                to: now,
            })
            .await;
        assert!(out.applied, "{out:?}");

        assert_eq!(h.raw_log.frame_count().await.unwrap(), 1);
        assert_eq!(h.raw_log.context_event_count().await.unwrap(), 1);
        assert_eq!(h.episodic.lock().await.event_count().await.unwrap(), 1);
        assert!(!h.data_dir.join("in.jpg").exists());
        assert!(h.data_dir.join("out.jpg").exists());
    }

    // --- fixwave 2 ---------------------------------------------------------

    /// I2: `[storage] delete_screenshots_with_rotation` is a **rotation**
    /// knob. A user who turns it off wants Continuum to stop tidying up on
    /// its own schedule — not to have "Forget this frame" silently leave
    /// the JPEG on disk with its row deleted, unreachable by any future
    /// targeted delete (and, with `screenshot_max_age_hours = 0`, by the
    /// mtime backstop either).
    #[tokio::test]
    async fn explicit_deletion_removes_the_screenshot_with_rotation_disabled() {
        let mut h = Harness::new().await;
        h.screenshot_policy = ScreenshotPolicy::KEEP_EVERYTHING;
        assert!(!h.screenshot_policy.deletes_referenced_files());

        let ts = Utc::now();
        let frame_id = uuid::Uuid::new_v4();
        let shot = h.data_dir.join("embarrassing.jpg");
        std::fs::write(&shot, b"jpeg").unwrap();
        h.raw_log
            .write_frame(&frame(
                frame_id,
                ts,
                Some(shot.to_string_lossy().to_string()),
            ))
            .await
            .unwrap();

        let out = h
            .apply(ContextAction::Forget {
                raw_reference: Some(frame_id.to_string()),
                event_id: None,
            })
            .await;
        assert!(out.applied, "{out:?}");
        assert_eq!(h.raw_log.frame_count().await.unwrap(), 0);
        assert!(
            !shot.exists(),
            "the row went but the JPEG was orphaned on disk"
        );
        assert_eq!(out.details["screenshots"], serde_json::json!(1));
    }

    /// Same rule for the range purge: an explicit "delete the last hour"
    /// must take the files with it whatever the rotation knob says.
    #[tokio::test]
    async fn delete_range_removes_screenshots_with_rotation_disabled() {
        let mut h = Harness::new().await;
        h.screenshot_policy = ScreenshotPolicy::KEEP_EVERYTHING;
        let now = Utc::now();

        let inside = h.data_dir.join("inside.jpg");
        std::fs::write(&inside, b"jpeg").unwrap();
        h.raw_log
            .write_frame(&frame(
                uuid::Uuid::new_v4(),
                now - Duration::minutes(10),
                Some(inside.to_string_lossy().to_string()),
            ))
            .await
            .unwrap();
        let outside = h.data_dir.join("outside.jpg");
        std::fs::write(&outside, b"jpeg").unwrap();
        h.raw_log
            .write_frame(&frame(
                uuid::Uuid::new_v4(),
                now - Duration::days(3),
                Some(outside.to_string_lossy().to_string()),
            ))
            .await
            .unwrap();

        let out = h
            .apply(ContextAction::DeleteRange {
                from: now - Duration::hours(1),
                to: now,
            })
            .await;
        assert!(out.applied, "{out:?}");
        assert!(!inside.exists(), "in-window screenshot was orphaned");
        // Outside the window nothing is touched — the knob is irrelevant
        // either way here, but the range bound still is.
        assert!(outside.exists());
    }

    /// M7: `forget` cascades to the vault candidate derived from the frame;
    /// `delete_range` purged everything *except* that, leaving a pending
    /// decision quoting a frame the user just erased.
    #[tokio::test]
    async fn delete_range_also_purges_vault_candidates_for_its_frames() {
        let mut h = Harness::new().await;
        let now = Utc::now();
        let inside_id = uuid::Uuid::new_v4();
        let outside_id = uuid::Uuid::new_v4();

        h.raw_log
            .write_frame(&frame(inside_id, now - Duration::minutes(10), None))
            .await
            .unwrap();
        h.raw_log
            .write_frame(&frame(outside_id, now - Duration::days(3), None))
            .await
            .unwrap();

        for id in [inside_id, outside_id] {
            h.vault
                .create(NoteDraft {
                    node_type: NodeType::Error,
                    title: format!("candidate for {id}"),
                    body: "…".into(),
                    project: None,
                    status: NodeStatus::Candidate,
                    confidence: 0.8,
                    importance: 0.8,
                    source: Source::Observed,
                    source_ref: Some(format!("triage:frame:{id}")),
                    sensitivity: Sensitivity::default(),
                    expires: None,
                    relations: Vec::new(),
                    tags: Vec::new(),
                })
                .await
                .unwrap();
        }
        assert_eq!(h.vault.pending().await.unwrap().len(), 2);

        let out = h
            .apply(ContextAction::DeleteRange {
                from: now - Duration::hours(1),
                to: now,
            })
            .await;
        assert!(out.applied, "{out:?}");
        assert_eq!(out.details["candidates"], serde_json::json!(1));

        let pending = h.vault.pending().await.unwrap();
        assert_eq!(pending.len(), 1, "only the out-of-window candidate remains");
        assert!(
            pending[0].title.contains(&outside_id.to_string()),
            "the wrong candidate survived: {:?}",
            pending[0].title
        );
    }

    /// I1: the two deletion kinds are the ones the runtime hands to its
    /// spawned worker; everything else stays inline (those handlers mutate
    /// the loop-owned resolver).
    #[test]
    fn only_deletion_intents_are_deferred() {
        let now = Utc::now();
        assert!(is_deferred(&ContextAction::Forget {
            raw_reference: Some("x".into()),
            event_id: None
        }));
        assert!(is_deferred(&ContextAction::DeleteRange {
            from: now,
            to: now
        }));
        for inline in [
            ContextAction::ConfirmProject {
                project_id: "p".into(),
            },
            ContextAction::Pin {
                field: SessionField::Project,
                value: None,
            },
            ContextAction::SetToggle {
                name: ToggleName::Screen,
                value: false,
            },
        ] {
            assert!(!is_deferred(&inline), "{inline:?} must stay inline");
        }
    }

    /// I1: applying a deletion from owned handles (what the spawned worker
    /// does) has exactly the effect and the audit line the inline path has.
    #[tokio::test]
    async fn deferred_application_matches_the_inline_path() {
        let h = Harness::new().await;
        let now = Utc::now();
        let shot = h.data_dir.join("deferred.jpg");
        std::fs::write(&shot, b"jpeg").unwrap();
        h.raw_log
            .write_frame(&frame(
                uuid::Uuid::new_v4(),
                now - Duration::minutes(10),
                Some(shot.to_string_lossy().to_string()),
            ))
            .await
            .unwrap();

        let ctx = DeferredIntentContext {
            raw_log: h.raw_log.clone(),
            episodic: h.episodic.clone(),
            vault: h.vault.clone(),
            audit: h.audit.clone(),
            screenshot_policy: ScreenshotPolicy::default(),
        };
        // Runs on a spawned task, as the runtime does.
        let intent = ContextIntent::new(ContextAction::DeleteRange {
            from: now - Duration::hours(1),
            to: now,
        });
        let out = tokio::spawn(async move { apply_deferred_intent(&ctx, &intent).await })
            .await
            .unwrap();
        assert!(out.applied, "{out:?}");
        assert_eq!(h.raw_log.frame_count().await.unwrap(), 0);
        assert!(!shot.exists());

        // The audit line is written by the deferred path too.
        let audit = std::fs::read_to_string(h.audit.path()).unwrap();
        assert!(audit.contains("deletion"), "audit: {audit}");
    }

    /// A non-deletion action must never be silently swallowed by the
    /// worker — it reports as a failed intent instead.
    #[tokio::test]
    async fn deferred_application_refuses_inline_kinds() {
        let h = Harness::new().await;
        let ctx = DeferredIntentContext {
            raw_log: h.raw_log.clone(),
            episodic: h.episodic.clone(),
            vault: h.vault.clone(),
            audit: h.audit.clone(),
            screenshot_policy: ScreenshotPolicy::default(),
        };
        let intent = ContextIntent::new(ContextAction::SetToggle {
            name: ToggleName::Screen,
            value: false,
        });
        let out = apply_deferred_intent(&ctx, &intent).await;
        assert!(!out.applied);
        assert!(
            out.summary.contains("not a deferred intent kind"),
            "{out:?}"
        );
    }

    #[tokio::test]
    async fn an_inverted_delete_range_is_refused() {
        let mut h = Harness::new().await;
        let now = Utc::now();
        h.raw_log
            .write_frame(&frame(uuid::Uuid::new_v4(), now, None))
            .await
            .unwrap();
        let out = h
            .apply(ContextAction::DeleteRange {
                from: now,
                to: now - Duration::hours(1),
            })
            .await;
        assert!(!out.applied);
        assert_eq!(h.raw_log.frame_count().await.unwrap(), 1);
    }

    // --- toggles ---

    #[tokio::test]
    async fn set_toggle_updates_live_state_and_config() {
        let mut h = Harness::new().await;
        let out = h
            .apply(ContextAction::SetToggle {
                name: ToggleName::Mic,
                value: false,
            })
            .await;
        assert!(out.applied, "{out:?}");
        assert!(!h.toggles.value(ToggleName::Mic));

        let cfg = crate::config::load_config(&h.data_dir.join("config.toml")).unwrap();
        assert!(!cfg.privacy.toggles.mic);
        assert!(cfg.privacy.toggles.screen, "others keep their defaults");
    }

    #[tokio::test]
    async fn setting_a_toggle_to_its_current_value_is_a_no_op() {
        let mut h = Harness::new().await;
        let out = h
            .apply(ContextAction::SetToggle {
                name: ToggleName::Git,
                value: true,
            })
            .await;
        assert!(!out.applied);
        // No config file written for a no-op.
        assert!(!h.data_dir.join("config.toml").exists());
    }

    #[tokio::test]
    async fn record_user_command_updates_session_and_audits_without_the_text() {
        let mut h = Harness::new().await;
        let command = "  deploy the private release  ";

        let out = h
            .apply(ContextAction::RecordUserCommand {
                text: command.to_string(),
            })
            .await;

        assert!(out.applied, "{out:?}");
        assert_eq!(
            h.session
                .snapshot()
                .last_user_command
                .as_ref()
                .map(|command| command.text.as_str()),
            Some("deploy the private release")
        );
        let entries = h.audit.read_all();
        assert_eq!(entries.len(), 1);
        assert_eq!(entries[0].kind, "user_command");
        assert_eq!(entries[0].actor, Actor::User);
        let audit_json = std::fs::read_to_string(h.audit.path()).unwrap();
        assert!(!audit_json.contains("deploy the private release"));
    }

    // --- end to end: file -> drain -> effect ---

    /// The whole C5 loop as the dashboard actually drives it: a file
    /// written by `context_write_intent`, drained by the runtime's
    /// `IntentDrainer`, applied against real stores. Covers every kind
    /// that has an observable effect without needing a frame first.
    #[tokio::test]
    async fn intent_files_drain_and_apply_end_to_end() {
        use crate::context::intents::{write_intent, IntentDrainer};

        let mut h = Harness::new().await;
        let intent_dir = h.data_dir.clone();

        let actions = vec![
            ContextAction::AddProject {
                name: "Continuum".into(),
                root_path: "D:/code/continuum".into(),
                project_id: None,
            },
            ContextAction::SetToggle {
                name: ToggleName::Files,
                value: false,
            },
            ContextAction::Pin {
                field: SessionField::Task,
                value: Some("ship C5".into()),
            },
            ContextAction::NotThisProject {
                project_id: "continuum".into(),
                match_process: Some("chrome.exe".into()),
                match_title_substring: None,
            },
            ContextAction::Correct {
                field: SessionField::Goal,
                value: "close Plan C".into(),
                match_process: None,
                match_title_substring: None,
            },
            ContextAction::RecordUserCommand {
                text: "finish the context engine".into(),
            },
        ];
        for action in &actions {
            write_intent(&intent_dir, &ContextIntent::new(action.clone())).unwrap();
        }

        let mut drainer = IntentDrainer::new();
        let drained = drainer.drain(&intent_dir);
        assert_eq!(drained.len(), actions.len(), "every file drained");
        for intent in &drained {
            let mut ctx = h.ctx();
            let outcome = apply_intent(&mut ctx, intent).await;
            assert!(outcome.applied, "{outcome:?}");
        }

        // Effects, asserted against the real stores.
        assert_eq!(
            h.raw_log.list_projects().await.unwrap()[0].entry.id,
            "continuum"
        );
        assert!(!h.toggles.value(ToggleName::Files));
        assert_eq!(h.raw_log.list_session_pins().await.unwrap().len(), 1);
        assert_eq!(h.raw_log.list_project_rules().await.unwrap().len(), 1);
        assert_eq!(
            h.session.snapshot().current_goal.as_deref(),
            Some("close Plan C")
        );
        assert_eq!(
            h.session
                .snapshot()
                .last_user_command
                .as_ref()
                .map(|command| command.text.as_str()),
            Some("finish the context engine")
        );
        // Config mirrored both the project and the toggle.
        let cfg = crate::config::load_config(&h.data_dir.join("config.toml")).unwrap();
        assert_eq!(cfg.projects.known.len(), 1);
        assert!(!cfg.privacy.toggles.files);
        // One audit line per intent.
        assert_eq!(h.audit.read_all().len(), actions.len());
        // The directory is empty and a second drain is a no-op.
        assert!(drainer.drain(&intent_dir).is_empty());
    }

    // --- audit ---

    #[tokio::test]
    async fn every_applied_intent_writes_one_well_formed_audit_line() {
        let mut h = Harness::new().await;
        h.apply(ContextAction::SetToggle {
            name: ToggleName::Screen,
            value: false,
        })
        .await;
        h.apply(ContextAction::Correct {
            field: SessionField::Task,
            value: "ship it".into(),
            match_process: None,
            match_title_substring: None,
        })
        .await;
        h.apply(ContextAction::Forget {
            raw_reference: None,
            event_id: None,
        })
        .await;

        let raw = std::fs::read_to_string(h.audit.path()).unwrap();
        assert_eq!(raw.lines().count(), 3, "one line per intent, failures too");
        let entries = h.audit.read_all();
        let kinds: Vec<&str> = entries.iter().map(|e| e.kind.as_str()).collect();
        assert_eq!(kinds, vec!["toggle_change", "correction", "deletion"]);
        for entry in &entries {
            assert_eq!(entry.actor, Actor::User);
            assert!(!entry.summary.is_empty());
            let details = entry.details.as_ref().expect("details present");
            assert!(details.get("intent").is_some());
            assert!(details.get("intent_id").is_some());
            assert!(details.get("applied").is_some());
        }
        // The failed forget is recorded as not-applied rather than dropped.
        assert_eq!(entries[2].details.as_ref().unwrap()["applied"], false);
    }
}

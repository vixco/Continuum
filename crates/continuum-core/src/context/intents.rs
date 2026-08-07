//! # Context-intent file protocol (spec §4.13)
//!
//! The dashboard process and the runtime process are separate (see
//! `docs/dashboard.md`), and the dashboard links `continuum-core` with
//! `default-features = false` — it can never open the raw-log SQLite
//! database, the vault index, or the episodic store itself. Every Context
//! page action therefore travels through the filesystem as an *intent
//! file*, exactly like [`crate::voice::intent`] does for push-to-talk and
//! `workers::intent` does for MCP→runtime spawn calls.
//!
//! ## Directory layout
//!
//! ```text
//! ~/.continuum-dev/
//! └── context-intents/            ← dashboard writes, runtime drains every tick
//!     ├── 20260805T101112131-set_toggle.json
//!     └── 20260805T101112999-forget.bad        ← unparseable, kept for inspection
//!                                                (the `.json` extension is
//!                                                 replaced, not appended)
//! ```
//!
//! ## On-disk shape
//!
//! One file = one intent. The envelope is the action's own internally
//! tagged `kind` discriminant flattened next to two envelope fields:
//!
//! ```json
//! { "kind": "set_toggle", "name": "mic", "value": false,
//!   "id": "b1f0…", "created": 1785000000000 }
//! ```
//!
//! `id` makes application idempotent (a duplicated file is applied once
//! per [`IntentDrainer`] lifetime); `created` is milliseconds since the
//! epoch, used for ordering and audit lines only.
//!
//! ## Deliberately NOT stale-dropped
//!
//! Unlike voice intents, a context intent has **no TTL**. "Confirm this
//! project" or "forget that event" is a durable user decision: if the
//! runtime was down when the user clicked, the intent must still apply at
//! the next boot. A stale push-to-talk is surprising; a stale deletion
//! request is a privacy promise the user already made.
//!
//! # Layer
//!
//! Cross-layer control plane. Pure filesystem + serde — **ungated**, so
//! the desktop process (no `runtime` feature) can write intents and the
//! runtime can drain them from the same type definitions. The *effects*
//! live in [`crate::context::apply`], which is runtime-gated because it
//! touches the stores.

use std::collections::{HashSet, VecDeque};
use std::path::{Path, PathBuf};
use std::time::{SystemTime, UNIX_EPOCH};

use anyhow::{Context, Result};
use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};

/// Subdirectory under the Continuum data dir where the dashboard writes
/// context intents.
pub const CONTEXT_INTENTS_SUBDIR: &str = "context-intents";

/// How many applied intent ids the drainer remembers for idempotence.
/// Bounded so a long-running runtime never grows this set without limit;
/// far larger than any plausible burst of Context-page clicks.
const SEEN_IDS_CAP: usize = 512;

/// Which session-state field a correction or pin addresses (spec §4.13).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum SessionField {
    /// The resolver's current project.
    Project,
    /// The inferred larger goal.
    Goal,
    /// The inferred concrete task.
    Task,
}

impl SessionField {
    /// Stable snake_case token — also the `session_pins` row key.
    pub fn as_str(self) -> &'static str {
        match self {
            SessionField::Project => "project",
            SessionField::Goal => "goal",
            SessionField::Task => "task",
        }
    }

    /// Parses the stable token; unknown strings yield `None` (never guess
    /// which field a corrupt pin row meant).
    pub fn parse(s: &str) -> Option<Self> {
        match s {
            "project" => Some(SessionField::Project),
            "goal" => Some(SessionField::Goal),
            "task" => Some(SessionField::Task),
            _ => None,
        }
    }
}

/// Which honest observation toggle a `set_toggle` intent addresses
/// (spec §4.1). Mirrors [`crate::config::ObservationToggles`]'s fields.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ToggleName {
    /// Microphone / audio observation.
    Mic,
    /// Screen capture + vision observation.
    Screen,
    /// File watcher observation.
    Files,
    /// Git collector observation.
    Git,
    /// Master pause.
    PauseAll,
}

impl ToggleName {
    /// Stable snake_case token (also the config key under
    /// `[privacy.toggles]`).
    pub fn as_str(self) -> &'static str {
        match self {
            ToggleName::Mic => "mic",
            ToggleName::Screen => "screen",
            ToggleName::Files => "files",
            ToggleName::Git => "git",
            ToggleName::PauseAll => "pause_all",
        }
    }

    /// Parses the stable token.
    pub fn parse(s: &str) -> Option<Self> {
        match s {
            "mic" => Some(ToggleName::Mic),
            "screen" => Some(ToggleName::Screen),
            "files" => Some(ToggleName::Files),
            "git" => Some(ToggleName::Git),
            "pause_all" => Some(ToggleName::PauseAll),
            _ => None,
        }
    }
}

/// What a Context-page action asks the runtime to do (spec §4.13).
///
/// Internally tagged on `kind` so the envelope can flatten it — an unknown
/// `kind` fails deserialization, which routes the file to `.bad` rather
/// than being silently ignored.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum ContextAction {
    /// Add a project the user names by hand (spec §4.3 "Add project").
    ///
    /// `project_id` is optional; absent means "slugify the name,
    /// de-collide". It is deliberately **not** called `id`: the envelope
    /// already owns that key at the same JSON level (`#[serde(flatten)]`),
    /// and two `id` fields would collide silently on the wire.
    AddProject {
        /// Human-readable name.
        name: String,
        /// Absolute root directory.
        root_path: String,
        /// Explicit slug id, when the caller wants one.
        #[serde(default)]
        project_id: Option<String>,
    },
    /// Promote a discovered candidate to `confirmed`, which is what makes
    /// the git collector and the file watcher start touching it.
    ConfirmProject {
        /// Slug id of the row to confirm.
        project_id: String,
    },
    /// Correct a session-state field. For `project` this also persists a
    /// tier-0 `force_project` rule so the correction survives the frame.
    Correct {
        /// Which field is wrong.
        field: SessionField,
        /// What it should be (a project slug for `field: project`).
        value: String,
        /// Optional scope for the derived rule; defaults to the current
        /// foreground process.
        #[serde(default)]
        match_process: Option<String>,
        /// Optional title-substring scope for the derived rule.
        #[serde(default)]
        match_title_substring: Option<String>,
    },
    /// Persist a tier-0 `exclude_project` rule.
    NotThisProject {
        /// The project that must NOT win for matching frames.
        project_id: String,
        /// Process scope.
        #[serde(default)]
        match_process: Option<String>,
        /// Title-substring scope.
        #[serde(default)]
        match_title_substring: Option<String>,
    },
    /// Pin a session-state field, or clear the pin with `value: null`.
    ///
    /// A pin blocks *session-state overwrite* only — the resolver keeps
    /// resolving, collectors keep collecting. It is cleared by an explicit
    /// `value: null`, or automatically when the resolver reports a
    /// different project above confidence for `switch_min_secs`
    /// (spec §4.13, no pin/resolver deadlock).
    Pin {
        /// Which field to pin.
        field: SessionField,
        /// The pinned value, or `null` to clear.
        #[serde(default)]
        value: Option<String>,
    },
    /// Forget one observation, cascading across every derived store
    /// (spec §4.13).
    Forget {
        /// The `context_events.raw_reference` to cascade from (usually a
        /// perception-frame uuid).
        #[serde(default)]
        raw_reference: Option<String>,
        /// A specific `context_events` rowid. When both are given the row
        /// is deleted by id and its own `raw_reference` drives the
        /// cascade.
        #[serde(default)]
        event_id: Option<i64>,
    },
    /// Purge everything observed in a time window (Continuum.md §13).
    DeleteRange {
        /// Inclusive start.
        from: DateTime<Utc>,
        /// Inclusive end.
        to: DateTime<Utc>,
    },
    /// Flip an honest observation toggle live and persist it.
    SetToggle {
        /// Which toggle.
        name: ToggleName,
        /// New value.
        value: bool,
    },
    /// Record what the user just asked for, from a producer outside the
    /// runtime process (spec §4.8 `last_user_command` "voice/chat/hotkey").
    ///
    /// Fixwave 3b (I9): the desktop chat is a separate process, so it had
    /// no way to reach `SessionStateHub::note_user_command` — the spec's
    /// "chat" leg was voice-only in practice while a code comment claimed
    /// the bridge already carried it. This is that bridge. Voice still
    /// records in-process; this variant exists for the desktop chat.
    RecordUserCommand {
        /// The user's own words. Truncated by the session state to 400
        /// characters.
        text: String,
    },
}

impl ContextAction {
    /// Stable label for logs and audit lines.
    pub fn kind_label(&self) -> &'static str {
        match self {
            ContextAction::AddProject { .. } => "add_project",
            ContextAction::ConfirmProject { .. } => "confirm_project",
            ContextAction::Correct { .. } => "correct",
            ContextAction::NotThisProject { .. } => "not_this_project",
            ContextAction::Pin { .. } => "pin",
            ContextAction::Forget { .. } => "forget",
            ContextAction::DeleteRange { .. } => "delete_range",
            ContextAction::SetToggle { .. } => "set_toggle",
            ContextAction::RecordUserCommand { .. } => "record_user_command",
        }
    }
}

/// One intent file: the action plus its envelope.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct ContextIntent {
    /// The action itself; `kind` and its payload sit at the top level.
    #[serde(flatten)]
    pub action: ContextAction,
    /// Idempotence key. Two files with the same id apply once.
    pub id: String,
    /// Milliseconds since the epoch when the dashboard issued the click.
    #[serde(default)]
    pub created: u64,
}

impl ContextIntent {
    /// Wraps an action with a fresh id + timestamp.
    pub fn new(action: ContextAction) -> Self {
        let created = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .map(|d| d.as_millis() as u64)
            .unwrap_or(0);
        // uuid is already a dependency of this crate (frame ids); no need
        // for a second id scheme.
        Self {
            action,
            id: uuid::Uuid::new_v4().to_string(),
            created,
        }
    }
}

/// Returns the context-intents directory for a data dir, creating it on
/// first call.
pub fn ensure_intents_dir(data_dir: &Path) -> Result<PathBuf> {
    let p = data_dir.join(CONTEXT_INTENTS_SUBDIR);
    std::fs::create_dir_all(&p)
        .with_context(|| format!("Failed to create context intents dir at {}", p.display()))?;
    Ok(p)
}

/// Writes an intent file atomically (`.tmp` + rename), so the runtime
/// never observes a partially-written file mid-drain.
pub fn write_intent(data_dir: &Path, intent: &ContextIntent) -> Result<PathBuf> {
    let dir = ensure_intents_dir(data_dir)?;
    let ts = Utc::now().format("%Y%m%dT%H%M%S%3f").to_string();
    // The id suffix keeps two clicks inside the same millisecond from
    // overwriting each other.
    let short = intent.id.chars().take(8).collect::<String>();
    let path = dir.join(format!("{ts}-{}-{short}.json", intent.action.kind_label()));
    let tmp = path.with_extension("json.tmp");
    let payload =
        serde_json::to_string_pretty(intent).context("Failed to serialize context intent")?;
    std::fs::write(&tmp, payload)
        .with_context(|| format!("Failed to write context intent tmp at {}", tmp.display()))?;
    std::fs::rename(&tmp, &path).with_context(|| {
        format!(
            "Failed to rename context intent {} -> {}",
            tmp.display(),
            path.display()
        )
    })?;
    Ok(path)
}

/// Reads and removes every intent file from the directory, oldest first.
///
/// Unparseable files are renamed to `.bad` (the curator wipe-request
/// precedent) so they can be inspected without starving the loop. Never
/// returns an error for an absent directory — a runtime that has never
/// seen a Context-page click simply drains nothing.
pub fn drain_intents(data_dir: &Path) -> Result<Vec<ContextIntent>> {
    let dir = ensure_intents_dir(data_dir)?;
    let mut out = Vec::new();

    let read_dir = match std::fs::read_dir(&dir) {
        Ok(rd) => rd,
        Err(_) => return Ok(out),
    };

    let mut entries: Vec<PathBuf> = read_dir
        .filter_map(|e| e.ok().map(|e| e.path()))
        .filter(|p| {
            p.extension()
                .and_then(|e| e.to_str())
                .map(|e| e == "json")
                .unwrap_or(false)
        })
        .collect();
    // Filenames start with a sortable timestamp, so lexical order is
    // chronological order.
    entries.sort();

    for path in entries {
        let contents = match std::fs::read_to_string(&path) {
            Ok(c) => c,
            Err(e) => {
                tracing::warn!(
                    layer = "context",
                    component = "intent",
                    path = %path.display(),
                    error = %e,
                    "Could not read context intent; leaving it for the next tick"
                );
                continue;
            }
        };
        match serde_json::from_str::<ContextIntent>(&contents) {
            Ok(intent) => {
                let _ = std::fs::remove_file(&path);
                out.push(intent);
            }
            Err(e) => {
                tracing::warn!(
                    layer = "context",
                    component = "intent",
                    path = %path.display(),
                    error = %e,
                    "Unparseable context intent; renamed to .bad"
                );
                let _ = std::fs::rename(&path, path.with_extension("bad"));
            }
        }
    }
    Ok(out)
}

/// Stateful drainer that de-duplicates by intent id.
///
/// Files are removed as they are read, so a replay only happens when the
/// same id is written twice (a retrying dashboard, a restored backup). The
/// seen set is bounded at [`SEEN_IDS_CAP`] entries, evicted oldest-first.
#[derive(Debug, Default)]
pub struct IntentDrainer {
    order: VecDeque<String>,
    seen: HashSet<String>,
}

impl IntentDrainer {
    /// Creates an empty drainer.
    pub fn new() -> Self {
        Self::default()
    }

    /// Drains the directory and returns only intents whose id this drainer
    /// has not applied before.
    pub fn drain(&mut self, data_dir: &Path) -> Vec<ContextIntent> {
        let intents = match drain_intents(data_dir) {
            Ok(v) => v,
            Err(e) => {
                tracing::warn!(
                    layer = "context",
                    component = "intent",
                    error = %e,
                    "Failed to drain context intents"
                );
                return Vec::new();
            }
        };
        let mut fresh = Vec::with_capacity(intents.len());
        for intent in intents {
            if self.seen.contains(&intent.id) {
                tracing::debug!(
                    layer = "context",
                    component = "intent",
                    intent_id = %intent.id,
                    "Dropping duplicate context intent"
                );
                continue;
            }
            self.remember(intent.id.clone());
            fresh.push(intent);
        }
        fresh
    }

    fn remember(&mut self, id: String) {
        self.order.push_back(id.clone());
        self.seen.insert(id);
        while self.order.len() > SEEN_IDS_CAP {
            if let Some(old) = self.order.pop_front() {
                self.seen.remove(&old);
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use tempfile::TempDir;

    fn toggle(name: ToggleName, value: bool) -> ContextIntent {
        ContextIntent::new(ContextAction::SetToggle { name, value })
    }

    #[test]
    fn every_action_kind_round_trips_through_a_file() {
        let tmp = TempDir::new().unwrap();
        let actions = [
            ContextAction::AddProject {
                name: "SimCharts".into(),
                root_path: "D:/work/simcharts".into(),
                project_id: None,
            },
            ContextAction::ConfirmProject {
                project_id: "simcharts".into(),
            },
            ContextAction::Correct {
                field: SessionField::Project,
                value: "continuum".into(),
                match_process: Some("Code.exe".into()),
                match_title_substring: None,
            },
            ContextAction::NotThisProject {
                project_id: "continuum".into(),
                match_process: None,
                match_title_substring: Some("scratch".into()),
            },
            ContextAction::Pin {
                field: SessionField::Task,
                value: Some("ship C5".into()),
            },
            ContextAction::Forget {
                raw_reference: Some("frame-1".into()),
                event_id: Some(7),
            },
            ContextAction::DeleteRange {
                from: "2026-08-05T10:00:00Z".parse().unwrap(),
                to: "2026-08-05T11:00:00Z".parse().unwrap(),
            },
            ContextAction::SetToggle {
                name: ToggleName::Files,
                value: false,
            },
            ContextAction::RecordUserCommand {
                text: "run the focused tests".into(),
            },
        ];
        let written: Vec<ContextIntent> = actions
            .iter()
            .map(|a| {
                let intent = ContextIntent::new(a.clone());
                write_intent(tmp.path(), &intent).unwrap();
                intent
            })
            .collect();

        let mut drained = drain_intents(tmp.path()).unwrap();
        drained.sort_by(|a, b| a.action.kind_label().cmp(b.action.kind_label()));
        let mut expected = written;
        expected.sort_by(|a, b| a.action.kind_label().cmp(b.action.kind_label()));
        assert_eq!(drained, expected);
    }

    #[test]
    fn kind_sits_at_the_top_level_next_to_the_envelope() {
        let intent = ContextIntent::new(ContextAction::SetToggle {
            name: ToggleName::Mic,
            value: false,
        });
        let value: serde_json::Value = serde_json::to_value(&intent).unwrap();
        let obj = value.as_object().unwrap();
        assert_eq!(obj.get("kind").unwrap(), "set_toggle");
        assert_eq!(obj.get("name").unwrap(), "mic");
        assert_eq!(obj.get("value").unwrap(), false);
        assert!(obj.contains_key("id"));
        assert!(obj.contains_key("created"));
    }

    #[test]
    fn drain_removes_files_and_is_empty_afterwards() {
        let tmp = TempDir::new().unwrap();
        let p = write_intent(tmp.path(), &toggle(ToggleName::Git, false)).unwrap();
        assert!(p.exists());
        assert_eq!(drain_intents(tmp.path()).unwrap().len(), 1);
        assert!(!p.exists());
        assert!(drain_intents(tmp.path()).unwrap().is_empty());
    }

    #[test]
    fn malformed_intent_is_renamed_to_bad() {
        let tmp = TempDir::new().unwrap();
        let dir = ensure_intents_dir(tmp.path()).unwrap();
        let bad = dir.join("20260805T000000000-oops.json");
        std::fs::write(&bad, "{ not json").unwrap();

        assert!(drain_intents(tmp.path()).unwrap().is_empty());
        assert!(!bad.exists());
        assert!(bad.with_extension("bad").exists());
    }

    #[test]
    fn unknown_kind_is_treated_as_malformed() {
        let tmp = TempDir::new().unwrap();
        let dir = ensure_intents_dir(tmp.path()).unwrap();
        let bad = dir.join("20260805T000000001-future.json");
        std::fs::write(
            &bad,
            r#"{"kind":"teleport","id":"x","created":1,"where":"mars"}"#,
        )
        .unwrap();

        assert!(drain_intents(tmp.path()).unwrap().is_empty());
        assert!(bad.with_extension("bad").exists());
    }

    #[test]
    fn intents_are_not_dropped_for_age() {
        // Deliberate difference from voice intents: a correction issued
        // while the runtime was down must still apply at next boot.
        let tmp = TempDir::new().unwrap();
        let mut intent = toggle(ToggleName::Screen, false);
        intent.created = 1; // 1970
        write_intent(tmp.path(), &intent).unwrap();
        assert_eq!(drain_intents(tmp.path()).unwrap().len(), 1);
    }

    #[test]
    fn drainer_applies_an_id_once() {
        let tmp = TempDir::new().unwrap();
        let intent = toggle(ToggleName::Mic, false);
        write_intent(tmp.path(), &intent).unwrap();
        let mut drainer = IntentDrainer::new();
        assert_eq!(drainer.drain(tmp.path()).len(), 1);
        // Same id written again (dashboard retry).
        write_intent(tmp.path(), &intent).unwrap();
        assert!(drainer.drain(tmp.path()).is_empty());
    }

    #[test]
    fn drainer_seen_set_stays_bounded() {
        let mut drainer = IntentDrainer::new();
        for i in 0..(SEEN_IDS_CAP + 10) {
            drainer.remember(format!("id-{i}"));
        }
        assert_eq!(drainer.order.len(), SEEN_IDS_CAP);
        assert_eq!(drainer.seen.len(), SEEN_IDS_CAP);
        assert!(!drainer.seen.contains("id-0"));
        assert!(drainer.seen.contains(&format!("id-{}", SEEN_IDS_CAP + 9)));
    }

    #[test]
    fn drain_on_missing_dir_creates_it() {
        let tmp = TempDir::new().unwrap();
        let dir = tmp.path().join(CONTEXT_INTENTS_SUBDIR);
        assert!(!dir.exists());
        assert!(drain_intents(tmp.path()).unwrap().is_empty());
        assert!(dir.exists());
    }

    #[test]
    fn field_and_toggle_tokens_round_trip() {
        for f in [
            SessionField::Project,
            SessionField::Goal,
            SessionField::Task,
        ] {
            assert_eq!(SessionField::parse(f.as_str()), Some(f));
        }
        assert_eq!(SessionField::parse("nope"), None);
        for t in [
            ToggleName::Mic,
            ToggleName::Screen,
            ToggleName::Files,
            ToggleName::Git,
            ToggleName::PauseAll,
        ] {
            assert_eq!(ToggleName::parse(t.as_str()), Some(t));
        }
        assert_eq!(ToggleName::parse("nope"), None);
    }
}

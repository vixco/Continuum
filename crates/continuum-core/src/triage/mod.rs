//! # Layer 2 — Triage
//!
//! The triage layer is a small local LLM (3–4B parameters) that reads every
//! salient perception frame and decides what to do. It is the gatekeeper that
//! decides whether to spend money on Opus or not.
//!
//! Default model: Qwen 3 4B (Q4_K_M quantization).
//!
//! Decisions:
//! - `ignore` — nothing worth doing, discard the frame
//! - `remember` — worth remembering but no action needed
//! - `whisper` — say a short sentence via local TTS (no orchestrator)
//! - `execute_simple` — perform a pre-approved simple action
//! - `wake_orchestrator` — wake Claude Opus for genuine reasoning

pub mod coalesce;
/// Classification consumption (Task B3, spec §4.7): events, vault
/// candidates, and the `triage_decision` raw-log column.
#[cfg(feature = "runtime")]
pub mod consume;
#[cfg(feature = "runtime")]
pub mod handlers;
#[cfg(feature = "runtime")]
pub mod llm;
#[cfg(feature = "runtime")]
pub mod prompts;

use serde::{Deserialize, Serialize};

/// The five triage decision variants.
///
/// The triage LLM outputs one of these as JSON on every evaluation. The
/// grammar constraint ensures the output is always valid JSON matching
/// one of these variants.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(tag = "decision", rename_all = "snake_case")]
pub enum TriageDecision {
    /// Nothing worth doing, discard the frame.
    Ignore,
    /// Worth remembering but no action needed.
    Remember {
        /// Brief summary of what to remember (max 200 chars).
        summary: String,
    },
    /// Say a short sentence aloud via local TTS.
    Whisper {
        /// The text to speak (max 200 chars).
        text: String,
    },
    /// Perform a simple pre-approved action.
    ExecuteSimple {
        /// The action to execute (e.g., "launch_app:notepad").
        action: String,
    },
    /// The situation needs Claude Opus to think about it.
    WakeOrchestrator {
        /// Why the orchestrator should wake up.
        reason: String,
        /// Optional hint naming a Continuum skill that probably applies
        /// (e.g. `"code-review"`). The orchestrator treats it as advisory —
        /// the real match still happens from the skill loader's triggers.
        #[serde(default, skip_serializing_if = "Option::is_none")]
        suggested_skill: Option<String>,
    },
}

impl TriageDecision {
    /// Returns the decision variant name as a static string for logging.
    pub fn variant_name(&self) -> &'static str {
        match self {
            Self::Ignore => "ignore",
            Self::Remember { .. } => "remember",
            Self::Whisper { .. } => "whisper",
            Self::ExecuteSimple { .. } => "execute_simple",
            Self::WakeOrchestrator { .. } => "wake_orchestrator",
        }
    }

    /// Truncate string fields to 200 characters, logging a warning if any
    /// field was over the limit. Grammar mode cannot enforce length bounds,
    /// so we apply this in post-processing.
    pub fn truncated(self) -> Self {
        fn trunc(s: String) -> String {
            if s.len() > 200 {
                tracing::warn!(
                    layer = "triage",
                    component = "decision",
                    original_len = s.len(),
                    "Truncating triage decision field to 200 chars"
                );
                let mut t = s;
                t.truncate(200);
                t
            } else {
                s
            }
        }

        match self {
            Self::Ignore => Self::Ignore,
            Self::Remember { summary } => Self::Remember {
                summary: trunc(summary),
            },
            Self::Whisper { text } => Self::Whisper { text: trunc(text) },
            Self::ExecuteSimple { action } => Self::ExecuteSimple {
                action: trunc(action),
            },
            Self::WakeOrchestrator {
                reason,
                suggested_skill,
            } => Self::WakeOrchestrator {
                reason: trunc(reason),
                suggested_skill: suggested_skill.map(trunc),
            },
        }
    }

    /// Parse a JSON string into a TriageDecision.
    ///
    /// Strips markdown code fences and any text before the first `{` to
    /// handle models that wrap JSON in prose or thinking tokens.
    /// Returns `None` if the JSON is malformed or doesn't match any variant.
    pub fn from_json(raw: &str) -> Option<Self> {
        let cleaned = extract_json_object(raw);
        serde_json::from_str(cleaned).ok()
    }
}

impl std::fmt::Display for TriageDecision {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Ignore => write!(f, "ignore"),
            Self::Remember { summary } => write!(f, "remember: {summary}"),
            Self::Whisper { text } => write!(f, "whisper: {text}"),
            Self::ExecuteSimple { action } => write!(f, "execute_simple: {action}"),
            Self::WakeOrchestrator {
                reason,
                suggested_skill,
            } => match suggested_skill {
                Some(s) => write!(f, "wake_orchestrator[{s}]: {reason}"),
                None => write!(f, "wake_orchestrator: {reason}"),
            },
        }
    }
}

// ---------------------------------------------------------------------------
// Classification riding triage (spec §4.7)
// ---------------------------------------------------------------------------

/// Importance assigned to a classification whose JSON omitted the key
/// (fixwave 3a, I2). Mid-scale and deliberately unremarkable: high enough
/// to clear `[memory] distillation_min_event_importance`, low enough that
/// an unscored event never outranks one the model actually scored.
#[cfg(feature = "runtime")]
pub const DEFAULT_CLASSIFICATION_IMPORTANCE: f32 = 0.4;

#[cfg(feature = "runtime")]
fn default_classification_importance() -> f32 {
    DEFAULT_CLASSIFICATION_IMPORTANCE
}

/// Context-Model classification emitted alongside the triage decision
/// (spec §4.7 — the triage call is the Context Model call, no second GPU
/// pass). Lives behind the `runtime` feature because [`Classification`]
/// reuses the classification variants of the runtime-gated
/// [`crate::memory::events::EventType`] registry.
///
/// All fields except `event_type` are serde-defaulted so a sloppy model
/// output degrades gracefully instead of killing the whole block.
#[cfg(feature = "runtime")]
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Classification {
    /// What kind of event this frame represents. Must be one of the
    /// screen/audio classification variants (`error`, `success`,
    /// `decision`, `preference`, `task_progress`, `communication`,
    /// `routine`, `other`) — [`Classification::sanitized`] drops the
    /// whole block if the model emits a non-classification type.
    pub event_type: crate::memory::events::EventType,
    /// Project slug the frame is about, or `None` when unclear. Invalid
    /// slugs are dropped to `None` (log-only); an unknown-but-valid slug
    /// is dropped to the resolver's value downstream (Task B3, spec §4.6).
    #[serde(default)]
    pub project: Option<String>,
    /// How much this matters later, clamped to `[0, 1]`.
    ///
    /// A model that omits the key gets
    /// [`DEFAULT_CLASSIFICATION_IMPORTANCE`], not `0.0` (fixwave 3a, I2).
    /// Zero was a double cliff: the event fell below every distillation
    /// threshold *and* — because it still became an event row — it
    /// suppressed its own source frame from the raw-frame fallback, so a
    /// single missing JSON key erased the moment from memory entirely. An
    /// omitted score means "the model did not say", which is not the same
    /// claim as "worthless".
    #[serde(default = "default_classification_importance")]
    pub importance: f32,
    /// Model self-confidence, clamped to `[0, 1]`.
    #[serde(default)]
    pub confidence: f32,
    /// One-line factual summary (truncated to 200 chars).
    #[serde(default)]
    pub summary: String,
    /// Whether this frame should become a vault memory candidate.
    #[serde(default)]
    pub should_store: bool,
}

#[cfg(feature = "runtime")]
impl Classification {
    /// Apply the spec §4.7 clamps: importance/confidence into `[0, 1]`,
    /// summary truncated to 200 chars (char-boundary safe), project
    /// slug-validated (invalid → `None`, log-only). Returns `None` when
    /// `event_type` is not a screen/audio classification variant — the
    /// registry is closed and triage must not mint window/git/file/system
    /// events.
    pub fn sanitized(mut self) -> Option<Self> {
        use crate::memory::events::EventSource;

        if !self.event_type.valid_for(EventSource::Screen) {
            tracing::debug!(
                layer = "triage",
                component = "classification",
                event_type = ?self.event_type,
                "Dropping classification — event_type is not a classification variant"
            );
            return None;
        }

        fn clamp01(v: f32) -> f32 {
            if v.is_nan() {
                0.0
            } else {
                v.clamp(0.0, 1.0)
            }
        }
        self.importance = clamp01(self.importance);
        self.confidence = clamp01(self.confidence);

        if self.summary.len() > 200 {
            let mut end = 200;
            while !self.summary.is_char_boundary(end) {
                end -= 1;
            }
            tracing::debug!(
                layer = "triage",
                component = "classification",
                original_len = self.summary.len(),
                "Truncating classification summary to 200 chars"
            );
            self.summary.truncate(end);
        }

        if let Some(project) = self.project.take() {
            if crate::context::project::is_valid_project_id(&project) {
                self.project = Some(project);
            } else {
                tracing::debug!(
                    layer = "triage",
                    component = "classification",
                    project = %project,
                    "Dropping classification project — not a valid slug"
                );
            }
        }

        Some(self)
    }
}

/// Parse container for the full triage output (spec §4.7, serde-proven).
///
/// The `#[serde(flatten)]` wrapper is REQUIRED: [`TriageDecision`] is an
/// internally-tagged enum that silently ignores sibling keys (proven by
/// `test_parse_extra_keys_accepted`), so parsing the raw output straight
/// into `TriageDecision` would silently drop the classification block.
#[cfg(feature = "runtime")]
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct TriageOutput {
    /// The triage decision — the flattened `decision`-tagged keys of the
    /// single top-level JSON object.
    #[serde(flatten)]
    pub decision: TriageDecision,
    /// The Context-Model classification block, `None` when the model
    /// omitted it or it failed to parse/sanitize. A malformed or
    /// truncated classification NEVER costs the decision (spec §4.7).
    #[serde(default)]
    pub classification: Option<Classification>,
}

#[cfg(feature = "runtime")]
impl TriageOutput {
    /// Wrap a bare decision with no classification.
    pub fn from_decision(decision: TriageDecision) -> Self {
        Self {
            decision,
            classification: None,
        }
    }

    /// Parse raw model output into a [`TriageOutput`].
    ///
    /// Ladder (spec §4.7 — a broken classification block never burns a
    /// GPU retry):
    /// 1. Extract the JSON object ([`extract_json_object`]) and parse the
    ///    full wrapper; the classification is then sanitized (clamps,
    ///    slug validation, registry check).
    /// 2. On failure, parse the same string as a plain [`TriageDecision`]
    ///    (the tagged enum ignores the unparseable sibling block).
    /// 3. On failure, salvage a decision truncated mid-classification:
    ///    cut the raw output at the `"classification"` key, close the
    ///    object, and parse the prefix as a plain decision.
    ///
    /// Returns `None` only when no decision can be recovered at all.
    pub fn from_json(raw: &str) -> Option<Self> {
        let cleaned = extract_json_object(raw);

        if let Ok(mut output) = serde_json::from_str::<TriageOutput>(cleaned) {
            output.classification = output.classification.and_then(Classification::sanitized);
            return Some(output);
        }

        // Malformed classification sibling (wrong type, unknown
        // event_type string, …) — the tagged enum skips it.
        if let Ok(decision) = serde_json::from_str::<TriageDecision>(cleaned) {
            tracing::debug!(
                layer = "triage",
                component = "classification",
                "Classification block unparseable — keeping bare decision"
            );
            return Some(Self::from_decision(decision));
        }

        // Truncated mid-classification: the brace-balanced extractor
        // cannot find the (unclosed) outer object, so cut the ORIGINAL
        // output at the classification key and close the decision object.
        Self::salvage_truncated(raw)
    }

    /// Recover a decision from output truncated inside the classification
    /// block (max_tokens hit). Never recovers a truncated decision itself.
    fn salvage_truncated(raw: &str) -> Option<Self> {
        let s = raw.trim();
        let search_area = match s.rfind("</think>") {
            Some(pos) => s[pos + 8..].trim(),
            None => s,
        };
        let start = search_area.find('{')?;
        let key_pos = search_area.find("\"classification\"")?;
        if key_pos <= start {
            return None;
        }
        let mut prefix = search_area[start..key_pos].trim_end().to_string();
        if prefix.ends_with(',') {
            prefix.pop();
        }
        let repaired = format!("{}}}", prefix.trim_end());
        let decision = serde_json::from_str::<TriageDecision>(&repaired).ok()?;
        tracing::debug!(
            layer = "triage",
            component = "classification",
            "Salvaged decision from output truncated mid-classification"
        );
        Some(Self::from_decision(decision))
    }
}

/// Extract the last complete top-level JSON object from raw model output.
///
/// Handles common model output patterns:
/// - Markdown code fences: `` ```json\n{...}\n``` ``
/// - `<think>...</think>` blocks before the JSON (Qwen 3 thinking mode)
/// - Trailing prose after the JSON
///
/// Strategy: scan left-to-right for brace-balanced objects and keep the
/// LAST **top-level** one. Nested objects (the §4.7 `classification` block)
/// are consumed inside their parent, so a trailing nested block never
/// shadows the real output object; JSON-like content in prose before the
/// real object is skipped by keeping the last match.
/// Task B5: also used by `context::session_state::parse_inference` — the
/// session-state inference reply comes off the same local model and needs
/// the same lenient extraction ladder.
pub(crate) fn extract_json_object(raw: &str) -> &str {
    let s = raw.trim();

    // If there's a </think> tag, only look at text after it.
    let search_area = if let Some(pos) = s.rfind("</think>") {
        &s[pos + 8..]
    } else {
        s
    };
    let search_area = search_area.trim();

    let mut best: Option<(usize, usize)> = None;
    let mut from = 0usize;
    while let Some(rel) = search_area[from..].find('{') {
        let start = from + rel;
        match balanced_object_end(search_area, start) {
            Some(end) => {
                best = Some((start, end));
                from = end + 1;
            }
            // Unbalanced from here to EOF (truncated output) — no later
            // start can close either.
            None => break,
        }
    }

    match best {
        Some((start, end)) => &search_area[start..=end],
        None => search_area,
    }
}

/// Given `s[start] == '{'`, return the byte index of its matching `}`
/// (string- and escape-aware), or `None` when the object never closes.
fn balanced_object_end(s: &str, start: usize) -> Option<usize> {
    let mut depth = 0i32;
    let mut in_string = false;
    let mut escape_next = false;
    for (i, ch) in s[start..].char_indices() {
        if escape_next {
            escape_next = false;
            continue;
        }
        match ch {
            '\\' if in_string => escape_next = true,
            '"' => in_string = !in_string,
            '{' if !in_string => depth += 1,
            '}' if !in_string => {
                depth -= 1;
                if depth == 0 {
                    return Some(start + i);
                }
            }
            _ => {}
        }
    }
    None
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_extract_json_from_code_fence() {
        let raw = "```json\n{\"decision\":\"ignore\"}\n```";
        assert_eq!(extract_json_object(raw), r#"{"decision":"ignore"}"#);
    }

    #[test]
    fn test_extract_json_with_thinking_prefix() {
        let raw = "Let me analyze this frame.\n{\"decision\":\"ignore\"}";
        assert_eq!(extract_json_object(raw), r#"{"decision":"ignore"}"#);
    }

    #[test]
    fn test_extract_json_after_think_tags() {
        let raw = "<think>\nThis is idle.\n</think>\n\n{\"decision\":\"ignore\"}";
        assert_eq!(extract_json_object(raw), r#"{"decision":"ignore"}"#);
    }

    #[test]
    fn test_extract_json_think_with_json_inside() {
        let raw =
            "<think>\n{\"inner\":true}\n</think>\n{\"decision\":\"remember\",\"summary\":\"test\"}";
        assert_eq!(
            extract_json_object(raw),
            r#"{"decision":"remember","summary":"test"}"#
        );
    }

    #[test]
    fn test_extract_json_clean() {
        let raw = r#"{"decision":"remember","summary":"test"}"#;
        assert_eq!(extract_json_object(raw), raw);
    }

    #[test]
    fn test_extract_json_nested_object_returns_outer() {
        // A trailing nested block (§4.7 classification) must never shadow
        // the top-level object.
        let raw = r#"{"decision":"ignore","classification":{"event_type":"error"}}"#;
        assert_eq!(extract_json_object(raw), raw);
    }

    #[test]
    fn test_extract_json_balanced_object_before_truncated_tail() {
        // A complete object followed by truncated junk still extracts.
        let raw = "{\"decision\":\"ignore\"} and then {\"broken\":";
        assert_eq!(extract_json_object(raw), r#"{"decision":"ignore"}"#);
    }

    #[test]
    fn test_parse_ignore() {
        let d = TriageDecision::from_json(r#"{"decision":"ignore"}"#).unwrap();
        assert_eq!(d, TriageDecision::Ignore);
    }

    #[test]
    fn test_parse_remember() {
        let d =
            TriageDecision::from_json(r#"{"decision":"remember","summary":"user opened VS Code"}"#)
                .unwrap();
        assert!(matches!(d, TriageDecision::Remember { .. }));
        if let TriageDecision::Remember { summary } = d {
            assert_eq!(summary, "user opened VS Code");
        }
    }

    #[test]
    fn test_parse_whisper() {
        let d =
            TriageDecision::from_json(r#"{"decision":"whisper","text":"meeting in 5 minutes"}"#)
                .unwrap();
        assert!(matches!(d, TriageDecision::Whisper { .. }));
    }

    #[test]
    fn test_parse_execute_simple() {
        let d = TriageDecision::from_json(
            r#"{"decision":"execute_simple","action":"launch_app:notepad"}"#,
        )
        .unwrap();
        assert!(matches!(d, TriageDecision::ExecuteSimple { .. }));
    }

    #[test]
    fn test_parse_wake_orchestrator() {
        let d = TriageDecision::from_json(
            r#"{"decision":"wake_orchestrator","reason":"user asked a complex question"}"#,
        )
        .unwrap();
        assert!(matches!(d, TriageDecision::WakeOrchestrator { .. }));
    }

    #[test]
    fn test_parse_malformed_json() {
        assert!(TriageDecision::from_json("not json at all").is_none());
    }

    #[test]
    fn test_parse_empty_string() {
        assert!(TriageDecision::from_json("").is_none());
    }

    #[test]
    fn test_parse_wrong_decision_value() {
        assert!(TriageDecision::from_json(r#"{"decision":"explode"}"#).is_none());
    }

    #[test]
    fn test_parse_missing_required_field() {
        // remember requires summary
        assert!(TriageDecision::from_json(r#"{"decision":"remember"}"#).is_none());
    }

    #[test]
    fn test_parse_extra_keys_accepted() {
        // serde by default ignores extra keys
        let d = TriageDecision::from_json(r#"{"decision":"ignore","extra":"field","another":42}"#)
            .unwrap();
        assert_eq!(d, TriageDecision::Ignore);
    }

    #[test]
    fn test_parse_unicode_in_summary() {
        let d = TriageDecision::from_json(
            r#"{"decision":"remember","summary":"gebruiker opende het bestand \u00e9\u00e8n.rs"}"#,
        )
        .unwrap();
        if let TriageDecision::Remember { summary } = d {
            assert!(summary.contains("gebruiker"));
        }
    }

    #[test]
    fn test_parse_empty_summary_accepted() {
        let d = TriageDecision::from_json(r#"{"decision":"remember","summary":""}"#).unwrap();
        assert!(matches!(d, TriageDecision::Remember { summary } if summary.is_empty()));
    }

    #[test]
    fn test_truncate_long_summary() {
        let long = "x".repeat(300);
        let d = TriageDecision::Remember {
            summary: long.clone(),
        }
        .truncated();
        if let TriageDecision::Remember { summary } = d {
            assert_eq!(summary.len(), 200);
        }
    }

    #[test]
    fn test_truncate_short_summary_unchanged() {
        let d = TriageDecision::Remember {
            summary: "short".to_string(),
        }
        .truncated();
        if let TriageDecision::Remember { summary } = d {
            assert_eq!(summary, "short");
        }
    }

    #[test]
    fn test_variant_name() {
        assert_eq!(TriageDecision::Ignore.variant_name(), "ignore");
        assert_eq!(
            TriageDecision::WakeOrchestrator {
                reason: "test".to_string(),
                suggested_skill: None,
            }
            .variant_name(),
            "wake_orchestrator"
        );
    }

    #[test]
    fn test_display() {
        assert_eq!(format!("{}", TriageDecision::Ignore), "ignore");
        assert_eq!(
            format!(
                "{}",
                TriageDecision::Remember {
                    summary: "opened file".to_string()
                }
            ),
            "remember: opened file"
        );
    }

    #[test]
    fn test_roundtrip_serialize_deserialize() {
        let original = TriageDecision::WakeOrchestrator {
            reason: "error detected".to_string(),
            suggested_skill: None,
        };
        let json = serde_json::to_string(&original).unwrap();
        let parsed: TriageDecision = serde_json::from_str(&json).unwrap();
        assert_eq!(original, parsed);
    }
}

/// Flatten-wrapper parsing matrix for [`TriageOutput`] (spec §4.7).
#[cfg(all(test, feature = "runtime"))]
mod output_tests {
    use super::*;
    use crate::memory::events::EventType;

    #[test]
    fn test_output_parse_full_valid() {
        // The spec §4.7 example object, verbatim shape.
        let raw = r#"{ "decision": "ignore", "classification": { "event_type": "error", "project": "continuum", "importance": 0.8, "confidence": 0.9, "summary": "one line", "should_store": false } }"#;
        let out = TriageOutput::from_json(raw).unwrap();
        assert_eq!(out.decision, TriageDecision::Ignore);
        let c = out.classification.unwrap();
        assert_eq!(c.event_type, EventType::Error);
        assert_eq!(c.project.as_deref(), Some("continuum"));
        assert!((c.importance - 0.8).abs() < f32::EPSILON);
        assert!((c.confidence - 0.9).abs() < f32::EPSILON);
        assert_eq!(c.summary, "one line");
        assert!(!c.should_store);
    }

    #[test]
    fn test_output_parse_decision_fields_plus_classification() {
        let raw = r#"{"decision":"remember","summary":"tests passed","classification":{"event_type":"success","project":"continuum","importance":0.6,"confidence":0.8,"summary":"triage tests green","should_store":true}}"#;
        let out = TriageOutput::from_json(raw).unwrap();
        assert!(
            matches!(out.decision, TriageDecision::Remember { ref summary } if summary == "tests passed")
        );
        let c = out.classification.unwrap();
        assert_eq!(c.event_type, EventType::Success);
        assert!(c.should_store);
    }

    #[test]
    fn test_output_parse_missing_importance_defaults_sanely() {
        // Fixwave 3a (I2): `#[serde(default)]` scored an omitted
        // importance 0.0, which is below every distillation threshold —
        // and the event still suppressed its own source frame, so one
        // missing JSON key erased the moment from memory entirely.
        let raw = r#"{"decision":"ignore","classification":{"event_type":"error","summary":"build broke"}}"#;
        let c = TriageOutput::from_json(raw)
            .unwrap()
            .classification
            .unwrap();
        assert!(
            (c.importance - DEFAULT_CLASSIFICATION_IMPORTANCE).abs() < f32::EPSILON,
            "unscored importance was {}",
            c.importance
        );
        assert!(
            c.importance > crate::config::MemoryConfig::default().distillation_min_event_importance,
            "the default must clear the distiller's own threshold"
        );
        // The clamp keeps it (sanitized() must not zero it out).
        let sanitized = c.sanitized().unwrap();
        assert!((sanitized.importance - DEFAULT_CLASSIFICATION_IMPORTANCE).abs() < f32::EPSILON);
    }

    #[test]
    fn test_output_parse_missing_classification_block() {
        let out = TriageOutput::from_json(r#"{"decision":"ignore"}"#).unwrap();
        assert_eq!(out.decision, TriageDecision::Ignore);
        assert!(out.classification.is_none());
    }

    #[test]
    fn test_output_parse_malformed_classification_not_an_object() {
        // classification is a bare string — wrapper parse fails, the
        // tagged enum ignores the sibling key, decision survives.
        let raw = r#"{"decision":"ignore","classification":"garbage"}"#;
        let out = TriageOutput::from_json(raw).unwrap();
        assert_eq!(out.decision, TriageDecision::Ignore);
        assert!(out.classification.is_none());
    }

    #[test]
    fn test_output_parse_malformed_classification_field_type() {
        // importance as a string breaks the sub-object; decision survives.
        let raw = r#"{"decision":"whisper","text":"hi","classification":{"event_type":"routine","importance":"high"}}"#;
        let out = TriageOutput::from_json(raw).unwrap();
        assert!(matches!(out.decision, TriageDecision::Whisper { .. }));
        assert!(out.classification.is_none());
    }

    #[test]
    fn test_output_parse_truncated_mid_classification() {
        // max_tokens hit inside the classification block — salvage the
        // decision, never burn a retry.
        let raw = r#"{"decision":"remember","summary":"user committed","classification":{"event_type":"task_pro"#;
        let out = TriageOutput::from_json(raw).unwrap();
        assert!(
            matches!(out.decision, TriageDecision::Remember { ref summary } if summary == "user committed")
        );
        assert!(out.classification.is_none());
    }

    #[test]
    fn test_output_parse_truncated_after_complete_classification() {
        // The inner classification object is balanced but the outer brace
        // never closed — the extractor locks onto the inner object, the
        // salvage path still recovers the decision from the raw prefix.
        let raw =
            r#"{"decision":"ignore","classification":{"event_type":"error","importance":0.5}"#;
        let out = TriageOutput::from_json(raw).unwrap();
        assert_eq!(out.decision, TriageDecision::Ignore);
        assert!(out.classification.is_none());
    }

    #[test]
    fn test_output_parse_truncated_decision_still_fails() {
        // Truncation inside the decision itself is unrecoverable.
        assert!(TriageOutput::from_json(r#"{"decision":"remem"#).is_none());
        assert!(TriageOutput::from_json("").is_none());
        assert!(TriageOutput::from_json("not json at all").is_none());
    }

    #[test]
    fn test_output_parse_out_of_range_clamped() {
        let raw = r#"{"decision":"ignore","classification":{"event_type":"error","importance":3.2,"confidence":-1.0,"summary":"s"}}"#;
        let c = TriageOutput::from_json(raw)
            .unwrap()
            .classification
            .unwrap();
        assert_eq!(c.importance, 1.0);
        assert_eq!(c.confidence, 0.0);
    }

    #[test]
    fn test_output_parse_unknown_event_type_string() {
        // "banana" is not in the EventType registry — the sub-object
        // fails to parse, decision intact, classification None.
        let raw = r#"{"decision":"ignore","classification":{"event_type":"banana","summary":"s"}}"#;
        let out = TriageOutput::from_json(raw).unwrap();
        assert_eq!(out.decision, TriageDecision::Ignore);
        assert!(out.classification.is_none());
    }

    #[test]
    fn test_output_parse_non_classification_event_type_dropped() {
        // "commit" is a real EventType but not a screen/audio
        // classification variant — sanitize drops the block (closed
        // registry, spec §4.6).
        let raw = r#"{"decision":"ignore","classification":{"event_type":"commit","summary":"s"}}"#;
        let out = TriageOutput::from_json(raw).unwrap();
        assert_eq!(out.decision, TriageDecision::Ignore);
        assert!(out.classification.is_none());
    }

    #[test]
    fn test_output_parse_invalid_project_slug_dropped() {
        let raw = r#"{"decision":"ignore","classification":{"event_type":"routine","project":"My Project!","summary":"s"}}"#;
        let c = TriageOutput::from_json(raw)
            .unwrap()
            .classification
            .unwrap();
        assert!(c.project.is_none());
        // The rest of the block survives the bad slug.
        assert_eq!(c.event_type, EventType::Routine);
        assert_eq!(c.summary, "s");
    }

    #[test]
    fn test_output_parse_defaults_for_missing_optional_fields() {
        let raw = r#"{"decision":"ignore","classification":{"event_type":"other"}}"#;
        let c = TriageOutput::from_json(raw)
            .unwrap()
            .classification
            .unwrap();
        assert!(c.project.is_none());
        // Fixwave 3a (I2): an omitted importance means "the model did not
        // say", not "worthless" — see [`DEFAULT_CLASSIFICATION_IMPORTANCE`].
        assert_eq!(c.importance, DEFAULT_CLASSIFICATION_IMPORTANCE);
        assert_eq!(c.confidence, 0.0);
        assert!(c.summary.is_empty());
        assert!(!c.should_store);
    }

    #[test]
    fn test_output_parse_extra_keys_in_classification_accepted() {
        let raw = r#"{"decision":"ignore","classification":{"event_type":"routine","summary":"s","banana":42}}"#;
        let out = TriageOutput::from_json(raw).unwrap();
        assert!(out.classification.is_some());
    }

    #[test]
    fn test_output_parse_with_code_fence_and_prose() {
        let raw = "Sure, here is the triage:\n```json\n{\"decision\":\"ignore\",\"classification\":{\"event_type\":\"routine\",\"summary\":\"idle\"}}\n```";
        let out = TriageOutput::from_json(raw).unwrap();
        assert_eq!(out.decision, TriageDecision::Ignore);
        assert_eq!(out.classification.unwrap().event_type, EventType::Routine);
    }

    #[test]
    fn test_output_parse_after_think_block() {
        let raw = "<think>\n{\"decision\":\"whisper\"}\n</think>\n{\"decision\":\"ignore\",\"classification\":{\"event_type\":\"routine\"}}";
        let out = TriageOutput::from_json(raw).unwrap();
        assert_eq!(out.decision, TriageDecision::Ignore);
        assert!(out.classification.is_some());
    }

    #[test]
    fn test_output_summary_truncated_to_200() {
        let long = "x".repeat(300);
        let raw = format!(
            r#"{{"decision":"ignore","classification":{{"event_type":"routine","summary":"{long}"}}}}"#
        );
        let c = TriageOutput::from_json(&raw)
            .unwrap()
            .classification
            .unwrap();
        assert_eq!(c.summary.len(), 200);
    }

    #[test]
    fn test_output_summary_truncation_is_char_boundary_safe() {
        // 100 two-byte chars = 200 bytes; adding one more must not panic
        // mid-codepoint.
        let s = "é".repeat(150);
        let c = Classification {
            event_type: EventType::Routine,
            project: None,
            importance: 0.0,
            confidence: 0.0,
            summary: s,
            should_store: false,
        }
        .sanitized()
        .unwrap();
        assert!(c.summary.len() <= 200);
        assert!(c.summary.is_char_boundary(c.summary.len()));
    }

    #[test]
    fn test_output_from_decision_helper() {
        let out = TriageOutput::from_decision(TriageDecision::Ignore);
        assert_eq!(out.decision, TriageDecision::Ignore);
        assert!(out.classification.is_none());
    }

    #[test]
    fn test_output_roundtrip_serialize_deserialize() {
        let original = TriageOutput {
            decision: TriageDecision::Remember {
                summary: "done".to_string(),
            },
            classification: Some(Classification {
                event_type: EventType::Success,
                project: Some("continuum".to_string()),
                importance: 0.7,
                confidence: 0.9,
                summary: "tests green".to_string(),
                should_store: true,
            }),
        };
        let json = serde_json::to_string(&original).unwrap();
        // Flatten keeps ONE top-level object (brace-depth early-stop safe).
        assert!(json.starts_with('{') && json.ends_with('}'));
        assert!(json.contains("\"decision\":\"remember\""));
        let parsed: TriageOutput = serde_json::from_str(&json).unwrap();
        assert_eq!(original, parsed);
    }

    #[test]
    fn test_output_flatten_wrapper_beats_tagged_enum_key_drop() {
        // The reason the wrapper exists: the internally-tagged enum alone
        // parses this fine but silently drops the classification sibling
        // (see test_parse_extra_keys_accepted). The wrapper keeps it.
        let raw = r#"{"decision":"ignore","classification":{"event_type":"error","summary":"s"}}"#;
        let bare = TriageDecision::from_json(raw).unwrap();
        assert_eq!(bare, TriageDecision::Ignore); // enum: block silently gone
        let wrapped = TriageOutput::from_json(raw).unwrap();
        assert!(wrapped.classification.is_some()); // wrapper: block kept
    }
}

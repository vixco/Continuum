//! # Context package — one struct, three consumer profiles (spec §4.9)
//!
//! Everything the layer-3 orchestrator, the `context_package` MCP tool and
//! the desktop chat prompt need to know about "right now", assembled once
//! into [`ContextPackage`] and rendered by one pure renderer.
//!
//! This module is deliberately **NOT gated behind the `runtime` feature**
//! (spec §3): it is plain owned structs plus string formatting, so all
//! three processes link it and the `--no-default-features` parity gate
//! proves it. Nothing here opens a store, touches a clock, or imports a
//! runtime-gated type — every consumer converts its own data into these
//! small mirror structs at the assembler boundary (the runtime-full
//! assembler lives in `crate::orchestrator::wake_context` + `bin/continuum.rs`).
//!
//! Three things make the renderer safe to hand cloud-bound text:
//!
//! 1. **Cloud gate.** Every section item carries `local_only`. When the
//!    budget says `cloud_bound`, `local_only` items are skipped, and the
//!    two sections that must always render something (current moment,
//!    session state) are *generalized* instead of dropped (spec §4.1
//!    propagation rule 2).
//! 2. **Order contract.** Section order is fixed by [`ContextPackage::render`]
//!    and pending memory decisions are always the **last section before
//!    the wake reason** — asserted by tests here and in `wake_context`.
//! 3. **Budget + drop order.** Over-budget packages shed content in a
//!    documented, tested order ([`DROP_ORDER`]); the never-drop set is
//!    why-woken, current moment, session state and pending decisions.

use crate::context::session_state::{PRIVATE_CONTEXT_PHRASE, PRIVATE_PLACEHOLDER};

/// The generic wake reason a cloud-bound package renders when the wake was
/// triggered from a `local_only` zone — spec §4.1 propagation rule (3),
/// verbatim.
///
/// The literal is fixed by the spec: do not "improve" it. The orchestrator
/// is expected to recognise it and behave accordingly (ask, do not guess).
pub const PRIVATE_WAKE_REASON: &str = "user activity in a private context";

// ---------------------------------------------------------------------------
// Token estimation
// ---------------------------------------------------------------------------

/// Bytes per token used by every context-package budget check.
///
/// A deliberate heuristic, not a tokenizer: the packager runs on the wake
/// hot path in a process that has no tokenizer for the *cloud* model. 3.5
/// bytes/token is the same proxy the triage prompt-fit gate uses (Task B1,
/// spec §4.7), so budgets are comparable across the two.
pub const BYTES_PER_TOKEN: f32 = 3.5;

/// Estimates the token cost of `text` with the [`BYTES_PER_TOKEN`] proxy.
/// Always rounds up, so a budget check never under-counts.
pub fn estimate_tokens(text: &str) -> usize {
    (text.len() as f32 / BYTES_PER_TOKEN).ceil() as usize
}

// ---------------------------------------------------------------------------
// Budget + caps
// ---------------------------------------------------------------------------

/// Per-section item caps (spec §6 `[context_package]`). Applied *before*
/// the whole-package budget check, so a single huge section can never
/// starve the rest of the package.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SectionCaps {
    /// Max "just before" lines.
    pub just_before: usize,
    /// Max episodic memories.
    pub memories: usize,
    /// Max semantic facts.
    pub facts: usize,
    /// Max confirmed vault notes.
    pub vault_notes: usize,
    /// Max pending vault decisions.
    pub pending_decisions: usize,
    /// Max file/git change lines.
    pub recent_changes: usize,
    /// Max failed-attempt lines.
    pub failed_attempts: usize,
    /// Max open files listed in the session section.
    pub open_files: usize,
    /// Max tool names listed (the rest collapse into "+N more").
    pub tools: usize,
    /// Max characters of the `world_compact` blob (Task B4).
    pub world_compact_chars: usize,
}

impl Default for SectionCaps {
    fn default() -> Self {
        Self {
            just_before: 5,
            memories: 3,
            facts: 8,
            vault_notes: 5,
            pending_decisions: 5,
            recent_changes: 5,
            failed_attempts: 3,
            open_files: 5,
            tools: 12,
            world_compact_chars: 1_200,
        }
    }
}

/// One render's budget: how many tokens the rendered package may cost,
/// whether the destination is cloud-bound, and the per-section caps.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PackageBudget {
    /// Whole-package token budget (spec §6: 1000 wake / 600 chat).
    pub token_budget: usize,
    /// Whether the render leaves the machine. `true` for the wake message
    /// and the chat system prompt; `false` for a hypothetical local-model
    /// render, which may see `local_only` content (spec §4.1).
    pub cloud_bound: bool,
    /// Per-section item caps.
    pub caps: SectionCaps,
}

impl Default for PackageBudget {
    fn default() -> Self {
        Self {
            token_budget: 1_000,
            cloud_bound: true,
            caps: SectionCaps::default(),
        }
    }
}

impl PackageBudget {
    /// A cloud-bound budget with the given token cap and default caps.
    pub fn cloud(token_budget: usize) -> Self {
        Self {
            token_budget,
            cloud_bound: true,
            caps: SectionCaps::default(),
        }
    }

    /// A local (not cloud-bound) budget — `local_only` content renders.
    pub fn local(token_budget: usize) -> Self {
        Self {
            token_budget,
            cloud_bound: false,
            caps: SectionCaps::default(),
        }
    }

    /// Replaces the caps (builder style).
    pub fn with_caps(mut self, caps: SectionCaps) -> Self {
        self.caps = caps;
        self
    }
}

/// One rung of the documented drop ladder (spec §4.9).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum DropStep {
    /// The session section's open-files list (cheapest to lose).
    OpenFiles,
    /// The whole recent-changes section.
    RecentChanges,
    /// One line off the end of "just before".
    JustBeforeTail,
    /// One memory off the end of "relevant memories".
    MemoriesTail,
}

/// The drop order under budget pressure, in the order it is applied.
///
/// **Never dropped:** why-woken, current moment, session state, pending
/// memory decisions — the four sections that carry the wake's reason for
/// existing and the order contract.
pub const DROP_ORDER: [DropStep; 4] = [
    DropStep::OpenFiles,
    DropStep::RecentChanges,
    DropStep::JustBeforeTail,
    DropStep::MemoriesTail,
];

/// One heading the renderer can emit, in contract order.
///
/// Fixwave 3b (I1): consumers used to derive "which sections are present"
/// from the *pre-render* package, while [`ContextPackage::render_with_report`]
/// applies caps, the cloud gate and the drop ladder to an internal clone.
/// A vault note filtered out by the cloud gate was therefore still reported
/// present, and the orchestrator concluded "the vault is in the text" while
/// reasoning over withheld content. [`RenderOutcome::sections`] reports what
/// the renderer actually wrote.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PackageSection {
    /// `## Current moment`
    CurrentMoment,
    /// `## Session state`
    Session,
    /// `## Just before`
    JustBefore,
    /// `## Relevant memories`
    Memories,
    /// `## What you know about the user`
    Facts,
    /// `## Long-term memory (vault)`
    VaultNotes,
    /// `## Recent changes`
    RecentChanges,
    /// `## Failed attempts`
    FailedAttempts,
    /// `## Last success`
    LastSuccess,
    /// `## Available tools`
    Tools,
    /// `## Recommended next step`
    RecommendedNextStep,
    /// `## Pending memory decisions`
    PendingDecisions,
    /// `## Why you were woken`
    WhyWoken,
}

impl PackageSection {
    /// Stable snake_case token — the wire vocabulary the MCP
    /// `context_package` tool reports in `sections_present`.
    pub fn token(self) -> &'static str {
        match self {
            PackageSection::CurrentMoment => "current_moment",
            PackageSection::Session => "session",
            PackageSection::JustBefore => "just_before",
            PackageSection::Memories => "memories",
            PackageSection::Facts => "facts",
            PackageSection::VaultNotes => "vault_notes",
            PackageSection::RecentChanges => "recent_changes",
            PackageSection::FailedAttempts => "failed_attempts",
            PackageSection::LastSuccess => "last_success",
            PackageSection::Tools => "tools",
            PackageSection::RecommendedNextStep => "recommended_next_step",
            PackageSection::PendingDecisions => "pending_decisions",
            PackageSection::WhyWoken => "why_woken",
        }
    }
}

// ---------------------------------------------------------------------------
// Section types
// ---------------------------------------------------------------------------

/// The trigger moment: caption, window title, foreground app, the compact
/// world-state blob (Task B4) and any audio.
#[derive(Debug, Clone, Default, PartialEq)]
pub struct CurrentMoment {
    /// One-sentence vision caption (spec §4.10 — never the blob).
    pub caption: String,
    /// Foreground window title (finally rendered, Task B7).
    pub window_title: Option<String>,
    /// Foreground process name.
    pub app: Option<String>,
    /// `ScreenObservation.world_compact` (Task B4), capped on render.
    pub world_compact: Option<String>,
    /// Audio transcript, when the frame carried one.
    pub audio: Option<String>,
    /// Whether the moment came from a `local_only` zone.
    pub local_only: bool,
}

/// The session-state section (spec §4.8 snapshot, projected).
#[derive(Debug, Clone, Default, PartialEq)]
pub struct SessionSection {
    /// Resolved project id/name.
    pub project: Option<String>,
    /// Inferred goal (already confidence-filtered by the assembler).
    pub goal: Option<String>,
    /// Inferred task (already confidence-filtered by the assembler).
    pub task: Option<String>,
    /// Confidence of the inferred fields.
    pub confidence: f32,
    /// Best-effort open files.
    pub open_files: Vec<String>,
    /// Whether the inferred fields are `local_only`.
    pub local_only: bool,
}

impl SessionSection {
    /// Whether this section would render nothing. `pub` since Task C4:
    /// `context_package` reports which sections it filled, and it must
    /// use the renderer's own definition of "empty" rather than a second
    /// one that can drift.
    pub fn is_empty(&self) -> bool {
        self.project.is_none()
            && self.goal.is_none()
            && self.task.is_none()
            && self.open_files.is_empty()
    }
}

/// One deduped context event rendered as a line (spec §4.6 count-aware).
#[derive(Debug, Clone, Default, PartialEq)]
pub struct EventLine {
    /// Pre-rendered relative age ("3m ago"). Pre-rendered on purpose: the
    /// renderer stays clock-free and therefore trivially testable.
    pub age: String,
    /// One-line summary (already scrubbed upstream).
    pub text: String,
    /// Occurrences collapsed into this row; `> 1` renders "(×N)".
    pub count: i64,
    /// Whether the event is `local_only`.
    pub local_only: bool,
}

/// One episodic memory line.
#[derive(Debug, Clone, Default, PartialEq)]
pub struct MemoryLine {
    /// Pre-rendered relative age.
    pub age: String,
    /// Memory summary.
    pub text: String,
    /// Whether the memory is `local_only`.
    pub local_only: bool,
}

/// One semantic fact line.
#[derive(Debug, Clone, Default, PartialEq)]
pub struct FactLine {
    /// Human-readable label ("Name", "continuum stack").
    pub label: String,
    /// Fact value as stored.
    pub value: String,
    /// Whether the fact is `local_only`.
    pub local_only: bool,
}

/// One confirmed vault note line.
#[derive(Debug, Clone, Default, PartialEq)]
pub struct VaultNoteLine {
    /// Node type token ("preference", "decision", …).
    pub node_type: String,
    /// Note title.
    pub title: String,
    /// Optional snippet.
    pub snippet: Option<String>,
    /// Importance in [0, 1].
    pub importance: f32,
    /// Whether the note is `local_only`.
    pub local_only: bool,
}

/// One unresolved vault candidate (the order-contract section).
#[derive(Debug, Clone, Default, PartialEq)]
pub struct PendingDecisionLine {
    /// Vault node id (the orchestrator resolves by id).
    pub id: String,
    /// Node type token.
    pub node_type: String,
    /// Candidate title.
    pub title: String,
    /// Extraction confidence.
    pub confidence: f32,
    /// Source token ("observed", "user_stated", …).
    pub source: String,
    /// Whether the candidate is `local_only`.
    pub local_only: bool,
}

/// The available-tools section, summarized from the composed wake config.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct ToolsSection {
    /// Tool/tool-family names as passed to `--allowedTools`.
    pub names: Vec<String>,
    /// The CLI `--permission-mode` this wake runs under.
    pub permission_mode: String,
}

/// The continuation resolver's suggestion (spec §4.12) — only ever set by
/// the assembler when it clears the configured confidence floor.
#[derive(Debug, Clone, Default, PartialEq)]
pub struct NextStep {
    /// One-line suggestion.
    pub text: String,
    /// Ranked confidence.
    pub confidence: f32,
    /// Whether the suggestion is derived from `local_only` evidence.
    ///
    /// Fixwave 2 (C1): the continuation resolver ranks
    /// `SessionState::current_task` — the **raw** field, before
    /// `cloud_view` generalizes it — so a session tagged `local_only` by
    /// spec §4.1 propagation rule (2) used to launder its private task into
    /// the cloud through this section, which had no zone tag at all. Every
    /// other section struct has one; now so does this.
    pub local_only: bool,
}

// ---------------------------------------------------------------------------
// The package
// ---------------------------------------------------------------------------

/// One context package — every section any consumer profile can render.
///
/// Assemblers fill what their profile allows (spec §4.9 matrix) and leave
/// the rest empty; empty sections never render a heading.
#[derive(Debug, Clone, Default, PartialEq)]
pub struct ContextPackage {
    /// Current moment (never dropped).
    pub current_moment: Option<CurrentMoment>,
    /// Session state (never dropped).
    pub session: Option<SessionSection>,
    /// Deduped events just before the trigger, newest first.
    pub just_before: Vec<EventLine>,
    /// Episodic memories.
    pub memories: Vec<MemoryLine>,
    /// Semantic facts.
    pub facts: Vec<FactLine>,
    /// Confirmed vault notes.
    pub vault_notes: Vec<VaultNoteLine>,
    /// File/git changes, newest first.
    pub recent_changes: Vec<EventLine>,
    /// `error` events with their collapse counts, newest first.
    pub failed_attempts: Vec<EventLine>,
    /// The most recent `success` event.
    pub last_success: Option<EventLine>,
    /// Tools + permission mode.
    pub tools: Option<ToolsSection>,
    /// Recommended next step (confidence-gated by the assembler).
    pub recommended_next_step: Option<NextStep>,
    /// Unresolved vault candidates — ORDER CONTRACT: last section before
    /// the wake reason, never dropped.
    pub pending_decisions: Vec<PendingDecisionLine>,
    /// Why the orchestrator was woken (never dropped).
    pub why_woken: Option<String>,
    /// Whether the wake reason is derived from `local_only` evidence.
    ///
    /// Spec §4.1 propagation rule (3): *"a wake triggered by a `local_only`
    /// frame gets the generic reason 'user activity in a private context'"*.
    /// Before fixwave 2 this rule was implemented nowhere — the reason
    /// string is authored by the triage LLM (and, for continue-class wakes,
    /// enriched with the resolved continuation target) and flowed verbatim
    /// into the cloud-bound package.
    ///
    /// Held as a flag rather than pre-generalized text for the same reason
    /// the session section is: a **not** cloud-bound render (the local
    /// triage/curator profiles) still gets the real reason.
    pub why_woken_local_only: bool,
}

/// The outcome of one render: the text, its estimated cost, and which drop
/// rungs had to be applied. Tests and the bench read the report; callers
/// that only want the string use [`ContextPackage::render`].
#[derive(Debug, Clone, PartialEq)]
pub struct RenderOutcome {
    /// The rendered markdown.
    pub text: String,
    /// Estimated tokens of `text` (see [`estimate_tokens`]).
    pub tokens: usize,
    /// Drop rungs applied, in the order they were applied.
    pub dropped: Vec<DropStep>,
    /// Whether the result still exceeds the budget after the whole ladder
    /// (the never-drop set alone can, in principle, be over budget).
    pub over_budget: bool,
    /// The headings actually written to `text`, in contract order — after
    /// caps, after the cloud gate, after the drop ladder (fixwave 3b, I1).
    pub sections: Vec<PackageSection>,
}

impl ContextPackage {
    /// Renders the package to the markdown the consumer sends to its model.
    pub fn render(&self, budget: &PackageBudget) -> String {
        self.render_with_report(budget).text
    }

    /// Renders and reports what the budget cost (see [`RenderOutcome`]).
    pub fn render_with_report(&self, budget: &PackageBudget) -> RenderOutcome {
        let mut working = self.clone();
        working.apply_caps(&budget.caps);

        let mut dropped = Vec::new();
        let (mut text, mut sections) = working.render_sections(budget);
        let mut tokens = estimate_tokens(&text);
        while tokens > budget.token_budget {
            let Some(step) = working.drop_next() else {
                break;
            };
            dropped.push(step);
            let rendered = working.render_sections(budget);
            text = rendered.0;
            sections = rendered.1;
            tokens = estimate_tokens(&text);
        }

        RenderOutcome {
            over_budget: tokens > budget.token_budget,
            text,
            tokens,
            dropped,
            sections,
        }
    }

    /// Truncates every capped section in place (spec §6 per-section caps).
    pub fn apply_caps(&mut self, caps: &SectionCaps) {
        self.just_before.truncate(caps.just_before);
        self.memories.truncate(caps.memories);
        self.facts.truncate(caps.facts);
        self.vault_notes.truncate(caps.vault_notes);
        self.pending_decisions.truncate(caps.pending_decisions);
        self.recent_changes.truncate(caps.recent_changes);
        self.failed_attempts.truncate(caps.failed_attempts);
        if let Some(session) = self.session.as_mut() {
            session.open_files.truncate(caps.open_files);
        }
        if let Some(moment) = self.current_moment.as_mut() {
            if let Some(compact) = moment.world_compact.as_mut() {
                if compact.len() > caps.world_compact_chars {
                    *compact = truncate_on_char_boundary(compact, caps.world_compact_chars);
                }
            }
        }
    }

    /// Applies the next rung of [`DROP_ORDER`], returning which one ran.
    /// `None` once only the never-drop set is left.
    fn drop_next(&mut self) -> Option<DropStep> {
        if let Some(session) = self.session.as_mut() {
            if !session.open_files.is_empty() {
                session.open_files.clear();
                return Some(DropStep::OpenFiles);
            }
        }
        if !self.recent_changes.is_empty() {
            self.recent_changes.clear();
            return Some(DropStep::RecentChanges);
        }
        if !self.just_before.is_empty() {
            self.just_before.pop();
            return Some(DropStep::JustBeforeTail);
        }
        if !self.memories.is_empty() {
            self.memories.pop();
            return Some(DropStep::MemoriesTail);
        }
        None
    }

    /// Renders every non-empty section, in the contract order, together
    /// with the list of headings actually written (fixwave 3b, I1 — the
    /// only honest source for `sections_present`).
    fn render_sections(&self, budget: &PackageBudget) -> (String, Vec<PackageSection>) {
        let cloud = budget.cloud_bound;
        let mut out = String::with_capacity(2_048);
        let mut sections: Vec<PackageSection> = Vec::new();

        // 1. Current moment — never dropped; generalized when local_only.
        if let Some(moment) = &self.current_moment {
            sections.push(PackageSection::CurrentMoment);
            out.push_str("## Current moment\n");
            let hide = cloud && moment.local_only;
            if hide {
                out.push_str(&format!("Screen: {PRIVATE_PLACEHOLDER}\n"));
            } else {
                out.push_str(&format!("Screen: {}\n", moment.caption));
                if let Some(title) = non_empty(&moment.window_title) {
                    out.push_str(&format!("Window: {title}\n"));
                }
                if let Some(app) = non_empty(&moment.app) {
                    out.push_str(&format!("App: {app}\n"));
                }
                if let Some(audio) = non_empty(&moment.audio) {
                    out.push_str(&format!("Audio: \"{audio}\"\n"));
                }
                if let Some(compact) = non_empty(&moment.world_compact) {
                    out.push_str("World state:\n");
                    out.push_str(compact);
                    if !compact.ends_with('\n') {
                        out.push('\n');
                    }
                }
            }
        }

        // 2. Session state — never dropped; goal/task generalized when
        // local_only (spec §4.1 propagation rule 2).
        if let Some(session) = self.session.as_ref().filter(|s| !s.is_empty()) {
            let hide = cloud && session.local_only;
            // Fixwave 3b (minor): the body is built first and the heading
            // only written when there is something under it. A session
            // whose only content is `open_files` on a `local_only` state
            // used to render a bare "## Session state" with no body — a
            // heading that says nothing but reads as a section.
            let mut body = String::new();
            // M4: the project slug is an identity, not a neutral label —
            // rendering it while goal and task are generalized told the
            // cloud exactly *which* private thing was being worked on.
            if let Some(project) = non_empty(&session.project).filter(|_| !hide) {
                body.push_str(&format!("Project: {project}\n"));
            }
            let goal = if hide {
                session.goal.as_ref().map(|_| PRIVATE_CONTEXT_PHRASE)
            } else {
                session.goal.as_deref()
            };
            if let Some(goal) = goal.filter(|g| !g.is_empty()) {
                body.push_str(&format!("Goal: {goal}\n"));
            }
            let task = if hide {
                session.task.as_ref().map(|_| PRIVATE_CONTEXT_PHRASE)
            } else {
                session.task.as_deref()
            };
            if let Some(task) = task.filter(|t| !t.is_empty()) {
                body.push_str(&format!(
                    "Task: {task} (confidence {:.1})\n",
                    session.confidence
                ));
            }
            if !hide && !session.open_files.is_empty() {
                body.push_str(&format!("Open files: {}\n", session.open_files.join(", ")));
            }
            if !body.is_empty() {
                sections.push(PackageSection::Session);
                out.push_str("\n## Session state\n");
                out.push_str(&body);
            }
        }

        // 3. Just before (deduped, count-aware events — Task B6).
        if push_event_section(&mut out, "## Just before", &self.just_before, cloud) {
            sections.push(PackageSection::JustBefore);
        }

        // 4. Relevant memories.
        let memories: Vec<&MemoryLine> = self
            .memories
            .iter()
            .filter(|m| !(cloud && m.local_only))
            .collect();
        if !memories.is_empty() {
            sections.push(PackageSection::Memories);
            out.push_str("\n## Relevant memories\n");
            for memory in memories {
                out.push_str(&format!("- [{}] {}\n", memory.age, memory.text));
            }
        }

        // 5. What you know about the user.
        let facts: Vec<&FactLine> = self
            .facts
            .iter()
            .filter(|f| !(cloud && f.local_only))
            .collect();
        if !facts.is_empty() {
            sections.push(PackageSection::Facts);
            out.push_str("\n## What you know about the user\n");
            for fact in facts {
                out.push_str(&format!("- {}: {}\n", fact.label, fact.value));
            }
        }

        // 6. Long-term memory (vault).
        let notes: Vec<&VaultNoteLine> = self
            .vault_notes
            .iter()
            .filter(|n| !(cloud && n.local_only))
            .collect();
        if !notes.is_empty() {
            sections.push(PackageSection::VaultNotes);
            out.push_str("\n## Long-term memory (vault)\n");
            for note in notes {
                let snippet = note
                    .snippet
                    .as_deref()
                    .filter(|s| !s.is_empty())
                    .map(|s| format!(" — {s}"))
                    .unwrap_or_default();
                out.push_str(&format!(
                    "- [{}] {}{} (importance {:.1})\n",
                    note.node_type, note.title, snippet, note.importance
                ));
            }
        }

        // 7. Recent changes (file/git).
        if push_event_section(&mut out, "\n## Recent changes", &self.recent_changes, cloud) {
            sections.push(PackageSection::RecentChanges);
        }

        // 8. Failed attempts (error events with counts).
        if push_event_section(
            &mut out,
            "\n## Failed attempts",
            &self.failed_attempts,
            cloud,
        ) {
            sections.push(PackageSection::FailedAttempts);
        }

        // 9. Last success.
        if let Some(success) = self
            .last_success
            .as_ref()
            .filter(|s| !(cloud && s.local_only))
        {
            sections.push(PackageSection::LastSuccess);
            out.push_str("\n## Last success\n");
            out.push_str(&format!("- {}\n", render_event_line(success)));
        }

        // 10. Available tools + permission mode.
        if let Some(tools) = self.tools.as_ref().filter(|t| !t.names.is_empty()) {
            sections.push(PackageSection::Tools);
            out.push_str("\n## Available tools\n");
            let shown = budget.caps.tools.max(1);
            let extra = tools.names.len().saturating_sub(shown);
            let mut listed = tools.names[..tools.names.len().min(shown)].join(", ");
            if extra > 0 {
                listed.push_str(&format!(", +{extra} more"));
            }
            out.push_str(&format!("{listed}\n"));
            if !tools.permission_mode.is_empty() {
                out.push_str(&format!("Permission mode: {}\n", tools.permission_mode));
            }
        }

        // 11. Recommended next step (continuation resolver, §4.12).
        // Cloud-gated exactly like §9: a suggestion derived from
        // `local_only` evidence is dropped rather than generalized — a
        // "next step" the orchestrator cannot read is worse than no
        // section, and §13 already says the context is private.
        if let Some(next) = self
            .recommended_next_step
            .as_ref()
            .filter(|n| !n.text.is_empty())
            .filter(|n| !(cloud && n.local_only))
        {
            sections.push(PackageSection::RecommendedNextStep);
            out.push_str("\n## Recommended next step\n");
            out.push_str(&format!(
                "{} (confidence {:.1})\n",
                next.text, next.confidence
            ));
        }

        // 12. Pending memory decisions — ORDER CONTRACT: the last section
        // before the wake reason, never dropped.
        let pending: Vec<&PendingDecisionLine> = self
            .pending_decisions
            .iter()
            .filter(|p| !(cloud && p.local_only))
            .collect();
        if !pending.is_empty() {
            sections.push(PackageSection::PendingDecisions);
            out.push_str("\n## Pending memory decisions\n");
            for note in pending {
                out.push_str(&format!(
                    "- id: {} — [{}] \"{}\" (confidence {:.1}, source {})\n",
                    note.id, note.node_type, note.title, note.confidence, note.source,
                ));
            }
            out.push_str(
                "Resolve these with the memory_vault_resolve tool (confirm/reject/supersede) \
                 or improve them with memory_vault_save. Skip any you are unsure about.\n",
            );
        }

        // 13. Why you were woken — spec §4.1 propagation rule (3): a wake
        // triggered by a `local_only` frame (or over a `local_only`
        // session) gets the generic reason instead of the triage LLM's
        // verbatim sentence, which quotes what it saw.
        if let Some(reason) = non_empty(&self.why_woken) {
            sections.push(PackageSection::WhyWoken);
            out.push_str("\n## Why you were woken\n");
            if cloud && self.why_woken_local_only {
                out.push_str(PRIVATE_WAKE_REASON);
            } else {
                out.push_str(reason);
            }
            out.push('\n');
        }

        (out, sections)
    }
}

/// Renders one `- [age] text (×N)` line.
fn render_event_line(line: &EventLine) -> String {
    let count = if line.count > 1 {
        format!(" (×{})", line.count)
    } else {
        String::new()
    };
    format!("[{}] {}{}", line.age, line.text, count)
}

/// Pushes a heading + cloud-gated event lines, or nothing when every line
/// was gated away. Returns whether the section was written.
fn push_event_section(out: &mut String, heading: &str, lines: &[EventLine], cloud: bool) -> bool {
    let visible: Vec<&EventLine> = lines.iter().filter(|l| !(cloud && l.local_only)).collect();
    if visible.is_empty() {
        return false;
    }
    out.push_str(heading);
    out.push('\n');
    for line in visible {
        out.push_str(&format!("- {}\n", render_event_line(line)));
    }
    true
}

fn non_empty(value: &Option<String>) -> Option<&str> {
    value.as_deref().filter(|s| !s.is_empty())
}

/// Truncates `s` to at most `max` bytes on a UTF-8 char boundary, appending
/// `...`. Never panics on multi-byte input.
pub fn truncate_on_char_boundary(s: &str, max: usize) -> String {
    if s.len() <= max {
        return s.to_string();
    }
    let mut cut = max.saturating_sub(3).min(s.len());
    while cut > 0 && !s.is_char_boundary(cut) {
        cut -= 1;
    }
    format!("{}...", &s[..cut])
}

// ---------------------------------------------------------------------------
// Post-wake structured record (spec §4.9)
// ---------------------------------------------------------------------------

/// The best-effort structured trailer parsed out of an orchestrator reply.
///
/// Feeds the continuation resolver (spec §4.12) via a `wake_result` system
/// event; every field is optional because the parse is *best effort* — a
/// reply without a trailer produces `None`, never an error and never a
/// fabricated record.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct WakeTrailer {
    /// What Continuum did.
    pub action: Option<String>,
    /// How it went.
    pub result: Option<String>,
    /// What should happen next — the field the continuation resolver ranks.
    pub next_step: Option<String>,
}

impl WakeTrailer {
    /// Whether anything at all was parsed.
    pub fn is_empty(&self) -> bool {
        self.action.is_none() && self.result.is_none() && self.next_step.is_none()
    }

    /// The one-line summary stored on the `wake_result` event — **only**
    /// the next step.
    ///
    /// Fixwave 3b (minor): this used to fall back to `result` and then
    /// `action`. The `wake_result` event summary is exactly what the §4.12
    /// continuation resolver ranks as a `wake_next_step` candidate at 0.8
    /// base confidence, so "result: fixed the flaky test" came back as a
    /// 0.72 *next step* and "ga door" recommended redoing finished work.
    /// A wake with no next step contributes no candidate — its `action` and
    /// `result` still reach the vault timeline via [`Self::render_line`].
    pub fn summary(&self) -> Option<&str> {
        self.next_step.as_deref()
    }

    /// A compact `key: value` rendering for the vault timeline entry.
    pub fn render_line(&self) -> String {
        let mut parts = Vec::new();
        if let Some(action) = &self.action {
            parts.push(format!("action: {action}"));
        }
        if let Some(result) = &self.result {
            parts.push(format!("result: {result}"));
        }
        if let Some(next) = &self.next_step {
            parts.push(format!("next step: {next}"));
        }
        parts.join(" | ")
    }
}

/// Max characters kept per trailer field (a runaway "next step:" paragraph
/// must not become an unbounded event summary).
pub const TRAILER_FIELD_MAX_CHARS: usize = 200;

/// Label prefixes recognised on a trailer line, mapped to their field.
/// Dutch aliases are included because Continuum's user speaks Dutch and
/// the orchestrator mirrors the user's language (SOUL.md).
const TRAILER_LABELS: &[(&str, TrailerField)] = &[
    ("next step:", TrailerField::NextStep),
    ("next_step:", TrailerField::NextStep),
    ("volgende stap:", TrailerField::NextStep),
    ("result:", TrailerField::Result),
    ("resultaat:", TrailerField::Result),
    ("action:", TrailerField::Action),
    ("actie:", TrailerField::Action),
];

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum TrailerField {
    Action,
    Result,
    NextStep,
}

/// Parses the best-effort `{action, result, next_step}` trailer out of an
/// orchestrator reply (spec §4.9 post-wake structured record).
///
/// Rules, all deliberately forgiving:
/// - labels are matched case-insensitively at the start of a line, after
///   stripping markdown list/emphasis noise (`-`, `*`, `#`, `**`);
/// - the **last** occurrence of a label wins (a reply that restates its
///   next step at the end means the last one);
/// - empty values and the literals `none`/`geen`/`n/a`/`-` are ignored;
/// - values are trimmed and capped at [`TRAILER_FIELD_MAX_CHARS`];
/// - nothing parseable → `None`. Garbage input can never produce a record.
pub fn parse_wake_trailer(text: &str) -> Option<WakeTrailer> {
    let mut trailer = WakeTrailer::default();
    for raw_line in text.lines() {
        let line = raw_line
            .trim()
            .trim_start_matches(['-', '*', '#', '>', ' ']);
        let line = line.trim_start_matches('*').trim();
        let lowered = line.to_lowercase();
        let Some((label, field)) = TRAILER_LABELS
            .iter()
            .find(|(label, _)| lowered.starts_with(label))
            .map(|(label, field)| (*label, *field))
        else {
            continue;
        };
        let value = line[label.len()..].trim_matches(trailer_noise);
        if value.is_empty() || is_negative_value(value) {
            continue;
        }
        let value = truncate_on_char_boundary(value, TRAILER_FIELD_MAX_CHARS);
        match field {
            TrailerField::Action => trailer.action = Some(value),
            TrailerField::Result => trailer.result = Some(value),
            TrailerField::NextStep => trailer.next_step = Some(value),
        }
    }
    (!trailer.is_empty()).then_some(trailer)
}

/// Markdown noise stripped from both ends of a trailer value. Applied as
/// one `trim_matches` pass so `** \`value\` **` collapses regardless of the
/// order the model wrapped it in.
fn trailer_noise(c: char) -> bool {
    c.is_whitespace() || c == '*' || c == '`' || c == '_'
}

/// Values that mean "nothing here" and must not become a record.
fn is_negative_value(value: &str) -> bool {
    matches!(
        value.trim_end_matches('.').to_lowercase().as_str(),
        "none" | "geen" | "n/a" | "na" | "-" | "–" | "unknown" | "nvt" | "n.v.t"
    )
}

// ---------------------------------------------------------------------------
// Curator session trailer (spec §4.9)
// ---------------------------------------------------------------------------

/// The structured trailer line the curator's session summary must end with
/// so the continuation resolver (Task B8) can read the open task without an
/// LLM call.
pub const OPEN_TASK_PREFIX: &str = "open_task:";

/// The normalized "nothing open" value.
pub const OPEN_TASK_NONE: &str = "none";

/// Normalizes a curator session-summary body so it ends with exactly one
/// well-formed `open_task:` line (spec §4.9).
///
/// The prompt asks for the line, but an LLM is an LLM: it may omit it,
/// emit it in the middle, emit it twice, or wrap it in markdown. This
/// function is the guarantee the resolver relies on:
///
/// - every existing `open_task:` line is removed from the body;
/// - the **last** non-empty one wins as the value;
/// - if none was found, the `## Next step` section's line is used;
/// - anything still missing or negative normalizes to `open_task: none`;
/// - the value is single-line and capped at [`TRAILER_FIELD_MAX_CHARS`].
pub fn normalize_open_task_trailer(body: &str) -> String {
    let mut kept: Vec<&str> = Vec::new();
    let mut found: Option<String> = None;
    let mut next_step_from_section: Option<String> = None;
    let mut in_next_step = false;

    for raw_line in body.lines() {
        let trimmed = raw_line.trim();
        let stripped = trimmed.trim_start_matches(['-', '*', '#', '>', ' ']).trim();
        if stripped.to_lowercase().starts_with(OPEN_TASK_PREFIX) {
            let value = stripped[OPEN_TASK_PREFIX.len()..].trim_matches(trailer_noise);
            if !value.is_empty() && !is_negative_value(value) {
                found = Some(truncate_on_char_boundary(value, TRAILER_FIELD_MAX_CHARS));
            } else {
                found = Some(OPEN_TASK_NONE.to_string());
            }
            continue;
        }
        if trimmed.to_lowercase().starts_with("## ") {
            in_next_step = trimmed.to_lowercase().starts_with("## next step");
        } else if in_next_step && !trimmed.is_empty() && next_step_from_section.is_none() {
            let value = trimmed.trim_start_matches(['-', '*', ' ']).trim();
            if !value.is_empty() && !is_negative_value(value) {
                next_step_from_section =
                    Some(truncate_on_char_boundary(value, TRAILER_FIELD_MAX_CHARS));
            }
        }
        kept.push(raw_line);
    }

    let value = match found {
        Some(v) if v != OPEN_TASK_NONE => v,
        Some(_) => next_step_from_section.unwrap_or_else(|| OPEN_TASK_NONE.to_string()),
        None => next_step_from_section.unwrap_or_else(|| OPEN_TASK_NONE.to_string()),
    };

    let mut out = kept.join("\n").trim_end().to_string();
    if !out.is_empty() {
        out.push_str("\n\n");
    }
    out.push_str(OPEN_TASK_PREFIX);
    out.push(' ');
    out.push_str(&value);
    out
}

/// Reads the `open_task:` value back out of a normalized session-summary
/// body. `None` when absent or `none` (Task B8's resolver entry point).
pub fn read_open_task(body: &str) -> Option<String> {
    let mut found = None;
    for raw_line in body.lines() {
        let stripped = raw_line
            .trim()
            .trim_start_matches(['-', '*', '#', '>', ' '])
            .trim();
        if stripped.to_lowercase().starts_with(OPEN_TASK_PREFIX) {
            let value = stripped[OPEN_TASK_PREFIX.len()..].trim_matches(trailer_noise);
            if !value.is_empty() && !is_negative_value(value) {
                found = Some(value.to_string());
            } else {
                found = None;
            }
        }
    }
    found
}

// ---------------------------------------------------------------------------
// Tests — these run under BOTH feature sets (the module is ungated, spec §3)
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;

    fn event(age: &str, text: &str, count: i64) -> EventLine {
        EventLine {
            age: age.to_string(),
            text: text.to_string(),
            count,
            local_only: false,
        }
    }

    fn full_package() -> ContextPackage {
        ContextPackage {
            current_moment: Some(CurrentMoment {
                caption: "VS Code showing a failing test".to_string(),
                window_title: Some("wake_context.rs - continuum".to_string()),
                app: Some("Code.exe".to_string()),
                world_compact: Some("live-context/v2 seq=7\n[monitor:display-1] main".to_string()),
                audio: Some("waarom faalt dit".to_string()),
                local_only: false,
            }),
            session: Some(SessionSection {
                project: Some("continuum".to_string()),
                goal: Some("ship the context engine".to_string()),
                task: Some("fix the wake packager".to_string()),
                confidence: 0.8,
                open_files: vec!["src/context/package.rs".to_string()],
                local_only: false,
            }),
            just_before: vec![
                event("1m ago", "build failed", 14),
                event("3m ago", "edit", 1),
            ],
            memories: vec![MemoryLine {
                age: "2h ago".to_string(),
                text: "User was debugging triage JSON parsing".to_string(),
                local_only: false,
            }],
            facts: vec![FactLine {
                label: "Name".to_string(),
                value: "\"Toshan\"".to_string(),
                local_only: false,
            }],
            vault_notes: vec![VaultNoteLine {
                node_type: "preference".to_string(),
                title: "Prefers dark mode".to_string(),
                snippet: Some("asked for dark theme".to_string()),
                importance: 0.9,
                local_only: false,
            }],
            recent_changes: vec![event("2m ago", "modified package.rs", 3)],
            failed_attempts: vec![event("1m ago", "cargo test failed", 4)],
            last_success: Some(event("20m ago", "cargo build succeeded", 1)),
            tools: Some(ToolsSection {
                names: vec!["mcp__continuum__*".to_string()],
                permission_mode: "default".to_string(),
            }),
            recommended_next_step: Some(NextStep {
                text: "re-run the failing test".to_string(),
                confidence: 0.7,
                local_only: false,
            }),
            pending_decisions: vec![PendingDecisionLine {
                id: "mem_pending1".to_string(),
                node_type: "decision".to_string(),
                title: "Switching to PostgreSQL".to_string(),
                confidence: 0.6,
                source: "observed".to_string(),
                local_only: false,
            }],
            why_woken: Some("User appears stuck on test failures".to_string()),
            why_woken_local_only: false,
        }
    }

    // --- per-section render ------------------------------------------------

    #[test]
    fn renders_every_section_when_filled() {
        let msg = full_package().render(&PackageBudget::cloud(10_000));
        for heading in [
            "## Current moment",
            "## Session state",
            "## Just before",
            "## Relevant memories",
            "## What you know about the user",
            "## Long-term memory (vault)",
            "## Recent changes",
            "## Failed attempts",
            "## Last success",
            "## Available tools",
            "## Recommended next step",
            "## Pending memory decisions",
            "## Why you were woken",
        ] {
            assert!(msg.contains(heading), "missing section {heading}:\n{msg}");
        }
    }

    #[test]
    fn render_report_lists_only_sections_that_survive_the_cloud_gate() {
        let mut pkg = full_package();
        pkg.vault_notes[0].local_only = true;
        pkg.session = Some(SessionSection {
            project: None,
            goal: None,
            task: None,
            confidence: 0.8,
            open_files: vec!["C:/private/secret.rs".to_string()],
            local_only: true,
        });

        let outcome = pkg.render_with_report(&PackageBudget::cloud(10_000));

        assert!(!outcome.text.contains("## Session state"));
        assert!(!outcome.text.contains("## Long-term memory (vault)"));
        assert!(!outcome.sections.contains(&PackageSection::Session));
        assert!(!outcome.sections.contains(&PackageSection::VaultNotes));
        assert!(outcome.sections.contains(&PackageSection::CurrentMoment));
        assert!(outcome.sections.contains(&PackageSection::Memories));
    }

    #[test]
    fn render_report_removes_sections_dropped_for_budget() {
        let pkg = ContextPackage {
            current_moment: Some(CurrentMoment {
                caption: "terminal".to_string(),
                ..Default::default()
            }),
            recent_changes: vec![event("1m ago", &"changed file ".repeat(200), 1)],
            why_woken: Some("test wake".to_string()),
            ..Default::default()
        };

        let outcome = pkg.render_with_report(&PackageBudget::cloud(40));

        assert!(outcome.dropped.contains(&DropStep::RecentChanges));
        assert!(!outcome.text.contains("## Recent changes"));
        assert!(!outcome.sections.contains(&PackageSection::RecentChanges));
        assert!(outcome.sections.contains(&PackageSection::CurrentMoment));
        assert!(outcome.sections.contains(&PackageSection::WhyWoken));
    }

    #[test]
    fn current_moment_renders_window_title_and_world_compact() {
        let msg = full_package().render(&PackageBudget::cloud(10_000));
        assert!(msg.contains("Screen: VS Code showing a failing test"));
        assert!(msg.contains("Window: wake_context.rs - continuum"));
        assert!(msg.contains("App: Code.exe"));
        assert!(msg.contains("Audio: \"waarom faalt dit\""));
        assert!(msg.contains("World state:\nlive-context/v2 seq=7"));
    }

    #[test]
    fn session_section_renders_fields_and_confidence() {
        let msg = full_package().render(&PackageBudget::cloud(10_000));
        assert!(msg.contains("Project: continuum"));
        assert!(msg.contains("Goal: ship the context engine"));
        assert!(msg.contains("Task: fix the wake packager (confidence 0.8)"));
        assert!(msg.contains("Open files: src/context/package.rs"));
    }

    #[test]
    fn count_aware_event_lines_render_the_multiplier() {
        let msg = full_package().render(&PackageBudget::cloud(10_000));
        assert!(msg.contains("- [1m ago] build failed (×14)"));
        assert!(msg.contains("- [3m ago] edit\n"), "count=1 has no suffix");
        assert!(msg.contains("- [1m ago] cargo test failed (×4)"));
        assert!(msg.contains("- [20m ago] cargo build succeeded"));
    }

    #[test]
    fn tools_section_lists_names_and_permission_mode() {
        let msg = full_package().render(&PackageBudget::cloud(10_000));
        assert!(msg.contains("mcp__continuum__*"));
        assert!(msg.contains("Permission mode: default"));
    }

    #[test]
    fn tools_beyond_the_cap_collapse_into_a_counter() {
        let mut pkg = full_package();
        pkg.tools = Some(ToolsSection {
            names: (0..20).map(|i| format!("tool_{i}")).collect(),
            permission_mode: "default".to_string(),
        });
        let msg = pkg.render(&PackageBudget::cloud(10_000));
        assert!(msg.contains("+8 more"), "{msg}");
    }

    #[test]
    fn empty_sections_render_no_heading() {
        let pkg = ContextPackage {
            current_moment: Some(CurrentMoment {
                caption: "idle desktop".to_string(),
                ..Default::default()
            }),
            why_woken: Some("Test wake".to_string()),
            ..Default::default()
        };
        let msg = pkg.render(&PackageBudget::cloud(10_000));
        assert!(msg.contains("## Current moment"));
        assert!(msg.contains("## Why you were woken"));
        for absent in [
            "## Session state",
            "## Just before",
            "## Relevant memories",
            "## Recent changes",
            "## Failed attempts",
            "## Last success",
            "## Available tools",
            "## Recommended next step",
            "## Pending memory decisions",
        ] {
            assert!(!msg.contains(absent), "unexpected section {absent}");
        }
    }

    // --- order contract ----------------------------------------------------

    #[test]
    fn pending_decisions_are_the_last_section_before_the_reason() {
        let msg = full_package().render(&PackageBudget::cloud(10_000));
        let pending = msg.find("## Pending memory decisions").unwrap();
        let reason = msg.find("## Why you were woken").unwrap();
        assert!(pending < reason);
        // Nothing else may sit between them.
        assert!(!msg[pending..reason]
            .trim_start_matches("## Pending memory decisions")
            .contains("\n## "));
        // And every other section comes before pending.
        for heading in [
            "## Current moment",
            "## Session state",
            "## Just before",
            "## Long-term memory (vault)",
            "## Recent changes",
            "## Failed attempts",
            "## Last success",
            "## Available tools",
            "## Recommended next step",
        ] {
            assert!(
                msg.find(heading).unwrap() < pending,
                "{heading} after pending"
            );
        }
    }

    // --- cloud gate --------------------------------------------------------

    #[test]
    fn cloud_bound_render_skips_local_only_items() {
        let mut pkg = full_package();
        pkg.just_before[0].local_only = true;
        pkg.memories[0].local_only = true;
        pkg.facts[0].local_only = true;
        pkg.vault_notes[0].local_only = true;
        pkg.recent_changes[0].local_only = true;
        pkg.failed_attempts[0].local_only = true;
        pkg.last_success.as_mut().unwrap().local_only = true;
        pkg.pending_decisions[0].local_only = true;

        let cloud = pkg.render(&PackageBudget::cloud(10_000));
        assert!(!cloud.contains("build failed"));
        assert!(!cloud.contains("## Relevant memories"));
        assert!(!cloud.contains("## What you know about the user"));
        assert!(!cloud.contains("## Long-term memory (vault)"));
        assert!(!cloud.contains("## Recent changes"));
        assert!(!cloud.contains("## Failed attempts"));
        assert!(!cloud.contains("## Last success"));
        assert!(!cloud.contains("## Pending memory decisions"));
        // The non-local sibling survives.
        assert!(cloud.contains("- [3m ago] edit"));

        let local = pkg.render(&PackageBudget::local(10_000));
        assert!(local.contains("build failed"));
        assert!(local.contains("## Pending memory decisions"));
    }

    #[test]
    fn cloud_bound_render_generalizes_local_only_moment_and_session() {
        let mut pkg = full_package();
        pkg.current_moment.as_mut().unwrap().local_only = true;
        pkg.session.as_mut().unwrap().local_only = true;

        let cloud = pkg.render(&PackageBudget::cloud(10_000));
        assert!(cloud.contains("## Current moment"));
        assert!(cloud.contains(&format!("Screen: {PRIVATE_PLACEHOLDER}")));
        assert!(!cloud.contains("VS Code showing a failing test"));
        assert!(!cloud.contains("Window: wake_context.rs"));
        // Session state still renders — generalized, not dropped.
        assert!(cloud.contains("## Session state"));
        // M4: the project slug is identity. Generalizing goal + task while
        // still naming the project told the cloud *which* private thing
        // was being worked on.
        assert!(!cloud.contains("Project: continuum"));
        assert!(cloud.contains(&format!("Goal: {PRIVATE_CONTEXT_PHRASE}")));
        assert!(cloud.contains(&format!("Task: {PRIVATE_CONTEXT_PHRASE}")));
        assert!(!cloud.contains("Open files:"));

        let local = pkg.render(&PackageBudget::local(10_000));
        assert!(local.contains("VS Code showing a failing test"));
        assert!(local.contains("Goal: ship the context engine"));
        // A local render keeps the project — the gate is the cloud, not
        // the zone alone.
        assert!(local.contains("Project: continuum"));
    }

    // --- fixwave 2: the two sections the gate never touched -----------------

    /// C1: the continuation resolver ranks `SessionState::current_task`
    /// **raw**, so a `local_only` session's private task rode into the
    /// cloud through "## Recommended next step" — a section that had no
    /// zone tag at all — while the same package's "## Session state"
    /// correctly printed the generic phrase.
    #[test]
    fn a_local_only_next_step_is_dropped_from_a_cloud_render() {
        let mut pkg = full_package();
        pkg.session.as_mut().unwrap().local_only = true;
        pkg.recommended_next_step = Some(NextStep {
            text: "finish reviewing the Q3 layoff list".to_string(),
            confidence: 0.9,
            local_only: true,
        });

        let cloud = pkg.render(&PackageBudget::cloud(10_000));
        assert!(
            !cloud.contains("## Recommended next step"),
            "the heading itself must not render:\n{cloud}"
        );
        assert!(
            !cloud.contains("layoff"),
            "the private task leaked into the cloud package:\n{cloud}"
        );

        // Locally the suggestion is exactly as useful as before.
        let local = pkg.render(&PackageBudget::local(10_000));
        assert!(local.contains("## Recommended next step"));
        assert!(local.contains("finish reviewing the Q3 layoff list"));

        // A cloud-safe next step is untouched by the new filter.
        pkg.recommended_next_step.as_mut().unwrap().local_only = false;
        assert!(pkg
            .render(&PackageBudget::cloud(10_000))
            .contains("finish reviewing the Q3 layoff list"));
    }

    /// C1 / spec §4.1 propagation rule (3): the wake reason is authored by
    /// the triage LLM from what it saw (and, for continue-class wakes,
    /// enriched with the resolved continuation target), so a wake from a
    /// private zone gets the generic phrase instead.
    #[test]
    fn a_local_only_wake_reason_renders_the_generic_phrase() {
        let mut pkg = full_package();
        pkg.why_woken =
            Some("User is reviewing the Q3 layoff list in the private browser".to_string());
        pkg.why_woken_local_only = true;

        let cloud = pkg.render(&PackageBudget::cloud(10_000));
        assert!(cloud.contains("## Why you were woken"));
        assert!(
            cloud.contains(PRIVATE_WAKE_REASON),
            "expected the spec's generic reason:\n{cloud}"
        );
        assert!(
            !cloud.contains("layoff"),
            "the triage LLM's sentence leaked verbatim:\n{cloud}"
        );

        // Local models still get the real reason — same pattern as the
        // session section.
        let local = pkg.render(&PackageBudget::local(10_000));
        assert!(local.contains("Q3 layoff list"));
        assert!(!local.contains(PRIVATE_WAKE_REASON));

        // Untagged reasons are unaffected.
        pkg.why_woken_local_only = false;
        assert!(pkg
            .render(&PackageBudget::cloud(10_000))
            .contains("Q3 layoff list"));
    }

    /// The generic reason is never dropped by the budget ladder either —
    /// "why you were woken" is in the never-drop set, and the replacement
    /// happens at render time, not by emptying the field.
    #[test]
    fn the_generic_wake_reason_survives_the_drop_ladder() {
        let mut pkg = full_package();
        pkg.why_woken = Some("something very private and very long".repeat(20));
        pkg.why_woken_local_only = true;
        let out = pkg.render_with_report(&PackageBudget::cloud(50));
        assert!(out.text.contains(PRIVATE_WAKE_REASON));
        assert!(!out.text.contains("something very private"));
    }

    // --- caps + budget + drop order ---------------------------------------

    #[test]
    fn per_section_caps_truncate_before_the_budget_check() {
        let mut pkg = full_package();
        pkg.just_before = (0..20)
            .map(|i| event("1m ago", &format!("e{i}"), 1))
            .collect();
        pkg.memories = (0..20)
            .map(|i| MemoryLine {
                age: "1h ago".to_string(),
                text: format!("m{i}"),
                local_only: false,
            })
            .collect();
        let msg = pkg.render(&PackageBudget::cloud(100_000));
        assert!(msg.contains("e4"));
        assert!(!msg.contains("e5"), "just_before cap is 5");
        assert!(msg.contains("m2"));
        assert!(!msg.contains("m3"), "memories cap is 3");
    }

    #[test]
    fn world_compact_is_capped_on_a_char_boundary() {
        let mut pkg = full_package();
        pkg.current_moment.as_mut().unwrap().world_compact = Some("é".repeat(2_000));
        let caps = SectionCaps {
            world_compact_chars: 50,
            ..Default::default()
        };
        let msg = pkg.render(&PackageBudget::cloud(100_000).with_caps(caps));
        assert!(msg.contains("World state:"));
        assert!(msg.contains("..."));
    }

    #[test]
    fn drop_order_is_open_files_then_changes_then_just_before_then_memories() {
        let pkg = full_package();
        // Squeeze the budget progressively and watch the ladder.
        let full = pkg.render_with_report(&PackageBudget::cloud(10_000));
        assert!(full.dropped.is_empty());

        let steps: Vec<DropStep> = pkg.render_with_report(&PackageBudget::cloud(1)).dropped;
        // The ladder ran to exhaustion, in order.
        assert_eq!(steps[0], DropStep::OpenFiles);
        assert_eq!(steps[1], DropStep::RecentChanges);
        assert!(steps[2..4].iter().all(|s| *s == DropStep::JustBeforeTail));
        assert_eq!(steps[4], DropStep::MemoriesTail);
        assert_eq!(steps.len(), DROP_ORDER.len() + 1); // just_before had 2 lines
    }

    #[test]
    fn the_never_drop_set_survives_an_impossible_budget() {
        let msg = full_package().render(&PackageBudget::cloud(1));
        assert!(msg.contains("## Current moment"));
        assert!(msg.contains("## Session state"));
        assert!(msg.contains("## Pending memory decisions"));
        assert!(msg.contains("## Why you were woken"));
        assert!(!msg.contains("## Recent changes"));
        assert!(!msg.contains("## Just before"));
        assert!(!msg.contains("## Relevant memories"));
        assert!(!msg.contains("Open files:"));
    }

    #[test]
    fn a_package_within_budget_is_never_trimmed() {
        let report = full_package().render_with_report(&PackageBudget::cloud(1_000));
        assert!(report.dropped.is_empty(), "{:?}", report.dropped);
        assert!(!report.over_budget);
        assert!(report.tokens <= 1_000, "{} tokens", report.tokens);
    }

    #[test]
    fn estimate_tokens_rounds_up() {
        assert_eq!(estimate_tokens(""), 0);
        assert_eq!(estimate_tokens("abcd"), 2); // 4 / 3.5 = 1.14 → 2
    }

    // --- wake trailer ------------------------------------------------------

    #[test]
    fn trailer_parses_all_three_fields() {
        let text = "Ik heb de test gefixt.\n\nAction: fixed the failing assert\n\
                    Result: cargo test is green\nNext step: run clippy";
        let trailer = parse_wake_trailer(text).unwrap();
        assert_eq!(trailer.action.as_deref(), Some("fixed the failing assert"));
        assert_eq!(trailer.result.as_deref(), Some("cargo test is green"));
        assert_eq!(trailer.next_step.as_deref(), Some("run clippy"));
        assert_eq!(trailer.summary(), Some("run clippy"));
    }

    #[test]
    fn trailer_is_case_insensitive_and_survives_markdown() {
        let text = "- **NEXT STEP:** `re-run the bench`\n* result: ok";
        let trailer = parse_wake_trailer(text).unwrap();
        assert_eq!(trailer.next_step.as_deref(), Some("re-run the bench"));
        assert_eq!(trailer.result.as_deref(), Some("ok"));
    }

    #[test]
    fn trailer_accepts_dutch_labels() {
        let trailer = parse_wake_trailer("Volgende stap: draai de tests opnieuw").unwrap();
        assert_eq!(trailer.next_step.as_deref(), Some("draai de tests opnieuw"));
    }

    #[test]
    fn trailer_last_occurrence_wins() {
        let text = "Next step: first guess\n...\nNext step: the real one";
        let trailer = parse_wake_trailer(text).unwrap();
        assert_eq!(trailer.next_step.as_deref(), Some("the real one"));
    }

    #[test]
    fn trailer_absent_yields_none() {
        assert!(parse_wake_trailer("Just a normal answer with no trailer.").is_none());
        assert!(parse_wake_trailer("").is_none());
    }

    #[test]
    fn trailer_garbage_and_empty_values_yield_none() {
        assert!(parse_wake_trailer("Next step:\nResult:   \nAction: none").is_none());
        assert!(parse_wake_trailer("{\"next_step\": 42}").is_none());
        assert!(parse_wake_trailer("nextstep run it").is_none());
    }

    #[test]
    fn trailer_fields_are_capped() {
        let long = "x".repeat(500);
        let trailer = parse_wake_trailer(&format!("Next step: {long}")).unwrap();
        assert!(trailer.next_step.as_deref().unwrap().len() <= TRAILER_FIELD_MAX_CHARS);
    }

    #[test]
    fn trailer_render_line_is_compact() {
        let trailer = parse_wake_trailer("Action: a\nResult: b\nNext step: c").unwrap();
        assert_eq!(
            trailer.render_line(),
            "action: a | result: b | next step: c"
        );
    }

    // --- curator open_task trailer ----------------------------------------

    #[test]
    fn open_task_trailer_is_kept_and_moved_to_the_end() {
        let body = "## Goal\nship B7\nopen_task: wire the packager\n## Result\ndone";
        let normalized = normalize_open_task_trailer(body);
        assert!(normalized.ends_with("open_task: wire the packager"));
        assert_eq!(normalized.matches("open_task:").count(), 1);
        assert_eq!(
            read_open_task(&normalized).as_deref(),
            Some("wire the packager")
        );
    }

    #[test]
    fn open_task_trailer_falls_back_to_the_next_step_section() {
        let body = "## Goal\nship B7\n## Next step\nre-run the gates\n";
        let normalized = normalize_open_task_trailer(body);
        assert!(normalized.ends_with("open_task: re-run the gates"));
    }

    #[test]
    fn open_task_trailer_normalizes_missing_and_negative_values() {
        let normalized = normalize_open_task_trailer("## Goal\nidle\n## Next step\nnone");
        assert!(normalized.ends_with("open_task: none"));
        assert!(read_open_task(&normalized).is_none());

        let normalized = normalize_open_task_trailer("## Goal\nidle\nopen_task:");
        assert!(normalized.ends_with("open_task: none"));
    }

    #[test]
    fn open_task_trailer_collapses_duplicates_keeping_the_last() {
        let body = "open_task: first\nbody line\n- **open_task:** second";
        let normalized = normalize_open_task_trailer(body);
        assert_eq!(normalized.matches("open_task:").count(), 1);
        assert!(normalized.ends_with("open_task: second"));
        assert!(normalized.contains("body line"));
    }

    #[test]
    fn open_task_is_idempotent() {
        let once = normalize_open_task_trailer("## Goal\nship\nopen_task: land B7");
        let twice = normalize_open_task_trailer(&once);
        assert_eq!(once, twice);
    }
}

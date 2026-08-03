# Continuum Architecture

Status: Phase 0 architecture contract
Last updated: 2026-08-01

## Product contract

Continuum is a local context, handoff, and permission layer for coding agents.
Its first promise is deliberately narrow:

> Every coding agent starts with the right project context, can continue a
> previous agent's work, and records evidence for what happened.

Continuum is not a general life assistant in v0.1. Voice, ambient screen
capture, proactive coaching, hardware, hosted sync, and a universal personal
world model are later input channels or products. They are not the core.

## Phase 0 boundary

The repository started as the Apache-2.0 Continuum project. Phase 0 preserves the
parts that are expensive and useful to rebuild while introducing a clean
boundary for Continuum.

### Keep as donor infrastructure

- Tauri 2 desktop host and Next.js frontend build pipeline
- Rust daemon, structured logging, health checks, and process supervision
- SQLite and LanceDB storage adapters
- MCP protocol implementation and filesystem/network hardening primitives
- worker process management, skills loading, and audit event capture

### Replace behind stable adapters

- Continuum's string-key semantic memory becomes a migration input, not the new model
- hardcoded project recognition is replaced by repository detection
- Claude-specific orchestration becomes model-independent agent adapters
- post-execution auditing becomes pre-execution policy enforcement
- senses, voice, and vision become optional event producers

### Freeze during the vertical slice

- audio, TTS, wake word, and ambient vision features
- hardware integrations
- hosted accounts, sync, billing, and team collaboration
- broad renaming of internal `continuum-*` crates before compatibility adapters exist

## Target architecture

```text
Local sources
  Git | filesystem | project docs | agent runs | user confirmations
                               |
                               v
                    Append-only event store
                               |
                               v
                    Verified world-model projection
                               |
                 +-------------+-------------+
                 |                           |
                 v                           v
          Context compiler            Permission gateway
                 |                           |
                 +-------------+-------------+
                               |
                               v
                Codex | Claude Code | local agents
                               |
                               v
                 Outcomes + evidence events
```

The event store is authoritative. The world model is a rebuildable projection.
Embeddings help retrieval, but never decide whether a fact is current or true.

## Domain model

The first typed entities are:

- `Project`, `Repository`, `Goal`, `Decision`, `Task`, and `Blocker`
- `Convention`, `Artifact`, and `Person`
- `AgentRun`, `Outcome`, and `Evidence`
- `PermissionPolicy`, `ActionRequest`, and `Approval`

Every durable record includes:

```text
id, entity_type, project_id, content, source, source_reference,
created_at, valid_from, valid_until, confidence, status, sensitivity,
supersedes, confirmed_by
```

Valid status values in v0.1 are `observed`, `inferred`, `confirmed`,
`disputed`, `superseded`, and `expired`.

## Required data flow

1. Detect the project from cwd, Git root, remote, and project documents.
2. Record source observations as immutable events.
3. Rebuild or incrementally update typed project projections.
4. Compile a bounded context package with source references.
5. Launch an adapter with that package and explicit execution constraints.
6. Capture diff, command exit codes, tests, logs, and agent output as evidence.
7. Propose inferred memories; require confirmation for important conclusions.
8. Record the verified outcome and make it available to the next agent.

## Permission invariant

Audit logging after execution is not permission enforcement.

The only acceptable protected flow is:

```text
action request -> policy evaluation -> allow / ask / deny
               -> execute only when allowed -> record evidence
```

The gateway must default to deny when policy cannot be resolved. A tool marked
`blocked` must be unreachable from the execution path. Session approval must be
represented by a scoped, expiring grant rather than a prompt convention.

Agent-native shell and filesystem tools cannot be intercepted by MCP alone.
Continuum may claim enforced permissions only when it launches the agent with
matching sandbox/tool restrictions or routes execution through its own broker.

## Context package contract

`continuum context --project current` will produce a versioned package with:

- current objective and project identity
- relevant, current decisions with provenance
- open tasks and blockers
- conventions and explicit constraints
- recent Git state and likely relevant files
- previous agent attempts, errors, and test evidence
- allowed tools/resources and recommended next action

The compiler must fit a configured token budget and explain why every included
item was selected. It must exclude expired or superseded records by default.

## Agent adapter contract

Codex and Claude Code are the first adapters. Each adapter must support:

- availability and version check
- bounded context injection
- working-directory and project selection
- streamed lifecycle events
- explicit tool/sandbox configuration where supported
- cancellation, timeout, and exit-status capture
- normalized evidence and outcome recording

No adapter may become the authoritative owner of project memory.

## Desktop UI contract

The Continuum shell exposes real navigation for ten tabs, grouped as:

- **Daily**: Home, Chat, Voice, Memory
- **Configure**: Brain, Tools & Skills, Automations
- **Advanced**: Health, Logs
- **Settings** (system-level)

The four-layer pipeline (Senses, Triage, Orchestrator, Workers) is reflected
in the Brain tab. The full per-tab status, scope (live / fixture / hybrid),
and known gaps is tracked in `docs/CURRENT_TABS.md` — that file is the
source of truth for what each tab actually does today.

Phase 0 may display fixture data. Fixture data must stay isolated from runtime
state and must not be presented as a live integration. The visual language is:

- graphite/black desktop surfaces
- amber/gold primary actions and selected navigation
- green, orange, and red only for operational status
- dense but legible cards, 4/8px spacing rhythm, Lucide vector icons
- keyboard-visible focus, reduced-motion support, and no hover-only actions

Desktop distribution uses Tauri's signed updater for Windows. Release metadata
is a public `latest.json` projection committed by the release workflow; the
private signing key remains a GitHub Actions secret. The UI checks for updates
at startup, exposes a manual check in Settings, and stores the user's automatic
installation preference locally. Update installation is only delegated to the
Tauri updater after its artifact signature has been verified.

## Migration sequence

1. Make CI and dependency installation reproducible and blocking.
2. Ship the approved Continuum shell with fixture-backed tabs.
3. Add schema migrations against the typed world model.
4. Implement project detection and `continuum context --project current`.
5. Implement Codex/Claude handoff with evidence capture.
6. Enforce permission policy on brokered actions.
7. Connect the fixture-backed UI to real read models one panel at a time.

Step 7 is already partly underway outside its listed order: `crates/continuum-gateway`
now exists as a standalone, pure-Rust crate (provider trait, OpenAI-compatible/
Anthropic/Claude-CLI adapters, a static provider catalog) as the first concrete
piece of the Model Gateway described in `Continuum.md` §16, and two panels that
used to show fixture data — Settings → Integrations and the Chat tab — are live,
backed by real Tauri commands, `providers.json`/`~/.continuum-dev/chats/*.json`
storage, and Windows Credential Manager for secrets, not mockups. This does not
change the migration order for the remaining fixture-backed panels (Projects,
Agents, Permissions, Timeline) or pull forward the two-model architecture from
Phase 3 — it is a narrow, reviewed early slice scoped to Chat only. See
`docs/chat.md` and `docs/superpowers/specs/2026-08-02-chat-tab-design.md`.

## v0.1 acceptance demo

Claude Code attempts a task and fails. Continuum records its objective, changed
files, command output, tests, errors, decisions, and constraints. Codex starts
without manual copy/paste, receives the bounded package, avoids the failed
approach, completes the task, and writes a verified outcome with test evidence.

## Phase 0 definition of done

- frozen pnpm install succeeds from the committed lockfile
- desktop typecheck/build and docs build succeed
- formatting and Git diff checks succeed
- Rust formatting, light checks, and full checks are blocking in CI
- failing full CI jobs cannot produce a green workflow
- provenance and unresolved release-license risks are documented
- this architecture contract is committed and linked from contributor guidance
- all approved desktop tabs render and can be navigated with mouse and keyboard
- browser screenshots prove no critical clipping or horizontal overflow at the
  approved 1600x1000 desktop reference viewport

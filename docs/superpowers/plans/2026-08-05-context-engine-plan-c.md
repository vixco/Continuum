# Context Engine Plan C — Ontsluiting

> **For agentic workers:** REQUIRED SUB-SKILL: superpowers:subagent-driven-development (accelerated mode: implementer tests only; the whole-branch FINAL review + full gate sweep follows this plan and covers A+B+C together).

**Spec:** `docs/superpowers/specs/2026-08-05-context-engine.md` v2. **Depends on Plans A+B.** **Goal:** everything is visible and queryable — session state published, the 10 context tools + privacy retrofit of existing tools, the Context page with the correction loop, and the four benches proving the engine works.

## Global Constraints

Cargo rules as before; MCP gates: `cargo test -p continuum-mcp`, clippy, fmt. Frontend gates (PowerShell, apps/desktop): `corepack pnpm typecheck`, `lint` — **NO `pnpm build` while the user's dev app may be running** (shared .next corruption; typecheck+lint are the gate, build runs once in the final sweep). Non-negotiable #7: every MCP change additive; §5.1 content filtering changes no schema (state in docs). English UI strings; Tailwind tokens (accent-amber). All published-file additions to state.json/live-context.json follow the 4-touch contract (runtime_publish.rs:20-24) resp. schema-version bump.

### Task C1: Session-state publishing (spec §4.8 consumers, §4.9 matrix)
RuntimeSnapshot gains session_state (4-touch: publish, bridge apply, StateHandle setter, DEFAULT_STATE); LiveWorldState gains session_state (LIVE_CONTEXT_SCHEMA_VERSION bump, additive JSON). Desktop bridge + TS types updated. Tests: snapshot serde round-trip, bridge apply, legacy state.json without the field loads.

### Task C2: Read-only DB path + §5.1 privacy retrofit (spec §5.1, §5.2 preamble)
raw_log.rs: read-only constructor (no DDL, PRAGMA query_only=ON, busy_timeout 2000ms, WAL-recovery tolerant, missing-file → typed NotYetCreated). MCP server: `system_active_window`, `system_clipboard_get`, `system_live_context` (compact AND state fields) route through PrivacyFilter scrub + cloud gate; clipboard kill-switch `[context_tools] clipboard_tool_enabled`; docs reworded "schema-stable but content-filtered". Tests: RO open against live-WAL and missing DB; retrofit filtering (secret in title/clipboard/state comes out redacted); schemas byte-identical (protocol test unchanged for these three).

### Task C3: Context tool family, part 1 — published-state tools (spec §5.2)
`context_session`, `context_window`, `context_screen`, `context_audio`, `context_projects`: schema structs + handlers reading state.json/live-context.json/Projects table (RO), staleness flags, privacy cloud-gate on every response. Permission entries (observation tier) + docs/mcp-tools.md + EXPECTED_TOOLS +5. Tests: protocol presence, staleness, gating.

### Task C4: Context tool family, part 2 — events/git/package tools (spec §5.2)
`context_timeline`, `context_search`, `context_files` (context_events RO + FTS), `context_git` (live state; named CONFIRMED project → on-demand bounded probe), `context_package` (mcp-published assembler profile: sections/omissions/staleness per the §4.9 matrix; own lazy store opens, live-context compact as query). Permissions + docs + EXPECTED_TOOLS +5. Tests: filter/limit clamps, missing-DB stale responses, named-project probe rejection for unconfirmed, package profile section list.

### Task C5: Context page UI + intents (spec §4.13)
Dashboard tab "Context": session state + confidence bars, per-source health + privacy toggles, recent events strip, project candidates; empty state with Add-project CTA. Intent files (context-intents/) drained in the main loop: add/confirm project, correct, not-this-project, pin, forget (CASCADE per spec: events row+FTS, frame row+screenshot, derived episodic, unconfirmed candidate), delete-range, toggle changes. Audit JSONL appender (~/.continuum/logs/actions.jsonl) for wake/tool/toggle/correction/delete actions. Frontend gates typecheck+lint. Tests (Rust side): every intent kind round-trips; forget cascade against temp stores; audit lines well-formed.

### Task C6: Record mode + fixture + four benches (spec §9)
`--record` flag on the perception bin (post-privacy frames + collector events → JSONL, relative ts); synthetic 20-min fixture + labels sidecar committed under crates/continuum-core/benches/data/ (scripted narrative per spec; NO real recordings); replay harness (fake clock) feeding frame loop + classification. Bench bins: continuum-context-bench (recall thresholds per spec), safety-redaction (incl. every MCP tool response path; commit-id survival), dedupe-precision (the defined algorithm), memory-precision (duplicate ≤10%, precision ≥70%, later-used report). Triage bench: re-baseline assertions live (B-exit) — verify here against the fixture too. Tests: benches compile + run on the fixture in CI-feasible time (debug ok).

### Task C7: Docs + drift + changelog
docs/: context-engine user doc (what is observed, zones, toggles, tools table, Context page, config reference §6); self-healing.md entries for every new component; ARCHITECTURE.md context-engine section; documented-drift fixes (salience threshold docs, stale perception_screenshot reference, /no_think claim); CHANGELOG one consolidated entry for A+B+C. Every claim verified against shipped code.

**After C7: the combined final phase (not part of this plan file):** full gate sweep across all crates + frontend (incl. one `pnpm build` with the dev app stopped), whole-branch adversarial review of A+B+C, one fix wave, push, live smoke test.

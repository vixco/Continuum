# Context Engine Plan B — Begrip & pakket

> **For agentic workers:** REQUIRED SUB-SKILL: superpowers:subagent-driven-development (accelerated mode: implementer tests only, NO per-task reviews; whole-branch review after Plan C). Spec sections are normative — read them fully.

**Spec:** `docs/superpowers/specs/2026-08-05-context-engine.md` v2. **Depends on Plan A** (events channel, Projects table, resolver, privacy filter). **Goal:** the Context Model understands events (classification riding triage), live session state exists and survives restarts, and one packager serves wake + chat.

## Global Constraints

Same cargo/gate/config/health rules as Plan A. LLM-touching tasks must respect the token budget and lock discipline of spec §4.7 — these are Critical-severity design constraints, not suggestions. Prompt files live in `prompts/`; keep `/no_think`; lenient JSON parsing with clamps everywhere (grammar decoding stays disabled).

### Task B1: TriageOutput + classification (spec §4.7)
Wrapper `TriageOutput { #[serde(flatten)] decision: TriageDecision, classification: Option<Classification> }` (the flatten wrapper is REQUIRED — the tagged enum silently drops sibling keys, proven by test_parse_extra_keys_accepted); `Classification {event_type (enum, serde snake_case), project, importance, confidence, summary, should_store}` with clamps + slug validation (unknown project → resolver value, log-only). `TriageLayer::evaluate` returns TriageOutput; update continuum.rs match, handlers, continuum-triage-bench. Prompt: extend prompts/triage-system.md with the classification block INSIDE the single top-level object, compact keys; system-prompt byte-cap test adjusted; triage context_size default → 4096; truncated classification still yields decision (no retry burn). Tests: flatten parsing matrix (missing block, malformed block, truncated, out-of-range), every construction site compiles, bench still runs.

### Task B2: Triage off the main loop + LocalLlm priority discipline (spec §4.7)
Triage evaluation `tokio::spawn`ed per gated frame with coalesce/busy flag (mirror do_wake CAS pattern); result handled via channel back into the loop (decision consumption unchanged in order). LocalLlm: two-priority acquisition (interactive triage first; background callers try-acquire/backoff, max_tokens ≤ 256 per chunked call — adjust curator complete() call sites). Tests: coalescing under burst (pure logic), priority acquisition unit test with a mock lock.

### Task B3: Classification consumption (spec §4.7 consumption, §4.6)
Classified frames → ContextEvent (source screen/audio, zone-tagged per propagation rule) onto the Plan-A channel; should_store/Remember → vault candidate (status Candidate, source Observed, project, per-type expiry TTL from `[memory]` config, epistemic label user_stated/system_inferred, mapping table event_type→vault type per spec — project_switch/routine produce no candidate); triage_decision raw-log column populated; distiller SQL predicate stays as fallback. Tests: mapping table exhaustive, candidate fields, no-candidate types, column write.

### Task B4: Description disentanglement (spec §4.10)
`ScreenObservation.world_compact: Option<String>` additive (types, frame builder, raw-log column, live-context); description = one-sentence caption again; triage prompt uses caption + slim context (token-budget test: prompt_tokens < n_ctx − max_tokens asserted in bench); packager (B7) uses world_compact. Tests: budget assert, field routing.

### Task B5: SessionStateHub + inference + rehydration (spec §4.8)
`context/session_state.rs`: hub (Arc<RwLock>), mechanical updates in frame loop (project/app/title/open_files-best-effort/last_error/last_success from events, last_user_command from intents); inference in OWN spawned task (event-driven triggers per spec knobs, max_tokens 256, background priority from B2, lenient JSON, confidence_floor rendering, local_only tagging per propagation rule); boot rehydration from state.json snapshot + recent context_events (staleness-discounted confidence); SessionTracker public snapshot accessor + project-change boundary (post-hysteresis). Feed triage memory_summary (char-cap 600) + skills MatchContext.task/project (continuum.rs:1802-1816 hardwired Nones die). Tests: mechanical updates, trigger predicate, rehydration, boundary, memory_summary cap.

### Task B6: Compression ladder + episodic project (spec §4.11)
Distiller reads deduped context_events (count-aware summaries "…×14"); EpisodicEvent gains project (additive Lance migration) + retrieval optional project filter; screenshot mtime-backstop sweep + delete_screenshots_with_rotation. Tests: count-aware distillation, migration on legacy rows, sweep.

### Task B7: Packager + wake profile + post-wake record (spec §4.9)
`context/package.rs` UNGATED (struct + renderer + section types + cloud-gate application; compiles in --no-default-features — parity gate proves it); runtime-full assembler in do_wake (never-fail pattern): all sections, order contract preserved (pending last-before-reason, existing test updated not deleted), token_budget=1000 + per-section caps + drop order; recent_frames ring → Arc<std::sync::Mutex<VecDeque>> shared with maintenance ticker (no more &[] history); window title finally rendered; tools/permissions summarized from compose config. Post-wake structured record: best-effort trailer parse {action, result, next_step} → wake_result system event + wake vault-event fields. Curator session summary gains structured `open_task:` trailer line. Tests: render per section, budget/drop order, order contract, trailer parse (present/absent/garbage), ring sharing.

### Task B8: Chat profile + continuation resolver (spec §4.9 matrix, §4.12)
Chat (desktop): KEEP in-process vault search as memory section; ADD session-state section read from state.json (absent/stale → "runtime not running"; [chat] include_session_context knob; chat_token_budget preset) — desktop crate gates: `cargo test -p continuum-desktop` green. `context/continuation.rs` (pure): trigger phrase config, candidate ranking (session-state task incl. rehydrated, open_task trailer, last error event, wake_result.next_step, last_user_command), confidence_floor routing (recommend vs ask); wired into do_wake reason handling for continue-class triggers. Tests: ranking matrix incl. post-restart, trigger matching, chat section stale handling.

**Plan-B exit gates:** core both feature sets + desktop + clippy + fmt green; triage bench re-baselined (prompt-fit + p95 + voice-ticker-delay p99 assertions in place, thresholds per spec §9).

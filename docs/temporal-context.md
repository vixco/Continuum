# Temporal context contract

Continuum's event store remains authoritative. Temporal context is a rebuildable,
bounded synthesis over already-scrubbed observations; it does not create a second
history database and it never upgrades an inference into an observed fact.

## Core types

`continuum_core::context::temporal` exposes:

- `TemporalObservation`: source, stable source reference, time span, optional
  project/application/event kind, scrubbed summary, source confidence and
  inherited sensitivity.
- `TemporalScope`: optional `since`, `until` and project bounds plus an explicit
  `include_local_only` flag. Cloud-bound consumers leave this flag false.
- `TemporalSession`: one continuity-bounded activity span with retained evidence,
  applications, dominant project, conflict marker and a conclusion.
- `TemporalConclusion`: text, confidence, supporting source references and one of
  `observed`, `strongly_inferred`, `weakly_inferred`, or `unknown`.
- `TemporalSynthesizer`: deterministic grouping and conservative inference.

The default continuity gap is 12 minutes. A project change also breaks a session
when both sides have resolved project ids. Evidence in one synthesized session is
bounded to the latest 32 observations.

## Provenance and uncertainty

Every semantic conclusion contains the exact source references used to support it.
Direct collector records remain observations; phrases such as "appears to be" are
inferences. Independent source diversity raises confidence slightly. Conflicting
success/error signals or competing projects lower confidence. Low-confidence
conclusions are downgraded rather than stated as facts.

The initial deterministic hypotheses intentionally cover only combinations that
are useful and testable without another model pass:

- test execution + debugging/error signals + note/file activity -> testing the
  current project and recording defects;
- browser/PDF/documentation research + note/file activity -> researching a topic
  and consolidating notes.

Everything else falls back to a weak project-level description or `unknown`.
This is deliberately conservative; richer local-model synthesis can sit on top of
the same evidence/provenance contract later.

## Privacy boundary

The synthesizer accepts no screenshot bytes, raw file contents, clipboard data,
command lines, environment variables or unredacted text. Collectors must supply
scrubbed summaries and stable source references.

`local_only` observations are withheld by default and counted in
`omitted_private`. Time/project exclusions are counted separately in
`omitted_out_of_scope`. This allows a cloud consumer to distinguish "nothing
matched" from "matching private evidence was withheld" without receiving the
private content.

## Adapters

Existing `context_events` are the preferred historical input. A thin adapter can
map a `ContextEventRow` to `TemporalObservation` as follows:

- `context_event:<row id>` -> `source_reference`;
- `source`, `ts_first`, `ts_last`, `project_id`, `application`, `event_type`,
  `summary`, `confidence`, `sensitivity` map directly;
- `local_only` sensitivity remains `local_only`; it must not be widened.

Vision (A2), watcher/process (A5), agent-run, chat and memory evidence should enter
through their existing event producers rather than bypassing the event store.

## MCP and chat integration rule

Historical/activity questions should use focused retrieval, not a full timeline
dump. Consumers should derive a `TemporalScope`, fetch a bounded event set, run
`TemporalSynthesizer`, and include only the most relevant one or two sessions.
Useful deterministic trigger families include:

- current activity: "what am I doing", "what am I working on";
- historical activity: "what was I doing", "earlier", "before";
- change questions: "what changed", "since I last asked";
- failure questions: "why did this break", "what failed".

The trigger selects retrieval; it does not answer the question itself. Answers
must cite the returned source references internally and preserve the conclusion's
strength/confidence.

## Long-term memory promotion

A temporal session is a candidate for A4 memory promotion only when it is salient
and coherent. The default recommendation is to require a non-`unknown` conclusion,
multiple supporting observations and either strong inference or explicit user
confirmation. The durable memory should store the compact conclusion and evidence
references, never the full transient event window or sensitive screen content.

## Synthetic acceptance scenarios

Unit tests cover:

1. Continuum editor + test process + failed test + bug-notes change -> one strong
   "testing and recording defects" inference with multiple source references.
2. Browser research + PDF + notes -> one coherent research session.
3. Long idle gaps and project switches -> separate sessions.
4. Conflicting success/error evidence -> lower confidence.
5. `local_only` evidence -> omitted and counted for cloud scope.
6. Time/project scope -> rows outside the requested bounds never appear.

# Persistent-intelligence evaluation

Status: A8 synthetic evaluation contract, 2026-08-10 (Europe/Amsterdam)

## Purpose

This layer answers a narrower, testable question than “is Continuum intelligent?”:

> Do normalized cross-layer outputs satisfy the concrete invariants Continuum needs for persistent intelligence without regressing privacy, provenance, cache correctness, failure handling, or UI truthfulness?

The committed suite is fully synthetic. It contains no user activity, screenshots, private paths, credentials, raw logs, command lines, or model prompts.

## Trust boundary

Schema version 1 is an executable **contract-fixture evaluator only**. It cannot prove runtime behavior.

- The only accepted `evidence_mode` is `contract_fixture`.
- Relabeling a fixture as `runtime` is a schema error.
- A passing suite reports `contract_status: pass`, `runtime_status: unsupported`, and `runtime_evidence_supported: false`.
- `--require-runtime` intentionally exits non-zero.

This hard block remains until production adapters have a structurally separate, versioned artifact path with explicit producer identity, source revision, artifact hashes, generator command, and scenario-specific provenance. Fixture prose or labels can never unlock a runtime pass.

## Run it

```bash
python3 -m unittest -v scripts/test_persistent_intelligence_eval.py
python3 scripts/persistent_intelligence_eval.py \
  --suite evals/persistent-intelligence/reference-suite.json \
  --report persistent-intelligence-report.json
```

The reserved release gate is expected to fail today:

```bash
python3 scripts/persistent_intelligence_eval.py \
  --suite evals/persistent-intelligence/reference-suite.json \
  --require-runtime
```

Exit codes are `0` for a passing synthetic contract, `1` for failed invariants, regressions, or the unsupported runtime gate, and `2` for malformed evidence. Malformed exporter shapes are converted into deterministic evaluation errors rather than uncaught `TypeError` or `KeyError` tracebacks.

## Score meanings

There is no opaque aggregate “intelligence score.” Each required dimension is an invariant or a unit-bearing metric:

| Dimension | Meaning |
| --- | --- |
| Perception latency | Every eligible frame is accounted for as cache reuse or semantic inference. Schema v1 does not claim measured runtime milliseconds; future runtime adapters may add those measurements against the architecture cadence. |
| Semantic relevance | Claims match the labeled activity/project rather than merely naming the foreground application. |
| Temporal coherence | Related observations across time and applications resolve to one session when the evidence warrants it. |
| Historical retrieval | Earlier labeled activity remains available with provenance. |
| Confidence calibration | Confidence changes with evidence strength; observed text remains untrusted input. |
| Evidence provenance | Every claim and durable update cites known immutable observations. Mixed unknown references fail the check; malformed reference shapes fail safely with exit code 2. |
| Memory precision | Durable records require salience, lifecycle state, and provenance. `cloud_allowed` may use policy-controlled egress; `local_only` may remain in local lifecycle storage but never cloud egress or reusable/cross-scope cache; `never_observe` creates no record. Contradictions create source-backed supersession rather than silent overwrite. |
| Privacy | Zero synthetic-secret occurrences, no prompt-injection privilege elevation, no automatic sensitive-memory promotion, and canonical sensitivity handling. |
| Cache correctness | At least 90% of duplicate-frame semantic work is collapsed, every meaningful change is retained, contradictions invalidate stale conclusions, and privacy-sensitive cache disposition is explicit. |
| Failure explanation | Health names a probable cause, affected capability, unaffected capability, and degraded state. |
| Repair verification | Policy is honored and success is reported only after a newer healthy probe. |
| UI truthfulness | Runtime/fixture origin is explicit, degraded state is not shown as healthy, and chat scroll follows user intent. |
| Regression safety | A baseline `pass` may not become `fail`; meaningful-change loss and missing scroll cases are hard failures. |

The duplicate-collapse floor is 90% because it has an operational meaning: no more than one semantic inference per ten duplicate frames, with zero loss of labeled meaningful changes.

## Canonical sensitivity invariants

The evaluator follows Continuum's current privacy behavior rather than treating every sensitive record as forbidden:

- `never_observe`: no observation record, event, content hash, cache entry, or durable memory may be created.
- `local_only`: redacted evidence may produce a local event or salience-gated local memory. It may use bounded process-local ephemeral reuse, but it must not enter cloud-safe output, reusable/exportable cache, or cross-scope cache.
- `cloud_allowed`: normal context egress remains subject to provenance, lifecycle, and policy.

Cache fields intentionally distinguish process-local ephemeral reuse from reusable, exportable, and cross-scope entries. A single ambiguous `cache_entry_created` boolean is not sufficient evidence.

## Required scenarios

The reference suite includes all seven required scenarios and records each future adapter as `status: not_implemented`:

1. **Testing Continuum** — editor, tests, desktop, and synthetic bug notes become one grounded testing/debugging session with current activity, earlier activity, project, confidence, and provenance.
2. **School research** — browser, PDF reader, and notes remain one coherent research session across application switches.
3. **Repeated unchanged frames** — duplicate work is bounded and every labeled meaningful change is retained.
4. **Vision failure** — capture degrades safely, health explains the affected capability, policy is honored, and repair success requires a newer healthy probe.
5. **Privacy-sensitive observation** — secret-like text is redacted and untrusted; `local_only` remains local and non-reusable while `never_observe` creates no artifacts.
6. **Contradictory evidence** — confidence changes, stale cache is invalidated, and durable knowledge is superseded with non-empty provenance.
7. **Chat streaming and scroll** — bottom-follow works only when appropriate, user scroll position is preserved, and streaming never jumps to the top.

Scenario 7 remains contract evidence. A6's deterministic chat-scroll state-machine test is the intended producer seam, but a DOM/browser artifact adapter is still required before runtime scroll behavior can be claimed.

## Adversarial coverage

The dedicated test module currently contains 18 tests. It covers fixture-to-runtime relabel spoofing, canonical sensitivity policy, local-only egress/cache boundaries, never-observe artifact suppression, malformed exporter shapes without tracebacks, mixed unknown provenance, empty supersession provenance, raw sentinel leakage, stale-cache reuse, meaningful-change loss, unverified repair success, incomplete scroll behavior, deterministic reporting, and pass-to-fail regression detection.

## Future runtime-adapter requirements

A future runtime artifact format must not reuse the fixture's `evidence_mode` switch. It needs a separate schema and validator that binds evidence to actual test output. At minimum it must carry:

- adapter and producer version;
- source commit/revision;
- deterministic artifact SHA-256 values;
- the exact synthetic test/generator command;
- bounded scenario evidence with immutable IDs and monotonic timestamps;
- canonical sensitivity and privacy disposition;
- policy decision, action time, verification time, and probe result for repairs;
- cache hit/invalidation decisions without cached private payloads;
- source attribution for health and UI state.

Runtime artifacts must remain synthetic and bounded. They must never serialize raw screenshots, unrestricted screen text, secrets, environment variables, command lines, private paths, or unrestricted logs.

## Relationship to existing benches

This layer does not replace the existing Rust replay, context recall, dedupe, memory-precision, or MCP-redaction benches. Those exercise production functions and storage paths. This evaluator adds an adversarial normalized-evidence contract, explicit trust-boundary reporting, deterministic baseline comparison, and the missing cross-subsystem scenarios. Runtime adapters must later connect those production-path benches and selected A2–A7 implementations to a separate artifact validator before a system-level runtime pass is possible.

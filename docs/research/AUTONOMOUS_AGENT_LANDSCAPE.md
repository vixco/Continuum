# Autonomous-agent landscape and Continuum differentiation

Research date: 2026-08-10  
Purpose: architecture comparison and benchmark planning, not marketing proof

This note records high-level patterns studied while reviewing Continuum's
persistent-intelligence and Agent OS foundations. No source code, prompts,
assets or tests from the projects below are incorporated by this document.
Every upstream repository and service keeps its own license and terms.

## Honest position

No responsible review can prove that Continuum is “100% better than every AGI
tool.” The compared projects optimize for different jobs and many change
rapidly.

Continuum's defensible target is narrower and measurable:

> combine local ambient context, temporal evidence, connected memory,
> provider-neutral action, independent permission enforcement, durable
> execution, verified recovery and a truthful desktop control surface in one
> coherent system.

A capability is considered stronger only after the same published scenario,
hardware class and evidence rules are applied to both systems.

## Projects reviewed

| Project | Strong pattern studied | Continuum decision |
| --- | --- | --- |
| [LangGraph](https://github.com/langchain-ai/langgraph) | Durable graph execution, checkpoints, human-in-the-loop state inspection and short/long-term memory | Keep Continuum's Rust run store and leases; formalize equivalent recovery invariants without adding a second orchestration framework |
| [Letta](https://github.com/letta-ai/letta) | Stateful agents whose memory and identity persist across sessions; remote environments and scheduled work | Treat context as durable system state, but keep memory provenance, privacy and desktop evidence under Continuum control |
| [OpenHands](https://github.com/All-Hands-AI/OpenHands) | Composable agent SDK, coding-agent runtime, evaluation infrastructure and horizontal agent scaling | Reuse the pattern of isolated workers and evidence-backed evaluation; do not specialize Continuum solely for coding |
| [OpenAI Agents SDK](https://github.com/openai/openai-agents-python) | Agents, handoffs, guardrails, sessions and tracing with a small set of primitives | Make handoff and guardrail semantics explicit in the autonomy contract while remaining provider-neutral |
| [Microsoft Agent Framework](https://github.com/microsoft/agent-framework) | Event-driven multi-agent runtime and migration path from AutoGen/Semantic Kernel | Prefer typed events and deterministic runtime ownership over model-only routing |
| [Microsoft Conductor](https://github.com/microsoft/conductor) | Versioned deterministic workflows, parallel branches and replay-safe orchestration | Separate deterministic policy/checkpoint/budget logic from model planning |
| [screenpipe](https://github.com/screenpipe/screenpipe) | Local event-driven screen/audio capture, searchable history, MCP access and explicit data permissions | Use event/change-triggered perception and bounded local history rather than naive high-frequency model inference |
| [browser-use](https://github.com/browser-use/browser-use) | Browser-specialized observation/action loops, persistent browser state and domain controls | Keep browser automation behind the same Agent OS policy/evidence boundary; accept that specialized browser agents may outperform Continuum on browser-only tasks |
| [Mem0](https://github.com/mem0ai/mem0) | Memory extraction, vector retrieval and optional graph relationships | Build hybrid evidence-backed memory incrementally; do not ship a graph visualization without authoritative persistence and lifecycle semantics |

## Capability matrix

Legend:

- **Implemented** — production code exists and is covered by repository tests.
- **Contract-tested** — deterministic synthetic or unit evidence exists, but
  native runtime proof is incomplete.
- **Partial** — a useful foundation exists; important wiring or validation is
  still missing.
- **Not integrated** — reviewed proposal was rejected from this PR.

| Capability | Continuum status | Strong external reference | Review conclusion |
| --- | --- | --- | --- |
| Durable resumable execution | Implemented / contract-tested | LangGraph, Conductor | Stable run IDs, atomic checkpoints and leases are strong; add multi-hour restart evidence |
| Unknown mutation handling | Implemented / contract-tested | Durable workflow systems generally | Continuum's explicit no-replay rule is a meaningful differentiator |
| Independent permission boundary | Implemented | OpenAI guardrails, screenpipe endpoint gating | Continuum's native allow/ask/deny gate is strong; native smoke tests remain required |
| Computer and connected-app action plane | Implemented / partial runtime proof | browser-use, OpenHands | Broad provider-neutral surface exists; browser-specific depth remains lower |
| Temporal desktop context | Implemented / contract-tested | screenpipe | Bounded evidence-backed sessions are integrated; live capture quality remains unproven |
| Ambient perception efficiency | Partial | screenpipe | The swarm vision proposal was rejected because authoritative health, privacy generations and cache invalidation were incomplete |
| Long-term connected memory | Partial | Letta, Mem0 | Existing memory and temporal evidence are useful; the proposed graph write path was rejected until migration/provenance rules are real |
| Multi-agent specialization and handoff | Partial | OpenHands, OpenAI Agents SDK, Microsoft Agent Framework | Workers exist; the normative handoff envelope is now documented and evaluated, but adaptive runtime handoff needs more work |
| Scheduling and event-triggered autonomy | Partial | Letta, OpenHands Agent Canvas | Automation surfaces exist, but long-running trigger reliability lacks end-to-end proof |
| Self-diagnosis and verified repair | Implemented / contract-tested | Few projects combine this with desktop context | Watcher health, root causes and before/after repair verification are integrated |
| Truthful user control surface | Implemented / contract-tested | Product-specific | UI distinguishes live, idle, disabled, degraded and unavailable states instead of presenting fixtures as proof |
| Compute/token reuse | Partial | Framework-specific caches | Synthetic freshness rules exist; no selectable cross-layer production cache implementation emerged from this swarm |
| Benchmarkable claims | Contract-tested | OpenHands evaluation, LangGraph/LangSmith workflows | Continuum now has explicit persistent-intelligence and bounded-autonomy suites, both intentionally separated from runtime proof |

## Patterns adopted as inspiration

### Durable execution

- checkpoint before side effects;
- stable run identity;
- deterministic state transitions;
- resume from durable state rather than replaying the whole prompt;
- isolate non-deterministic model choices from deterministic bookkeeping.

### Memory-first agents

- preserve useful state across sessions;
- make memory editable and inspectable;
- distinguish working context from durable memory;
- evaluate extraction precision and forgetting, not only retrieval recall.

### Ambient context

- capture on meaningful events and changes;
- align screen/accessibility/window evidence by timestamp;
- store searchable compact representations;
- expose local permissions and retention explicitly;
- avoid treating every frame as durable knowledge.

### Multi-agent systems

- use specialized workers with bounded authority;
- pass compact handoff state rather than entire transcripts;
- preserve one authoritative run owner;
- make routing and escalation observable.

### Evaluation

- synthetic contract fixtures are useful for regression;
- native/runtime artifacts must be labelled separately;
- compare against fixed scenarios and units;
- keep privacy and failure honesty as blocking dimensions, not optional scores.

## Explicitly rejected approaches

The final integration does not include:

- a second parallel perception lifecycle;
- a custom cache identity scheme without authoritative invalidation;
- a graph-memory API without a real migration and persistence owner;
- self-modifying GitHub workflow artifacts;
- raw screen or user-history fixtures;
- model-only permission enforcement;
- retrying uncertain mutations;
- universal “best AGI” claims without reproducible evidence.

## Benchmark gates for future superiority claims

A comparative claim requires a pinned revision of each system, public scenario
definitions and raw sanitized artifacts.

Minimum gates:

1. **Context recall:** answer “what was I doing?” from a 30–120 minute synthetic
   desktop trace with evidence precision and latency.
2. **Perception efficiency:** relevant-change recall, inference count, CPU/GPU,
   RAM/VRAM and retained bytes per hour.
3. **Memory precision:** durable-memory precision, contradiction handling,
   provenance completeness and deletion correctness.
4. **Autonomy safety:** policy bypass rate, duplicate mutation rate, unknown
   outcome replay rate, postcondition coverage and cancellation latency.
5. **Recovery:** successful resume after injected process death, corrupt
   checkpoint, unavailable model and permission denial.
6. **Task success:** fixed desktop, coding and connected-app tasks with the same
   models and authority.
7. **Privacy:** secret egress, cross-project leakage, raw-history retention and
   prompt-injection success rate.
8. **Human review cost:** time to understand why an action happened and whether
   the claimed outcome is supported.

Until those artifacts exist, the correct claim is:

> Continuum has an unusually integrated local context + governed action +
> verified recovery architecture, with important native validation and memory /
> perception work still open.

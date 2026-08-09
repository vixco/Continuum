# Agent OS landscape research

Research date: 2026-08-08

This note records the design research behind Continuum Agent OS. It separates
patterns learned from other projects from source code incorporated into this
repository.

## Decision summary

The strongest assistants are not defined by one clever prompt. They combine a
persistent control plane, extensible capabilities, an observe/act/recover loop,
long-term user context, and explicit trust boundaries. Continuum already owns a
strong local context, memory, privacy, MCP, and audit base, so the highest-value
move was not to replace it with another framework. The chosen design adds a
provider-neutral action broker on top of those boundaries.

No source files from the compared repositories were copied or adapted in this
change. The implementation is original Rust and PowerShell built against
Continuum's existing dependencies and the documented Composio HTTP API.

## Compared projects

| Project | Useful pattern | Continuum decision | Code incorporated |
| --- | --- | --- | --- |
| [OpenClaw](https://github.com/openclaw/openclaw) | A local Gateway as the control plane for models, sessions, tools, channels, companion nodes, skills, and plugins; single-operator security and pairing defaults | Keep one local action plane exposed to all orchestrators through MCP; make policy and evidence first-class instead of agent-specific | None |
| [Hermes Agent](https://github.com/NousResearch/hermes-agent) | Provider-neutral core, persistent memory, reusable skills, scheduled/gateway surfaces, terminal and browser tools, learning across sessions | Add one bundled Agent OS skill and resumable plans while keeping Continuum model-agnostic | None |
| [OpenHuman](https://github.com/tinyhumansai/openhuman) | UI-first personal assistant, local memory tree/vault, proactive integrations and Composio-backed OAuth | Use Composio as a broad integration plane but keep identity, policy, evidence, and local memory under Continuum | None; GPL-3.0 source was explicitly not copied |
| [Browser Use](https://github.com/browser-use/browser-use) | Repeated observation, bounded actions, custom tools, domain-aware safety, retries and recovery rather than one-shot scripts | Make observe → plan → act → verify → recover the server instruction and return post-action state on every mutation | None |
| [Composio](https://docs.composio.dev/docs/composio-connect) | Session-based discovery and execution across 1000+ apps through seven meta-tools and managed OAuth | Implement the official Tool Router REST surface with secret isolation and Continuum-side risk classification | API integration only |

## OpenClaw

OpenClaw's current README describes a single-operator personal assistant that
runs on the user's devices, connects channels and tools through one Gateway, and
extends through skills, plugins, and companion nodes. Its security guidance
explicitly treats inbound messages as untrusted and pairs unknown direct-message
senders by default. The repository is MIT licensed.

The useful architectural insight is the Gateway, not a specific TypeScript
module. Continuum therefore keeps the existing MCP registry as the universal
agent boundary and adds `continuum-agent-os` as a separate action server. This
avoids coupling host permissions to one chat UI or one model provider.

## Hermes Agent

Hermes presents the same agent core through terminal, desktop, messaging, and
IDE surfaces, supports multiple model providers, connects MCP servers, persists
memory, and creates reusable skills from experience. Its repository is MIT
licensed.

Continuum adopts two patterns at the product level:

1. Skills should encode repeatable operating procedures, not merely prompts.
2. A long-running assistant needs durable task state outside the model context.

The bundled `agent-os` skill teaches the control loop, while `RunStore` persists
immutable plans and per-step outcomes for resume.

## OpenHuman

OpenHuman emphasizes a low-friction desktop experience, local memory, proactive
sync, and a large set of OAuth integrations through Composio. Its repository is
GPL-3.0 licensed.

The product lesson is that a personal assistant becomes materially more useful
when it can act across the user's real services. The licensing consequence is
also clear: no OpenHuman implementation code was copied into Apache-2.0
Continuum. Continuum talks to Composio through the public API and implements its
own persistence, policy, and tools.

## Browser Use

Browser Use packages a browser agent around repeated state observation,
constrained actions, custom tools, and recovery. It also demonstrates that
vision and DOM/accessibility state are complementary rather than interchangeable.
The repository is MIT licensed.

Continuum's first native backend is broader Windows computer use rather than a
browser-only agent. It exposes UI Automation semantics first, screenshots when
needed, and coordinate fallback last. Every mutation is followed by another
bounded observation. A later CDP/Playwright backend can plug into the same
policy/evidence/run contracts.

## Composio

Composio Connect exposes seven meta-tools: search, schema retrieval,
multi-execution, connection management, connection waiting, remote workbench,
and remote bash. Its Tool Router API creates a session for a user, searches by
use case, returns schemas and connection state, and executes direct or meta
tools with the session context.

Continuum implements this at the REST level so the action broker can classify
and authorize the concrete operation before sending it upstream. The API key is
kept outside the MCP argument surface. Toolkits can be allowlisted per user.
Destructive verb families and arbitrary remote code tools receive the strongest
risk class.

## Why not fork one project wholesale

A wholesale fork would duplicate the exact areas Continuum already has:
context capture, privacy filters, memory stores, MCP infrastructure, agent
handoff, project graph work, health, and a native desktop shell. It would also
inherit a second configuration model and, in OpenHuman's case, incompatible GPL
obligations for an Apache-2.0 codebase.

The selected approach keeps Continuum's differentiator—the evidence-backed local
world model—and adds the missing action loop through narrow, auditable seams.

## Follow-on opportunities

The next highest-leverage additions are:

1. a dedicated desktop Agent OS panel for policy presets, approvals, run replay,
   Composio connections, and screenshot retention;
2. a CDP/Playwright browser backend with DOM snapshots, tab state, downloads,
   and domain policy;
3. scheduled goals and triggers that create resumable runs rather than free-form
   background prompts;
4. skill extraction from successful run traces with human review before a skill
   becomes active;
5. multi-agent delegation where a planner can spawn scoped workers but only the
   action broker holds host/SaaS authority;
6. evaluators for outcome quality, not only whether a tool returned success.

## License references

- OpenClaw: MIT, copyright OpenClaw Foundation.
- Hermes Agent: MIT, copyright Nous Research.
- Browser Use: MIT, copyright Gregor Zunic.
- OpenHuman: GPL-3.0.
- Continuum: Apache-2.0.

These labels are repository-level evidence, not legal advice. Any future source
reuse still requires file-level provenance and notice review.

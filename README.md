# Continuum

[![CI](https://github.com/vixco/continuum/actions/workflows/ci.yml/badge.svg)](https://github.com/vixco/continuum/actions/workflows/ci.yml)

**A local context and action operating system for persistent AI assistants.**

Continuum is evolving from the Apache-2.0 donor codebase into a local control
plane for Codex, Claude Code, local models, and future agents. The product
promise is:

> Every agent starts with the right context. Every important action passes a
> real permission boundary. Every claimed outcome has evidence.

## Current status

Continuum now has two complementary execution boundaries:

- the existing context MCP server publishes privacy-filtered memory, files,
  processes, project context, workers, and repair tools;
- the new **Agent OS alpha** adds policy-gated Windows computer use, Composio
  access across connected apps, resumable plans, native approvals, verification,
  and append-only evidence.

The desktop shell and its seven navigation tabs are real, while parts of the
new Continuum-specific dashboard remain fixture-backed during the typed world
model migration. A green frontend build still does not prove every runtime,
model, memory, or OS integration on a real Windows machine.

Read [CONTINUUM_ARCHITECTURE.md](./CONTINUUM_ARCHITECTURE.md) for the product
boundary and [docs/AGENT_OS.md](./docs/AGENT_OS.md) for the action plane.

## Agent OS alpha

`continuum-agent-os` is a standalone stdio MCP server registered through
Continuum's existing local MCP registry. It gives every supported orchestrator
one shared, model-neutral set of tools:

- semantic Windows UI Automation, screenshots, mouse, keyboard, window focus,
  navigation, waits, and before/after verification;
- Composio Tool Router sessions, discovery, managed OAuth, direct app tools,
  all seven official meta-tools, and per-action risk classification;
- allow/ask/deny capabilities with independent native Windows approval dialogs;
- immutable, resumable multi-step plans that skip completed work;
- append-only evidence with recursive input/result secret and typed-text redaction.

Install it on the Windows development machine:

```powershell
.\scripts\install-agent-os.ps1
```

Install computer use without Composio:

```powershell
.\scripts\install-agent-os.ps1 -SkipComposio
```

The default policy allows observation, asks before screenshots and mutations,
and denies destructive Composio actions. See
[`config/agent-os-policy.example.json`](./config/agent-os-policy.example.json).

## Approved desktop structure

- Home — current focus, project health, agents, decisions, and next actions
- Projects — project graph, context, blockers, commits, and outcomes
- Memory — verified, inferred, disputed, superseded, and expired knowledge
- Agents — handoff, launch, active runs, verification, and recovery
- Permissions — pre-execution policies and approval queue
- Timeline — append-only project and agent evidence
- Settings — integrations, models, adapters, scopes, safety, and diagnostics

The visual contract is a graphite/black desktop UI with amber/gold actions and
semantic green/orange/red status colors.

## Run the desktop UI

Prerequisites: Node.js 20+ and pnpm 10.11.1.

```powershell
pnpm install --frozen-lockfile
pnpm dev
```

Open `http://localhost:3000` for browser development. Tauri packaging and native
Agent OS development require the Rust and Windows toolchain described in
[`AGENTS.md`](./AGENTS.md).

## Validation

```powershell
pnpm install --frozen-lockfile
pnpm typecheck
pnpm build
cargo fmt --all -- --check
cargo test -p continuum-core --no-default-features --lib
cargo test -p continuum-mcp --lib
cargo build --release -p continuum-mcp --bin continuum-agent-os
```

The full Rust workspace adds LLVM/libclang, CMake, Ninja, ONNX Runtime, protoc,
and MSVC requirements. CI must run those checks without masking failures. Native
computer-use behavior additionally requires real Windows smoke tests.

## Repository structure

```text
apps/desktop/              Tauri + Next.js Continuum desktop app
apps/docs/                 Existing donor documentation
crates/continuum-core/     Runtime, memory, senses, workers, and health
crates/continuum-mcp/      Context MCP plus Agent OS action MCP
crates/continuum-llm/      Local LLM wrapper
crates/continuum-vision/   Local vision wrapper
config/                    Runtime and permission defaults
skills/                    Bundled reusable agent procedures
docs/                      Engineering, research, and provenance documentation
```

## Safety boundary

Audit logging after a tool executes is not permission enforcement. Continuum
only claims enforced permissions for actions that pass through the Agent OS
allow/ask/deny gate. Native approvals default to No, destructive SaaS actions
default to deny, and plan approval never overrides an explicit deny.

No desktop automation layer is intrinsically safe after a user grants broad
host authority. Inbound messages, web pages, documents, and tool output remain
untrusted input and must be treated as possible prompt injection.

## License and provenance

Continuum is Apache License 2.0. Retain the donor license and attribution.
Model, voice, binary, service, and transitive dependency licensing requires
separate release review; see [docs/PROVENANCE.md](./docs/PROVENANCE.md) and
[docs/AGENT_OS_PROVENANCE.md](./docs/AGENT_OS_PROVENANCE.md).

## License

Apache License 2.0. See [LICENSE](./LICENSE).

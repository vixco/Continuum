# Continuum

**Local context, handoff, and permission infrastructure for coding agents.**

Continuum is evolving from the Apache-2.0 Kairo codebase into a local control
plane for Codex, Claude Code, and local agents. The first product promise is:

> Every coding agent starts with the right project context. Every important
> outcome has evidence. Every protected action stays under your control.

## Current status

Phase 0 establishes a trustworthy donor baseline and the approved Continuum
desktop experience.

- The desktop shell and its seven navigation tabs are real.
- Dashboard content is fixture data while the typed world model is built.
- Existing Kairo Rust crates remain as donor infrastructure and compatibility
  boundaries; they have not yet been renamed or connected to every new panel.
- A green frontend build does not prove live agent, model, memory, or permission
  integrations.

Read [CONTINUUM_ARCHITECTURE.md](./CONTINUUM_ARCHITECTURE.md) for the product
boundary, data flow, permission invariant, and migration sequence.

## Approved desktop structure

- Home — current focus, project health, agents, decisions, and next actions
- Projects — project graph, context, blockers, commits, and outcomes
- Memory — verified, inferred, disputed, superseded, and expired knowledge
- Agents — Handoff/Relay plus Context Compiler/Launch Pad
- Permissions — pre-execution policies and approval queue
- Timeline — append-only project and agent audit trail
- Settings — integrations, models, adapters, scopes, safety, and diagnostics

The visual contract is a graphite/black desktop UI with amber/gold actions and
semantic green/orange/red status colors.

## Run the desktop UI

Prerequisites: Node.js 20+ and pnpm 10.11.1.

```powershell
pnpm install --frozen-lockfile
pnpm dev
```

Open `http://localhost:3000` for browser development. Tauri packaging also
requires the Rust and native Windows toolchain described in
[`AGENTS.md`](./AGENTS.md).

## Validation

```powershell
pnpm install --frozen-lockfile
pnpm typecheck
pnpm build
cargo fmt --all -- --check
cargo test -p kairo-core --no-default-features --lib
```

The full Rust workspace adds LLVM/libclang, CMake, Ninja, ONNX Runtime, protoc,
and MSVC requirements. CI must run those checks without masking failures.

## Repository structure

```text
apps/desktop/          Tauri + Next.js Continuum desktop app
apps/docs/             Existing Kairo donor documentation
crates/kairo-core/     Donor runtime, memory, senses, workers, and health
crates/kairo-mcp/      Donor MCP server and hardened tool primitives
crates/kairo-llm/      Local LLM wrapper
crates/kairo-vision/   Local vision wrapper
config/                Runtime and permission defaults
docs/                  Engineering and provenance documentation
```

New Continuum domain/store/context crates are introduced after Phase 0. We do
not hide that migration behind a mass rename.

## Safety boundary

Audit logging after a tool executes is not permission enforcement. Continuum
will only claim enforced permissions for actions that pass through a real
allow/ask/deny gate and cannot bypass it through agent-native tools.

## License and provenance

The donor repository is Apache License 2.0. Retain its license and attribution.
Model, voice, binary, and transitive dependency licensing requires separate
release review; see [docs/PROVENANCE.md](./docs/PROVENANCE.md).

## License

Apache License 2.0. See [LICENSE](./LICENSE).

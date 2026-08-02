# Contributing to Continuum

Continuum is open source and welcomes real contributions. This guide explains how to set up a development environment, pick something to work on, and get a PR merged.

If you are reading this to use Continuum, not contribute to it, you probably want [README.md](./README.md) and the [docs](https://vixco.github.io/Continuum) instead.

## Code of conduct

Be respectful, be constructive, be specific. Continuum is an ambitious project built in the open.

- Assume good faith. Most confusion is a documentation gap, not malice.
- Prefer questions over assumptions.
- Disagree with ideas, not with people. A vigorous technical debate is welcome; a personal one is not.
- Harassment, discrimination, or abuse will get you banned from the project. The maintainer decides; the decision is final.

Report issues privately to `toshan@toshan.nl` if you don't want them handled in public.

## Dev environment

### Prerequisites

- **Windows 10 (1903+) or Windows 11.** macOS and Linux are not supported yet — see the [Windows-specific APIs](./ARCHITECTURE.md) Continuum leans on.
- **Rust stable** (toolchain pinned in `rust-toolchain.toml`). Install via [rustup](https://rustup.rs).
- **Node.js 20+** and **pnpm 9+** (`npm install -g pnpm`).
- **Claude Code CLI** (`npm install -g @anthropic-ai/claude-code`) and an active Claude Max or API subscription.
- **Native build deps for Rust crates**: LLVM/libclang, CMake, Ninja, protoc, MSVC Build Tools.

Run `.\scripts\dev-setup.ps1` to check that all the above are present before you start.

### Build workflow

See [CLAUDE.md](./CLAUDE.md) for the canonical reference. The short version:

```bash
# Runtime binary — release mode is the default (see CLAUDE.md for why)
cargo run --release --bin continuum

# Library tests and examples — debug is fine
cargo test -p continuum-core
cargo run --example voice_test -p continuum-core

# Dashboard — debug is fine
cd apps/desktop
pnpm install
pnpm tauri dev
```

### `sccache` is your friend

`.cargo/config.toml` enables [sccache](https://github.com/mozilla/sccache) for C++/CUDA compiles. First build is slow (~10 minutes release); after that, clean rebuilds across branches are near-instant for the native parts.

Install once: drop `sccache.exe` into `~/.cargo/bin/`.

## How to pick an issue

1. Browse the [issue tracker](https://github.com/vixco/Continuum/issues). Labels that are safe to grab:
   - `good first issue` — small, well-scoped, documented
   - `help wanted` — larger but still contained
   - `skill request` — bundled-skill additions (write a new `SKILL.md` workflow)
   - `docs` — improve docs or examples
2. Comment on the issue saying you're working on it so we don't duplicate effort.
3. If nothing on the tracker appeals, open a new issue proposing what you want to do **before** you start coding. PRs without prior discussion that touch architecture are likely to be rejected.

## PR workflow

1. **Fork** the repo and clone your fork locally.
2. **Branch** off main: `git checkout -b feat/short-description` (`feat/`, `fix/`, `docs/`, `refactor/`, `chore/`, `test/`).
3. **Make your changes.** Follow the rules below.
4. **Verify locally** before pushing:
   ```bash
   cargo fmt --all
   cargo clippy --all-targets --all-features -- -D warnings
   cargo test --workspace
   cd apps/desktop && pnpm build && pnpm lint
   ```
5. **Commit** using [Conventional Commits](https://www.conventionalcommits.org): `feat(scope): message`, `fix(scope): message`, etc. Scope is the crate or directory: `core`, `mcp`, `desktop`, `llm`, `vision`, `docs`, etc.
6. **Update `CHANGELOG.md`** under `## [Unreleased]` with a short description of your change.
7. **Update `ARCHITECTURE.md`** in the same commit if your change affects system design.
8. **Push** and open a PR against `main`. Use the PR template.
9. **Respond to review.** CI must pass, clippy must stay clean, and new behaviour needs tests.
10. Squash-merge is the default. Keep commits small and descriptive — they become the changelog.

## What we welcome

Any PR that makes Continuum **more ambient, more local, or more self-reliant**. Specifically:

- **Bug fixes** with regression tests.
- **Performance improvements** with benchmarks showing the win.
- **New MCP tools** with integration tests and docs in [docs/mcp-tools.md](./docs/mcp-tools.md). See [docs/developers/writing-mcp-tools](./docs) for the step-by-step.
- **New bundled skills** (`skills/<name>/SKILL.md`) with clear trigger descriptions. See [docs/developers/writing-skills](./docs).
- **Dashboard polish** — better tabs, better live updates, better keyboard nav.
- **Docs** — if something confused you, a PR fixing it is gold.
- **Accessibility** improvements, especially in the dashboard.

## What needs a very good reason

These are not automatic rejections, but they need a clear justification in the PR description and maintainer approval **before** you start implementing:

- Adding a dependency on a hosted service or cloud API.
- Adding a proprietary runtime dependency.
- **Breaking changes** to the MCP tool API (tool name, schema, or semantics). Versioned additions are fine.
- Modifications to the four-layer architecture.
- Adding telemetry or any new outbound network call.
- Changes to the build workflow for `continuum.exe` (debug/release layering — see `CLAUDE.md`).

## Coding standards

Full details in [CLAUDE.md](./CLAUDE.md). Summary:

### Rust

- Edition 2021, stable toolchain (pinned in `rust-toolchain.toml`).
- `cargo fmt --all` must pass.
- `cargo clippy --all-targets --all-features -- -D warnings` must pass.
- `anyhow` for application errors, `thiserror` for library errors. No `.unwrap()` in production code paths.
- Single tokio runtime. Never spawn a second one.
- `tracing` for logs, with structured fields. Every event carries a layer + component name.
- Every public item has a doc comment. Every module has a module-level doc comment.
- Every public function has at least one unit test.

### TypeScript / Next.js

- `strict: true`. No `any` without a `// eslint-disable-next-line` + reason.
- Prettier with 2-space indentation.
- ESLint with Next.js + TypeScript recommended rules.
- Functional components only. Hooks over classes.
- Tailwind exclusively. No CSS modules, no styled-components.
- Zustand for global state, React state for local.

### Git hygiene

- `main` is always releasable.
- Feature branches are `feat/<short-description>`, fixes are `fix/<short-description>`.
- Conventional commits. Squash-merge on PR.
- No force-pushing to `main`, ever. Feature branches can be rebased.

## Writing skills

A skill is a `SKILL.md` file in `skills/<name>/` with frontmatter describing when it triggers. The orchestrator picks them up automatically at wake time.

See the [skills docs](./docs/skills.md) and the five bundled skills in [`skills/`](./skills/) for examples. A good skill has:

- A short, specific `description` (the triage layer reads this to decide when to suggest the skill).
- A narrow scope — one workflow, not ten.
- A clear output format.
- A refusal policy for out-of-scope requests.

## Writing MCP tools

MCP tools live in `crates/continuum-mcp/src/tools/`. See [docs/mcp-tools.md](./docs/mcp-tools.md) for the existing surface and [CLAUDE.md](./CLAUDE.md#how-to-write-and-test-mcp-tools) for the template.

A good MCP tool:

- Does one thing and has a narrow schema.
- Writes an audit event to episodic memory.
- Has a unit test for the happy path and every denial path.
- Has a permission entry in `config/default-permissions.toml`.
- Is documented in `docs/mcp-tools.md` with a JSON example.

## Releasing

Only the maintainer cuts releases, but the process is documented in [docs/release.md](./docs/release.md) so that contributors know what's going to happen to their merged PR.

## License

By contributing to Continuum, you agree that your contributions will be licensed under the Apache License 2.0.

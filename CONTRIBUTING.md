# Contributing to Kairo

## Current status

Kairo is in **pre-alpha**. The architecture is designed, the scaffolding is in place, and active development is underway. We are not yet accepting code contributions from external contributors.

**What you can do right now:**
- Star the repo and watch for updates
- Read [ARCHITECTURE.md](./ARCHITECTURE.md) and open issues with concerns, ideas, or things you think the design misses
- Try to break the architecture on paper and tell us where it falls apart

## How contributions will work (post-alpha)

Once the initial alpha lands and stabilizes, contributions will follow this workflow:

### Getting started

1. Fork the repo
2. Clone your fork locally
3. Run `scripts/dev-setup.ps1` to verify prerequisites
4. Create a feature branch: `git checkout -b feat/your-feature`
5. Make your changes, following the rules in [CLAUDE.md](./CLAUDE.md)
6. Verify: `cargo fmt --all`, `cargo clippy --all-targets -- -D warnings`, `cargo test --workspace`
7. Push and open a PR against `main`

### Rules

- **Follow CLAUDE.md.** That file is the coding standards and non-negotiables for everyone, human or AI.
- **Conventional commits.** `feat(scope): message`, `fix(scope): message`, etc.
- **Update CHANGELOG.md** under `## [Unreleased]` in every PR.
- **Update ARCHITECTURE.md** if your change affects the system design.
- **Every PR that adds a component** must include self-healing hooks (health check, recovery procedure, structured logging).
- **No force-pushing to main.** Feature branches can be rebased freely.

### What we welcome

Any PR that makes Kairo more ambient, more local, or more self-reliant is welcome. Specifically:
- Bug fixes with regression tests
- Performance improvements with benchmarks
- New MCP tools with integration tests
- New skills with clear trigger descriptions
- Documentation improvements
- Accessibility improvements in the dashboard

### What needs a very good reason

Any PR that:
- Adds a dependency on a hosted service or cloud API
- Adds a proprietary runtime dependency
- Changes the MCP tool API (breaking change)
- Modifies the four-layer architecture
- Adds telemetry or network calls

These are not automatically rejected, but they need a clear justification in the PR description and maintainer approval before implementation begins.

## Code of conduct

Be respectful, be constructive, be specific. Kairo is an ambitious project built in the open. We value clarity over cleverness and questions over assumptions.

## License

By contributing to Kairo, you agree that your contributions will be licensed under the Apache License 2.0.

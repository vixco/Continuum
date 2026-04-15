<!--
Thanks for opening a PR. The checklist below is short on purpose — it's the things that actually
block a merge. If you haven't read CONTRIBUTING.md yet, please do so first.
-->

## What this PR does

<!-- One or two paragraphs. Focus on the *why* more than the *what*. -->

## Why this change is needed

<!-- The problem, bug, or use case. Link to issues with `Fixes #123` / `Refs #456`. -->

## How to test it

<!-- The exact commands a reviewer should run. If manual steps are needed, list them. -->

```bash

```

## Screenshots / recordings

<!-- UI changes: before/after screenshots or a short recording. -->

## Checklist

- [ ] Conventional commit title (e.g. `feat(voice): add dutch voice fallback`)
- [ ] `cargo fmt --all` passes
- [ ] `cargo clippy --all-targets --all-features -- -D warnings` passes
- [ ] `cargo test --workspace` passes
- [ ] Frontend changes: `pnpm --filter desktop build` passes, `pnpm --filter desktop lint` passes
- [ ] Added or updated tests for the changed behaviour
- [ ] Updated `CHANGELOG.md` under `## [Unreleased]`
- [ ] Updated `ARCHITECTURE.md` if the change affects system design
- [ ] Updated user-facing docs (in `docs/`) if the change affects user-visible behaviour
- [ ] New component / crate: includes self-healing hooks (health check, recovery procedure, structured logs)

## Notes for the reviewer

<!-- Anything you want the reviewer to pay special attention to. Known limitations, open questions, follow-up work. -->

# Releasing Kairo

This document is the canonical runbook for cutting a Kairo release. It's written for the maintainer, but contributors are welcome to read it so they know what's going to happen to their merged PR.

## Cadence

- **Alpha and beta releases**: cut whenever a phase milestone lands and stabilises. No fixed cadence.
- **Stable releases (post-1.0)**: target roughly every 6–8 weeks, plus out-of-band patch releases for security issues.

## Versioning

Kairo uses [SemVer](https://semver.org/) with explicit pre-release tags:

- `0.1.0-alpha.N` — first public releases. Expect breaking changes between alphas.
- `0.1.0-beta.N` — feature-stable, bug-fixing phase. Config compatibility is preserved across betas.
- `0.1.0` — first stable. `0.x.y` is still pre-1.0 in spirit; breaking changes are allowed between minors with a deprecation window.
- `1.0.0` — stable API. MCP tool schemas frozen. Config backward compatible within majors.

## Pre-release checklist

Run through this on a **clean Windows 10 or 11 VM** (or at least a fresh `~/.kairo/`) before tagging.

### 1. Code readiness

- [ ] `cargo fmt --all -- --check` is clean.
- [ ] `cargo clippy --all-targets --all-features -- -D warnings` is clean.
- [ ] `cargo test --workspace` passes (note any flaky tests; if anything is newly flaky, stop).
- [ ] `pnpm --filter desktop build` succeeds.
- [ ] `pnpm --filter desktop lint` is clean (once lint is configured).
- [ ] `docs-site/` builds without errors (if docs changes landed).

### 2. Version bump

- [ ] Run `scripts\bump-version.ps1 -NewVersion 0.1.0-alpha.N`.
- [ ] Verify `Cargo.toml`, `apps/desktop/package.json`, `apps/desktop/src-tauri/tauri.conf.json`, `apps/desktop/src/lib/tauri.ts` (`DEFAULT_STATE.system.version`) all match.
- [ ] Run `cargo check --workspace` to regenerate `Cargo.lock` with the new version.

### 3. Changelog

- [ ] Move all `## [Unreleased]` items under a new `## [0.1.0-alpha.N] — YYYY-MM-DD` section.
- [ ] Leave `## [Unreleased]` empty (or delete it — either is fine).
- [ ] Make sure user-visible changes are actually mentioned, not just internal refactors.

### 4. Documentation

- [ ] Update `ROADMAP.md` phase statuses and bump the "last updated" line.
- [ ] Update `KNOWN_ISSUES.md` with anything discovered late.
- [ ] Update `README.md` status badge and table row for the released phase.
- [ ] Spot-check the docs site — build locally, click through every top-level page, no 404s.

### 5. Install test

On a clean VM:

- [ ] `irm https://raw.githubusercontent.com/vixco/kairo-ai/<tag>/scripts/install.ps1 | iex`
  - Pick a moment when the release is already published so the tag URL resolves.
- [ ] Onboarding wizard completes end-to-end (Claude check → models → voice → permissions → diagnostics → done).
- [ ] `kairo setup` reports all green.
- [ ] Runtime starts, senses produce frames, triage decisions flow.
- [ ] Say "hey kairo" — wake + orchestrator + voice reply works.
- [ ] Dashboard opens, Home tab renders live state.
- [ ] Trigger "Fix Issues" from the Health tab — repair agent starts.

### 6. Uninstall test (optional but recommended)

Run `scripts\uninstall.ps1` if present. Verify:

- [ ] `%LOCALAPPDATA%\Kairo` is removed.
- [ ] `~/.kairo/` is preserved (we never destroy user data without explicit confirmation).
- [ ] Start Menu shortcut is gone.
- [ ] Registry `Run` entry is gone (if auto-start was enabled).

## Release steps

### 1. Tag locally

```powershell
# From a clean checkout of main at the commit you want to release
git tag -a v0.1.0-alpha.N -m "Kairo 0.1.0-alpha.N"
```

Do **not** push the tag yet.

### 2. Review before pushing

- [ ] `git log v<previous>..v0.1.0-alpha.N --oneline` — is the commit list what you expected?
- [ ] `git show v0.1.0-alpha.N` — does the signed tag point at the right commit with the right message?

### 3. Push the tag

```powershell
git push origin main
git push origin v0.1.0-alpha.N
```

Pushing the tag kicks off `.github/workflows/release.yml`, which:

1. Builds `kairo.exe`, `kairo-mcp.exe`, and the Tauri desktop bundle in release mode on `windows-latest`.
2. Runs the full test suite (last-chance verification).
3. Produces:
   - `kairo-<version>-windows-x64.zip` (portable) — `kairo.exe`, `kairo-mcp.exe`, `kairo-desktop.exe`, default configs.
   - `kairo-<version>-windows-x64.msi` (Tauri 2 bundled installer).
4. Creates a **draft** GitHub release with auto-generated changelog from commits since the previous tag.

### 4. Finalise the release

- [ ] Open the draft release on GitHub.
- [ ] Paste the relevant `CHANGELOG.md` section into the release notes.
- [ ] Call out breaking changes explicitly.
- [ ] Link to [KNOWN_ISSUES.md](../KNOWN_ISSUES.md).
- [ ] Mark as pre-release for alpha and beta tags.
- [ ] Publish.

### 5. Post-release

- [ ] Announce in the Discord / Matrix community (once set up).
- [ ] Post a summary in the repo's Discussions tab.
- [ ] Update the docs site's "latest version" banner if applicable.
- [ ] Open a new `## [Unreleased]` section at the top of `CHANGELOG.md` for the next cycle.
- [ ] Close any `backport/<version>` milestone.

## Code signing

As of 0.1.0-alpha.1, Kairo binaries are **not signed**. Windows SmartScreen will warn on first launch. The release notes should say this explicitly.

### Signing setup (when we have a cert)

1. Obtain an EV or OV Windows code-signing certificate from a CA (DigiCert, Sectigo, SSL.com).
2. Store the `.pfx` or hardware-token PIN in the repo's GitHub Actions secrets as `WINDOWS_CODE_SIGN_CERT` and `WINDOWS_CODE_SIGN_PASSWORD` (never commit the cert).
3. Wire `signtool.exe` into the release workflow — see [`scripts/sign-release.ps1`](../scripts/sign-release.ps1) for the placeholder script.
4. Update `apps/desktop/src-tauri/tauri.conf.json` → `bundle.windows.certificateThumbprint` with the thumbprint for Tauri's built-in signing.
5. Re-run the release build locally on a signed-cert-equipped machine to verify before publishing.

Timestamp server: `http://timestamp.digicert.com` (or the equivalent for your CA).

## Rollback

If a release turns out to be bad:

1. **Mark the GitHub release as "pre-release"** (or delete it entirely) so the installer no longer picks it as `latest`.
2. **Do not delete the tag** — tags are cheap history and someone may already have built from it.
3. **Cut a new patch release** with the fix. Don't try to move the tag or amend history.

For critical security fixes, prefer shipping a new patch release within 24 hours over quietly editing the bad one.

## Post-1.0 changes to this runbook

Once Kairo hits 1.0:

- Add a mandatory signed-build step.
- Add `winget` publication (`winget submit`).
- Add an auto-update check in the runtime against the GitHub releases API.
- Consider an `msix` build for Microsoft Store distribution.

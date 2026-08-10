# Known issues — Continuum alpha

Continuum remains an alpha. This document describes current limitations rather
than historical plans that no longer match the repository.

Last updated: 2026-08-09.

## Platform coverage

- The desktop application is built for **Windows x64**, **Apple Silicon macOS**
  and **Intel macOS** by the publication workflow.
- Native computer input and Windows UI Automation remain **Windows-first**.
  macOS can run the desktop, context MCP, memory and provider surfaces, but the
  Agent OS computer backend does not yet provide equivalent native macOS input.
- Linux desktop packaging is not part of the automatic release matrix.
- Windows ARM64 is not currently published.

## Releases and updates

- Automatic publication starts only after CI succeeds on the current `main`
  commit. It validates the complete Windows, Apple Silicon and Intel Mac asset
  matrix before creating the GitHub Release.
- macOS releases contain both a user-facing DMG and a signed Tauri updater
  archive. Tauri updater signing is not the same as Apple notarization.
- Windows Authenticode signing and Apple code signing/notarization still require
  external certificates and secrets. Users may therefore see SmartScreen or
  Gatekeeper warnings until those are configured.
- A missing `TAURI_SIGNING_PRIVATE_KEY` blocks publication by design.

### Opening the macOS DMG (no Apple Developer ID yet)

Because the DMG is not Apple Developer ID-signed and notarized, macOS attaches a
quarantine flag when the file is downloaded from a browser. On macOS 13+ this
surfaces as **"Continuum" is damaged and can't be opened. You should move it to
the Trash.** — the app is *not* actually corrupt; Gatekeeper is refusing an
unnotarized download. Pick one workaround:

- **Easiest:** drag Continuum into `/Applications`, then in Finder right-click
  (or Control-click) the app and choose **Open**. Confirm the prompt. This is a
  one-time per-version step.
- **From the terminal:** `xattr -cr /Applications/Continuum.app` strips the
  quarantine attribute, after which a normal double-click opens it.
- **Before installing:** `xattr -cr ~/Downloads/continuum-*-macos-aarch64.dmg`
  removes the flag on the installer itself so the drag-and-drop path is clean.

The release pipeline deep ad-hoc signs the `.app` bundle (including the bundled
`continuum`, `continuum-mcp` and `continuum-agent-os` executables) so that once
the quarantine flag is removed `codesign --verify` passes and the app launches
cleanly. Ad-hoc signing is *not* a Gatekeeper trust anchor, so it does not
remove the warning on its own — only Apple notarization can do that.
- Release publication refuses to publish binaries when `main` changes while the
  platform builds are running. A later successful CI run will retry from the new
  commit.

## Agent OS

- Public mutations must run through `agent_run_plan` with a stable `run_id` and
  typed postcondition. Direct mutation tools are intentionally refused.
- Cross-process locks and write-ahead journals prevent automatic duplicate
  execution. When a process crashes after dispatch, the result becomes
  `unknown`; Continuum stops and requires manual reconciliation rather than
  guessing whether the external action happened.
- Fail-closed crash locks are not removed automatically. An operator must inspect
  the destination before clearing an abandoned run.
- UI Automation cannot expose every canvas, game, remote desktop or custom
  control. Coordinate fallback remains necessary on some applications.
- Connected-app exactly-once behavior depends on authoritative destination
  lookup or an upstream idempotency key. When neither exists, Continuum can stop
  safely but cannot prove the remote state by itself.
- Computer-use smoke tests still need a real interactive Windows session; hosted
  CI validates code and packaging but is not a substitute for end-to-end UI
  interaction testing.

## Permissions and privacy

- Ordinary MCP permissions are enforced natively and can be changed from the
  Tools page. Changes apply to newly spawned MCP processes, not one that is
  already running.
- Unknown ordinary MCP tools require confirmation until classified in
  `config/default-permissions.toml`.
- Native approval dialogs are implemented for Windows and macOS. Headless
  sessions fail closed when confirmation is required.
- Sensitive vault notes are withheld from cloud-bound MCP results. Applications
  embedding the local vault directly must preserve the same sensitivity policy.
- Tool-call audit is intentionally lossy: private payloads, prompts, paths and
  URL queries are minimized. It is useful for continuity, not a forensic replay
  of every byte sent to a tool.

## Voice

- Whisper `small` can mishear “Continuum”; the larger default model is more
  reliable.
- Dutch Piper voice quality remains noticeably below the English default.
- ElevenLabs configuration exists, but availability depends on the selected
  backend and credentials.
- Global push-to-talk behavior remains platform-dependent.
- Long model responses may be shortened before text-to-speech playback.

## Orchestrator and providers

- Some legacy runtime paths still assume Claude Code CLI event formats. The chat
  gateway supports multiple providers, but the complete background orchestrator
  is not yet fully provider-neutral.
- Claude Code CLI fields and event types can change between releases. Unknown
  event types may be skipped with a warning.
- Model selection for workers is currently a deterministic heuristic rather than
  a learned quality/cost router.
- A provider outage, rate limit or dropped stream can leave an incomplete turn;
  Continuum surfaces the failure but cannot reconstruct missing provider output.

## Workers

- Worker output is not rendered consistently as rich Markdown in every view.
- Failed workers cannot always be resumed directly from the dashboard; spawning
  a corrected worker may still be required.
- Worker concurrency is bounded and intentionally conservative.
- Worker spawning can execute code and spend provider credits, so it requires an
  enforced native confirmation by default.

## Memory

- The vault and legacy semantic store still coexist during migration. Vault-first
  reads fall back to legacy facts when needed.
- Semantic quality depends on the local embedding model and is weaker for vague
  conceptual queries than exact project or decision retrieval.
- Multi-user isolation is based on the local operating-system account, not an
  in-app household account model.
- Agent-authored facts still need broader provenance and contradiction
  evaluation before every inferred statement can be treated as durable truth.
- Full encrypted-at-rest storage for every memory database is not yet provided;
  protect the operating-system account and data directory.

## Skills and integrations

- User skills may require a restart where a watcher does not cover the configured
  user-skill directory.
- Skill matching is heuristic and can select a suboptimal skill.
- Third-party skills and MCP servers run with the local user's privileges. Install
  only software you trust.
- Composio requires its hosted service and a project API key for OAuth and remote
  app execution.

## Dashboard

- Dark mode is the primary visual system.
- The dashboard and headless runtime are separate processes. The fast runtime
  bridge reduces latency, but some state can temporarily lag after a config
  change or process restart.
- Some configuration changes apply only to the next runtime or MCP process.
- Permission controls are live, but an already-open MCP session retains its
  session approval cache until that process exits.

## Self-healing

- Repair capability grants are scoped and short-lived, but automatic rollback is
  not available for every component or external side effect.
- Backups cover Continuum-owned state; they cannot roll back actions already
  committed in third-party SaaS applications.
- A repair that would mutate state is blocked when its pre-mutation backup or
  audit checkpoint cannot be verified.

## Testing gaps

- CI covers formatting, Clippy, Rust tests, frontend contracts, Windows/macOS
  builds and release-asset contracts.
- Real Windows computer-use testing still requires an interactive test session
  with standard applications, multi-monitor/DPI cases and approval dialogs.
- Real connected-app tests should use dedicated sandbox accounts; production
  accounts are not suitable for destructive test coverage.
- Prompt-injection, ambiguous-outcome and crash-injection evaluations need to
  continue expanding as the action surface grows.

Please report security-sensitive findings through the private process described
in [SECURITY.md](./SECURITY.md), not a public issue.

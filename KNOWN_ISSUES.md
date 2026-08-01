# Known issues — 0.1.0-alpha.1

Kairo is in **alpha**. This document lists things that don't yet work, don't yet work well, or would bite you on a clean install. Items are grouped by severity so you can decide whether an alpha-grade install is right for you.

Last updated: 2026-04-15.

---

## Platform coverage

- **Windows 10 (1903+) and Windows 11 only.** macOS and Linux support are explicitly post-1.0 — the Windows-specific APIs (Graphics Capture, UI Automation foreground tracking, `RegisterHotKey`) run deep through the senses and voice layers. Track [#macos-support] and [#linux-support] if you want to follow the ports.
- **No ARM64 builds.** x64 only for now.

## Installer / setup

- **Binaries are not code-signed.** Windows SmartScreen will warn on first launch; click "More info" → "Run anyway". Signing is tracked for the first non-alpha release — see [docs/release.md](./docs/release.md).
- **No auto-update mechanism.** Upgrading is "rerun the installer" today. `winget` publication is planned for 0.1.x.
- **The installer cannot verify Claude Code login** reliably — `claude config get` output is chatty and version-sensitive. If `claude login` fails silently, Kairo will only notice at wake time (via a clear error). Run `claude login` manually if you have any doubt.
- **Models download ~6.5 GB.** On slow connections this dominates onboarding time. The onboarding wizard lets you skip models you already have locally at `~/.kairo-dev/models/`.

## Voice

- **Whisper `small` mishears "Kairo".** It isn't in the vocab and tends to hallucinate "Cairo", "Hey Cairo", "kayo" variants, or split it across words. `medium` is the default for a reason. If you downgrade to `small`, expect missed wake-word detections.
- **Dutch Piper voice is barely intelligible.** The `nl_NL-mls-medium` voice ships with noticeable artifacts. The default config disables it; English (`en_US-norman-medium`) is the current default. Multilingual output is waiting on better small Piper voices.
- **ElevenLabs TTS is stubbed.** The config and backend selector are in place; the actual streaming client is a Phase 5 extension point that returns an error. Setting `tts.engine = "elevenlabs"` logs a warning and falls back to Piper.
- **There is a known failing test**, `matches_whisper_k_to_c_homophone`, that asserts `"hey cairo"` should match the `"hey kairo"` wake phrase. It's being left failing on purpose: the wake matcher is intentionally strict to avoid false positives from Discord voice traffic. Ignore for now.
- **Global hotkey is Windows-only.** `Ctrl+Shift+K` push-to-talk uses `RegisterHotKey` — there is no cross-platform abstraction yet.
- **No wake word customization in the wizard.** You can edit `~/.kairo/config/default-models.toml` manually (`[voice].wake_keyword`). A UI is planned.

## Triage

- **Qwen 3 8B occasionally over-wakes on visible errors** when the user is mid-edit of a file with a transient compile error. We're still calibrating the "error visible ≥ N seconds + idle" heuristic.
- **Qwen 3 4B fallback loses ~5% accuracy** on boundary cases (ignore vs remember, whisper vs wake). If you're on a 6 GB GPU or CPU-only, budget for more false wakes.
- **Non-English reasoning is untested.** The triage prompt and benchmark are English + Dutch only.

## Orchestrator / Claude Code

- **Claude Code CLI surface can drift between releases.** Kairo is pinned to the event schema seen in v2.1.100; a new CLI release may surface fields Kairo's `events.rs` doesn't recognise. The parser is forward-compatible (unknown fields are dropped) but new event *types* will be silently skipped. Watch the logs for `"unknown event type"` warnings.
- **Long Opus responses (>2000 words) may be truncated** when streamed into the TTS queue. The orchestrator prompt nudges Opus toward terse replies, but the truncation logic in `voice::tts` is heuristic.

## Workers

- **Worker output is not rendered as Markdown.** The Home tab's active-workers panel shows raw text. Formatting passes are planned.
- **No way to resume a failed worker** from the dashboard yet — you have to respawn with the same task.
- **Max concurrent workers is capped at 10**, default 3. There's no visible UI to change it mid-session; edit `~/.kairo/config/default-models.toml` under `[workers]`.

## Dashboard

- **No search inside the Memory tab's raw log view** — only semantic + episodic panels have search.
- **Dark theme only.** Light theme is not planned.
- **The dashboard process and the runtime process are separate.** They communicate via `~/.kairo-dev/state.json` and intent-file queues. Occasionally state lags by ~2 s after a config change. See [docs/dashboard.md](./docs/dashboard.md).
- **No keyboard shortcuts** for tab switching yet.

## Self-healing

- **The repair agent is spawned in the repo root as cwd.** On a release install, that path is `%LOCALAPPDATA%\Kairo`. The agent can currently read logs and call `mcp__kairo__repair_*` tools, but it cannot edit the Rust source tree directly (by design).
- **Rollback only covers `config.toml`.** Nightly backups snapshot five files but the rollback path currently only restores the main config.

## Memory

- **Vector search over episodic memory uses fastembed BGE-small** (384-dim, ~66 MB). Quality is acceptable for "what did Kairo see last Tuesday" queries but weak for fuzzy concept queries.
- **No multi-user mode.** Everything is scoped to the local OS user. A household with two people on one PC will see each other's episodic memory.
- **Memory export is manual** — a full export feature is planned. For now, the SQLite and LanceDB files under `~/.kairo/memory/` are self-describing.

## Skills

- **Hot reload watches `skills/`** under the repo root, not under `~/.kairo/skills/`. The intended user-skills directory is honored on load but not auto-reloaded. Restart Kairo after adding a user skill until the watcher is generalized.
- **Skill trigger scoring is heuristic.** Weights in `skills/matcher.rs` are hand-tuned. Expect the occasional miss where the orchestrator picks up the wrong skill.

## MCP tools

- **No write-side filesystem tools** yet. Reads only (`fs_read_file`, `fs_list_dir`). This is by design for alpha — write tools need stronger permission scaffolding.
- **No shell tool, and there will never be one** at the Kairo MCP layer. This is a non-negotiable from [CLAUDE.md](./CLAUDE.md). Workers can use Claude Code's built-in Bash tool, gated by Claude Code's own allowlist.
- **`web_fetch` is GET-only** and capped at 50 KB. Redirects are disabled outright to close redirect-SSRF.

## Not yet

These aren't bugs, they're things the alpha deliberately defers:

- **Discord / Matrix community space.** Will come with 0.1.0-beta once the first round of external users has given feedback.
- **A proper marketing website.** The README and the docs site are it for now.
- **Mobile companion app.** Post-1.0 idea.
- **Memory marketplace / shared skills registry.** Post-1.0 idea.
- **Voice cloning.** Post-1.0 idea.

---

Spotted something not on this list? Please [open an issue](https://github.com/vixco/kairo-ai/issues/new/choose). An alpha that documents its own limits is much easier to work with than one that hides them.

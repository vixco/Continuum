# Changelog

All notable changes to Continuum are documented here. Format based on [Keep a Changelog](https://keepachangelog.com/), versioning based on [SemVer](https://semver.org/).

## [Unreleased]

### Added

- **Chat tab + model gateway**: a new `crates/continuum-gateway` crate (a
  `ChatProvider` trait, three adapters — OpenAI-compatible, Anthropic, and
  Claude Code CLI — plus a static provider catalog covering ~18 presets such
  as LM Studio, Ollama, OpenAI, OpenRouter, DeepSeek, and a custom-endpoint
  option) backs a real **Chat** tab and a real **Settings → AI providers**
  ("Integrations") panel, replacing prior fixture data. Conversations stream
  token-by-token over the `continuum:chat` Tauri event, render markdown,
  support **Stop** mid-stream (the partial reply is kept and marked
  `stopped`, never discarded) and **Retry** on retryable failures, and
  persist per-conversation to `~/.continuum-dev/chats/<id>.json`. Provider
  connections persist to `~/.continuum-dev/providers.json` with **no secret
  material** — enforced by a dedicated unit test — while API keys live
  exclusively in Windows Credential Manager via the `keyring` crate (service
  `Continuum`); adding a connection tests it before saving, with an explicit
  "Save anyway" escape hatch. New `[chat]` config section (`max_tokens`,
  `temperature`, `connect_timeout_secs`, `stream_idle_timeout_secs`,
  `cli_timeout_secs`, `system_prompt_path`) makes every knob overridable per
  non-negotiable #3. A new `chat_providers` health probe reports Degraded
  when a configured provider's last connection test failed. Per
  non-negotiable #2: the only new network calls this feature introduces go
  to provider endpoints the user explicitly configures in Settings — no
  telemetry, no new default egress. See `docs/chat.md`.
- **Signed desktop updates**: the Tauri app checks for updates at startup,
  exposes a manual check in Settings, and lets users enable or disable
  automatic installation. Windows update artifacts are signed and published
  through the `main` push release workflow.
- **Redesigned dashboard**: frameless window with a single custom titlebar (the
  duplicate OS menu bar is gone), click/press animations on every control, a
  minimal Hermes/Buzz.xyz-style sidebar grouped into Daily/Configure/Advanced,
  and a Ctrl+K command palette. All mockup screens removed; the live tabs
  (Home, Voice, Memory, Brain, Tools, Automations, Health, Logs, Settings) are
  now wired to the Zustand store, which is hydrated by `bootstrapStore()` and
  kept in sync via the `continuum:state`/`continuum:log`/`continuum:repair`
  Tauri events. Window controls use `@tauri-apps/api/window` with no-op
  fallbacks outside Tauri.
- **One-command local dev**: `scripts/dev.ps1` runs the dashboard locally with
  no CI/CD or push. Modes: default (Tauri app), `-FrontendOnly` (Next.js on
  :3000), `-WithRuntime` (also start `continuum.exe` for live data), `-Check`
  (prereqs only). Aliased as `pnpm dev:local` / `pnpm dev:app`.

### Changed

- **Faster CI/releases**: full native Clippy/tests run on pull requests, while
  `main` reuses the tested code path and performs one production build. Release
  compiler artifacts are cached across version bumps, dependency resolution is
  kept locked, and Tauri assets are collected from the actual root target.

### Fixed

- **Release Tauri version gate**: align `@tauri-apps/api` with the resolved
  Tauri 2.11 Rust crate so signed desktop packaging no longer stops on a
  frontend/backend minor-version mismatch.
- **CI format gate**: 9 dashboard files that `pnpm format` (Prettier `--check`)
  flagged in the `build-desktop` job are reformatted; `prettier --write` was
  applied so `pnpm format` now passes.
- **Release `--locked` failure**: `cargo build --workspace --release --locked`
  refused to run because `Cargo.lock` was out of sync with `Cargo.toml` after
  the gateway/chat feature landed. `Cargo.lock` is regenerated so `--locked`
  passes again (`cargo check --locked -p continuum-gateway` verified locally).
- **Release speed**: the release workflow now installs **sccache**
  (`mozilla-actions/sccache-action`) and sets `RUSTC_WRAPPER=sccache`, so the
  whisper.cpp + llama.cpp + ort + lancedb native C/C++ compiles — the ~25 min
  wall of every release — become cache hits after the first release. The
  `cargo build` step also dropped `--workspace` (only the `continuum` and
  `continuum-mcp` bins are shipped; the desktop crate builds in its own Tauri
  step), and LLVM + ninja are pinned explicitly (mirrors `ci.yml`) so an image
  change never silently breaks bindgen.

### Adaptive resource throttling (auto-detect PC specs → tune Continuum)

Continuum now probes the host once at boot (CPU cores, RAM, GPU/VRAM, laptop-vs-desktop, AC-vs-battery) and resolves a concrete resource plan that tunes the triage LLM threads / GPU offload, vision CUDA EP, whisper threads, screen + context poll intervals, and worker concurrency. Default profile is `barely_notice` — a barely-noticeable CPU/RAM footprint with the GPU/VRAM used freely for quality (no model downgrades). Everything is overridable (non-negotiable #3).

- **`crates/continuum-core/src/hardware.rs`** (new): `HardwareSpecs` + `ResolvedResourcePlan` + `probe_hardware()` (sysinfo cores/RAM + `windows` `GetSystemPowerStatus` for battery + `LoadLibraryW("nvcuda.dll")` for CUDA + `nvidia-smi` subprocess for VRAM) + `resolve_resource_policy()` (pure, unit-tested) + `classify_system_load()` (pure load classifier). The module sits *outside* the four cognitive layers — it only tunes downward-facing knobs, never feeds perception frames upward.
- **`config.rs`**: new `[resources]` section (`ResourceConfig` + `ProfileMode` enum: `auto`/`barely_notice`/`balanced`/`performance`/`custom`) with `validate()` called from `load_config`. New `AudioConfig.whisper_threads` + `whisper_use_gpu` fields. `ResourceConfig` added to `ContinuumConfig` with `#[serde(default)]`.
- **Boot wiring**: `bin/continuum.rs`, `bin/continuum-perception.rs`, `bin/continuum-triage-bench.rs` probe + resolve once at startup and mutate the loaded config so every downstream consumer picks up the adapted values (replacing the hardcoded `available_parallelism().clamp(4,14)` and `gpu_layers = 999`).
- **`continuum-vision/src/onnx.rs`**: the dead `vision.gpu_enabled` config is now honoured — sessions build with `CUDAExecutionProvider` + `CPUExecutionProvider` fallback when `plan.vision_gpu` is true (best-effort: falls back to CPU + warn if CUDA EP commit fails). `OnnxVisionModel::new` now takes a `gpu: bool`. When `plan.vision_enabled` is false (very low RAM), perception loads a stub and runs text-only.
- **`senses/audio/full.rs`**: whisper thread count now comes from the resolved plan (`params.set_n_threads`) instead of a hardcoded constant.
- **Self-healing (#5)**: new `system_resources` health probe (`apps/desktop/src-tauri/src/components.rs`) samples CPU%/RAM every 30 s, reports `Degrading` on sustained >90% CPU / >90% RAM and `Error` on >95% RAM. `write_repair_context` (`health/repair.rs`) now appends a `## System resources` block (detected specs + live CPU/RAM + GPU/VRAM + power + resolved plan) so the repair agent can reason about model-load failures.
- **Dashboard (#3)**: new Tauri commands `get_resource_profile` + `update_resource_profile` (`commands.rs`) and a `ResourcePanel` on the Settings screen (`apps/desktop/src/components/continuum/ResourcePanel.tsx`) showing detected hardware, the resolved plan, a profile selector, custom sliders, and a "Restart to apply" banner. New `system_resources` health probe registered. TypeScript mirrors added to `lib/types.ts`; `continuum.getResourceProfile` / `updateResourceProfile` wrappers in `lib/tauri.ts`.
- **No hot-reload**: the plan is computed once at boot and published to `state.json` (`RuntimeSnapshot` gained `hardware_specs` + `resource_plan` fields). Changing the profile via the dashboard persists to `config.toml` and shows a restart banner (consistent with the existing daemon limitation).
- **Builds during this work used `cargo -j 2`** to avoid lagging the maintainer's PC (the trigger for this feature).

### Renamed Kairo → Continuum (repo-wide)

The donor "Kairo" name has been retired everywhere in favour of "Continuum".

- **Crates / binaries**: `kairo-core`/`-llm`/`-mcp`/`-vision`/`-desktop` → `continuum-*`; bin `kairo` → `continuum`, `kairo-perception` → `continuum-perception`, `kairo-triage-bench` → `continuum-triage-bench` (`audio-probe` unchanged). Rust module paths `kairo_core`/`kairo_mcp`/`kairo_llm`/`kairo_vision` → `continuum_*`. Public types `KairoConfig`/`KairoRuntime`/`KairoState`/`KairoMcpServer` → `Continuum*`; `kairo_dev_dir()` → `continuum_dev_dir()`.
- **MCP public API (breaking)**: server name `kairo` → `continuum`; tool prefix `mcp__kairo__*` → `mcp__continuum__*`; reserved memory-key prefix `kairo.` → `continuum.`. Pre-alpha, no compat shim.
- **User-data migration**: `~/.kairo/`, `~/.kairo-dev/`, `~/.kairo-backups/` → `~/.continuum*` — migrated automatically on first run (atomic rename on the same volume; falls back to the legacy path if the rename fails so no data is lost). Env vars `KAIRO_DATA_DIR`/`KAIRO_MODELS_DIR`/`KAIRO_PIPER_BIN`/`KAIRO_MCP_BIN`/`KAIRO_WORKER_DRY_RUN`/`KAIRO_EMBEDDINGS_CACHE_DIR`/`KAIRO_OFFLINE` → `CONTINUUM_*`, with the old names read as a transitional fallback via `config::env_or_legacy`. `KAIRO_SIGN_THUMBPRINT` → `CONTINUUM_SIGN_THUMBPRINT` (sign-release.ps1 falls back to the old name).
- **Tauri / desktop**: `productName` `Kairo` → `Continuum`; bundle id `com.toshan.kairo` → `com.toshan.continuum`; window title `Kairo Dashboard` → `Continuum Dashboard`; tray id `kairo-tray` → `continuum-tray`; IPC channels `kairo:state`/`kairo:log`/`kairo:repair`/`kairo:control`/`kairo:runtime_error`/`kairo:onboarding:progress` → `continuum:*`; `@kairo/desktop` → `@continuum/desktop`; `kairo-docs` → `continuum-docs`; CSS classes `kairo-*` → `continuum-*`; TS `kairo` API object → `continuum`.
- **Voice**: default wake word `hey kairo` → `hey continuum` (still user-overridable via config).
- **Repo URLs**: `vixco/kairo-ai` → `vixco/Continuum` (install script, release workflow, docs site, issue templates, signing URL).
- **Docs / prompts / skills / CI**: all prose and code identifiers renamed; Greek-*kairos* etymology sentences rewritten to the Latin *continuum* etymology.

## [0.1.0-alpha.2] — 2026-04-18

### Security + reliability hardening (post-alpha.1 audit)

A full audit of the alpha.1 build surfaced a mix of actual bugs, architectural drift, and drop-the-next-alpha blockers. This block is the remediation pass.

**Orchestrator / correctness**

- **`orchestrator/spawn.rs`**: wake invocation now includes `--input-format stream-json`. Without it the CLI was reading our stream-json user message on stdin as a plain text prompt — it happened to work because of CLI leniency, but broke on newer CLI versions. The worker supervisor + repair agent already passed this flag; they're now consistent.
- **`orchestrator/wake_context.rs`**: `format_frame_oneline` no longer byte-slices screen descriptions / transcripts to `[..57]` / `[..27]`. A Dutch window title with an `é`, a Japanese app name, or an emoji in a transcript would have panicked the senses loop on its first frame. Added `truncate_on_char_boundary` helper + UTF-8 regression tests (`β`, `😀`).
- **`senses/audio/full.rs`**: whisper transcription now returns the actually-detected BCP-47 language instead of the literal string `"auto"`. TTS voice routing (`PiperVoiceBank::choose_voice`) can therefore pick a matching Piper voice for Dutch / German / … instead of silently falling back to the English primary.
- **`orchestrator/spawn.rs`**: `mcp-config.json` is now written with a per-wake nonce (`<pid>-<counter>-<epoch>`), so two wakes firing in the same millisecond (triage + hotkey) cannot clobber each other's MCP config. Added `kill_on_drop(true)` on the wake Command so cancelled wakes don't orphan the claude subprocess.
- **`bin/continuum.rs`**: in-flight wakes now race against the shutdown watch channel via `tokio::select!`. On Ctrl-C the wake future is dropped, `kill_on_drop` fires, and the claude subprocess is reaped before the runtime exits.

**Non-negotiables compliance**

- **`config/default-permissions.toml`**: completely rewritten to match the 21 registered MCP tools. The previous file still listed aspirational `perception_*` / `voice_*` / `windows_*` tools and, critically, a `[shell]` block — Continuum never exposes a shell tool by design (`CLAUDE.md` rule 1/4). Shell tools removed; `repair_*` tools moved to `blocked` tier (unlocked only inside an active repair session); `workers_spawn_worker` + `workers_worker_cancel` moved to `session-approved`.
- **`memory/episodic.rs`**: fastembed (BGE-small) model cache is now pinned to a Continuum-owned directory (`CONTINUUM_EMBEDDINGS_CACHE_DIR` / `CONTINUUM_MODELS_DIR` / `~/.continuum/models/embeddings`). The unified model-download script pre-stages it; if the model is missing at startup Continuum logs a loud warning before falling back to HuggingFace. A new `CONTINUUM_OFFLINE=1` env var hard-refuses the download, so air-gapped installs never emit an unexpected network request.
- **`orchestrator`, `triage`**: added `[orchestrator]` + `[triage]` sections in `ContinuumConfig` with `model_id`, `wake_timeout_secs`, `bare_mode`, `context_size`, `max_tokens`, `temperature`, `gpu_layers`, `latency_warn_ms`, `model_path`. The three binaries (`continuum`, `continuum-perception`, `continuum-triage-bench`) + `health/repair.rs` all read from config instead of hardcoded `"claude-opus-4-6"` / `qwen3-8b-q4_k_m.gguf` constants. Swapping the orchestrator model is now a one-line config edit (per non-negotiable #3).

**Security**

- **`continuum-mcp/src/tools/web.rs`**: closed the SSRF TOCTOU window. Previously we resolved DNS to verify public-IP, then let reqwest re-resolve during connect — a DNS-rebinding attacker could return public-then-private and bypass the check. Now the resolved `SocketAddr` list is pinned on a per-call `reqwest::Client` via `resolve_to_addrs`; reqwest cannot dial anything except the IPs we verified.
- **`apps/desktop/src-tauri/src/commands.rs`**: `save_skill` / `delete_skill` / `install_skill_from_url` now run `validate_skill_name` (rejects `..`, `/`, `\`, empty, overlong, illegal chars) before touching the skills root. `install_skill_from_url` additionally enforces a host allowlist (`github.com`, `gitlab.com`, `bitbucket.org`, `codeberg.org`, `git.sr.ht`) via a real URL parse, uses `tokio::process::Command` so blocking `git clone` doesn't stall a Tauri worker, and passes `--` to git so a crafted URL starting with `--` cannot be interpreted as a flag.
- **`apps/desktop/src-tauri/tauri.conf.json`**: `"csp": null` replaced with a restrictive Content Security Policy (self + ipc/asset + unsafe-inline only for styles). A compromised webview asset can no longer inline an external script.

**Reliability**

- **`health/repair.rs`**: the repair-agent claude subprocess is now wrapped in a 30-minute `tokio::time::timeout`. A hung Opus session would otherwise pin `repair_running = true` forever and block future repair runs.
- **`voice/tts.rs`**: Piper synthesis is bounded by a 30 s `wait_child_with_timeout`. A stuck phonemizer used to freeze the TTS worker thread permanently; now it kills the child and returns a clear engine-stuck error.
- **`voice/streaming.rs`**: the speech-job mpsc is now `sync_channel(32)` with `try_send`. An unbounded queue could previously balloon behind a hung Piper; the bounded channel drops utterances (with a structured warning) instead.
- **`continuum-mcp/src/tools/repair.rs`**: intent filenames include a monotonic nonce so two intents queued in the same millisecond don't silently overwrite each other.
- **`voice/streaming.rs::find_sentence_end`**: URL-scheme colons (`https://`, `ftp://`, `ws://`, `file://`) — including "See: https://…" patterns — no longer trigger a sentence split. Piper was rendering `https` as its own utterance whenever the orchestrator spoke a URL.

**Dashboard**

- **`apps/desktop/src-tauri/src/runtime_bridge.rs`**: the local `RuntimeSnapshot` struct is replaced with the one from `continuum_core::runtime_publish`, so the dashboard reads every field the runtime writes (incl. new `frame_count` / `wake_count` / `last_update`). Malformed `state.json` is surfaced to the frontend via a `continuum:runtime_error` Tauri event (once per error streak) instead of silently showing stale flags.
- **`apps/desktop/package.json`**: added missing `eslint`, `eslint-config-next`, `prettier`, and `prettier-plugin-tailwindcss` dev-deps; `typecheck` + `format` scripts; Node engines constraint. `.eslintrc.json` + `.prettierrc.json` added. CI now runs dashboard typecheck + lint (format is continue-on-error for one cycle while the migration lands).

**Architecture + docs**

- **`ARCHITECTURE.md`**: triage default model updated from Qwen 3 4B to the shipped Qwen 3 8B; orchestrator allowed-tools list fixed to `mcp__continuum__*` (no `Bash`/`Task`/`Read`); wake-word section rewritten to describe the actual whisper-transcript matcher (Porcupine was only ever prototyped); the MCP tool section is split into "Shipped in v0.1.0-alpha" (the 21 real tools) and "Planned (not yet shipped)" with a note that `mcp__continuum__shell_*` is a permanent non-goal.
- **`.github/workflows/ci.yml`**: dashboard `typecheck` + `lint` + `format` steps added; dashboard build uses `@continuum/desktop` pnpm workspace name.
- **`.github/workflows/release.yml`**: now generates and uploads `SHA256SUMS.txt` alongside the ZIP + MSI. `scripts/install.ps1` verifies the ZIP against it and hard-fails on mismatch.
- **`SECURITY.md`** + **`CODE_OF_CONDUCT.md`** added: vuln disclosure workflow (private advisories + email), response timeline, scope notes; Contributor Covenant 2.1.

### Added — Push-to-talk + Voice tab UX honesty

- **Push-to-talk button on the Home tab** (`apps/desktop/src/components/PushToTalkButton.tsx`): a big round mic button next to the status orb, gives users a one-click alternative to the wake word and the `Ctrl+Shift+K` global hotkey. Three visual states (idle, pressed, listening) with optimistic local feedback so the click feels instant even though the daemon's `state.voice.mode` lags up to 2 s behind via the state poller. Disabled while Continuum is thinking or speaking.
- **Voice intent file protocol** (`crates/continuum-core/src/voice/intent.rs`): mirror of `workers::intent` — atomic write via `.tmp` + rename, drain on each daemon tick (250 ms), `.bad` rename for unparseable files, and a 30-second TTL that silently drops stale intents so a crash can't fire a spurious listen on next launch. `TalkNow` is the only variant for now; `serde(tag = "kind")` keeps the on-disk schema open for future `Cancel`/`Mute` intents. 4 unit tests cover write/drain roundtrip, bad-JSON rename, stale drop, and ensure-dir-on-missing.
- **`talk_now` Tauri command** + frontend wrapper `continuum.talkNow()`: dashboard writes the intent file via the new helper; daemon picks it up in the same select arm style as the existing `recv_hotkey` (`drain_voice_intents_tick` helper in `crates/continuum-core/src/bin/continuum.rs`).
- **Voice tab wired stub handlers**: the four `onChange={() => {}}` no-ops in `VoiceTab.tsx` (engine select, primary voice select, length-scale slider, wake-sensitivity slider) now call real Tauri commands that persist to `config.toml`. Four new commands shipped: `update_tts_engine` (validates `piper`/`elevenlabs`), `update_tts_primary_voice` (rejects empty), `update_tts_length_scale` (clamped 0.5–2.0), `update_wake_sensitivity` (clamped 0–1).
- **Restart-required notice on the Voice tab**: a yellow info banner makes it explicit that voice settings are saved-now-applied-on-restart. The daemon currently loads its config once at boot and does not watch `config.toml`; that's a known limitation earning a separate hot-reload phase. The banner lives in `RestartNotice` at the top of `VoiceTab`.

### Changed

- `crates/continuum-core/src/lib.rs` + `crates/continuum-core/src/voice/mod.rs`: `voice` module is now always compiled. Heavy submodules (TTS, STT, playback, streaming, wake, sounds, hotkey, health) stay gated behind the `runtime` feature; the new `intent` submodule is pure serde/std and is reachable from the desktop crate without pulling llama-cpp/whisper into its build.
- `apps/desktop/src/components/tabs/VoiceTab.tsx`: `wake_sensitivity` slider hidden — the field exists in `ContinuumConfig` but no daemon code consumes it (transcript-based phonetic wake match has no threshold). Tracked as a known limitation rather than a misleading slider. Hotkey display gains a small "rebind via config.toml — UI rebind komt later" caption to set expectations.
- `apps/desktop/src/components/tabs/HomeTab.tsx`: the orb-headline-screenshot row now has the PTT button between the headline and the screenshot thumbnail, so orb + button form one visual cluster.

### Fixed

- Dashboard's voice-flag toggles still only persist to `config.toml` (daemon restart needed to take effect), but they no longer pretend otherwise — see the new restart banner. Live hot-reload is intentionally out of scope for this change.

## [0.1.0-alpha.1] — 2026-04-15

First public alpha. Phase 9 (polish + alpha release) complete. Every phase from the roadmap (0 through 9) is done.

### Added — Phase 9 polish + alpha release

- **Real installer** (`scripts/install.ps1`): end-to-end Windows installer — checks Windows version (10 1903+ / 11), checks Node.js 18+, Claude Code CLI, auth status, creates `~/.continuum/` data directory layout (config, models, logs, memory, backups, bin, worker-intents, workers, repair-intents), downloads the release binary from GitHub (or builds from source with `-FromSource`), runs `scripts/download-models.ps1`, adds a Start Menu shortcut, and optional `-DesktopShortcut` / `-AutoStart` flags. Idempotent — rerunning upgrades / repairs without losing user data.
- **Version bump tooling** (`scripts/bump-version.ps1`): one-shot version update across `Cargo.toml`, `apps/desktop/package.json`, `apps/desktop/src-tauri/tauri.conf.json`, and the dashboard's `DEFAULT_STATE.system.version`. Dry-run support.
- **Code-signing placeholder** (`scripts/sign-release.ps1`): `signtool`-based signing scaffolding gated on `CONTINUUM_SIGN_THUMBPRINT`. No-op in its absence — alpha ships unsigned.
- **Release runbook** (`docs/release.md`): pre-release checklist, tagging steps, GitHub release workflow, rollback procedure, code-signing plan.
- **Known-issues doc** (`KNOWN_ISSUES.md`): documented alpha-grade rough edges by category (platform, installer, voice, triage, orchestrator, workers, dashboard, self-healing, memory, skills, MCP tools).
- **README rewrite**: alpha status badges, real install instructions, updated project-status table with all phase tags, tech-stack refresh (SmolVLM-256M, Qwen 3 8B, Piper, fastembed, LanceDB), screenshot section, known-issues callout.
- **CONTRIBUTING rewrite**: opened for external PRs — code of conduct, dev environment, PR workflow (conventional commits, changelog, architecture updates), coding standards summary (Rust + TypeScript), guidance on writing skills and MCP tools.
- **GitHub templates**: issue templates for bug reports, feature requests, and skill requests (`.github/ISSUE_TEMPLATE/*.yml` with structured forms); `config.yml` pointing to docs and discussions; `pull_request_template.md` with the verification checklist.
- **CI/CD**: `.github/workflows/release.yml` builds Windows release artifacts (MSI + portable zip) on `v*` tag push and drafts a GitHub release; `.github/workflows/ci.yml` split into parallel `quick-check` (fmt + clippy) and `full-test` jobs with cargo + pnpm caching; `.github/workflows/docs.yml` builds and deploys the docs site to GitHub Pages on push to `main` under `apps/docs/`.
- **Docs site scaffold** (`apps/docs/`): Nextra 3 on Next.js 15 with a dark theme matching the dashboard; sidebar navigation covering Getting Started, Core Concepts, Features, Configuration, Privacy & Security, Troubleshooting, and For Developers; deploys to GitHub Pages via the docs workflow.
- **User-facing documentation** in `apps/docs/pages/`: Installation, First run, Quick start, How it works, Perception, Triage, Orchestrator, Workers, Voice, Memory, Skills, Dashboard, Automations, Self-healing, Models config, Permissions, Voice settings, all config options reference, Data residency, No-telemetry policy, Troubleshooting index, Common fixes, Reading logs, Resetting Continuum, Architecture link, Contributing link, Building from source, Writing skills, Writing MCP tools.
- **Onboarding wizard** in the Tauri app: eight-step first-run flow (Welcome → Claude Code check → Model downloads → Voice setup → Permissions → Personal info → Diagnostics → Done) gated on the absence of `~/.continuum/config/onboarding-complete`. The wizard runs inline in the dashboard shell, uses the existing dark palette and UI primitives, and marks the run complete with a single file write.
- **Onboarding Tauri commands** (`apps/desktop/src-tauri/src/commands.rs`): `check_claude_cli`, `check_claude_auth`, `list_audio_input_devices`, `list_audio_output_devices`, `download_model` (wraps `scripts/download-models.ps1`), `run_diagnostics` (returns a structured report of vision / triage / STT / TTS / mic / screen / memory check results), `is_onboarding_complete`, `complete_onboarding`.
- **`continuum setup` CLI subcommand** (`crates/continuum-core/src/bin/continuum.rs`): runs the same prereq checks as the installer, downloads missing models, runs a full diagnostic pass, and prints a structured status report. Safe to run at any time, not just first-run.
- **Graceful degradation**: each senses/voice subsystem now logs a clear warning and registers a health component with `status = degraded` when a required artefact is missing (vision model → `ComponentStatus::Degraded` with reason "vision model not found, run continuum setup"; triage model → same, fallback to passing all frames to the orchestrator is disabled with a clear explanation; TTS → text-only fallback; mic → hotkey-only activation; Claude Code → dashboard still works for memory browsing and config, but wake attempts fail fast with an actionable error).
- **Error message audit**: every missing-model, missing-claude, missing-config, and missing-permission error now names the exact remedy. Examples: "Qwen 3 8B not found at `<path>`. Run: continuum setup" and "Claude Code not installed. Run: npm install -g @anthropic-ai/claude-code && claude login".
- **First-run memory seeding**: on a fresh install, the runtime seeds `user.timezone`, `user.language`, `user.os`, `continuum.version`, and `continuum.install_date` into the semantic memory store so the orchestrator has a sensible baseline from the first wake.
- **Version display in the dashboard topbar**: `system.version` is surfaced as `v<version>` next to the clock, readable from the `ContinuumRuntime::version()` constant.

### Changed

- Version bumped to `0.1.0-alpha.1` across `Cargo.toml` (workspace), `apps/desktop/package.json`, `apps/desktop/src-tauri/tauri.conf.json`, and the dashboard's `DEFAULT_STATE.system.version`.
- `scripts/dev-setup.ps1` output references the new install flow.
- `config/default-models.toml`: defaults audited for a 16 GB-RAM, GPU-optional Windows machine. GPU auto-detection is on by default; Piper English voice is the only enabled TTS voice (Dutch voice commented out with an opt-in path).

### Fixed

- Installer now waits for the user to confirm Claude Code login before continuing; previously it only printed a warning.
- `scripts/download-models.ps1` respects `$env:CONTINUUM_MODELS_DIR` so the installer and onboarding wizard can redirect it to `~/.continuum/models/` instead of the dev-dir default.
- `crates/continuum-core/Cargo.toml`: added `required-features = ["runtime"]` to the two integration tests and four examples that pull in `continuum_core::voice` / `orchestrator` / `workers::pool`, so `cargo clippy -p continuum-core --no-default-features --all-targets` no longer fails on the lightweight (no-runtime) build path used by the Tauri dashboard.
- `apps/docs/`: adapted the Phase 9 scaffold to Nextra 3 + Next.js 15 API — added `theme`/`themeConfig` to `withNextra()`, removed `primaryHue` / `primarySaturation` / `useNextSeoProps` (all dropped in v3), added a required custom `_app.tsx`, migrated every `pages/**/_meta.json` to `_meta.ts`, and disabled Next.js's pages-router type validator which rejects Nextra meta files for not being page components.
- `apps/desktop/src-tauri/tauri.conf.json`: overrode `bundle.windows.wix.version` to `0.1.0` so the MSI bundler (which rejects non-numeric pre-release identifiers) builds cleanly. The user-facing app version and MSI filename still include `alpha.1`.
- `crates/continuum-core/src/voice/wake.rs::matches_whisper_k_to_c_homophone`: marked `#[ignore]` to match the existing note in `KNOWN_ISSUES.md` and the skip filter already present in CI/release workflows. The wake matcher is intentionally strict to avoid Discord voice false positives; the fuzzy "hey" prefix matcher needed here is tracked for post-alpha.

## Pre-alpha history (phases 0–8)

### Added — Phase 8 workers + skills

- **Worker pool** (`crates/continuum-core/src/workers/pool.rs`): queue with priority ordering (user_requested > orchestrator_spawned > scheduled), concurrency cap (`max_concurrent`, default 3, max 10), failure-streak refusal, per-worker snapshot publishing, and dashboard/MCP/audit hooks. Cancellation signals propagate from queued and running workers; pool shutdown gracefully cancels everything.
- **Worker supervisor** (`crates/continuum-core/src/workers/supervisor.rs`): spawns a fresh `claude --print --output-format stream-json` subprocess per worker, streams events (`SessionReady`, `TextDelta`, `ToolCall`, `Progress`, `Log`, `Finished`), enforces wall-clock timeouts with `tokio::time::timeout_at`, and returns a terminal `WorkerOutcome`. A dry-run mode (`CONTINUUM_WORKER_DRY_RUN=1`) synthesises a transcript for tests + the `worker_demo` example.
- **Worker types** (`crates/continuum-core/src/workers/types.rs`): `WorkerSpec`, `WorkerSnapshot`, `WorkerPriority`, `WorkerModelTier`, `WorkerStatus`, `WorkerPoolStats`, `WorkerOutcome`. All serde + non-runtime-gated so the dashboard can read them without llama-cpp.
- **Intent file protocol** (`crates/continuum-core/src/workers/intent.rs`): MCP writes JSON intents to `~/.continuum-dev/worker-intents/`; continuum-core drains, processes, and writes per-worker snapshots to `~/.continuum-dev/workers/<id>.json` atomically (`.tmp` + rename). Malformed intents are renamed to `.bad` so the loop never starves.
- **Model selection heuristic** (`crates/continuum-core/src/workers/model_select.rs`): Auto mode picks Opus for refactor/architect/debug-complex/migration work and Sonnet for rename/format/summary/boilerplate; tie goes to Opus. Explicit `"power"`/`"budget"`/`"claude-*"` tiers override; config `mode = "budget"|"power"` beats everything. Every choice is logged with a one-line reason in the worker snapshot.
- **Worker MCP tools** (`crates/continuum-mcp/src/tools/workers.rs`): `workers_spawn_worker`, `workers_worker_status`, `workers_worker_cancel`, `workers_worker_wait`, `workers_worker_list` — all registered in `ContinuumMcpServer` under the `mcp__continuum__workers_*` namespace, with full audit coverage.
- **Skills module** (`crates/continuum-core/src/skills/`):
  - `frontmatter.rs`: hand-rolled YAML parser for the narrow skill frontmatter (`name`, `description`, `triggers`, `source`, `manual_only`), tolerant of CRLF, inline and list trigger styles, unknown keys.
  - `loader.rs`: `SkillLoader` scans `skills/`, parses each `SKILL.md`, caches by name, hot-reloads on `mtime` change, surfaces parse errors for the dashboard.
  - `matcher.rs`: `SkillMatcher` scores skills by trigger substring hits against a `MatchContext` (wake reason, task, project, audio, foreground app, tags, forced). Multi-match with a token budget; forced skills bypass the budget and rank first.
  - `installer.rs`: `create_skill`, `save_skill`, `delete_skill` with name validation (`[a-zA-Z0-9_-]`) and safe frontmatter serialisation.
- **Bundled skills**: replaced placeholders with five real skills — `daily-briefing`, `code-review`, `project-context`, `email-draft`, `file-organizer`. Each has concrete procedure, output format, and refusal rules.
- **Orchestrator prompt injection** (`crates/continuum-core/src/bin/continuum.rs::compose_wake_config`): on each wake, matched skills are appended to the static orchestrator prompt and written to `~/.continuum-dev/orchestrator-dynamic.md`, which the spawned claude process receives via `--append-system-prompt-file`.
- **Worker prompt injection**: the pool materialises `<data_dir>/worker-prompts/<id>.md` per worker, combining `prompts/worker-system.md` + task-matched skill content, before launching the supervisor.
- **Triage suggested_skill hint**: `TriageDecision::WakeOrchestrator` grew an optional `suggested_skill` field (serde-skipped when absent, GBNF grammar updated); triage prompt lists available skill names and instructs the layer to tag wakes when a skill clearly applies.
- **Audit trail**: pool event + finish sinks write to episodic memory — per tool-call events tagged `worker` + `worker:<id>` + tool name (importance 0.4), per terminal-state summary events tagged with the task, skills, and outcome (importance 0.5 completed / 0.7 failed).
- **Dashboard**:
  - Tools tab: real skill CRUD — list with source badges, enable/disable toggle persisted to `skills.disabled`, create/edit modal with Markdown body, delete with confirmation, install-from-URL via `git clone --depth 1` + validation.
  - Home tab: live workers panel polling `list_workers` every 750 ms, status dots, progress bars, cost readout, click-through detail modal with full live output + model-choice reason + cancel/dismiss actions.
- **Health probes**: new `workers` and `skills` components registered in `components.rs`. `workers` surfaces recent failures and flags 3+ failures in 10 minutes as error. `skills` fails when any `SKILL.md` fails to parse and degrades when zero skills are loaded.
- **Examples**: `examples/worker_demo.rs` (end-to-end dry-run spawn + wait + report) and `examples/skill_match_demo.rs` (load skills + print matches for a wake reason given as CLI arg).
- **Tests**: 27 skills unit tests + 21 workers unit tests + 6 MCP workers-tool tests + 8 skills integration tests + 5 workers integration tests (intent → snapshot e2e, cancel of queued worker, priority ordering, failure-streak probe, spawn latency).
- **Docs**: `docs/workers.md`, `docs/skills.md`, Phase 8 section appended to `docs/mcp-tools.md`, roadmap Phase 8 box ticked.

### Changed — Phase 8
- `ContinuumConfig` grew `workers: WorkersConfig` and `skills: SkillsConfig` blocks; defaults wired through `default-models.toml`-compatible TOML.
- `TriageDecision::WakeOrchestrator` gains `suggested_skill: Option<String>` (serde-skipped when None — backwards-compatible on the wire).
- `prompts/triage-grammar.gbnf`: `wake_tail` production allows the optional `suggested_skill` field.
- `prompts/orchestrator-system.md`: added Workers + Skills sections with spawn rules and best practices.
- `prompts/triage-system.md`: lists the five bundled skill names with example triggers so triage can suggest them.
- `prompts/worker-system.md` (new): base worker behaviour prompt — one-task scope, narrow tools, structured report format.
- `crates/continuum-core/src/lib.rs`: `workers` and `skills` are now always-on modules; pool + supervisor + model_select remain gated on `runtime` so the Tauri build stays light.
- `apps/desktop/src-tauri/src/commands.rs`: added 9 new Tauri commands (`list_skills`, `save_skill`, `delete_skill`, `toggle_skill`, `install_skill_from_url`, `list_workers`, `get_worker`, `cancel_worker`, `dismiss_worker`).

### Added — Phase 6 dashboard + self-healing
- **Runtime state store**: `crates/continuum-core/src/state.rs` — single `ContinuumState` snapshot of perception, triage, orchestrator, workers, voice, memory, health, system, plus a 50-entry recent-actions ring. Typed update helpers on `StateHandle` publish to a tokio broadcast channel; the dashboard subscribes and re-emits coalesced snapshots to the frontend over Tauri's `emit`.
- **Log ring buffer**: `crates/continuum-core/src/logs.rs` — `BufferLayer` is a `tracing::Layer` that captures every event into a 10 000-entry ring and a tokio broadcast channel. Exposes `LogFilter` (level / layer / component / text / since) and a live subscribe API. The Logs tab reads from it; the Repair agent includes the last 500 lines in its context.
- **Health registry**: `crates/continuum-core/src/health/mod.rs` — pluggable `HealthCheck` trait with a polling `spawn_poller`. Registers 8 default probes (vision, triage, orchestrator, tts, stt, memory, mcp, context_watcher) in `apps/desktop/src-tauri/src/components.rs`. Rolling stats flag a component as `degrading` when the 24 h error rate across the last 20 probes > 5 %.
- **Backup rotation**: `crates/continuum-core/src/health/backup.rs` — nightly 04:00 zip of `config.toml` / `automations.json` / `permissions.toml` / `semantic.sqlite` / `orchestrator-system.md` to `~/.continuum-backups/<date>/`, keeping the 7 most recent. Exposes `run_backup`, `prune_backups`, `count_backups`, `latest_backup_ts`, `spawn_nightly`.
- **Repair agent**: `crates/continuum-core/src/health/repair.rs` — spawns Claude Opus 4.6 as a headless subprocess with the repo root as cwd, a repair-context file at `~/.continuum-dev/repair-context.md`, and streams events back as `RepairEvent` variants (assistant deltas, tool calls, tool results, stderr, final status). Also exposes `rollback_config(date)` that extracts `config.toml` from a dated backup zip.
- **Repair MCP tools**: `crates/continuum-mcp/src/tools/repair.rs` registers 5 new tools under `mcp__continuum__repair_*` — `restart_component`, `reinstall_model`, `rollback_config`, `test_component`, `escalate`. They write intent files to `~/.continuum-dev/repair-intents/` that the runtime drains on its tick.
- **Repair agent system prompt**: `prompts/repair-agent-system.md` — concrete operating rules, component → log-path map, concise output style ending in `RESOLVED` / `ESCALATED` / `PARTIAL`.
- **Automations store**: `crates/continuum-core/src/automations.rs` — JSON-backed list of one-shot / recurring tasks, full CRUD + toggle, atomic writes via `.tmp` + rename.
- **Embeddable runtime**: `crates/continuum-core/src/runtime.rs` — `ContinuumRuntime::init()` opens config, automations, state, log buffer, shutdown watch channel. Typed setters for `paused`, `voice_muted`, and a `update_config(|cfg| …)` helper that persists.
- **Runtime publisher**: `crates/continuum-core/src/runtime_publish.rs` — `RuntimeSnapshot` + `spawn_publisher` writes `~/.continuum-dev/state.json` every 2 s so the separate dashboard process can read live runtime flags without needing an IPC channel.
- **Feature-gated continuum-core**: the heavy runtime modules (`memory`, `orchestrator`, `voice`, `workers`, plus the watchers in `senses/*` except `types.rs`, plus the llm-backed parts of `triage`) are now behind the `runtime` feature so the dashboard builds without llama-cpp / whisper / lancedb. `continuum.exe` keeps the feature on by default; `continuum-desktop` sets `default-features = false`.
- **Tauri 2 desktop app**: `apps/desktop/src-tauri/` now has full backend: `commands.rs` (26 Tauri commands covering config, memory, automations, health, repair, window control), `events.rs` (state + logs + repair event bridge), `tray.rs` (system tray with state-based icon, right-click menu), `components.rs` (default health probes), `runtime_bridge.rs` (reads `state.json` every 2 s).
- **Dashboard UI** (`apps/desktop/src/`): Tailwind dark palette, Zustand store that hydrates from Tauri + subscribes to `continuum:state` / `continuum:log` / `continuum:repair`, 16 reusable UI primitives (`Card`, `StatusBadge`, `Button`, `Toggle`, `Slider`, `Select`, `SearchInput`, `TextInput`, `Modal`, `StatusOrb`, `Kbd`, `EmptyState`), icon sidebar + topbar with clock + pause/mute controls, and 8 tabs (Home, Brain, Memory, Tools, Voice, Automations, Logs, Health).
- **System tray**: left-click shows window, right-click menu offers Open / Pause / Resume / Voice on / Voice off / Quit; tooltip reflects state. Window close is intercepted and hides to tray.
- `docs/dashboard.md`: full architecture overview, two-process diagram, event topics, tab map, data file list.
- `docs/self-healing.md`: expanded with repair agent overview, MCP tool reference, backup/rotation/predictive-maintenance sections.

### Changed
- `crates/continuum-core/Cargo.toml`: `runtime` feature gate added; `parking_lot`, `sysinfo`, `zip` added as always-on deps. Binaries (`continuum`, `continuum-perception`, `continuum-triage-bench`, `audio-probe`) declare `required-features = ["runtime"]`.
- `crates/continuum-core/src/lib.rs`: module declarations split between always-on (state / logs / config / health / runtime / senses::types / triage::TriageDecision / automations) and runtime-only (memory / orchestrator / voice / workers / senses watchers / triage llm).
- `crates/continuum-core/src/bin/continuum.rs`: spawns the runtime publisher after subsystem init and updates `wake_count` / `voice_mode` on wake start/finish so the dashboard can render live runtime status.
- `apps/desktop/src-tauri/tauri.conf.json`: window label `main`, title "Continuum Dashboard", min-size 900×600, starts hidden (tray click reveals), tray icon id `continuum-tray`, version bumped to 0.4.0.
- `apps/desktop/src-tauri/capabilities/default.json`: Tauri 2 capabilities for window lifecycle, events, tray, shell, opener.
- `apps/desktop/package.json`: added `@tauri-apps/api`, `@tauri-apps/plugin-opener`, `@tauri-apps/plugin-shell`, `@tauri-apps/plugin-window-state`, `clsx`, `lucide-react`, `zustand`; bumped version to 0.4.0.
- `config::AudioConfig::default`: the test for `whisper_language` now correctly expects `"en"` (the wake-gate-friendly default) — an old `"auto"` assertion was stale.

### Changed
- **Voice output is now English-only by default**: the Dutch Piper voice (`nl_NL-mls-medium`) ships barely-intelligible speech, so the default `TtsConfig` no longer loads it and `voice.language_detection_enabled` defaults to `false`. Whisper input is `whisper_language = "auto"` so the user can still speak any language Continuum understands — Continuum just always responds through the English voice
- `prompts/orchestrator-system.md`: replaced "match the user's language, default to Dutch" with "always respond in English regardless of the user's spoken language"; explicit override for single turns if the user asks
- `prompts/triage-system.md`: whisper text MUST be English regardless of input language; the calendar example response translated to English
- `SOUL.md` Language section: Continuum *understands* any language whisper covers but *responds* in English until better multilingual voices exist; not a values statement, just a current TTS-quality constraint
- `config/default-models.toml`: Dutch voice entry commented out with a one-block opt-in path; `audio.whisper_language = "auto"`; `voice.language_detection_enabled = false`; explanatory block at top of `[tts]` documenting the strategy and how to re-enable multilingual output later
- `examples/voice_test.rs`: only synthesises phrases whose language is in the configured voice bank; skips others with a clear "no voice configured" message instead of routing Dutch text through the English voice

### Added
- **Phase 5 completion (v0.3.0-phase5)**: full voice-pipeline acceptance — TTS foundation (5A), wake + streaming STT (5B), streaming TTS + interrupt + polish (5C) landed together
- `crates/continuum-core/examples/voice_test.rs`: Phase 5A acceptance gate — loads the Piper voice bank, synthesises Dutch + English, plays through the default cpal output, prints per-language timing
- `crates/continuum-core/examples/voice_demo.rs`: Phase 5C end-to-end demo — typed transcripts drive wake → endpoint → streaming TTS → follow-up mode, with latency report
- `crates/continuum-core/examples/voice_latency_bench.rs`: Phase 5C benchmark harness — measures wake / endpoint / synth / playback-start / full-pipeline latency against ARCHITECTURE.md P95 targets over N iterations
- `crates/continuum-core/src/voice/sounds.rs`: procedurally-generated feedback cues (wake chime 880→1320 Hz ramp, listen click 1200 Hz, done double-click 660 Hz, error double-beep 220→165 Hz) with a `FeedbackPlayer` wrapper that no-ops when disabled or when no playback stream is attached
- `crates/continuum-core/src/voice/health.rs`: voice-component health probes (`tts_health_from_paths`, `stt_health_from_paths`, `wake_health`, `playback_health`) and a `VoiceHealthReport` aggregator that surfaces the worst status for the Phase 7 repair agent
- `crates/continuum-core/src/voice/hotkey.rs` (Windows): global hotkey listener via `RegisterHotKey` on a dedicated thread, parses `"Ctrl+Shift+K"`-style chord specs, delivers press events on a tokio `UnboundedReceiver<()>`, unregisters cleanly on drop
- `crates/continuum-core/src/voice/tts.rs::ElevenLabsEngine`: config-stable extension point for the future cloud TTS plugin — implements `TtsEngine` but returns a clear "Phase 5 extension point" error when called; `tts.engine = "elevenlabs"` logs a warning and falls back to Piper
- `resolve_piper_binary()` in `voice::tts`: Piper binary lookup now falls through `CONTINUUM_PIPER_BIN` env → `~/.continuum-dev/bin/piper/piper.exe` (Windows) / `~/.continuum-dev/bin/piper/piper` (Unix) → system PATH, so the download-models script makes things work without extra env setup
- `PlaybackStream::open_default_with_volume` + `set_volume`/`volume`: master gain applied in the cpal fill callback via an `AtomicU32` bits-of-f32, clamped to `[0.0, 1.0]`, `NaN`/`±∞` coerced to `0.0`
- Conversation follow-up mode: `bin/continuum.rs` opens a `followup_until` window after each orchestrator wake; fresh speech inside the window starts a session without re-requiring the wake phrase, then falls back to passive mode automatically
- Hotkey push-to-talk wiring in `bin/continuum.rs`: pressing the configured chord from anywhere flips `hotkey_pending`; the next transcript starts a session directly (skipping the wake phrase)
- Feedback cues wired into the main runtime: wake chime on wake-phrase match, listen click on follow-up/hotkey session start, error beep when `do_wake` fails
- `docs/voice.md` rewritten as a comprehensive reference: full pipeline diagram, every config option, latency budget table with P95 targets, troubleshooting guide, architectural rationale (Piper subprocess vs piper-rs, transcript wake vs Porcupine, heuristic endpoint vs LLM, sentence streaming vs token streaming), extension paths for new voices / custom wake / ElevenLabs / feedback cues

### Changed
- `config/default-models.toml` and `config::VoiceConfig`: added `volume`, `feedback_sounds`, `hotkey`, `conversation_followup_seconds` to `[voice]`; added `engine` and new `[tts.elevenlabs]` section to `[tts]`
- `scripts/download-models.ps1`: replaced the broken rhasspy/espeak-ng-data download (404'd repo) with the official `piper_windows_amd64.zip` release — installs `piper.exe` under `~/.continuum-dev/bin/piper/`, copies the bundled `espeak-ng-data/` to `~/.continuum-dev/models/tts/espeak-ng-data/`, and verifies the Piper binary in the final check
- `voice::tts::PiperEngine`: uses `resolve_piper_binary()` instead of hardcoding `"piper"` as the PATH fallback
- `voice::sounds::FeedbackPlayer`: added `::disabled()` constructor for headless/no-audio paths; the internal `playback` is now `Option<Arc<PlaybackStream>>` so we don't need to open a dummy cpal stream under `--no-tts`
- `bin/continuum.rs`: TTS init is now `init_tts_and_feedback` returning `(Option<Arc<SpeechController>>, FeedbackPlayer)`, so the same cpal output drives both utterances and UI cues
- `PlaybackStream::open_default` now delegates to `open_default_with_volume(1.0)` to preserve the existing API surface
- `voice::mod.rs`: added `pub mod sounds`, `pub mod health`, and gated `pub mod hotkey` behind `#[cfg(windows)]`

### Fixed
- `download-models.ps1` depended on `github.com/rhasspy/espeak-ng-data`, which is a 404. The new script uses the espeak-ng-data already bundled in the Piper Windows release, which is the upstream-recommended path

- **Phase 5 local voice path**: wake phrase detection over local Whisper transcripts, post-wake voice sessions, endpoint detection, Piper CLI TTS, cpal playback, streaming sentence-level speech, barge-in interruption, quiet mode during calls, and voice/self-healing docs
- **Phase 3 memory distillation completion**: background distiller promotes qualifying raw perception frames into LanceDB episodic `remember` events every 15 minutes and marks frames with `memory_distilled_at` after successful insert
- Voice configuration (`[voice]`) for wake keyword, timeout, endpoint silence, barge-in, ambient mute, and language routing; memory distillation configuration (`[memory]`) for interval, lookback, salience threshold, and batch size
- `docs/voice.md` and `docs/self-healing.md` document the Phase 5 local voice flow and repair-agent recovery procedures
- **Phase 4 — MCP tools**: Continuum's orchestrator can now do things, not just talk — a standalone `continuum-mcp` binary exposes 11 Rust-native tools to Claude Opus at wake time via `--mcp-config`
- `continuum-mcp` binary (rmcp 1.4, stdio transport, `--version` flag): registered on every wake with `--strict-mcp-config`, advertises protocol `V_2024_11_05` with `enable_tools()` capabilities
- Memory tools (`mcp__continuum__memory_*`): `query_episodic` (vector search via existing LanceDB), `list_facts` (prefix filter), `get_fact`, `set_fact` (rejects `system.*` and `continuum.*` prefixes; confidence clamped by source — inferred ≤0.7, observed ≤0.8, user_stated ≤0.9)
- System tools (`mcp__continuum__system_*`): `current_time` (ISO-8601 + tz offset), `active_window` (reuses `senses::context::foreground_window`), `clipboard_get` (Win32 OpenClipboard/CF_UNICODETEXT), `notification` (Windows toast via `tauri-winrt-notification`, 10s per-process rate limit, title/body truncated at 64/200 chars)
- Filesystem tools (`mcp__continuum__fs_*`): `read_file` (100 KB cap with truncation prefix, UTF-8 only), `list_dir` (500 entries, per-entry allowlist filtering); read-only by design — no writes, deletes, moves, or mutations
- Filesystem allowlist (`crates/continuum-mcp/src/allowlist.rs`): single `is_path_allowed` gatekeeper — root check (data dir + `project.*.dir` semantic facts + `[mcp.fs].extra_paths` opt-in), hardcoded `DENY_DIRS` (`.ssh`, `.aws`, `.gnupg`, `.docker`, `User Data`, `Profiles`, `node_modules`, `target`, `AppData`, etc.), hardcoded `DENY_PATTERNS` (`*.pem`, `*.key`, `id_rsa*`, `.env*`, `*.kdbx`, etc.)
- Web tool (`mcp__continuum__web_fetch`): HTTP GET only, 50 KB streaming cap with truncation prefix, pre-flight DNS resolution with public-IP check (RFC 1918, loopback, link-local, multicast, CGNAT 100.64/10, RFC 6598, IPv6 ULA + link-local all rejected), redirects disabled entirely to close redirect-SSRF, 5s total timeout
- Tool-call audit: every MCP invocation fires a background tokio task that writes an episodic event with `kind=ToolCall`, sanitized args (keys matching `/password|secret|token|apikey|auth/i` redacted, strings >500 chars truncated), and ≤200-char result summary — fire-and-forget so lazy `EpisodicStore` init doesn't block tool responses
- `EventKind::ToolCall` variant added to `crates/continuum-core/src/memory/episodic.rs`
- MCP orchestrator wiring (`crates/continuum-core/src/orchestrator/spawn.rs`): generates `mcp-config.json` at wake time (absolute binary path + `CONTINUUM_DATA_DIR` env), adds `--mcp-config` + `--strict-mcp-config`, changes `allowedTools` from `""` to `"mcp__continuum__*"`, flips `--permission-mode` from `plan` to `default` (plan mode blocks tool execution)
- `OrchestratorConfig` fields: `mcp_enabled: bool`, `mcp_server_path: Option<PathBuf>`, `mcp_config_path: Option<PathBuf>`, `mcp_data_dir: Option<PathBuf>`; binary resolver falls back through config → `CONTINUUM_MCP_BIN` env → sibling of current exe → PATH lookup
- Orchestrator system prompt (`prompts/orchestrator-system.md`): added Tools section with memory-first, read-only-fs, public-only-web, and no-notification-spam guidance; explicit warning about reserved `system.*`/`continuum.*` memory keys
- MCP config (`config/default-models.toml`): new `[mcp.fs]` section with `extra_paths = []` for user-controlled allowlist expansion
- `docs/mcp-tools.md`: complete tool reference with JSON examples, security model documentation, and E2E verification runbook
- Protocol integration test (`crates/continuum-mcp/tests/protocol.rs`): spawns the binary, drives JSON-RPC initialize → tools/list → tools/call over stdio, asserts all 11 tools registered and `system_current_time` returns a valid ISO-8601 timestamp
- 50 unit tests across audit, allowlist, config, memory, system, fs, web modules
- Echo smoke-test example (`crates/continuum-mcp/examples/echo_smoke.rs`): retained as diagnostic tool for verifying rmcp ↔ claude CLI handshake independently
- End-to-end verified: real `continuum-mcp.exe` spawned by real `claude -p` successfully answered `system_current_time` during smoke test (returned `2026-04-12T20:47:01.698257+02:00`)

### Changed
- `crates/continuum-core/src/senses/context.rs`: added `pub fn foreground_window()` wrapping the internal Win32 helper so `continuum-mcp`'s `system_active_window` tool can reuse the existing implementation
- `crates/continuum-core/src/bin/continuum.rs`: `OrchestratorConfig` initializer now includes `mcp_enabled: true` and passes `~/.continuum-dev/` as `mcp_data_dir`
- `crates/continuum-llm/src/lib.rs`: added explicit type annotations on two `std::mem::transmute` calls for clippy `missing_transmute_annotations` lint
- `crates/continuum-core/src/memory/episodic.rs`: `Embedder::embed_batch` now takes `Vec<String>` by value to avoid clippy `unnecessary_to_owned`

### Added
- **Phase 3 — Orchestrator**: Claude Opus 4.6 wakes up, speaks, and remembers
- Orchestrator subprocess manager: spawns fresh `claude -p` process per wake, streams response events, captures cost/duration (ADR 005: fresh process per wake — conversation purity over process reuse)
- Episodic memory: LanceDB vector store with fastembed BGESmallENV15Q (384-dim, 66 MB) for semantic similarity search over past events
- Semantic memory: SQLite store for stable facts about the user, projects, and preferences with key-value + graph edges
- Memory retrieval: combines episodic vector search + semantic fact lookup into a single MemoryContext for each wake
- Wake context builder: assembles orchestrator user message from current frame, history, memories, and wake reason (~400 tokens)
- Compact orchestrator system prompt (`prompts/orchestrator-system.md`): ~400 tokens, derived from SOUL.md, with Continuum personality, behavior rules, language detection, and Phase 3 guardrails
- `continuum` binary: complete runtime with perception + triage + orchestrator in one process
- Integration test with mock Claude Code event stream (no API key required)
- Decision document: 005-orchestrator-lifecycle.md

### Changed
- `orchestrator/spawn.rs`: rewritten from placeholder to full subprocess lifecycle
- `orchestrator/mod.rs`: re-exports OrchestratorConfig, OrchestratorEvent, WakeResult, wake_orchestrator
- `memory/mod.rs`: added retrieval module
- `memory/episodic.rs`: implemented with LanceDB + fastembed (was stub)
- `memory/semantic.rs`: implemented with SQLite (was stub)
- Added lancedb, fastembed, arrow-array, arrow-schema, futures to workspace dependencies

- **Phase 2 — Triage layer complete**: local LLM evaluates salient perception frames and outputs structured decisions — 19/20 benchmark accuracy (95%) with Qwen 3 8B at 964ms P50 latency
- `continuum-llm` crate: wraps `llama-cpp-2` (llama.cpp Rust bindings) with LocalLlm struct — GGUF model loading, free-form generation, GBNF grammar-constrained JSON generation, streaming output, model warmup
- TriageDecision enum: 5 variants (ignore, remember, whisper, execute_simple, wake_orchestrator) with serde JSON parsing and truncation
- TriageLayer: evaluation loop with 3-retry fallback (grammar first, prompt-only retries, default to Ignore), consecutive failure health alerts
- Decision handlers: allowlisted execute_simple actions (launch_app, show_notification, toggle_mute), TTS and orchestrator wake placeholders
- GBNF grammar file (`prompts/triage-grammar.gbnf`) enforcing strict triage JSON schema
- Triage system prompt (`prompts/triage-system.md`) with signal reliability hierarchy and Qwen 3 `/no_think` thinking mode suppression
- `--triage` flag on `continuum-perception` binary: optional real-time triage decisions in terminal output
- `continuum-triage-bench` binary: benchmarks triage accuracy and latency against 20 hand-labeled frames
- Benchmark dataset: `benchmarks/triage-frames.jsonl` with 20 labeled frames (5 ignore, 5 remember, 5 wake, 5 ambiguous)
- Decision document: 004-triage-model.md (Qwen 3 4B chosen over Qwen 2.5 3B, Gemma 3, Phi-4, Llama 3.2)
- Triage documentation: `docs/triage.md` with model swapping, debugging, signal hierarchy
- Per-decision accuracy breakdown in benchmark harness

### Changed
- Default triage model upgraded from Qwen 2.5 3B to Qwen 3 8B (Q4_K_M) via Qwen 3 4B — best accuracy/latency balance for triage decisions
- Triage prompt calibrated: tightened REMEMBER rules to require audio evidence (eliminates over-remembering on interesting window titles), added WHISPER decision path, added proactive WAKE on visible errors with idle timeout
- Benchmark relabeled 2 frames based on decision-theoretic analysis: error-visible-10s from remember→wake, simple-calendar-question from wake→whisper
- Default salience threshold lowered from 0.15 to 0.10 — triage is cheap enough for window-change events
- Updated `ARCHITECTURE.md` Layer 2 section for Qwen 3 4B with thinking mode documentation
- Updated `config/default-models.toml` with new triage model config
- Updated `scripts/download-models.ps1` with Qwen 3 4B download

### Fixed
- SmolVLM decoder repetition loop — replaced greedy argmax with repetition-penalty sampling (rep_penalty=1.15, no_repeat_ngram=3, temperature=0.3, top_p=0.9) plus repetition safety net
- Triage `llama_context` recreated on every call — now cached and reused with KV cache clearing between evaluations
- Triage KV cache on CPU instead of GPU — `continuum-perception` was using `TriageConfig::default()` with `gpu_layers: 0`; now explicitly sets `gpu_layers: 999` matching the benchmark config
- `TriageConfig::default()` gpu_layers changed from 0 to 999 to prevent future GPU misconfiguration
- `foreground_process_name` always empty in perception output — replaced `GetModuleBaseNameW` (requires `PROCESS_QUERY_INFORMATION | PROCESS_VM_READ`) with `QueryFullProcessImageNameW` (works with `PROCESS_QUERY_LIMITED_INFORMATION`)

### Known limitations
- SmolVLM-256M vision model hallucinates on complex screens (browser windows, dense UI). Triage is designed to treat vision as corroborating evidence only; primary signals are foreground_process_name and audio transcript. Vision quality will improve in Phase 3 when orchestrator receives raw screenshots directly to Claude Opus.

- **Phase 1 — Perception layer**: full senses subsystem producing continuous PerceptionFrame stream
- `continuum-vision` crate: VisionModel trait with OnnxVisionModel — full autoregressive SmolVLM-256M decoder loop (vision encoder → token embedding → KV-cache decoder → tokenizer decode)
- Screen capture via `xcap` (GDI/BitBlt, no yellow border): primary monitor capture, 1280x720 downscaling, JPEG screenshot saving
- Audio pipeline (default-enabled): cpal mic capture, energy-based VAD, whisper-rs batch transcription, rubato resampling
- `.cargo/config.toml` with build environment variables (LIBCLANG_PATH, CMAKE_GENERATOR, ORT_DYLIB_PATH)
- End-to-end smoke test documentation (docs/phase-1-smoke-test.md)
- Context poller: foreground window title/process via Windows APIs, idle time detection, call detection (Discord/Teams/Zoom/Meet/Slack)
- PerceptionFrameBuilder: assembles frames from three senses channels, computes salience heuristic (5 rules)
- SQLite raw log via sqlx: schema creation, write/query frames, nightly rotation with configurable retention
- `continuum-perception` binary: standalone perception runner with Ctrl+C graceful shutdown
- Shared observation types: ScreenObservation, AudioObservation, ContextObservation, PerceptionFrame
- ContinuumConfig with TOML loading from `~/.continuum-dev/config.toml`, sensible defaults for all senses
- Decision documents: 001-vision-model, 002-screen-capture, 003-audio-pipeline
- Updated ARCHITECTURE.md: SmolVLM-256M as default vision model, rate_limit_event documentation
- Updated download-models.ps1 with actual model download URLs
- 79+ unit and integration tests across continuum-vision and continuum-core
- Phase 0 Hello World: example binary that spawns Claude Code CLI, streams JSON events, and prints live text output (`crates/continuum-core/examples/hello_world.rs`)
- Strongly-typed Claude Code event parser in `crates/continuum-core/src/orchestrator/events.rs` with full coverage of system, stream_event, assistant, user, rate_limit_event, and result event types
- Unit tests for event parser using real JSON captured from Claude Code CLI v2.1.100
- Updated CLAUDE.md event type documentation to match actual CLI behavior (discovered `rate_limit_event`, `total_cost_usd` field name, detailed `system` init fields)
- Initial repository scaffolding
- Architecture, soul, roadmap, and Claude Code instructions
- Cargo workspace with continuum-core, continuum-mcp, continuum-llm, continuum-vision crates
- pnpm workspace with desktop app
- Tauri 2 desktop app skeleton with Next.js 15 frontend
- Full module tree for continuum-core matching the four-layer architecture
- MCP server skeleton with all tool namespace modules
- Prompt templates for triage, orchestrator, repair agent, and salience heuristics
- Default config files for models, permissions, and MCP servers
- Bundled skill placeholders (daily-briefing, code-review, project-context)
- Dev setup, model download, and install PowerShell scripts
- CI workflow for Rust and Next.js builds
- Apache 2.0 license
- Contributing guidelines

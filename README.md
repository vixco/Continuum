<div align="center">

# K**AI**ro

### The AI that knows when to act.

**Your second mind — always on, always local, always yours.**

Kairo is an open-source ambient AI assistant for Windows. It sees what you see, hears what you hear, remembers what matters, and acts only when the moment is right. Powered by Claude Code as its orchestrator, driven by small local models for its senses, and built to be legally distributable with your own Claude subscription.

[Docs](https://vixco.github.io/kairo-ai) · [Roadmap](./ROADMAP.md) · [Architecture](./ARCHITECTURE.md) · [Known issues](./KNOWN_ISSUES.md)

![status](https://img.shields.io/badge/status-alpha-yellow?style=for-the-badge)
![version](https://img.shields.io/badge/version-0.1.0--alpha.1-blue?style=for-the-badge)
![license](https://img.shields.io/badge/license-Apache%202.0-blue?style=for-the-badge)
![platform](https://img.shields.io/badge/platform-Windows%2010%2F11-0078D4?style=for-the-badge&logo=windows)
![powered by](https://img.shields.io/badge/powered%20by-Claude%20Code-D97757?style=for-the-badge)

</div>

---

## What is Kairo?

Kairo is not a chatbot. Kairo is a **cognitive presence** that lives on your PC.

Most AI assistants wait for you to open an app, type a question, and read an answer. Kairo is the opposite: it runs continuously in the background, observing your screen, listening in the room, and tracking what you're working on. A small local language model triages everything it perceives and decides — usually 20 to 50 times per day — whether the moment warrants waking up Claude Opus 4.6 to actually think, plan, and act on your behalf.

When that moment comes, Claude Opus 4.6 (the orchestrator) delegates real work to Claude Code workers running in headless mode. They can write code, edit files, send emails, summarize documents, reorganize folders, respond to messages, or anything else you've given Kairo tools for. You hear about it in a warm, human voice synthesized locally. You can interrupt it mid-sentence. It remembers everything.

It is named **Kairo** after the Greek *kairos* — the decisive moment when action must be taken. That is what separates this project from every other AI wrapper: **it knows when to act, and when to stay silent.**

## Screenshots

> The alpha ships with these dashboard views. Screenshots will be added as real deployments land.

| Home — live perception + workers | Brain — four-layer model pipeline |
|---|---|
| *See the current foreground app, the most recent triage decision, and any running Claude Code workers at a glance.* | *Swap vision, triage, and orchestrator models from a single pane. Each gets a "Test" button.* |

| Memory — episodic + semantic CRUD | Voice — waveform, voice picker, latency |
|---|---|
| *Search across everything Kairo has seen or remembered. Delete, edit, export, wipe.* | *Pick a Piper voice, preview it, and watch the end-to-end latency meter in real time.* |

| Health — repair agent streaming live | Onboarding — first-run wizard |
|---|---|
| *Click "Fix issues" and watch a Claude Code session diagnose and repair its own installation.* | *Eight steps: Claude Code check → model downloads → voice → permissions → diagnostics → done.* |

## Why Kairo exists

Today's AI tools sit on one of two extremes:

- **Chatbots** (ChatGPT, Claude.ai, Copilot Chat) are reactive. You must remember to ask. They don't know what you're working on. They forget you the moment the tab closes.
- **Chat-gateway agents** (OpenClaw, Open Interpreter, various "AI OS" projects) route messages from WhatsApp or Slack to an agent that uses tools. Useful if you want a remote assistant, but they don't *observe*, don't *remember across months*, and they often sit in a grey zone with OAuth-scraped subscription tokens that violate terms of service.

Kairo is built on a different premise: **the right AI assistant is one that shares your desk, not one that lives in a chat window.** The only way to make that work — cheaply, legally, and well — is a layered cognitive architecture where cheap local models handle 95% of the perception and triage, and Claude Opus 4.6 via the official Claude Code CLI only wakes up when it actually matters.

Kairo is legal because it uses your own Claude Max subscription through the official `claude` CLI that Anthropic ships and supports. You install Claude Code yourself, sign in once, and Kairo spawns it as a subprocess. No token scraping. No reverse engineering. No ToS violations. You can fork it, sell it, ship it, audit it.

Kairo is open source because this kind of tool should not be owned by a single company that can revoke your access or change the rules.

## The four-layer brain

Kairo's architecture is what makes the impossible possible. Each layer runs at a different speed and cost, and together they form something much more capable than any single-model agent.

```
┌────────────────────────────────────────────────────────────┐
│  LAYER 1 — SENSES                    (local, always on)    │
│  Vision (SmolVLM-256M) · Audio (Whisper) · Context (Win)   │
│                          │                                 │
│                          ▼ perception frame                │
├────────────────────────────────────────────────────────────┤
│  LAYER 2 — TRIAGE                    (Qwen 3 8B, local)    │
│  "does this need attention?" (95% benchmark accuracy)      │
│                          │                                 │
│          ┌───────────┬───┴────┬──────────┐                 │
│          ▼           ▼        ▼          ▼                 │
│       ignore      whisper  execute    WAKE UP              │
├────────────────────────────────────────────────────────────┤
│  LAYER 3 — ORCHESTRATOR              (Claude Opus 4.6)     │
│  via `claude -p --output-format stream-json`               │
│  Reads memory · Plans · Delegates · Speaks                 │
│                          │                                 │
│            ┌─────────────┼─────────────┐                   │
│            ▼             ▼             ▼                   │
├────────────────────────────────────────────────────────────┤
│  LAYER 4 — WORKERS              (Claude Opus or Sonnet)    │
│  Headless Claude Code sessions doing real work             │
└────────────────────────────────────────────────────────────┘
```

Layer 1 runs 24/7 and costs nothing. Layer 2 runs on a local 8B LLM you can swap at will. Layer 3 wakes up rarely but thinks deeply. Layer 4 does the actual work. Every layer is configurable, replaceable, and inspectable from the Kairo dashboard.

Read [ARCHITECTURE.md](./ARCHITECTURE.md) for the full breakdown.

## What Kairo can do for you

**While you're coding.** You stare at a TypeScript error for 30 seconds. Kairo notices, reads the error, cross-references your file with the project's memory of how you usually handle this pattern, and softly says: *"The error is because the ref isn't cleaned up before remount. Want me to write the fix?"* You say yes, and a Claude Code worker applies the edit in your editor.

**While you're gaming.** You've been in Counter-Strike for two hours. Kairo knows you have a meeting tomorrow morning and haven't prepared. It sends a polite notification: *"Heads up, your 9 AM prep is still open. I can summarize the material while you play — want me to?"*

**While you sleep.** Before bed you say: *"Check GitHub issues for my project, fix the small bugs, and have a briefing ready for me tomorrow morning."* At 3 AM, Kairo wakes a Claude Code worker, reads the issues, writes commits on an `auto-fixes` branch, and prepares a markdown briefing. You wake up to real work already done.

**When something breaks.** A dependency updates and your TTS stops working. You open the dashboard, click **Fix Issues**, and a dedicated Repair Agent (a Claude Code session with access to Kairo's own installation) reads the logs, diagnoses the crash, reinstalls the broken component, and reports back — live, with streaming output, so you can see exactly what it did.

These are not mockups. They are what a properly implemented four-layer agent architecture can do *today* with models that exist *today*.

## Install (alpha)

> ⚠️ **This is an unsigned alpha.** Windows SmartScreen and your AV will ask you to confirm on first launch. Binaries are published with SHA256 checksums you can verify yourself (see below) — code signing lands in a later milestone.

### Five-minute install (most people)

**Prerequisites**

- Windows 10 (build 1903+) or Windows 11
- [Node.js 20+](https://nodejs.org) (needed for the Claude Code CLI)
- A Claude subscription (Max recommended) via [Claude Code](https://code.claude.com)
- 16 GB RAM recommended (10 GB workable with the Qwen 3 4B triage fallback)
- Optional but recommended: NVIDIA GPU with 6+ GB VRAM for faster triage and vision
- ~10 GB free disk for the model bundle (vision + triage + whisper + Piper + espeak data)

**One-liner install**

```powershell
irm https://raw.githubusercontent.com/vixco/kairo-ai/main/scripts/install.ps1 | iex
```

The installer walks through seven steps and is idempotent — if anything fails you can re-run it safely:

1. Verify Windows version and dependencies (Node, Claude Code CLI)
2. Install the Claude Code CLI if missing (`npm install -g @anthropic-ai/claude-code`)
3. Prompt you to `claude login` if you aren't signed in
4. Download the latest release ZIP from GitHub Releases to `%LOCALAPPDATA%\Kairo`
5. Verify SHA256 against `SHA256SUMS.txt` from the release
6. Create `~/.kairo/` and run `scripts/download-models.ps1` to pre-stage all ML models (no implicit network calls at runtime)
7. Register a Start Menu shortcut and optional auto-start entry

Launch Kairo from the Start Menu, and the dashboard's onboarding wizard finishes the configuration.

### Download + verify manually

If you prefer to download the release artifacts yourself, grab both files from the latest release under <https://github.com/vixco/kairo-ai/releases>:

- `kairo-<version>-windows-x64.zip` — the portable bundle
- `SHA256SUMS.txt` — the checksum file that ships with every tagged release

Then verify:

```powershell
# In the directory where you downloaded both files
Get-FileHash kairo-*.zip -Algorithm SHA256 | Format-List
Get-Content SHA256SUMS.txt
```

The two hashes must match. If they don't, **do not run the binary**; open an issue with the tag name and your OS/Claude-CLI versions.

### Install from source (developers)

```powershell
git clone https://github.com/vixco/kairo-ai.git
cd kairo-ai
.\scripts\install.ps1 -FromSource
```

Source builds need the Rust toolchain, CMake, Ninja, LLVM, and protoc. See [CONTRIBUTING.md](./CONTRIBUTING.md) and `.cargo/config.toml` for the full dev-setup guide; `scripts/dev-setup.ps1` automates the prerequisites.

### Try the Phase 0 smoke test (no install)

```powershell
# Prerequisites: Rust toolchain + Claude Code CLI, both on PATH
cargo run --example hello_world -p kairo-core
```

Spawns Claude Code in headless mode, sends "What is 2+2?", prints the streamed response with cost and timing. This is the fastest way to confirm your Claude CLI is wired up before committing to the full install.

## Core principles

These are non-negotiable:

1. **Local first.** Senses, triage, voice synthesis, memory storage, and wake-word detection all run on your machine. Nothing is sent to the cloud unless the orchestrator (Opus) needs to think about it, and even then only the relevant context — never the raw stream.
2. **Legal and distributable.** Kairo uses Claude Code via its official headless mode. No OAuth scraping, no reverse-engineered subscription tokens. If you have a Claude Max or API plan, Kairo works. If Anthropic changes anything, Kairo adapts.
3. **You own your brain.** Every piece of memory can be viewed, edited, exported, and deleted from the dashboard. Kairo never phones home. There is no telemetry. There is no Kairo account.
4. **Swappable everything.** Vision model, triage LLM, orchestrator, worker model, TTS voice, STT engine, wake word, memory retention — all configurable from the dashboard.
5. **Self-healing.** When Kairo breaks, a built-in Repair Agent can read its own logs and fix itself using Claude Code. No terminal archaeology required.
6. **Human, not helpful.** Kairo has a personality defined in [SOUL.md](./SOUL.md). It speaks like a calm colleague, not a cheerful chatbot. It knows when to stay quiet.

## Tech stack

- **Desktop shell:** [Tauri 2](https://tauri.app) — Rust backend with a Next.js 15 + React 19 + Tailwind frontend
- **Orchestrator:** Claude Opus 4.6 via `claude -p --output-format stream-json --input-format stream-json`
- **Workers:** Claude Opus 4.6 or Sonnet 4.6 (user choice, with an `auto` mode that routes by task type)
- **Triage LLM:** [Qwen 3 8B](https://huggingface.co/Qwen/Qwen3-8B-GGUF) Q4_K_M via [llama.cpp](https://github.com/ggerganov/llama.cpp) (95% benchmark accuracy, 964ms P50)
- **Vision:** [SmolVLM-256M](https://huggingface.co/HuggingFaceTB/SmolVLM-256M-Instruct) on CPU or GPU via ONNX Runtime
- **Speech-to-text:** [whisper.cpp](https://github.com/ggerganov/whisper.cpp) medium (default) or small
- **Text-to-speech:** [Piper](https://github.com/rhasspy/piper) via subprocess (streaming sentence-level)
- **Wake word:** Whisper transcript matching (no Porcupine dependency)
- **Memory:** SQLite for logs + semantic facts, [LanceDB](https://lancedb.com) for episodic vector memory, [fastembed](https://github.com/Anush008/fastembed-rs) for embeddings
- **Tools layer:** Custom MCP server in Rust ([rmcp](https://github.com/modelcontextprotocol/rust-sdk)) exposing 16+ Windows-specific tools to Claude Code
- **Windows integration:** `windows` + `xcap` + `cpal` crates

See [ARCHITECTURE.md](./ARCHITECTURE.md) for the reasoning behind every choice.

## Project status

| Phase | Status | Tag |
|---|---|---|
| Phase 0 — Hello world via Claude Code | done | `v0.0.1-bootstrap` |
| Phase 1 — Perception loop | done | |
| Phase 2 — Triage (Qwen 3 8B, 95% acc) | done | |
| Phase 3 — Orchestrator + memory | done | |
| Phase 4 — MCP tools (11 tools) | done | |
| Phase 5 — Voice (wake, STT, TTS, barge-in) | done | `v0.3.0-phase5` |
| Phase 6 — Dashboard (Tauri 2 + Next.js) | done | `v0.4.0-phase6` |
| Phase 7 — Self-healing (repair agent) | done | `v0.4.0-phase6` |
| Phase 8 — Workers + skills | done | `v0.5.0-phase8` |
| **Phase 9 — Polish + alpha release** | **alpha** | **`v0.1.0-alpha.1`** |

See [ROADMAP.md](./ROADMAP.md) for the full plan and [CHANGELOG.md](./CHANGELOG.md) for per-phase details.

## Known issues

This is the first public alpha. Expect rough edges. See [KNOWN_ISSUES.md](./KNOWN_ISSUES.md) for the current list — the big ones are:

- No macOS or Linux support (Windows-only for now)
- No auto-update mechanism — upgrade by rerunning the installer
- The binaries are not code-signed yet; Windows SmartScreen will warn on first launch
- ElevenLabs TTS is stubbed; only Piper works in this build
- Triage occasionally misclassifies idle screens with visible errors

## Contributing

Kairo is open source and contributions are **actively welcome**. Read [CONTRIBUTING.md](./CONTRIBUTING.md) for the dev setup, PR workflow, and coding standards.

Good places to start:

- [Good first issues](https://github.com/vixco/kairo-ai/labels/good%20first%20issue) — small, well-scoped tasks
- [Bundled skills](./skills/) — add a new skill for a workflow you care about
- [MCP tools](./docs/mcp-tools.md) — add a new capability Kairo can use
- Documentation fixes — if something confused you, a PR fixing it is gold

Any PR that makes Kairo more ambient, more local, or more self-reliant is welcome. Any PR that adds a dependency on a hosted service, a cloud API, or a proprietary runtime needs a very good reason.

## License

Apache License 2.0. See [LICENSE](./LICENSE).

This license was chosen because it includes explicit patent grants, protecting contributors and users as the project grows. Kairo will stay open source forever.

## Credits

Kairo is built on the shoulders of the open source community and the work of Anthropic on Claude and Claude Code. It takes inspiration from OpenClaw's ambitious vision of a personal AI assistant, but reimagines it from the ground up as a desktop-native, legally distributable, self-repairing cognitive system.

Built in Breda, the Netherlands, by Toshan ([@vixco](https://github.com/vixco)) with help from Claude.

---

<div align="center">

**Kairo — the AI that knows when to act.**

*Your second mind. Always on. Always local. Always yours.*

</div>

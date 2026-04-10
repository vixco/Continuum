<div align="center">

# K**AI**ro

### The AI that knows when to act.

**Your second mind — always on, always local, always yours.**

Kairo is an open-source ambient AI assistant for your desktop. It sees what you see, hears what you hear, remembers what matters, and acts only when the moment is right. Powered by Claude Code as its orchestrator, driven by small local models for its senses, and built to be legally distributable with your own subscription.

[Website (soon)](#) · [Docs (soon)](#) · [Discord (soon)](#) · [Roadmap](./ROADMAP.md) · [Architecture](./ARCHITECTURE.md)

![status](https://img.shields.io/badge/status-pre--alpha-orange?style=for-the-badge)
![license](https://img.shields.io/badge/license-Apache%202.0-blue?style=for-the-badge)
![platform](https://img.shields.io/badge/platform-Windows-0078D4?style=for-the-badge&logo=windows)
![powered by](https://img.shields.io/badge/powered%20by-Claude%20Code-D97757?style=for-the-badge)

</div>

---

## What is Kairo?

Kairo is not a chatbot. Kairo is a **cognitive presence** that lives on your PC.

Most AI assistants wait for you to open an app, type a question, and read an answer. Kairo is the opposite: it runs continuously in the background, observing your screen, listening in the room, and tracking what you're working on. A small local language model triages everything it perceives and decides — usually 20 to 50 times per day — whether the moment warrants waking up Claude Opus 4.6 to actually think, plan, and act on your behalf.

When that moment comes, Claude Opus 4.6 (the orchestrator) delegates real work to Claude Code workers running in headless mode. They can write code, edit files, send emails, summarize documents, reorganize folders, respond to Discord messages, or anything else you've given Kairo tools for. You hear about it in a warm, human voice synthesized locally. You can interrupt it mid-sentence. It remembers everything.

It is named **Kairo** after the Greek *kairos* — the decisive moment when action must be taken. That is exactly what separates this project from every other AI wrapper: **it knows when to act, and when to stay silent.**

## Why Kairo exists

Today's AI tools all sit on one of two extremes:

- **Chatbots** (ChatGPT, Claude.ai, Copilot Chat) are reactive. You must remember to ask. They don't know what you're working on. They forget you the moment the tab closes.
- **Chat-gateway agents** (OpenClaw, Open Interpreter, various "AI OS" projects) route messages from WhatsApp or Slack to an agent that uses tools. Useful if you want a remote assistant, but they don't *observe*, don't *remember across months*, and they often sit in a grey zone with OAuth-scraped subscription tokens that violate terms of service.

Kairo is built on a different premise: **the right AI assistant is one that shares your desk, not one that lives in a chat window.** And the only way to make that work — cheaply, legally, and well — is a layered cognitive architecture where cheap local models handle 95% of the perception and triage, and Claude Opus 4.6 via the official Claude Code CLI only wakes up when it actually matters.

Kairo is legal because it uses your own Claude Max subscription through the official `claude` CLI that Anthropic ships and supports. You install Claude Code yourself, sign in once, and Kairo spawns it as a subprocess. No token scraping. No reverse engineering. No ToS violations. You can fork it, sell it, ship it, audit it.

Kairo is open source because this kind of tool should not be owned by a single company that can revoke your access or change the rules.

## The four-layer brain

Kairo's architecture is what makes the impossible possible. Each layer runs at a different speed and cost, and together they form something much more capable than any single-model agent.

```
┌────────────────────────────────────────────────────────────┐
│  LAYER 1 — SENSES                    (local, always on)    │
│  Vision (Moondream) · Audio (Whisper) · Context (Win API)  │
│                          │                                 │
│                          ▼ perception frame                │
├────────────────────────────────────────────────────────────┤
│  LAYER 2 — TRIAGE                    (local 3-4B LLM)      │
│  Qwen / Gemma / Phi — "does this need attention?"          │
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

Layer 1 runs 24/7 and costs nothing. Layer 2 runs on a tiny local LLM you can swap at will. Layer 3 wakes up rarely but thinks deeply. Layer 4 does the actual work. Every layer is configurable, replaceable, and inspectable from the Kairo dashboard.

Read [ARCHITECTURE.md](./ARCHITECTURE.md) for the full breakdown of how each layer works, how they communicate, how memory is stored, and how self-healing repairs broken components automatically.

## What Kairo can do for you

A few concrete scenarios that are not science fiction in this design:

**While you're coding.** You stare at a TypeScript error for 30 seconds. Kairo notices, reads the error, cross-references your file with the project's memory of how you usually handle this pattern, and softly says: *"The error is because the ref isn't cleaned up before remount. Want me to write the fix?"* You say yes, and a Claude Code worker applies the edit in your editor.

**While you're gaming.** You've been in Counter-Strike for two hours. Kairo knows you have a meeting tomorrow morning and haven't prepared. It sends a polite notification: *"Heads up, your 9 AM prep is still open. I can summarize the material while you play — want me to?"* No guilt-tripping, just a gentle hand on the shoulder.

**While you sleep.** Before bed you say: *"Check GitHub issues for my project, fix the small bugs, and have a briefing ready for me tomorrow morning."* At 3 AM, Kairo wakes a Claude Code worker, reads the issues, writes commits on an `auto-fixes` branch, and prepares a markdown briefing. You wake up to real work already done.

**When you leave the house.** You step into your car. Your phone connects to the desktop via Tailscale. You say *"status"* and Kairo answers in your earpiece: *"Everything's stable. One client emailed this morning — I drafted a reply in your drafts folder. Your flight lesson prep is done."*

**When something breaks.** A dependency updates and your TTS stops working. You open the dashboard, click **Fix Issues**, and a dedicated Repair Agent (a Claude Code session with access to Kairo's own installation) reads the logs, diagnoses the crash, reinstalls the broken component, and reports back — live, with streaming output, so you can see exactly what it did.

These are not mockups. They are what a properly implemented four-layer agent architecture can do *today* with models that exist *today*. Kairo is the plumbing that makes it happen.

## Core principles

These are non-negotiable:

1. **Local first.** Senses, triage, voice synthesis, memory storage, and wake-word detection all run on your machine. Nothing is sent to the cloud unless the orchestrator (Opus) needs to think about it, and even then only the relevant context — never the raw stream.
2. **Legal and distributable.** Kairo uses Claude Code via its official headless mode. No OAuth scraping, no reverse-engineered subscription tokens. If you have a Claude Max or API plan, Kairo works. If Anthropic changes anything, Kairo adapts.
3. **You own your brain.** Every piece of memory can be viewed, edited, exported, and deleted from the dashboard. Kairo never phones home. There is no telemetry. There is no Kairo account.
4. **Swappable everything.** Vision model, triage LLM, orchestrator, worker model, TTS voice, STT engine, wake word, memory retention — all configurable from the dashboard. Kairo is a framework, not a lock-in.
5. **Self-healing.** When Kairo breaks (and it will, because this is pre-alpha software), a built-in Repair Agent can read its own logs and fix itself using Claude Code. No terminal archaeology required.
6. **Human, not helpful.** Kairo has a personality defined in [SOUL.md](./SOUL.md). It speaks like a calm colleague, not a cheerful chatbot. It knows when to stay quiet.

## Try it now (Phase 0)

You can run the Phase 0 "hello world" example to verify Claude Code integration works on your machine:

```powershell
# Prerequisites: Rust toolchain and Claude Code CLI
npm install -g @anthropic-ai/claude-code
claude login

# Run the hello world example
cargo run --example hello_world -p kairo-core
```

This spawns Claude Code in headless mode, sends "What is 2+2?", and prints the streamed response in real time with cost and timing info at the end.

## Install (not yet — pre-alpha)

Kairo is in pre-alpha. There is no installer. The repo currently contains the architecture, the roadmap, and the scaffolding that Claude Code will use to build the first working version. If you want to follow along or contribute, star the repo and check back soon.

Planned first-release installation:

```powershell
# Install Claude Code first (required)
npm install -g @anthropic-ai/claude-code
claude login

# Install Kairo
winget install Kairo.Kairo
# or download from GitHub releases
```

The installer will check that `claude` is on your PATH, set up the local model runtime, pull default models, and register the system tray service.

## Tech stack

- **Desktop shell:** [Tauri 2](https://tauri.app) — Rust backend with a Next.js + React + Tailwind frontend
- **Orchestrator:** Claude Opus 4.6 via `claude -p --output-format stream-json --input-format stream-json`
- **Workers:** Claude Opus 4.6 or Claude Sonnet 4.6 (user choice) via headless Claude Code sessions
- **Local LLMs:** [llama.cpp](https://github.com/ggerganov/llama.cpp) via Rust bindings, configurable with any GGUF model
- **Vision:** [Moondream 2](https://moondream.ai) on CPU or GPU via ONNX Runtime
- **Speech-to-text:** [whisper.cpp](https://github.com/ggerganov/whisper.cpp) with `base` or `small` models
- **Wake word:** [Porcupine](https://picovoice.ai/platform/porcupine/) (free tier)
- **Text-to-speech:** [Piper](https://github.com/rhasspy/piper) by default, with optional ElevenLabs streaming
- **Memory:** SQLite for logs, [LanceDB](https://lancedb.com) for vector episodic memory, [fastembed](https://github.com/Anush008/fastembed-rs) for local embeddings
- **Tools layer:** Custom MCP server in Rust using the [rmcp](https://github.com/modelcontextprotocol/rust-sdk) crate, exposing Windows-specific capabilities to Claude Code
- **Windows integration:** The `windows` and `windows-rs` crates for UI Automation, screen capture, and system APIs

See [ARCHITECTURE.md](./ARCHITECTURE.md) for the reasoning behind every choice.

## Project status

| Phase | Status |
|---|---|
| Architecture + docs | ✅ done |
| Project scaffolding | ✅ done |
| Phase 0 — hello world via Claude Code | ✅ done |
| Phase 1 — perception loop | ⏳ planned |
| Phase 2 — orchestrator integration | ⏳ planned |
| Phase 3 — MCP tool suite | ⏳ planned |
| Phase 4 — voice pipeline | ⏳ planned |
| Phase 5 — dashboard + self-healing | ⏳ planned |

See [ROADMAP.md](./ROADMAP.md) for the full plan.

## Contributing

Kairo is open source and contributions are welcome once the initial scaffolding lands. Until then, the best way to help is to **read the architecture**, **try to break it**, and **open issues** with concerns, ideas, or things you think the design misses.

When code contributions open up, we will follow a simple rule: **any PR that makes Kairo more ambient, more local, or more self-reliant is welcome. Any PR that adds a dependency on a hosted service, a cloud API, or a proprietary runtime needs a very good reason.**

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
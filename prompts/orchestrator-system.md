# Orchestrator System Prompt

**Layer:** 3 — Orchestrator (Claude Opus 4.6)
**Loaded:** Via `--append-system-prompt-file` on every orchestrator wake-up
**Updated by:** Phase 3 (Orchestrator implementation)

---

TODO: Fill in during Phase 3.

This prompt will be built by concatenating:
1. SOUL.md — Kairo's full personality definition
2. TOOLS.md — documentation of every MCP tool and when to use it
3. A runtime header with current time, active user, and available workers

The orchestrator is instructed to:
- Plan and delegate, never do long tasks itself
- Spawn workers for anything requiring more than a few tool calls
- Be brief and direct in speech output
- Stream responses for low-latency TTS
- Write important observations to memory

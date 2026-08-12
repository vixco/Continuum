# Agent OS provenance note

Date: 2026-08-12

The Agent OS implementation added in this change is original Continuum code.
No source code, assets, prompts, tests, or generated files were copied or adapted
from OpenClaw, Hermes Agent, OpenAI Codex, OpenHuman, Browser Use, or their
forks.

Those repositories were used for architecture research only. Their high-level
patterns and repository-level licenses are recorded in
[`research/AGENT_OS_LANDSCAPE.md`](./research/AGENT_OS_LANDSCAPE.md).

The Composio integration is an independent client of the public v3 Tool Router
HTTP API. It does not vendor a Composio SDK. Service terms, user account terms,
OAuth scopes, retention, and upstream app terms remain separate from
Continuum's Apache-2.0 license.

This change adds no new Cargo or npm dependency. It uses dependencies already
locked in the repository, including Tokio, reqwest, serde, rmcp, chrono, uuid,
url, and Windows PowerShell/.NET APIs available on the target operating system.

Before release, regenerate the repository SBOM and license reports, inspect the
built binary, and validate that the installer contains the root `LICENSE` and
`NOTICE` materials required by the existing release checklist.

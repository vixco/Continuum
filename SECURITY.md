# Security policy

Kairo runs with deep access to a user's machine: microphone, screen, clipboard,
and an `mcp__kairo__*` tool surface that Claude Opus can invoke during wakes.
Security bugs in this project can be serious, so please help us fix them
responsibly.

## Supported versions

| Version     | Supported      |
| ----------- | -------------- |
| `0.1.x`     | :white_check_mark: (current alpha) |
| `< 0.1.0`   | :x: (pre-release) |

We will produce fixes for the latest alpha / beta / stable release only. Older
tags are archived as-is.

## Reporting a vulnerability

**Please do not open a public GitHub issue for security bugs.**

Use one of the following private channels instead:

1. **GitHub private advisory** (preferred): go to
   <https://github.com/vixco/kairo-ai/security/advisories/new> and file a
   draft. This keeps the report private while we coordinate a fix.
2. **Email**: send to `t.soekar@tovix.nl` with subject
   `[kairo-security] <short description>`. If you want end-to-end encryption,
   request a PGP key in the first message and we'll reply with one.

Include in the report:

- The affected component (Senses, Triage, Orchestrator, Workers, MCP tool X, …)
- Kairo version, Claude Code CLI version, OS version
- A proof-of-concept or the shortest reproduction you have
- The impact you see — information disclosure, privilege escalation, remote
  code execution, bypass of the MCP permission tier, etc.
- Any proposed mitigation

## Response timeline

| Stage                               | Target          |
| ----------------------------------- | --------------- |
| Acknowledgement of the report       | 3 business days |
| Initial severity assessment         | 7 days          |
| Fix or mitigation available         | 30 days (critical/high) / 90 days (medium/low) |
| Public disclosure after fix shipped | by default 14 days, negotiable |

Severity follows the CVSS 3.1 base score. We classify anything that bypasses
the non-negotiables in `CLAUDE.md` (e.g. a path that phones home, or lets
the orchestrator call a tool outside the registered MCP namespace) as
**high** at minimum.

## Threat model (non-goals and in-goals)

**In-scope (we want to hear about these):**

- Any path that causes Kairo to send data to a server other than the Anthropic
  API (via Claude Code), the optional ElevenLabs API (if the user has opted
  in), or the HuggingFace model CDN during a pre-staged model download.
- Any way to make `mcp__kairo__fs_*` read paths outside the allowlist.
- Any way to make `mcp__kairo__web_fetch` connect to a private IP (DNS
  rebinding, SSRF, IPv6 equivalents, resolver tricks).
- Any way to escape the Tauri command sandbox (unvalidated input passed to
  shell/git, path traversal through skill names, CSP bypass).
- Any way to cause the repair agent or orchestrator to run code with
  privileges the user did not grant for the session.
- Credentials or secrets written to logs, crash dumps, or episodic memory.

**Out of scope (please do not report these):**

- Findings that require an attacker to already have code execution on the
  user's machine as the same user Kairo runs as.
- Weaknesses in upstream Claude Code, Anthropic's API, Piper, whisper.cpp,
  or other vendored dependencies — report those to the respective projects
  directly; we'll pick up the fix when it ships.
- Social-engineering scenarios that require the user to deliberately
  mis-configure Kairo (e.g. "I added `/` to `[mcp.fs].extra_paths` and now
  the orchestrator can read everything").

## Disclosure

Once a fix is released, we will publish a GitHub security advisory with
the CVE (if assigned), the fixed version, and credit to the reporter (or
anonymised on request). The advisory links to the PR that fixed it.

Thanks for helping keep Kairo safe.

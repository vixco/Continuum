# Continuum Agent OS

Status: **alpha vertical slice**
Target: Windows desktop + provider-neutral MCP clients
Binary: `continuum-agent-os`

Agent OS turns Continuum's context and permission work into an execution plane.
It gives Codex, Claude Code, local models, and future orchestrators one shared
surface for desktop control, connected-app actions, durable plans, independent
user approvals, and evidence.

It is intentionally not described as AGI. The engineering target is more useful:
a persistent assistant that can observe enough state to choose an action, act
through constrained capabilities, verify the result, recover from failure, and
resume work later without pretending that a tool call equals success.

## Architecture

```text
model / orchestrator
        |
        | MCP tools
        v
continuum-agent-os
  +-- PolicyEngine -------- allow / ask / deny
  |      +-- native Windows approval dialog
  +-- ComputerBackend ----- Win32 + Windows UI Automation
  +-- ComposioClient ------ Tool Router sessions + OAuth + app tools
  +-- RunStore ------------ immutable plans + resumable step results
  +-- EvidenceStore ------- append-only, redacted JSONL
        |
        v
verified action response
```

The action server is separate from Continuum's observation/context MCP server.
That separation keeps read access from silently becoming execution authority and
lets the existing user-managed MCP registry expose the same action surface to
every supported agent.

## What is implemented

### Policy-gated action broker

Every capability resolves to `allow`, `ask`, or `deny` before the action body
runs. Missing capabilities default to `ask`. The default profile allows low-risk
observation, asks for mutations and screenshots, and denies destructive Composio
actions.

An agent cannot relax policy by calling the policy tool alone: moving a setting
toward more authority opens a native Windows dialog outside the model context.
Tightening policy is immediate.

### Windows computer use

The backend provides:

- foreground-window, cursor, monitor, and top-level-window observation;
- bounded Windows UI Automation trees;
- semantic element lookup by name, automation ID, control type, and class;
- foreground-window or virtual-desktop screenshots;
- semantic and coordinate clicking, Unicode typing, key sequences, scrolling,
  window focus, URL opening, bounded waits, and wait-for-element polling;
- before/after foreground and accessibility snapshots around mutations.

Semantic targeting is preferred because coordinates drift. Coordinate clicks
remain available for canvas, legacy, remote-desktop, and custom-rendered UI that
is absent from UI Automation.

Typing writes the payload to a unique local temporary file, pastes it through
the clipboard, restores the previous clipboard, and deletes the file. Evidence
stores only the character count, never the text.

### Composio across connected apps

The integration uses Composio's session-based Tool Router REST API rather than
hard-coding app SDKs. It can:

- create a session for a stable Continuum user ID;
- optionally allowlist toolkits;
- search up to seven natural-language use cases and receive tool slugs, schemas,
  connection state, guidance, and pitfalls;
- run direct app tools;
- run all seven official meta-tools for search, schemas, multi-execution,
  connection management, connection waiting, remote workbench, and remote bash;
- reuse a persisted session across agent runs.

The API key is loaded from `COMPOSIO_API_KEY` or, on Windows, from a DPAPI-bound
file created by the installer. It is never accepted as an MCP argument, placed
in the MCP registration, or returned by status tools.

Tool slugs are classified before execution. Read-like verbs map to
`composio.read`, write-like verbs map to `composio.write`, and delete/revoke/
remove/cancel-style verbs map to `composio.destructive`. Multi-execution takes
the highest risk of its child tools. Remote workbench and remote bash are
conservatively destructive.

### Resumable plans

`agent_run_plan` persists an immutable goal and step list. A run records each
step's result and evidence ID after execution. Resuming the same `run_id` skips
successful steps and retries the first unresolved step. `continue_on_error`
allows explicitly independent steps to continue.

A native approval can cover the exact serialized plan once. That plan grant can
satisfy `ask` steps without a dialog storm, while any capability currently set
to `deny` still blocks. Setting `agent.plan` to `allow` does not automatically
bypass per-step `ask` rules.

### Evidence

`agent-os/evidence/evidence.jsonl` is append-only and rotates at 20 MiB. Events
include timestamp, run ID, tool, capability, risk, authorization source,
outcome, duration, redacted input, bounded result summary, and error.

Secret-looking keys are recursively redacted in both inputs and results. Typed
text is replaced by a character count, including nested `computer_type` plan
steps. Large arrays, objects, and strings are bounded before logging.
The tool response includes the evidence ID; if evidence persistence fails after
an action, the action result carries an explicit warning instead of fabricating
an ID.

## Install on Windows

From the repository root:

```powershell
.\scripts\install-agent-os.ps1
```

The script:

1. builds `continuum-agent-os` in release mode;
2. copies it to `<data-dir>/bin/continuum-agent-os.exe`;
3. registers `agent-os` in `<data-dir>/mcp-servers/agent-os.json`;
4. optionally prompts for a Composio project API key and stores only its
   DPAPI-encrypted form;
5. writes the stable Composio user ID and optional toolkit allowlist.

Computer use can be installed without Composio:

```powershell
.\scripts\install-agent-os.ps1 -SkipComposio
```

Restrict Composio to selected toolkits:

```powershell
.\scripts\install-agent-os.ps1 `
  -ComposioUserId 'local-toshan' `
  -EnabledToolkits gmail,googlecalendar,linear,github,notion
```

Restart Continuum after installation so the next orchestrator launch reads the
new MCP registration.

## Data layout

```text
<data-dir>/
  bin/continuum-agent-os.exe
  mcp-servers/agent-os.json
  agent-os/
    policy.json
    composio.json
    composio-api-key.dpapi       # Windows user/machine-bound ciphertext
    evidence/evidence.jsonl
    computer/screenshots/*.png
    computer/tmp/                # transient typing payloads
    runs/*.json                  # action arguments; ACL-protected by installer
```

## Plan example

```json
{
  "goal": "Open Linear and create the issue described by the user",
  "run_id": "linear_issue_2026_08_08",
  "verify_each_step": true,
  "steps": [
    {
      "action": "composio_search",
      "arguments": {
        "queries": ["Create a Linear issue in a specific team"]
      },
      "expectation": "A Linear create-issue slug and schema are returned"
    },
    {
      "action": "composio_execute",
      "arguments": {
        "tool_slug": "LINEAR_CREATE_LINEAR_ISSUE",
        "intent": "Create the issue approved by the user",
        "arguments": {
          "team_id": "...",
          "title": "...",
          "description": "..."
        }
      },
      "expectation": "Composio returns the created issue identifier"
    }
  ]
}
```

Use `dry_run: true` first when a plan contains unfamiliar tools. Search results
should supply the current slug and schema; the example slug is illustrative and
must not be assumed stable.

## Security boundaries

- Native dialogs are independent from the agent transcript and default to No.
- Headless mode cannot satisfy `ask`; it returns approval-required.
- Composio destructive actions default to deny.
- Composio requests are pinned to `backend.composio.dev`; HTTP is allowed only for loopback development.
- Hosted Composio MCP URLs are not exposed because they would create a parallel path around Continuum policy.
- Composio redirects are disabled, request timeouts are bounded, and API errors
  become tool errors.
- Computer-use PowerShell children are non-interactive, time-bounded, and killed
  when the parent future is dropped.
- URLs are restricted to `http` and `https`.
- Selector sizes, tree sizes, plan lengths, text size, waits, and log output are
  bounded.

No policy layer can make arbitrary desktop automation intrinsically safe. A
user who sets broad capabilities to `allow` is intentionally granting broad
host authority. Remote messages and untrusted page content must still be treated
as prompt-injection input.

## Current limitations

- Native computer control is Windows-only in this slice.
- UI Automation cannot expose every canvas, game, remote desktop, or custom
  control. Screenshot interpretation and coordinate fallback remain necessary.
- Verification compares bounded foreground/accessibility state; some text fields
  do not expose values, so unchanged state is not automatic failure.
- Run files preserve action arguments for resume; the installer ACL-protects the full Agent OS state directory, but operators should still avoid placing reusable secrets in typing steps.
- The desktop UI does not yet have a dedicated visual Agent OS settings panel;
  policy is persisted and queryable through MCP plus the example config.
- Composio requires a project API key and the upstream service for hosted OAuth
  and execution.
- Browser DOM automation is not embedded in this binary. The same broker can
  later host a dedicated CDP/Playwright backend without changing policy, plan,
  or evidence contracts.

## Validation

Relevant checks:

```powershell
cargo fmt --all -- --check
cargo test -p continuum-mcp --lib
cargo build --release -p continuum-mcp --bin continuum-agent-os
.\target\release\continuum-agent-os.exe --version
```

A real Windows smoke test should additionally verify UI Automation against a
standard app, clipboard restoration, screenshot bounds on multiple monitors,
native approval denial/approval, Composio search, OAuth connection, a read tool,
a write tool, a denied destructive tool, plan interruption, and plan resume.

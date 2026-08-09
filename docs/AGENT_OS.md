# Continuum Agent OS

Status: **governed alpha execution plane**
Binary: `continuum-agent-os`
Primary desktop target: Windows; the MCP contract remains provider-neutral.

Agent OS lets Continuum operate the desktop and connected apps without treating
a successful API call as proof that the user's goal was achieved. Its public
boundary is built around four rules:

1. authorize before execution;
2. checkpoint before a mutation can leave the process;
3. verify an observable postcondition before continuing;
4. never replay an unknown mutation automatically.

## Architecture

```text
model / orchestrator
        |
        | MCP
        v
ReliableAgentOsServer
  +-- immutable plan validation
  +-- cross-process run lock
  +-- write-ahead run journal
  +-- typed postcondition verifier
        |
        v
AgentOsServer
  +-- PolicyEngine -------- allow / ask / deny
  +-- ComputerBackend ----- Win32 + UI Automation
  +-- ComposioClient ------ discovery, OAuth and app tools
  +-- EvidenceStore ------- minimized append-only evidence
```

The context MCP and action MCP remain separate processes. Observation authority
does not silently become execution authority.

## Reliable execution

### Stable run identity

Every mutating `agent_run_plan` requires a stable `run_id`. The goal and step
list are immutable for that ID. Reusing an ID with different work is rejected.

### Cross-process lock

Before executing, Continuum creates a `create_new` lock file under:

```text
<data-dir>/agent-os/reliable-runs/.<run-id>.lock
```

A second Agent OS process cannot execute the same run concurrently. A lock left
by a crashed process is deliberately not cleared automatically; the operator
must inspect the real destination before deciding what to do next.

### Write-ahead dispatch

A mutating step becomes `dispatched` in the durable journal **before** its tool
handler runs. This closes the unsafe retry path where a process could crash
between an external side effect and its success checkpoint.

If Continuum cannot prove whether the external action happened, the step and run
become `unknown`. Resume is blocked. The user receives a reconciliation request
instead of a duplicate action.

### Typed postconditions

Free-form expectations are not accepted for mutations. Supported contracts are:

```text
state_changed
json_pointer_exists:/result/response/data/id
json_pointer_equals:/result/response/data/status="completed"
text_contains:created issue
window_title_contains:Linear
element_present:{"name":"Save","control_type":"Button"}
```

`result_ok` is reserved for reads. A mutation must prove an observable result.
Later steps do not run after an unverified or contradicted mutation.

### Direct mutation closure

The public wrapper refuses direct computer and Composio mutations. Calls such as
`computer_click`, `computer_type`, `composio_create_session` and
`composio_execute` must be placed in `agent_run_plan`, where they receive a
stable identity, write-ahead checkpoint and verifier.

Read-only observation tools remain directly callable.

## Permission model

Agent OS capabilities resolve to:

- `allow` — execute automatically;
- `ask` — require an independent native dialog;
- `deny` — refuse the action.

Missing capabilities default to `ask`. Destructive connected-app actions default
to deny. A denied action may not be retried through a lower-level tool.

The ordinary Continuum MCP server has a separate, enforced per-tool broker with
`auto`, `session-approved`, `always-confirm` and `blocked` tiers. The desktop
Tools page edits the effective override file atomically; the next MCP process
loads the change.

## Computer use

The Windows backend supports:

- foreground-window, monitor, cursor and top-level-window observation;
- bounded UI Automation trees;
- semantic lookup by name, automation ID, control type and class;
- semantic or coordinate clicking;
- Unicode typing, key sequences, scrolling and window focus;
- URL opening and bounded wait operations;
- before/after foreground and accessibility snapshots.

Semantic targeting is preferred. Targets must be visible and enabled.
Coordinate input is a fallback for custom-rendered or legacy surfaces and must
still be paired with a typed postcondition.

## Composio

Continuum uses Composio Tool Router for discovery, OAuth connection management
and connected-app execution. The API key is loaded from an environment variable
or a Windows DPAPI-protected file and is never accepted as an MCP argument.

Tool slugs are classified before execution:

- read-like actions may be allowed;
- writes normally ask;
- deletes, revocations, money movement, purchases, account deactivation,
  credential rotation, remote workbench and remote bash are destructive.

Explicit failure envelopes are tool errors. A response with `success: false`,
`successful: false`, a failed status, a non-empty error or a failed child in a
multi-execute response is never reported as accepted.

## Privacy

Reliable journals store minimized summaries rather than full connected-app
payloads. Fields such as message bodies, recipients, accounts, credentials and
free text are reduced to shape metadata.

Evidence additionally minimizes:

- Composio arguments, account identifiers, intents, queries and responses;
- computer typing payloads;
- OAuth and signed URLs;
- third-party error text.

The ordinary MCP server applies a cloud-egress gate after tool execution.
Sensitive vault notes are withheld even when local read permission exists.
Memory, filesystem, clipboard, worker and web audit events are marked
`local_only` and do not re-enter cloud-bound memory retrieval.

## Plan example

```json
{
  "goal": "Create the approved Linear issue",
  "run_id": "linear_issue_2026_08_09_001",
  "verify_each_step": true,
  "steps": [
    {
      "id": "discover",
      "action": "composio_search",
      "arguments": {
        "queries": ["Create a Linear issue in a specific team"]
      },
      "expectation": "result_ok"
    },
    {
      "id": "create",
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
      "expectation": "json_pointer_exists:/result/response/data/id"
    }
  ]
}
```

Tool slugs and schemas are discovered at runtime; the example slug is
illustrative.

## Installation and packaging

Development install on Windows:

```powershell
.\scripts\install-agent-os.ps1
```

Production releases bundle `continuum-agent-os` alongside the runtime and
context MCP in the desktop resources and portable archives. The automatic
release workflow produces:

- a Windows x64 NSIS installer and updater signature;
- an Apple Silicon DMG and signed updater archive;
- an Intel Mac DMG and signed updater archive.

The macOS workflow builds both `app` and `dmg`. Building a DMG alone does not
produce the updater `.app.tar.gz` required by Tauri.

## Operational recovery

When a run is `unknown`:

1. inspect `agent_get_run`;
2. inspect the destination system or UI;
3. determine whether the action happened;
4. create a new corrective plan only after that determination;
5. never delete a crash lock merely to make a retry possible.

The reliable executor intentionally prefers a stopped workflow over a duplicated
email, payment, deletion or account mutation.

## Remaining limitations

- Native computer input remains Windows-first.
- UI Automation cannot expose every canvas, game, remote desktop or custom UI.
- Exactly-once guarantees rely on fail-closed reconciliation for destinations
  that do not expose an idempotency key or authoritative lookup.
- Code signing and notarization are separate from Tauri updater signing and must
  be configured before broad public distribution.
- Real application smoke tests are still required in addition to unit,
  integration and build checks.

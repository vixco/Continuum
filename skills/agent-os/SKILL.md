---
name: agent-os
description: Execute computer and connected-app work through Continuum's policy-gated, durable Agent OS
source: bundled
triggers:
  - do this on my computer
  - take over my computer
  - click this
  - open this app
  - go back to the app I was using
  - return to where I was
  - ga terug naar de app waar ik was
  - fill this in
  - send an email
  - update linear
  - create a github issue
  - use my apps
  - composio
  - computer use
  - automate this
  - handle this for me
---

# Continuum Agent OS

Use the `agent-os` MCP server when a request needs a real action on the desktop
or in a connected SaaS app. The server is an execution plane, not permission to
skip judgment. Every mutation must be authorized, durably checkpointed and
verified against an observable postcondition.

## Control loop

1. **Observe.** Read current state with `computer_observe`,
   `computer_list_windows`, `computer_accessibility` or `composio_search`.
2. **Plan.** Put every mutation in one `agent_run_plan`. Supply a stable
   `run_id`, small steps and a typed `expectation` for each mutation.
3. **Act.** Prefer semantic UI Automation selectors. Use coordinates only when
   the target is absent from accessibility state.
4. **Verify.** Continuum evaluates the typed postcondition before the next step
   may run. A non-error transport response alone is not success.
5. **Recover.** Resume the exact immutable plan only when the journal says it is
   safe. Never replay an `unknown` or unresolved `dispatched` mutation.

Direct mutation tools are intentionally closed at the public Agent OS boundary.
Do not call `computer_click`, `computer_type`, `composio_execute` or similar
write tools outside `agent_run_plan`.

## Reliable plan contract

A mutating plan must include:

- a stable `run_id` containing only letters, numbers, `-` and `_`;
- `verify_each_step: true`;
- an immutable goal and step list;
- a typed expectation for every mutating step;
- `continue_on_error: false` for every mutating step.

Read-only plans may omit `run_id` and default to `result_ok` verification.

### Typed expectations

Use one of these exact forms:

- `state_changed`
- `json_pointer_exists:/result/response/data/id`
- `json_pointer_equals:/result/response/data/status="completed"`
- `text_contains:created issue`
- `window_title_contains:Linear`
- `element_present:{"name":"Save","control_type":"Button"}`

`result_ok` is accepted only for reads. Descriptive prose such as “the form
should be saved” is rejected because it cannot be evaluated deterministically.

Choose a postcondition that proves the requested outcome, not merely that
something changed. For a SaaS write, prefer a returned object ID or status. For
a desktop action, prefer a semantic element or expected foreground-window title.

## Exactly-once behavior

Continuum maintains a write-ahead journal and a cross-process lock per `run_id`.
Before a mutation enters the tool handler, its step becomes `dispatched` on disk.

- Successful, verified steps are skipped on resume.
- A second process cannot execute the same `run_id` concurrently.
- A timeout, transport loss, process crash or failed postcondition after
  dispatch becomes `unknown`.
- `unknown` steps are never retried automatically.
- A lock left by a crashed process is deliberately not deleted automatically.
  The operator must inspect the real destination before clearing or replacing
  the run.

When `agent_get_run` reports `unknown`, stop. Explain the unresolved state and
ask for a deliberate reconciliation plan; do not invent a new run that repeats
the same mutation.

## Permissions

The ordinary Continuum MCP server enforces `auto`, `session-approved`,
`always-confirm` and `blocked` before a tool body runs. Unknown tools default to
confirmation. The desktop Tools page writes the effective overrides atomically
to the local permission policy; a fresh MCP process reads them on the next agent
run.

Agent OS has its own capability policy:

- observation may be allowed automatically;
- screenshots and mutations normally require a native dialog;
- destructive Composio actions default to deny;
- a denied action must never be retried through a lower-level tool.

## Computer use

- When the user refers to a previous app or place, resolve it from
  `context_window`, `context_timeline`, or `context_search` before planning.
  Then call `computer_list_windows`, choose the closest exact live
  process/title match, focus it, and verify the resulting foreground window.
  Never interpret "the app I was in" as merely the most popular installed app.
- A focus switch is only location evidence. Use screen-caption, error, file,
  and Git events to recover what the user was doing there and continue the
  requested task from observed state.
- Prefer accessibility names, automation IDs, control types and class names.
- Targets must be visible and enabled.
- Re-observe when a target moved, is duplicated or is offscreen.
- Focus a field before typing. Typed text is minimized in persistent evidence.
- Use `computer_wait_for_element` instead of blind long sleeps.
- A raw coordinate is not proof of the intended target; pair it with a typed
  postcondition.
- Computer clicks show the configured amber `AI` pointer marker so the user can
  distinguish Agent OS actions from their own pointer. Do not disable or hide
  this indicator inside a plan.

## Composio

1. Search by natural-language use case with `composio_search`.
2. Inspect the current slug, schema, connection state and pitfalls.
3. Use connection-management meta-tools only when OAuth is actually needed.
4. Put the concrete write in `agent_run_plan` with a stable `run_id`.
5. Verify an identifier or status returned by the destination.

Money movement, refunds, purchases, credential rotation, account deactivation,
remote workbench and remote bash are destructive surfaces. Do not enable
workbench offload merely to bypass a missing schema or denied action.

## Privacy and evidence

Do not place passwords, API keys, OAuth URLs, full message bodies or reusable
secrets in goals, step IDs, intents or expectation strings. Continuum minimizes
connected-app arguments, account identifiers, search queries, third-party
responses and errors in persistent evidence and reliable journals.

Sensitive memory results are withheld at the MCP egress boundary. Never try to
reconstruct or expose fields that were marked redacted or withheld.

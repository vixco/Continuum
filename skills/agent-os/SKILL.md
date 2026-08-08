---
name: agent-os
description: Execute real computer and connected-app work through Continuum's policy-gated, verified Agent OS
source: bundled
triggers:
  - do this on my computer
  - take over my computer
  - click this
  - open this app
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

Use the `agent-os` MCP server when a request needs real action on the local
Windows desktop or in a connected SaaS app. Agent OS is an execution plane, not
a reason to skip judgment: every action must be grounded in current state,
policy, and post-action evidence.

## Start with status

Call `agent_status` once near the beginning of an action-oriented session. It
shows whether Windows computer use is supported, whether Composio is configured,
the active permission modes, and recent resumable runs.

Do not ask the user to repeat configuration that `agent_status` or
`composio_status` can reveal.

## The control loop

1. **Observe.** Use `computer_observe`, `computer_list_windows`, or
   `computer_accessibility`. For connected apps, use `composio_search` to
   discover the exact tool and schema.
2. **Plan.** For more than one mutation, build an `agent_run_plan` with small,
   explicit steps and an expectation for each important transition. Use
   `dry_run: true` when the risk or chosen tools are uncertain.
3. **Act.** Prefer semantic `computer_click_element` over coordinate clicks.
   Use coordinate clicks only when UI Automation does not expose the target.
4. **Verify.** Read the returned before/after verification object. A tool call
   returning without transport error is not enough when the expected state is
   absent.
5. **Recover.** Re-observe, adjust the selector or wait for the UI, then resume
   the same `run_id`. Do not restart successful steps.

## Computer-use rules

- Use accessibility names, automation IDs, control types, and class names to
  target elements robustly.
- Take screenshots only when semantic state is insufficient. Screenshots are
  written locally and require their own policy capability.
- Before typing, focus or click the target. `computer_type` uses a temporary
  payload file and clipboard paste, then restores the previous clipboard. Its
  text is redacted from evidence.
- Use `computer_wait_for_element` instead of blind long sleeps where possible.
- Never infer that `state_changed: false` proves typing failed; some controls do
  not expose their value through UI Automation. Inspect the relevant UI or take
  an approved screenshot.
- Do not retry a denied action through a different low-level tool.

## Composio workflow

1. Call `composio_search` with a natural-language use case.
2. Inspect `primary_tool_slugs`, schemas, connection status, pitfalls, and
   recommended steps.
3. When no connection exists, call `composio_execute_meta` with
   `COMPOSIO_MANAGE_CONNECTIONS`. Surface the returned OAuth link to the user,
   then use `COMPOSIO_WAIT_FOR_CONNECTIONS`.
4. Execute the discovered app tool with `composio_execute` and the exact schema.
5. Use `COMPOSIO_MULTI_EXECUTE_TOOL` only when the actions are independent and
   their aggregate risk is acceptable.

Read tools default to allow, writes default to a native approval, and actions
classified as destructive default to deny. Remote workbench and remote bash are
treated as destructive because they can execute arbitrary code or bulk changes.

## Resumable plans

Use only these action names in `agent_run_plan`:

- `computer_observe`, `computer_list_windows`, `computer_accessibility`,
  `computer_screenshot`, `computer_find_element`
- `computer_click`, `computer_click_element`, `computer_type`, `computer_key`,
  `computer_scroll`, `computer_focus_window`, `computer_open_url`
- `computer_wait`, `computer_wait_for_element`
- `composio_create_session`, `composio_search`, `composio_execute`,
  `composio_execute_meta`

The plan is immutable for a given `run_id`. A native approval for the exact plan
can satisfy `ask` steps in that run once, but explicit `deny` rules still win.
Successful steps are skipped when the run is resumed.

## Evidence

Every authorized, denied, successful, and failed action attempts to append an
evidence event. Use `agent_evidence_query` by `run_id` when explaining what was
done. Never expose or reconstruct secret-redacted fields.

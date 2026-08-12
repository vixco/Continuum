# Current desktop tabs

The Continuum desktop shell exposes ten navigation tabs. This file is the
source of truth for what each tab actually does today and which systems it
backs. Update it when a tab changes scope (fixture → live or vice versa) so
docs and code stay in lockstep.

The ten tabs are grouped as:

- **Daily** — Home, Chat, Voice, Memory
- **Configure** — Brain, Tools & Skills, Automations
- **Advanced** — Health, Logs
- **Settings** (system-level, outside the three groups)

| Tab                | Scope          | Backing data / commands                                       | Status                                                                          |
|--------------------|----------------|---------------------------------------------------------------|---------------------------------------------------------------------------------|
| Home               | hybrid         | `continuum:state` events, runtime status                      | Live runtime flags + last-action feed; some hero panels remain fixture-backed.  |
| Chat               | live           | `chat_*` Tauri commands, `~/.continuum-dev/chats/*.json`     | Fully live. Streams deltas over `continuum:chat`.                              |
| Voice              | live           | `continuum:state` + `update_voice_*` + `talk_now`             | Native live voice, real microphone-level feedback, and a persisted auto-start preference. |
| Memory             | live           | markdown vault/index commands + local curator runtime state    | Editable markdown graph plus locally curated candidates; the empty state reports real curator progress/failures. |
| Brain              | live           | `update_brain_config`, agent/model discovery, runtime state    | Real vision/triage/orchestrator/worker configuration with installed-agent availability and per-layer usage. |
| Tools & Skills     | live           | `list_mcp_tools`, permission policy/requests/grants, local MCP server registry, `list_skills`, skill CRUD | Built-in tools and servers are live; per-tool policies are persisted and enforced for Continuum MCP calls, with approval and revocation controls. |
| Automations        | live           | `list_automations` + `create_/update_/delete_/toggle_`        | Fully live.                                                                     |
| Health             | live           | `get_health`, `preview_repair`, `trigger_repair`               | Live probes; guarded repair requires a one-time preview and verified backup.    |
| Logs               | live           | `get_logs` + `continuum:log` stream                           | Fully live; layer filter updated to real emitters in this PR.                  |
| Settings (system)  | live           | `get_config`, `update_*`, model-directory picker/downloader, provider secrets, GitHub CLI connect/disconnect | Fully live; model storage rewrites the configured local-model paths together, AI keys use the OS credential store, and GitHub accepts only keyring-backed official CLI auth. |

The shell-level observation status control is live on every tab. It derives
one explanatory state from the runtime heartbeat, durable pause lease, source
toggles, component health and model readiness. Pause/resume writes the real
durable lease and runtime intent; no frontend-only power state is maintained.

## Disabling / removing a tab

If a tab is being deprecated or its scope shrinks, prefer:

1. Mark its `TabId` entry as deprecated in `apps/desktop/src/components/layout/Shell.tsx`.
2. Update the table above to reflect the new scope.
3. Add a `KNOWN_ISSUES.md` entry if the gap is user-visible.

Never render a configurable Brain control unless its value is persisted and
consumed by the corresponding runtime on the next start. Unsupported vision
backends stay absent instead of appearing as decorative options.

## Cross-tab observation status

- **Data source:** live Zustand state published by the runtime, durable observation-pause status, and runtime heartbeat. No sample state is presented as live.
- **Controls:** supported screen, microphone, file and Git toggles write context intents; pause/resume uses the durable observation lease; screenshot storage uses the native config command.
- **Explanation:** disabled, idle, unavailable and degraded remain distinct, and current activity keeps the session contract's confidence, evidence and privacy uncertainty. Historical context also separates retention off, a live healthy writer, last-known projected data, missing writer state and a degraded writer. When the runtime heartbeat is absent, live source/activity confidence is withdrawn and any retained projection is explicitly labeled last-known.

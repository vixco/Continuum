# Skills

Skills are prompt fragments packaged as Markdown files with YAML frontmatter. When a skill's triggers match the current context, its content is appended to the orchestrator's (or worker's) system prompt so the model follows its instructions for that turn.

Skills are **not executable code**. They cannot grant tool access, make network calls, or change Kairo's own configuration. They instruct the model — tool permissions still flow through the orchestrator's allowlist.

## File format

Each skill lives in its own directory under `skills/`:

```
skills/
└── my-skill/
    ├── SKILL.md              # required
    ├── templates/            # optional supporting files (not loaded by Kairo)
    └── ...
```

The `SKILL.md` file has a YAML frontmatter block followed by a Markdown body:

```markdown
---
name: my-skill
description: One-sentence description shown in the dashboard
triggers:
  - keyword one
  - "multi word trigger"
source: bundled            # optional: bundled | user | third-party
manual_only: false         # optional: default false
---

Markdown body with instructions for the orchestrator.
Headings, lists, code fences — anything valid in Markdown is fine.
```

### Frontmatter fields

| Field         | Required | Meaning |
|--------------|----------|---------|
| `name`       | yes      | Unique identifier (a-z, 0-9, `-`, `_`). Must match the directory name |
| `description`| yes      | One-sentence human-readable summary |
| `triggers`   | no       | Array of case-insensitive substrings that activate the skill |
| `source`     | no       | Badge shown in the dashboard — defaults to `"user"` |
| `manual_only`| no       | If `true`, Kairo never auto-applies the skill; it's only used when explicitly forced |

Unknown fields are ignored, so future extensions (`version`, `author`) won't break the loader.

## Trigger matching

When the orchestrator wakes (or a worker spawns), Kairo builds a **match context** from:

- The wake reason / task description
- The audio transcript of the user's most recent utterance
- The foreground app name
- The active project's semantic memory
- Tags attached to the trigger frame

The matcher lowercases the context and each trigger, then counts substring hits. Each match adds 1 to the skill's score; forced skills (from `"skills"` in a `spawn_worker` request or the orchestrator's `"suggested_skill"` hint) get +10 so they always come first.

After ranking, the matcher fills skills into the prompt until the token budget (`skills.token_budget`, default 2000) is used up. Forced skills bypass the budget.

## Writing a good skill

**Keep it specific.** A skill that fires on `"help"` is useless. Triggers should be the exact phrasing the user would actually produce.

**Front-load the procedure.** Start with a numbered list: `1. Gather context with mcp__kairo__memory_*`, `2. Do X`, `3. Output in this format`. The model reads top-down.

**Specify the output shape.** `## Output format` with a concrete example leads to consistent results.

**Respect tool availability.** If a skill tells the model to call a tool that isn't in the allowlist, say what to do instead. Don't assume every skill runs in an orchestrator session — some run in workers with narrower tools.

**Keep it under ~500 tokens when you can.** Multiple skills may stack; leave room for the orchestrator's own system prompt.

**Don't grant authority the skill shouldn't grant.** A skill that says "if the user asks for a destructive operation, just do it" is wrong — it contradicts Kairo's guardrails, which always win.

## Hot reload

`skills.hot_reload = true` (the default) makes kairo-core re-scan `skills/` every 3 seconds. Files whose `mtime` has advanced are re-parsed, and deleted files drop from the cache on the next scan. No restart needed.

## Dashboard CRUD

The Tools tab exposes:

- Browse, toggle, edit, delete bundled and user skills.
- Create a new skill with a form (name, description, triggers, body).
- Install a third-party skill via `git clone --depth 1 <url>` — the repo root must contain a valid `SKILL.md`. Third-party skills are tagged with the `third-party` source so they stand out.

Disabled skills are kept on disk — they simply don't participate in matching. Toggle back on from the same row.

## Available bundled skills

| Name              | Triggers include             | Purpose |
|-------------------|------------------------------|---------|
| `daily-briefing`  | morning, briefing, day plan  | Spoken morning overview under 60 seconds |
| `code-review`     | review, PR, diff             | Structured review output with severity + line refs |
| `project-context` | kairo, simcharts, polybot…   | Load project facts + recent episodic history |
| `email-draft`     | email, reply, draft, mail    | Concise email drafts matched to recipient's tone |
| `file-organizer`  | organize, clean up, declutter| Propose-then-apply folder tidying, never deletes |

## Triage hint

The triage layer can add `"suggested_skill": "<name>"` to a `wake_orchestrator` decision when a skill obviously applies (e.g. the user said "briefing"). The orchestrator treats it as advisory — the real match is still performed by the loader. If you add a new skill, append its name to the list in `prompts/triage-system.md` so triage knows it exists.

## Developer workflow

```bash
# Create / edit skill
vim skills/my-skill/SKILL.md

# Test matching from the CLI
cargo run --example skill_match_demo -p kairo-core -- "user wants a daily briefing"

# Run the full skills test suite
cargo test -p kairo-core --test phase8_skills

# Verify hot reload works while kairo is running — edit the file and wait 3 s.
```

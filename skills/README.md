# Kairo Skills

Skills are `SKILL.md` files that extend Kairo's knowledge for specific workflows. Each skill lives in its own directory under `skills/` and tells the orchestrator how to handle a particular type of task.

## How skills work

When the orchestrator wakes up, it has access to a list of all installed skills. Each skill's frontmatter (`name` and `description`) is included in the orchestrator's context so it knows what's available. When a task matches a skill's description, the orchestrator loads the full skill content and follows its instructions.

## Skill format

Each skill is a directory containing at minimum a `SKILL.md` file:

```
skills/
└── my-skill/
    ├── SKILL.md          # Required: skill definition with frontmatter
    └── templates/        # Optional: supporting files
        └── ...
```

The `SKILL.md` file must have YAML frontmatter with `name` and `description` fields:

```markdown
---
name: my-skill
description: One-sentence description of when this skill triggers
---

Instructions for the orchestrator when this skill is active...
```

## Bundled skills

- `daily-briefing/` — Generates a morning briefing from memory and calendar
- `code-review/` — Reviews a codebase or pull request
- `project-context/` — Loads project-specific context for development workflows

## Adding skills

Place a new directory under `skills/` with a `SKILL.md` file. Kairo will pick it up on the next restart. Skills can also be installed from the Tools tab in the dashboard.

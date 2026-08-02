# Continuum Skills

Skills are `SKILL.md` files that extend Continuum's knowledge for specific workflows. Each skill lives in its own directory under `skills/` and tells the orchestrator how to handle a particular type of task.

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

- `daily-briefing/` — compact spoken morning briefing, < 60 seconds.
- `code-review/` — structured review of a diff, PR, or file.
- `project-context/` — tailors responses to a known project's stack and
  recent history.
- `email-draft/` — concise reply or new-message drafting with tone
  matching.
- `file-organizer/` — plan-then-apply folder tidying, never deletes.

## Adding skills

Drop a new `<name>/SKILL.md` under `skills/` and Continuum picks it up at
the next reload (hot reload is on by default — see `SkillsConfig`).
Skills can also be created, edited, or disabled from the dashboard
Tools tab.

## Source badge

Each skill's frontmatter may include `source: bundled | user |
third-party`. The dashboard renders the badge so operators know where
a skill came from before trusting it.

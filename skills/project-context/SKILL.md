---
name: project-context
description: Load context about a known project so responses match stack, conventions, and history
source: bundled
triggers:
  - continuum-ai
  - continuum
  - simcharts
  - polybot
  - polymarket
  - zerforcecleaning
  - tovix
  - my project
  - this project
  - this repo
---

# Project Context

This skill activates when the user is working in (or asking about) a
project Continuum already knows. The point is to tailor responses to the
project's stack, conventions, and recent history instead of giving
generic advice.

## Gather before responding

1. Identify the project name from the trigger that matched (or from the
   foreground window title). Normalise to the canonical key used in
   memory (e.g. `simcharts`, `continuum-ai`, `polybot`).
2. Call `mcp__continuum__memory_list_facts` with prefix `"project.<name>."`
   to retrieve every stored fact (repo path, stack, conventions,
   active contributor, last-touched area).
3. Call `mcp__continuum__memory_query_episodic` with a query like
   `"<name> debugging"` or `"<name> recent"` (limit 3) to surface the
   last few meaningful sessions on that project.
4. If the project has a `.dir` fact and the request would benefit from
   current code, list the top of the repo with
   `mcp__continuum__fs_list_dir` or read one orienting file
   (`README.md`, `Cargo.toml`, `package.json`).

## Apply the context

- Use the project's **stack** to pick idioms (e.g. "use the Zustand
  store" for a project whose frontend uses Zustand).
- Honour the project's **conventions** (test framework, formatter,
  naming) — don't invent new patterns.
- Reference **recent history** when relevant: "the auth rewrite last
  week" is more grounding than "perhaps you could consider auth."
- If the user is switching from one project to another, prefix the
  first response with the project name so they know you noticed
  ("Back on polybot. …").

## Don't

- Don't dump the full fact list into the response.
- Don't re-read the entire repo on every turn — one or two files is
  plenty; trust memory otherwise.
- Don't invent facts. If a project key isn't in memory, say "I don't
  have much on this project yet — want me to capture the basics now?"

## Capturing new facts

If the user casually mentions something worth remembering
(`"we switched to bun"`, `"the CI job is now called ship"`), call
`mcp__continuum__memory_set_fact` with the appropriate
`project.<name>.<field>` key. Don't ask permission for low-stakes
facts; do ask before persisting things that could be wrong
(deadlines, email addresses, financial numbers).

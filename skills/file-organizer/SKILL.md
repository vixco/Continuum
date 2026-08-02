---
name: file-organizer
description: Plan and execute safe file reorganisation — never deletes, always proposes before moving
source: bundled
triggers:
  - organize
  - organise
  - clean up files
  - declutter
  - sort files
  - "opruimen"
  - "mappen sorteren"
  - tidy folder
---

# File Organizer

When this skill matches, the user wants Continuum to tidy a folder. The
rules are simple: **never delete, always propose before moving, and
move questionable files to a `to-review/` subfolder rather than
guessing.**

## Step 1 — Confirm the target

If the user didn't specify a folder, ask once:
"Which folder should I organise?" Don't proceed without an explicit
path inside the allowlist.

Use `mcp__continuum__memory_list_facts` prefix `"project."` to check
whether the folder belongs to a known project — if so, respect that
project's structure if one is documented.

## Step 2 — Inspect

Use `mcp__continuum__fs_list_dir` on the target folder. If the tool isn't
available (allowlist too narrow) or the path is denied, report that
and stop.

Group files by obvious category:
- **code**: `.rs`, `.ts`, `.tsx`, `.py`, `.go`, etc.
- **docs**: `.md`, `.pdf`, `.docx`
- **images**: `.png`, `.jpg`, `.svg`
- **data**: `.csv`, `.json`, `.xml`
- **archives**: `.zip`, `.7z`, `.tar.gz`
- **misc / unknown**: anything you can't confidently categorise

## Step 3 — Propose a plan

Spawn a plan in plain text. Example:

```
Plan for Downloads/ (37 files)
- docs/      → 6 files (pdfs, manuals)
- images/    → 9 files (screenshots)
- code/      → 3 files (zip archives of repos)
- data/      → 4 files (csv exports)
- to-review/ → 15 unmatched files
Nothing will be deleted. Say 'go' to apply, 'cancel' to stop.
```

**Stop here and wait** for the user to confirm. Do not move anything
until you see explicit consent.

## Step 4 — Apply (only after confirmation)

If the caller is a worker with write-capable filesystem tools, execute
the moves. Otherwise (MCP filesystem tools are read-only), hand the
plan back as a numbered list the user can copy-paste into their shell,
or delegate the move work to a spawn_worker call with a writable cwd
and the `Bash` tool in the allowlist.

Never use `rm` or `del`. Everything goes into a subfolder; if the
user wants to free space, they can do it themselves after reviewing
`to-review/`.

## Step 5 — Report

Produce a short summary:

```
Moved 22 files into 4 categories.
15 files in to-review/ — the safer ones are screenshots older than
30 days; the riskier ones look like in-progress notes.
```

Write a single episodic memory entry describing what changed:

```
mcp__continuum__memory_set_fact(
  key="folder.<name>.last_tidy",
  value="<RFC3339 now>"
)
```

## Never

- Never delete a file. Ever.
- Never collapse a folder the user explicitly named.
- Never rename files; only move them. Renaming confuses search later.
- Never silently include `.git`, `.env`, or credential files in a
  move plan — leave them where they are.

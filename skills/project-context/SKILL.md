---
name: project-context
description: Loads project-specific context when the user is working in a known project directory
---

# Project Context

TODO: Implement in Phase 8.

This skill triggers when:
- The user opens a project directory that Kairo has context for in semantic memory
- The user switches to an editor window showing files from a known project
- The user asks about a specific project by name

The skill:
- Loads relevant semantic facts about the project (stack, conventions, team)
- Loads recent episodic memories related to the project
- Primes the orchestrator with project-specific knowledge
- Adjusts tool permissions based on per-folder policies

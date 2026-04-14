---
name: code-review
description: Review a diff, PR, or file for bugs, security issues, performance, and style violations
source: bundled
triggers:
  - review
  - code review
  - review this
  - "PR"
  - pull request
  - diff
  - "review de code"
  - look at this PR
---

# Code Review

When this skill is active, the user wants a structured review of code.
Your goal is honest, concise findings — no filler, no praise, no
padding.

## Gather context first

1. Read the target file(s) with `Read` (or `mcp__kairo__fs_read_file`
   when the allowlist permits it). If a diff is available, read both
   sides so you can compare.
2. Check semantic memory with `mcp__kairo__memory_list_facts` prefix
   `"project."` for the current project — get stack, conventions,
   and style preferences so you don't fight the house style.
3. If this review is for a PR (`pr:` or `#123` in the user's message),
   check for any recent episodic memory via
   `mcp__kairo__memory_query_episodic` — the user may have already
   discussed intent.

## What to check

For every change, ask in this order:

1. **Correctness** — does the code do what the surrounding API
   contract says? Any off-by-one, missing null checks at system
   boundaries, or wrong error propagation?
2. **Security** — command injection, SQL injection, path traversal,
   hard-coded secrets, unsafe deserialisation, missing auth checks,
   broken CORS, XSS in templated HTML.
3. **Data integrity** — atomic writes, transaction scope, race
   conditions between async tasks, non-idempotent operations that
   could retry.
4. **Performance** — N+1 queries, allocations in tight loops, blocking
   I/O on the async runtime, unbounded Vec growth.
5. **Maintainability** — non-obvious names, functions doing two
   things, magic numbers, dead code.

Skip cosmetic nits if a formatter/linter is configured — they're
cheaper to fix automatically than to flag manually.

## Output format

Return exactly this Markdown, in order:

```markdown
## Review — <file or PR reference>

**Summary:** <one sentence verdict>

### Issues

- **[severity] <file>:<line>** — <one-line description>
  <why it matters, 1–2 sentences max>
  Fix: <specific change, not advice>

### Suggestions (optional)

- <non-blocking ideas the author can take or leave>
```

Severity is `critical | high | medium | low`. Reserve `critical` for
security holes or correctness bugs that would break production.

## What NOT to do

- Do not start with "Great work" or any praise. The user can read the
  diff themselves.
- Do not list every style nit — the linter did that already.
- Do not rewrite the code for them; describe the change, don't paste
  50 lines of a rewrite unless the fix is genuinely that small.
- Do not speculate about bugs you can't locate. If the diff is
  incomplete and you can't tell, say "need to see <file>" and stop.

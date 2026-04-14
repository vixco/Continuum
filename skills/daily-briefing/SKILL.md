---
name: daily-briefing
description: Compose a compact spoken morning briefing from memory, calendar, and overnight worker activity
source: bundled
triggers:
  - briefing
  - morning
  - what's today
  - what's on today
  - day plan
  - "ochtendbriefing"
  - "wat staat er vandaag"
---

# Daily Briefing

When this skill is active, the user is asking for a concise morning
overview. Produce a spoken briefing under 60 seconds (~120 words).

## What to gather

Before speaking, call these tools in parallel and wait for all results:

1. `mcp__kairo__system_current_time` — confirm the date and day-of-week.
2. `mcp__kairo__memory_query_episodic` with query `"yesterday"` (limit 5) —
   capture what the user did recently; look for unfinished items.
3. `mcp__kairo__memory_list_facts` with prefix `"user."` (limit 20) —
   read the user's routines, preferences, and today-relevant reminders.
4. `mcp__kairo__memory_list_facts` with prefix `"schedule."` (limit 20) —
   recurring events and commitments.
5. `mcp__kairo__workers_worker_list` with `status="completed"` (limit 5) —
   any overnight worker runs with results the user should know about.

If a calendar or email tool is present in the available-tools list, use it
in the same parallel batch. Otherwise skip silently.

## How to structure the briefing

Speak in this order. Omit sections that have nothing to report — never
pad.

1. **Date + weather** — one short sentence if weather is known.
2. **Unfinished from yesterday** — max two items, most important first.
3. **Today's commitments** — appointments or deadlines from memory,
   earliest first. If none, skip. Never invent.
4. **Overnight worker activity** — "The cleanup worker moved 14 files and
   flagged 2 for review." Only if any worker ran overnight.
5. **One concrete suggestion** — the single next action the user should
   take first, phrased as an offer ("want me to start X?"), not a
   command. Only include if confidence is high.

## Tone rules

- No greetings. Start with the date or the first real fact.
- No "here's your briefing" preambles.
- Past tense for yesterday, present for today, conditional for
  suggestions.
- If you have zero real information, say so plainly and stop: "Nothing
  notable in memory from yesterday. Want me to plan the day from
  scratch?"

## Write the briefing to memory

After speaking, call `mcp__kairo__memory_set_fact` with key
`routine.last_briefing_at` and value `<now RFC3339>` so the next
briefing can deduplicate if asked again within the hour.

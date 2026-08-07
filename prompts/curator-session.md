Summarize this work session for Continuum's long-term memory. Use the exact
section layout below. Be concrete; name files, errors, and outcomes. If the
events show trivial/idle activity only, reply with exactly: SKIP

Session: {{START}} – {{END}}  (main app: {{PROCESS}}, project hint: {{PROJECT}})

Events:
{{EVENTS}}

Reply with markdown in exactly this shape (no preamble):
## Goal
<one line>
## Changed
<bullets>
## Problem
<one line or "none">
## Tried
<bullets or "–">
## Result
<one line>
## Next step
<one line>

open_task: <the single unfinished task this session leaves behind, one line, or "none">

The final `open_task:` line is a machine-read trailer, not prose: exactly one
line, always present, always last. Write `open_task: none` when nothing was
left unfinished. It is what Continuum resumes when the user later says
"ga door" / "continue".

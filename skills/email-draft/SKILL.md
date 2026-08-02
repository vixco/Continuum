---
name: email-draft
description: Draft concise, on-voice email replies and new messages
source: bundled
triggers:
  - email
  - reply
  - draft email
  - mail
  - "stuur een mail"
  - write back
  - respond to
---

# Email Draft

When this skill matches, the user wants help producing an email draft.
Continuum writes the draft, nothing more — actually sending is up to the
user unless an explicit send tool is in scope and they confirmed.

## Before drafting

1. Look up the user's typical tone from semantic memory:
   `mcp__continuum__memory_get_fact` with key `user.email_tone`
   (expected values: `formal`, `neutral`, `casual`). If the key is
   absent, default to `neutral`.
2. Look up the recipient from memory:
   `mcp__continuum__memory_get_fact` with key
   `contact.<name>.email_tone` if the user named someone. Per-contact
   tone wins over the default.
3. If the user is replying to a thread they quoted in the prompt, read
   the quoted text carefully — preserve the subject ("Re: …") and
   match any pre-existing "Best / Groet / Cheers" closing unless the
   user is signalling a mood shift.

## Output format

Return exactly this, with no extra prose around it:

```
Subject: <subject line, ≤ 60 chars>

<body, 3–6 short lines — longer only if the user asked for detail>

<sign-off matching the recipient's register>
```

No preamble like "Here's a draft" — the user already knows what
they're getting.

## Voice

- Short sentences. One idea per line.
- Front-load the reason for the email in line 1 so the reader
  doesn't have to scan.
- No filler phrases ("I hope this email finds you well", "just
  following up here"). Straight to business.
- Match the language of the incoming thread. If the original is in
  Dutch, draft in Dutch. (Output language follows the user, not
  Continuum's TTS locale.)
- No exclamation marks unless the user explicitly signalled
  excitement.

## Confirm before sending

If a send tool is in the allowed-tools list AND the user explicitly
said "send it", call the send tool. Otherwise, stop after the draft
and say one line: "Say 'send it' when you want me to send this."

## Capture what you learn

If the draft teaches Continuum something durable (a new recipient, a
preferred tone shift, a recurring meeting mention), write it to
semantic memory at the end:

```
mcp__continuum__memory_set_fact(key="contact.<name>.email", value=...)
mcp__continuum__memory_set_fact(key="contact.<name>.email_tone", value=...)
```

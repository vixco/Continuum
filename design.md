# Design — Continuum

A locked design system for the Continuum desktop app. Every page redesign reads
this file first. Extend this system when the product needs a new visual role;
do not invent a different theme per tab.

## Genre

Modern-minimal, technical, and austere. It should feel like a calm Windows
instrument panel: dense enough for real work, quiet enough to leave running.

## Macrostructure family

- Marketing pages: not in scope for the desktop shell.
- App pages: **Workbench** — fixed navigation, compact functional headings,
  broad working surfaces, and deliberate changes in density rather than stacks
  of identical cards.
- Conversational pages: Workbench split view — navigation/history beside the
  active conversation without changing the app shell.
- Content/settings pages: Workbench document lane — one readable column with
  sections divided by spacing and surface lightness.

## Theme — Foundry

The palette is **cool slate paper with a single electric indigo accent**.
Hue 260 for surfaces and ink, hue 277 for the accent. The accent class
names in Tailwind are deliberately `amber-*` / `accent.amber` instead of
`indigo-*` — see the "Naming note" in `tokens.css` for the rationale.

- `--color-paper` `oklch(13% 0.008 260)`
- `--color-paper-2` `oklch(16.5% 0.009 260)`
- `--color-paper-3` `oklch(20% 0.010 260)`
- `--color-ink` `oklch(96% 0.004 260)`
- `--color-ink-2` `oklch(82% 0.010 260)`
- `--color-rule` `oklch(26% 0.008 260)`
- `--color-muted` `oklch(66% 0.012 260)`
- `--color-accent` `oklch(70% 0.14 277)`
- `--color-focus` `oklch(72% 0.16 277)`

Indigo marks selection, primary actions, and focus. Teal/green, amber,
and red are reserved for operational state (healthy / warn / error).
Blue (hue 250) remains limited to the listening state. No decorative
gradients, coloured glows, or glass panels.

## Typography

- Display: Bahnschrift, weight 700, normal.
- Body: Segoe UI Variable Text, weight 400.
- Mono: Cascadia Mono with JetBrains Mono and Consolas fallbacks, weight 500.
- Wordmark: Bahnschrift, weight 700.
- Display tracking: `-0.025em`.
- Body floor: 14px; primary interface copy defaults to 15–16px.

These are native Windows faces by design: the packaged desktop app stays local,
loads without a font CDN, and matches its host platform.

## Spacing

Use the named 4px-derived scale in `tokens.css`. Page rhythm alternates compact
control groups with broader working gaps; cards, sections, and page padding must
not all use the same spacing value.

## Motion

- Use only `--ease-out`, `--ease-in`, and `--ease-in-out`.
- Tab content crossfades; it does not slide upward on every navigation.
- Button press uses a 1px translation, not a scale bounce.
- Functional loaders remain; decorative infinite motion is removed.
- Reduced motion is opacity-only and no longer than 150ms.

## Microinteractions stance

- Silent success when the result is already visible.
- Focus is instant and visible.
- Hover changes one signal only and always has a keyboard equivalent.
- Disabled controls keep an explanation through their existing label or title.

## CTA voice

- Primary: compact indigo fill, dark text, square-soft corners, direct verb.
- Secondary: graphite surface with a visible neutral rule.
- Ghost: text and surface shift only; no shadow or scale lift.

## Per-page allowances

- App pages use no enrichment; function is the visual content.
- Home may use the existing live/hybrid information hierarchy but cannot invent
  metrics or imply fixture panels are live.
- Chat and Memory may use split workspaces.
- Logs and Health use tabular numerals and operational colours with text/icons.

## What pages MUST share

- Continuum wordmark and mark.
- Foundry palette (cool slate paper, indigo accent) and native type system.
- Button, input, card, badge, modal, focus, and spacing language.
- Real sidebar navigation and `Ctrl+K` command access.
- Explicit live, hybrid, unavailable, disabled, error, and fixture meaning.

## What pages MAY differ on

- Density and column balance appropriate to the task.
- Cards may become ruled sections when containment is unnecessary.
- Chat/Memory may use persistent secondary panes; Settings remains document-like.

## Exports

### tokens.css

The canonical runtime tokens live in `/tokens.css` and are imported by the
desktop entry stylesheet.

### Tailwind v4 `@theme`

```css
@theme {
  --color-paper: oklch(13% 0.008 260);
  --color-paper-2: oklch(16.5% 0.009 260);
  --color-paper-3: oklch(20% 0.010 260);
  --color-ink: oklch(96% 0.004 260);
  --color-ink-2: oklch(82% 0.010 260);
  --color-rule: oklch(26% 0.008 260);
  --color-accent: oklch(70% 0.14 277);
  --font-display: "Bahnschrift", "Segoe UI Variable Display", sans-serif;
  --font-body: "Segoe UI Variable Text", "Segoe UI", sans-serif;
  --font-outlier: "Cascadia Mono", "JetBrains Mono", monospace;
  --spacing-md: 1rem;
  --spacing-lg: 1.5rem;
  --ease-out: cubic-bezier(0.16, 1, 0.3, 1);
}
```

### DTCG `tokens.json`

```json
{
  "$schema": "https://design-tokens.github.io/community-group/format/",
  "color": {
    "paper": { "$value": "oklch(13% 0.008 260)", "$type": "color" },
    "paper-2": { "$value": "oklch(16.5% 0.009 260)", "$type": "color" },
    "paper-3": { "$value": "oklch(20% 0.010 260)", "$type": "color" },
    "ink": { "$value": "oklch(96% 0.004 260)", "$type": "color" },
    "accent": { "$value": "oklch(70% 0.14 277)", "$type": "color" }
  },
  "font": {
    "display": { "$value": "Bahnschrift, Segoe UI Variable Display, sans-serif", "$type": "fontFamily" },
    "body": { "$value": "Segoe UI Variable Text, Segoe UI, sans-serif", "$type": "fontFamily" },
    "outlier": { "$value": "Cascadia Mono, JetBrains Mono, monospace", "$type": "fontFamily" }
  },
  "space": {
    "md": { "$value": "1rem", "$type": "dimension" },
    "lg": { "$value": "1.5rem", "$type": "dimension" }
  }
}
```

### shadcn/ui CSS variables

```css
:root {
  --background: 13% 0.008 260;
  --foreground: 96% 0.004 260;
  --card: 16.5% 0.009 260;
  --card-foreground: 96% 0.004 260;
  --primary: 70% 0.14 277;
  --primary-foreground: 16% 0.012 277;
  --secondary: 20% 0.010 260;
  --secondary-foreground: 82% 0.010 260;
  --muted: 26% 0.008 260;
  --muted-foreground: 66% 0.012 260;
  --border: 26% 0.008 260;
  --input: 32% 0.010 260;
  --ring: 72% 0.16 277;
  --radius: 10px;
}
```

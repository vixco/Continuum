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

## Theme — Foundry Amber

- `--color-paper` `oklch(12.5% 0.01 75)`
- `--color-paper-2` `oklch(15.5% 0.011 75)`
- `--color-paper-3` `oklch(19.5% 0.012 75)`
- `--color-ink` `oklch(95% 0.008 75)`
- `--color-ink-2` `oklch(82% 0.01 75)`
- `--color-rule` `oklch(28% 0.01 75)`
- `--color-muted` `oklch(68% 0.01 75)`
- `--color-accent` `oklch(72% 0.15 70)`
- `--color-focus` `oklch(80% 0.18 75)`

Amber marks selection, primary actions, and focus. Green, orange, and red are
reserved for operational state. Blue remains limited to the listening state.
No decorative gradients, coloured glows, or glass panels.

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

- Primary: compact amber fill, dark text, square-soft corners, direct verb.
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
- Foundry Amber palette and native type system.
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
  --color-paper: oklch(12.5% 0.01 75);
  --color-paper-2: oklch(15.5% 0.011 75);
  --color-paper-3: oklch(19.5% 0.012 75);
  --color-ink: oklch(95% 0.008 75);
  --color-ink-2: oklch(82% 0.01 75);
  --color-rule: oklch(28% 0.01 75);
  --color-accent: oklch(72% 0.15 70);
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
    "paper": { "$value": "oklch(12.5% 0.01 75)", "$type": "color" },
    "paper-2": { "$value": "oklch(15.5% 0.011 75)", "$type": "color" },
    "paper-3": { "$value": "oklch(19.5% 0.012 75)", "$type": "color" },
    "ink": { "$value": "oklch(95% 0.008 75)", "$type": "color" },
    "accent": { "$value": "oklch(72% 0.15 70)", "$type": "color" }
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
  --background: 12.5% 0.01 75;
  --foreground: 95% 0.008 75;
  --card: 15.5% 0.011 75;
  --card-foreground: 95% 0.008 75;
  --primary: 72% 0.15 70;
  --primary-foreground: 16% 0.012 75;
  --secondary: 19.5% 0.012 75;
  --secondary-foreground: 82% 0.01 75;
  --muted: 28% 0.01 75;
  --muted-foreground: 68% 0.01 75;
  --border: 28% 0.01 75;
  --input: 34% 0.012 75;
  --ring: 80% 0.18 75;
  --radius: 10px;
}
```

# Continuum Desktop Design Contract

Continuum should feel like a native agent workspace, not a web dashboard placed in a desktop frame.

This contract defines Continuum's native desktop interaction language and complements the shared tokens in `tokens.css`. It keeps the product's indigo identity and information architecture consistent across every surface.

## Product feeling

**Quiet intelligence.** The UI should make the agent feel capable because it is responsive, stateful, and understandable — not because the screen is full of gradients, glowing cards, or "AI" decoration.

**Operator, not dashboard.** Chat, context, memory, tools, automations, and computer use are working surfaces. Metrics support the work; they are never the visual hero.

**Local-first trust.** Permissions, runtime state, approvals, computer control, and destructive actions must be visible and legible. Never make autonomous behavior feel mysterious.

## Visual rules

1. **Flat over boxed.** Prefer whitespace and hierarchy. Do not create card-in-card layouts.
2. **Hairlines over frames.** Use `--ui-stroke-*` tokens for separation. Floating surfaces use `--stroke-overlay` with the shared shadow tokens.
3. **Tokens over literals.** Feature components must not introduce raw hex/rgb colors or bespoke shadows.
4. **One accent.** Continuum's indigo accent marks selection, focus, progress, and primary intent. Functional green/yellow/red remain status colors only.
5. **Quiet chrome.** Titlebar, sidebar, conversation rail, and status chrome should be visually subordinate to the active work surface.
6. **Native density.** Controls should feel like desktop controls: compact but comfortably targetable, without oversized mobile padding.
7. **No decorative AI gradients.** A gradient is allowed only when it communicates a real spatial transition or state; never as generic futuristic decoration.

## Information hierarchy

- **Chat is the primary agent work surface.** The transcript and composer must remain the strongest visual focus when Chat is active.
- **Home is an operational overview.** It answers: what is Continuum doing, what just happened, what requires me, and what is the current cost/health?
- **Context and Memory explain what the agent knows.** They should read as evidence, not analytics dashboards.
- **Tools & Skills, Automations, and Brain configure capability.** Settings configure the application itself.
- **Health and Logs are diagnostic surfaces.** They may be dense and technical, but must reuse the same primitives.

Background events may update badges or status, but must never navigate, open overlays, or steal focus automatically.

## Interaction rules

- Every direct action paints feedback immediately.
- One cancel gesture performs one cancellation only.
- Keyboard ownership follows focus; the shell must not steal editing or terminal shortcuts.
- Dangerous actions require explicit intent and must describe what will happen before execution.
- Loading, empty, degraded, offline, reconnecting, blocked, and error states are distinct states with an obvious recovery path.
- Long transcripts and live activity must stay performant; do not trade responsiveness for decorative motion.

## Motion

- Functional transitions should normally land around 100–220 ms.
- Animate opacity/color/transform where possible; avoid layout animation in hot paths.
- Never use `transition-all` in interaction-heavy surfaces.
- Respect `prefers-reduced-motion` for all non-essential animation.
- Motion follows state. It must never delay a click, cancel, selection, approval, or keyboard response.

## Components

Shared primitives own appearance. Call sites choose variants and content rather than rebuilding buttons, cards, form controls, modals, status badges, or focus treatments.

When a new visual need appears, first ask whether the existing primitive should gain a variant. Creating a one-off component is the last option.

## Chat

- Keep the transcript reading measure comfortable and centered.
- Tool calls and approvals are part of the conversation, not a separate dashboard language.
- The conversation rail is secondary and should collapse without destroying session state.
- The composer is the primary action surface. Focus should be unmistakable without a neon glow.
- Streaming must remain smooth under long histories; virtualization is an invariant.

## Accessibility

- Every icon-only control needs an accessible label.
- Navigation communicates the current destination with `aria-current`.
- Focus-visible state must remain clearly visible in both themes.
- Status cannot rely on color alone when the distinction matters to the task.
- Minimum useful desktop targets should remain easy to acquire with mouse, touchpad, keyboard, and accessibility tooling.

## Review checklist

Before merging UI work:

- Does it reduce or add visual noise?
- Is there a nested card that can become spacing instead?
- Are all colors/elevation token-driven?
- Does foreground intent still win over background activity?
- Does the feature remain understandable while offline/degraded?
- Are keyboard and cancellation semantics preserved?
- Does it stay fast with realistic transcript/log/tool volume?
- Does reduced motion still work?
- Do `pnpm test:desktop`, typecheck, lint, format, frontend build, Tauri build, and workspace Rust CI pass?

"use client";

import { useEffect, useRef, useState } from "react";
import { clsx } from "clsx";
import { Mic } from "lucide-react";

import { continuum } from "@/lib/tauri";
import type { VoiceMode } from "@/lib/types";

/**
 * Push-to-talk button - gives users a one-click alternative to the wake
 * word and the global hotkey. Click writes a `talk_now` voice intent which
 * the daemon picks up within ~250ms and treats exactly like a hotkey press
 * (next transcript opens a session).
 *
 * Visual states:
 *   - idle    → muted, hover glow, mic icon
 *   - pressed → 150ms scale pulse right after the click (optimistic feedback,
 *               since `state.voice.mode` lags up to 2s behind via the state
 *               poller)
 *   - listening → continuous pulse + accent color + tiny bar visualizer in
 *                 place of the mic icon (matches the StatusOrb on the same
 *                 row)
 *   - thinking / speaking → dimmed, no-op (Continuum is busy responding)
 *
 * A second click while listening is a visual no-op and sends no extra request,
 * matching the one-shot intent semantics.
 */
export function PushToTalkButton({ mode }: { mode: VoiceMode }) {
  const [isPressed, setIsPressed] = useState(false);
  const [isSending, setIsSending] = useState(false);
  const pressTimer = useRef<ReturnType<typeof setTimeout> | null>(null);

  useEffect(
    () => () => {
      if (pressTimer.current) clearTimeout(pressTimer.current);
    },
    []
  );

  const isListening = mode === "listening";
  const isBusy = mode === "thinking" || mode === "speaking";
  const disabled = isSending || isBusy;

  async function onClick() {
    if (disabled || isListening) return;
    setIsPressed(true);
    setIsSending(true);
    if (pressTimer.current) clearTimeout(pressTimer.current);
    pressTimer.current = setTimeout(() => setIsPressed(false), 180);
    try {
      await continuum.talkNow();
    } catch {
      // Tauri unavailable in pnpm-dev mode, or daemon not running. Either
      // way the user gets the local visual feedback; no toast needed for
      // a feature that's expected to fail in dev.
    } finally {
      setIsSending(false);
    }
  }

  const hint = hintFor(mode);

  return (
    <div className="flex flex-col items-center gap-2">
      <button
        type="button"
        onClick={onClick}
        disabled={disabled}
        aria-label={isListening ? "Listening" : "Click to talk to Continuum"}
        title={
          isListening
            ? "Listening - speak now"
            : isBusy
              ? "Continuum is busy"
              : "Click to talk (or say 'hey Continuum' / Ctrl+Shift+K)"
        }
        className={clsx(
          "relative flex h-20 w-20 items-center justify-center rounded-full",
          "border transition-[transform,background-color,border-color,color] duration-150 ease-[var(--ease-out)]",
          "focus-visible:outline-none focus-visible:ring-2 focus-visible:ring-accent-blue/60",
          "disabled:cursor-not-allowed",
          isListening
            ? "animate-pulse-slow border-accent-blue bg-accent-blue/15 text-accent-blue"
            : isBusy
              ? "border-bg-border bg-bg-elevated text-ink-dim opacity-60"
              : "border-bg-border bg-bg-elevated text-ink-muted hover:border-accent-blue/40 hover:bg-accent-blue/10 hover:text-accent-blue",
          isPressed && "scale-95"
        )}
      >
        {isListening ? <ListeningBars /> : <Mic size={28} strokeWidth={1.6} />}
      </button>
      <span
        className={clsx(
          "text-xs",
          isListening ? "text-accent-blue" : isBusy ? "text-ink-dim" : "text-ink-muted"
        )}
      >
        {hint}
      </span>
    </div>
  );
}

function hintFor(mode: VoiceMode): string {
  switch (mode) {
    case "listening":
      return "Listening…";
    case "thinking":
      return "Thinking…";
    case "speaking":
      return "Speaking…";
    case "muted":
      return "Voice muted";
    case "error":
      return "Voice error";
    default:
      return "Click to talk";
  }
}

/** Three thin vertical bars that animate while Continuum is listening. CSS-only. */
function ListeningBars() {
  return (
    <div className="flex items-end gap-1">
      <span className="h-3 w-1 animate-pulse-slow rounded-sm bg-accent-blue [animation-delay:0ms]" />
      <span className="h-5 w-1 animate-pulse-slow rounded-sm bg-accent-blue [animation-delay:150ms]" />
      <span className="h-4 w-1 animate-pulse-slow rounded-sm bg-accent-blue [animation-delay:300ms]" />
    </div>
  );
}

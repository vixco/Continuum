"use client";

import { useEffect, useRef, useState } from "react";
import { clsx } from "clsx";
import { Mic } from "lucide-react";

import { continuum } from "@/lib/tauri";
import type { VoiceMode } from "@/lib/types";

const RUNTIME_READY_TIMEOUT_MS = 20_000;
const RUNTIME_POLL_MS = 250;
const ARMED_FEEDBACK_MS = 12_000;

/**
 * Push-to-talk button — gives users a one-click alternative to the wake
 * word and the global hotkey.
 *
 * A talk intent is only useful while the headless runtime is alive. The
 * previous implementation wrote `talk_now` even when the runtime was off,
 * which succeeded from Tauri's point of view but left nobody to consume the
 * intent. That made the button look functional while effectively doing
 * nothing. We now make an explicit PTT click self-healing: start the runtime
 * when needed, resume/unmute an intentionally requested voice turn, wait
 * until the runtime is actually alive, and only then arm the next utterance.
 */
export function PushToTalkButton({ mode }: { mode: VoiceMode }) {
  const [isPressed, setIsPressed] = useState(false);
  const [isSending, setIsSending] = useState(false);
  const [isStarting, setIsStarting] = useState(false);
  const [isArmed, setIsArmed] = useState(false);
  const [error, setError] = useState<string | null>(null);
  const pressTimer = useRef<ReturnType<typeof setTimeout> | null>(null);
  const armedTimer = useRef<ReturnType<typeof setTimeout> | null>(null);

  useEffect(
    () => () => {
      if (pressTimer.current) clearTimeout(pressTimer.current);
      if (armedTimer.current) clearTimeout(armedTimer.current);
    },
    []
  );

  // Once the runtime reports a real voice state, the optimistic "speak now"
  // state is no longer needed. It mainly bridges the state-poller delay after
  // the intent has been accepted.
  useEffect(() => {
    if (mode !== "listening" && mode !== "thinking" && mode !== "speaking") return;
    setIsArmed(false);
    if (armedTimer.current) {
      clearTimeout(armedTimer.current);
      armedTimer.current = null;
    }
  }, [mode]);

  const runtimeListening = mode === "listening";
  const isListening = runtimeListening || isArmed;
  const isBusy = mode === "thinking" || mode === "speaking";
  const disabled = isSending || isBusy || isArmed;

  async function onClick() {
    if (disabled || runtimeListening) return;

    setError(null);
    setIsPressed(true);
    setIsSending(true);
    if (pressTimer.current) clearTimeout(pressTimer.current);
    pressTimer.current = setTimeout(() => setIsPressed(false), 180);

    try {
      let runtime = await continuum.getRuntimeStatus();
      if (!runtime.alive) {
        setIsStarting(true);
        await continuum.startRuntime();
        runtime = await waitForRuntime();
      }

      if (!runtime.alive) {
        throw new Error("Continuum runtime did not become ready for voice input");
      }

      // A direct click is an explicit request to interact. Resume/unmute here
      // instead of accepting an intent that the runtime would ignore.
      const state = await continuum.getState();
      if (state.system.paused) await continuum.setPaused(false);
      if (state.voice.muted) await continuum.setVoiceMuted(false);

      await continuum.talkNow();
      setIsArmed(true);
      if (armedTimer.current) clearTimeout(armedTimer.current);
      armedTimer.current = setTimeout(() => {
        setIsArmed(false);
        armedTimer.current = null;
      }, ARMED_FEEDBACK_MS);
    } catch (err) {
      setIsArmed(false);
      setError(toErrorMessage(err));
    } finally {
      setIsStarting(false);
      setIsSending(false);
    }
  }

  const hint = isStarting
    ? "Starting voice…"
    : isArmed
      ? "Speak now…"
      : error
        ? "Voice unavailable"
        : hintFor(mode);

  return (
    <div className="flex flex-col items-center gap-2">
      <button
        type="button"
        onClick={onClick}
        disabled={disabled}
        aria-label={isListening ? "Listening" : "Click to talk to Continuum"}
        aria-busy={isSending || undefined}
        title={
          error
            ? error
            : isStarting
              ? "Starting the Continuum runtime for voice input"
              : isListening
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
              : error
                ? "border-state-error/50 bg-state-error/10 text-state-error"
                : "border-bg-border bg-bg-elevated text-ink-muted hover:border-accent-blue/40 hover:bg-accent-blue/10 hover:text-accent-blue",
          isPressed && "scale-95"
        )}
      >
        {isListening ? <ListeningBars /> : <Mic size={28} strokeWidth={1.6} />}
      </button>
      <span
        className={clsx(
          "max-w-32 text-center text-xs",
          error
            ? "text-state-error"
            : isListening
              ? "text-accent-blue"
              : isBusy
                ? "text-ink-dim"
                : "text-ink-muted"
        )}
      >
        {hint}
      </span>
    </div>
  );
}

async function waitForRuntime() {
  const deadline = Date.now() + RUNTIME_READY_TIMEOUT_MS;
  let status = await continuum.getRuntimeStatus();

  while (!status.alive && Date.now() < deadline) {
    await delay(RUNTIME_POLL_MS);
    status = await continuum.getRuntimeStatus();
  }

  return status;
}

function delay(ms: number) {
  return new Promise<void>((resolve) => setTimeout(resolve, ms));
}

function toErrorMessage(error: unknown): string {
  if (error instanceof Error && error.message.trim()) return error.message;
  const message = String(error ?? "").trim();
  return message || "Voice input could not be started";
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
      return "Click to unmute & talk";
    case "error":
      return "Retry voice";
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

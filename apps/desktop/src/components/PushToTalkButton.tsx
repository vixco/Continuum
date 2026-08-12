"use client";

import { useCallback, useEffect, useRef, useState } from "react";
import { clsx } from "clsx";
import { Mic, MicOff } from "lucide-react";

import { continuum } from "@/lib/tauri";
import { useStore } from "@/lib/store";
import type { ContinuumState, VoiceMode } from "@/lib/types";

const RUNTIME_READY_TIMEOUT_MS = 20_000;
const VOICE_READY_TIMEOUT_MS = 12_000;
const CONTROL_APPLY_TIMEOUT_MS = 10_000;
const FIRST_SPEECH_TIMEOUT_MS = 25_000;
const RUNTIME_POLL_MS = 250;
const REARM_DELAY_MS = 300;
let autoStartAttemptedThisLaunch = false;

/**
 * One-click conversational voice.
 *
 * This deliberately reuses Continuum's native voice stack instead of browser
 * speech APIs: Windows default microphone -> CPAL/VAD -> local Whisper ->
 * orchestrator -> Piper/Kokoros. After one completed turn reaches idle, the
 * button automatically arms the next turn, so the user can keep talking
 * without clicking again or repeating the wake word.
 */
export function PushToTalkButton({ mode }: { mode: VoiceMode }) {
  const setConfig = useStore((state) => state.setConfig);
  const micInputLevel = useStore((state) => state.state.voice.mic_input_level);
  const [liveActive, setLiveActive] = useState(false);
  const [isStarting, setIsStarting] = useState(false);
  const [isArming, setIsArming] = useState(false);
  const [isArmed, setIsArmed] = useState(false);
  const [error, setError] = useState<string | null>(null);

  const liveRef = useRef(false);
  const armedRef = useRef(false);
  const armingRef = useRef(false);
  const hadRuntimeActivityRef = useRef(false);
  const previousModeRef = useRef<VoiceMode>(mode);
  const rearmTimerRef = useRef<ReturnType<typeof setTimeout> | null>(null);
  const speechWatchdogRef = useRef<ReturnType<typeof setTimeout> | null>(null);

  const clearRearmTimer = useCallback(() => {
    if (rearmTimerRef.current) {
      clearTimeout(rearmTimerRef.current);
      rearmTimerRef.current = null;
    }
  }, []);

  const clearSpeechWatchdog = useCallback(() => {
    if (speechWatchdogRef.current) {
      clearTimeout(speechWatchdogRef.current);
      speechWatchdogRef.current = null;
    }
  }, []);

  const stopLive = useCallback(() => {
    liveRef.current = false;
    armedRef.current = false;
    armingRef.current = false;
    hadRuntimeActivityRef.current = false;
    clearRearmTimer();
    clearSpeechWatchdog();
    setLiveActive(false);
    setIsStarting(false);
    setIsArming(false);
    setIsArmed(false);
  }, [clearRearmTimer, clearSpeechWatchdog]);

  const failLive = useCallback(
    (message: string) => {
      stopLive();
      setError(message);
    },
    [stopLive]
  );

  const armNextTurn = useCallback(async () => {
    if (!liveRef.current || armingRef.current || armedRef.current) return;

    armingRef.current = true;
    setIsArming(true);
    setError(null);

    try {
      let runtime = await continuum.getRuntimeStatus();
      if (!runtime.alive) {
        setIsStarting(true);
        runtime = await waitForRuntime();
      }

      if (!runtime.alive) {
        throw new Error(runtime.error ?? "The automatic Continuum runtime did not become ready.");
      }

      let state = await waitForVoiceReady();
      const readinessError = voiceReadinessError(state);
      if (readinessError) throw new Error(readinessError);

      const config = await continuum.getConfig();
      if (!config.audio.enabled) {
        throw new Error(
          "Live voice is disabled because audio capture is off. Enable audio in Continuum setup/config and restart the runtime."
        );
      }
      if (!config.voice.enabled) {
        throw new Error(
          "Live voice is disabled in Voice settings. Enable voice and restart the runtime before retrying."
        );
      }

      // The Context privacy toggles are the actual live control plane for the
      // separate headless runtime. An explicit live-voice click is permission
      // to resume observation and enable the mic; do not mutate dashboard-only
      // pause/mute flags and pretend the daemon changed.
      const toggles = state.context.page?.toggles;
      if (toggles?.pause_all) {
        await continuum.contextWriteIntent({
          kind: "set_toggle",
          name: "pause_all",
          value: false,
        });
      }
      if (toggles && !toggles.mic) {
        await continuum.contextWriteIntent({ kind: "set_toggle", name: "mic", value: true });
      }
      if (toggles?.pause_all || (toggles && !toggles.mic)) {
        state = await waitForMicControl();
        const applied = state.context.page?.toggles;
        if (applied?.pause_all || applied?.mic === false) {
          throw new Error(
            "Continuum could not enable the microphone in the running voice runtime. Check Context privacy controls and retry."
          );
        }
      }

      if (!liveRef.current) return;

      await continuum.talkNow();
      armedRef.current = true;
      hadRuntimeActivityRef.current = false;
      setIsArmed(true);

      clearSpeechWatchdog();
      speechWatchdogRef.current = setTimeout(() => {
        speechWatchdogRef.current = null;
        if (!liveRef.current || !armedRef.current) return;
        failLive(
          "No speech reached Whisper within 25 seconds. If you were talking, check the Windows microphone input/privacy settings and the Whisper model, then retry."
        );
      }, FIRST_SPEECH_TIMEOUT_MS);
    } catch (err) {
      failLive(toErrorMessage(err));
    } finally {
      armingRef.current = false;
      setIsStarting(false);
      setIsArming(false);
    }
  }, [clearSpeechWatchdog, failLive]);

  const persistAutoStart = useCallback(
    async (enabled: boolean) => {
      try {
        setConfig(await continuum.updateVoiceFlag("live_voice_auto_start", enabled));
      } catch (reason) {
        setError(`Could not save live voice preference: ${toErrorMessage(reason)}`);
      }
    },
    [setConfig]
  );

  useEffect(() => {
    if (autoStartAttemptedThisLaunch) return;
    autoStartAttemptedThisLaunch = true;
    let cancelled = false;
    void continuum.getConfig().then((config) => {
      if (cancelled || !config.voice.live_voice_auto_start) return;
      liveRef.current = true;
      setLiveActive(true);
      void armNextTurn();
    });
    return () => {
      cancelled = true;
    };
  }, [armNextTurn]);

  useEffect(() => {
    return () => {
      liveRef.current = false;
      clearRearmTimer();
      clearSpeechWatchdog();
    };
  }, [clearRearmTimer, clearSpeechWatchdog]);

  // Drive the continuous turn loop from the native runtime's real state.
  // `talk_now` is initially optimistic (runtime stays idle until Whisper has
  // a transcript). After a real listening/thinking/speaking cycle returns to
  // idle, arm exactly one next turn.
  useEffect(() => {
    const previous = previousModeRef.current;
    previousModeRef.current = mode;

    if (!liveRef.current) return;

    if (mode === "error") {
      failLive(
        "The native voice runtime reported an error. Check your microphone, Whisper model and TTS setup, then retry."
      );
      return;
    }

    const activeRuntimeMode = mode === "listening" || mode === "thinking" || mode === "speaking";

    if (activeRuntimeMode) {
      hadRuntimeActivityRef.current = true;
      clearSpeechWatchdog();
      if (armedRef.current) {
        armedRef.current = false;
        setIsArmed(false);
      }
      return;
    }

    const previousWasActive =
      previous === "listening" || previous === "thinking" || previous === "speaking";

    if (
      mode === "idle" &&
      previousWasActive &&
      hadRuntimeActivityRef.current &&
      !armedRef.current &&
      !armingRef.current
    ) {
      hadRuntimeActivityRef.current = false;
      clearRearmTimer();
      rearmTimerRef.current = setTimeout(() => {
        rearmTimerRef.current = null;
        void armNextTurn();
      }, REARM_DELAY_MS);
    }
  }, [armNextTurn, clearRearmTimer, clearSpeechWatchdog, failLive, mode]);

  async function onClick() {
    if (liveRef.current) {
      stopLive();
      setError(null);
      await persistAutoStart(false);
      return;
    }

    liveRef.current = true;
    setLiveActive(true);
    setError(null);
    await persistAutoStart(true);
    await armNextTurn();
  }

  const phase = livePhase(mode, liveActive, isStarting, isArming, isArmed, error);
  const visuallyListening = liveActive && (isArmed || mode === "listening");
  const visuallyBusy = liveActive && (mode === "thinking" || mode === "speaking");

  return (
    <div className="flex max-w-56 flex-col items-center gap-2">
      <button
        type="button"
        onClick={() => void onClick()}
        disabled={isStarting || isArming}
        aria-label={liveActive ? "Stop live voice" : "Start live voice"}
        aria-pressed={liveActive}
        aria-busy={isStarting || isArming || undefined}
        title={
          error
            ? `Voice error: ${error}`
            : liveActive
              ? "Stop live voice"
              : "Start live voice — keep talking without repeating the wake word"
        }
        className={clsx(
          "relative flex h-20 w-20 items-center justify-center rounded-full",
          "border transition-[transform,background-color,border-color,color] duration-150 ease-[var(--ease-out)]",
          "focus-visible:outline-none focus-visible:ring-2 focus-visible:ring-accent-blue/60",
          "disabled:cursor-wait disabled:opacity-70",
          visuallyListening
            ? "animate-pulse-slow border-accent-blue bg-accent-blue/15 text-accent-blue"
            : visuallyBusy
              ? "border-accent-amber/50 bg-accent-amber/10 text-accent-amber"
              : liveActive
                ? "border-state-healthy/50 bg-state-healthy/10 text-state-healthy"
                : error
                  ? "border-state-error/50 bg-state-error/10 text-state-error"
                  : "border-bg-border bg-bg-elevated text-ink-muted hover:border-accent-blue/40 hover:bg-accent-blue/10 hover:text-accent-blue"
        )}
      >
        {liveActive ? (
          visuallyListening ? (
            <ListeningBars level={micInputLevel} />
          ) : (
            <MicOff size={27} strokeWidth={1.6} />
          )
        ) : (
          <Mic size={28} strokeWidth={1.6} />
        )}
      </button>

      <span
        className={clsx(
          "text-center text-xs",
          error
            ? "text-state-error"
            : visuallyListening
              ? "text-accent-blue"
              : visuallyBusy
                ? "text-accent-amber"
                : liveActive
                  ? "text-state-healthy"
                  : "text-ink-muted"
        )}
      >
        {phase}
      </span>

      {error && (
        <div
          role="alert"
          className="rounded-md border border-state-error/35 bg-state-error/[0.08] px-2.5 py-2 text-center text-[11px] leading-relaxed text-state-error"
        >
          {error}
        </div>
      )}
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

async function waitForVoiceReady(): Promise<ContinuumState> {
  const deadline = Date.now() + VOICE_READY_TIMEOUT_MS;
  let state = await continuum.getState();

  while (
    Date.now() < deadline &&
    (!state.system.stt_loaded || !state.system.tts_loaded || !state.system.orchestrator_ready)
  ) {
    await delay(RUNTIME_POLL_MS);
    state = await continuum.getState();
  }

  return state;
}

async function waitForMicControl(): Promise<ContinuumState> {
  const deadline = Date.now() + CONTROL_APPLY_TIMEOUT_MS;
  let state = await continuum.getState();

  while (Date.now() < deadline) {
    const toggles = state.context.page?.toggles;
    if (toggles && !toggles.pause_all && toggles.mic) return state;
    await delay(RUNTIME_POLL_MS);
    state = await continuum.getState();
  }

  return state;
}

function voiceReadinessError(state: ContinuumState): string | null {
  const missing: string[] = [];
  if (!state.system.stt_loaded) missing.push("speech-to-text (Whisper)");
  if (!state.system.tts_loaded) missing.push("text-to-speech (Piper/Kokoros)");
  if (!state.system.orchestrator_ready) missing.push("the voice model/orchestrator");

  if (missing.length === 0) return null;

  return `Live voice is not ready: ${missing.join(", ")} ${missing.length === 1 ? "is" : "are"} unavailable. Open Voice/Setup and fix the reported component before retrying.`;
}

function livePhase(
  mode: VoiceMode,
  liveActive: boolean,
  isStarting: boolean,
  isArming: boolean,
  isArmed: boolean,
  error: string | null
): string {
  if (error) return "Voice error — click to retry";
  if (isStarting) return "Starting voice runtime…";
  if (isArming) return "Preparing microphone…";
  if (!liveActive) return "Start live voice";
  if (isArmed) return "Listening — speak now…";

  switch (mode) {
    case "listening":
      return "Listening…";
    case "thinking":
      return "Thinking…";
    case "speaking":
      return "Speaking — next turn starts automatically";
    case "muted":
      return "Preparing voice…";
    case "error":
      return "Voice error";
    default:
      return "Live voice active";
  }
}

function delay(ms: number) {
  return new Promise<void>((resolve) => setTimeout(resolve, ms));
}

function toErrorMessage(error: unknown): string {
  if (error instanceof Error && error.message.trim()) return error.message;
  const message = String(error ?? "").trim();
  return message || "Live voice could not be started.";
}

function ListeningBars({ level }: { level: number }) {
  const responsive = Math.max(0.12, Math.min(1, level));
  return (
    <div className="flex items-end gap-1">
      {[0.65, 1, 0.8].map((weight, index) => (
        <span
          key={weight}
          className="h-6 w-1 origin-bottom rounded-sm bg-accent-blue transition-transform duration-75 motion-reduce:transition-none"
          style={{ transform: `scaleY(${Math.max(0.16, responsive * weight)})` }}
          aria-hidden="true"
          data-bar={index}
        />
      ))}
    </div>
  );
}

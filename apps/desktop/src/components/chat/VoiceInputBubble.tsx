"use client";

// VoiceInputBubble — floating record indicator + audio waveform. Shown
// whenever the user hits the global Cmd+Shift+Space (or clicks the mic
// inside the InputBar). It overlays the bottom-right of the chat pane so
// it doesn't shove the layout around when voice mode starts. Silence
// detection lives in the orchestrator (see continuum-core/voice); the UI
// just mirrors the partial transcript and shows a quiet animated
// waveform. Reduced-motion users get a static dot instead of the bars.

import { useEffect, useState } from "react";
import { clsx } from "clsx";
import { X } from "lucide-react";

import { Kbd } from "@/components/ui/primitives";

interface VoiceInputBubbleProps {
  partial: string;
  open: boolean;
  onCancel: () => void;
  onCommit: (transcript: string) => void;
}

export function VoiceInputBubble({ partial, open, onCancel, onCommit }: VoiceInputBubbleProps) {
  const [elapsed, setElapsed] = useState(0);
  useEffect(() => {
    if (!open) {
      setElapsed(0);
      return;
    }
    const start = Date.now();
    const t = setInterval(() => setElapsed(Date.now() - start), 200);
    return () => clearInterval(t);
  }, [open]);

  if (!open) return null;
  return (
    <div
      role="dialog"
      aria-modal="true"
      aria-label="Voice input"
      className="pointer-events-auto fixed bottom-6 right-6 z-40 w-[320px] rounded-md border border-amber-500/30 bg-bg-surface/95 p-3 shadow-md backdrop-blur"
    >
      <div className="flex items-center gap-2 text-[11px] text-ink-muted">
        <span className="relative inline-flex h-2 w-2">
          <span className="absolute inset-0 animate-ping rounded-full bg-amber-400/40" />
          <span className="relative inline-block h-2 w-2 rounded-full bg-amber-400" />
        </span>
        <span className="font-medium tracking-wide">Listening</span>
        <span className="font-mono tabular-nums text-ink-dim">{(elapsed / 1000).toFixed(1)}s</span>
        <span className="flex-1" />
        <button
          onClick={onCancel}
          aria-label="Cancel voice input"
          className="press rounded p-1 text-ink-dim hover:bg-bg-elevated hover:text-ink"
        >
          <X size={11} />
        </button>
      </div>
      <Waveform />
      <div className="min-h-[36px] rounded-md border border-bg-border bg-bg-elevated px-2 py-1.5 text-[12.5px] text-ink">
        {partial || <span className="text-ink-dim">Speak now…</span>}
      </div>
      <div className="mt-2 flex items-center justify-between text-[10px] text-ink-dim">
        <span>
          Press <Kbd>esc</Kbd> to cancel · <Kbd>enter</Kbd> to commit
        </span>
        <button
          onClick={() => onCommit(partial.trim())}
          className="press rounded border border-amber-500/40 bg-amber-500/15 px-2 py-0.5 font-medium text-amber-300 hover:bg-amber-500/25"
        >
          Send
        </button>
      </div>
    </div>
  );
}

function Waveform() {
  // 12 bars, each animating with a staggered phase. Static on reduced motion.
  return (
    <div className="my-2.5 flex h-7 items-center justify-center gap-1" aria-hidden>
      {Array.from({ length: 12 }).map((_, i) => (
        <span
          key={i}
          className={clsx(
            "w-[2.5px] rounded-full bg-amber-400/70 motion-safe:animate-[wave_1.05s_ease-in-out_infinite]",
            "motion-reduce:opacity-60"
          )}
          style={{
            height: "100%",
            transformOrigin: "center",
            animationDelay: `${i * 70}ms`,
          }}
        />
      ))}
      <style>{`
        @keyframes wave {
          0%, 100% { transform: scaleY(0.25); opacity: 0.5; }
          50% { transform: scaleY(0.95); opacity: 1; }
        }
      `}</style>
    </div>
  );
}

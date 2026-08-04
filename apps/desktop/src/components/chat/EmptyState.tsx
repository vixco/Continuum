"use client";

// EmptyState — shown when a conversation has no messages yet, or before
// the user picks one. The design-system rule: "one-line copy + one CTA.
// No illustrations." We keep it to a single welcoming line, a row of
// suggestion prompts, and one compact kbd hint row. Everything the old
// 3-card hint grid said is folded into that kbd row so the surface stays
// calm instead of reading like a dashboard.

import { clsx } from "clsx";
import { ArrowUp } from "lucide-react";

import { Kbd } from "@/components/ui/primitives";

export function ChatEmptyState({
  hasProviders,
  hasSkills,
  onSuggest,
}: {
  hasProviders: boolean;
  hasSkills: boolean;
  onSuggest?: (text: string) => void;
}) {
  if (!hasProviders) {
    return (
      <div className="flex h-full items-center justify-center px-6">
        <div className="max-w-md space-y-2 text-center">
          <div className="text-sm font-medium text-ink">Connect a provider to start</div>
          <div className="text-xs text-ink-muted">
            Open <span className="text-ink">Settings → Integrations</span> to add a model. Once a
            provider is connected, conversations stream token-by-token from this tab.
          </div>
        </div>
      </div>
    );
  }

  return (
    <div className="flex h-full items-center justify-center px-6">
      <div className="w-full max-w-lg space-y-5">
        <div className="space-y-1.5 text-center">
          <div className="text-[15px] font-medium text-ink">Ask Continuum anything</div>
          <div className="text-xs text-ink-muted">
            I can read files, search memory, and call any of your local tools.
          </div>
        </div>
        <div className="grid gap-1.5 sm:grid-cols-2">
          {SUGGESTIONS.map((s) => (
            <button
              key={s}
              onClick={() => onSuggest?.(s)}
              className={clsx(
                "press flex items-center gap-2 rounded-md border border-bg-border bg-bg-elevated px-3 py-2 text-left text-[12px] text-ink",
                "transition-colors hover:border-amber-500/40 hover:bg-bg-hover"
              )}
            >
              <ArrowUp size={11} className="shrink-0 text-amber-400/80" />
              <span className="min-w-0 truncate">{s}</span>
            </button>
          ))}
        </div>
        <div className="flex flex-wrap items-center justify-center gap-x-4 gap-y-1.5 text-[10px] text-ink-dim">
          <span className="flex items-center gap-1">
            <Kbd>/</Kbd> skills
          </span>
          <span className="flex items-center gap-1">
            <Kbd>@</Kbd> tools
          </span>
          <span className="flex items-center gap-1">
            <Kbd>⌘⇧Space</Kbd> voice
          </span>
          {hasSkills && <span className="text-ink-dim/70">skills auto-attach when they match</span>}
        </div>
      </div>
    </div>
  );
}

const SUGGESTIONS = [
  "What's on my screen right now?",
  "Summarize the last 5 episodic events",
  "Plan a code review of the current diff",
  "Draft a quick morning briefing",
  "Tidy my Downloads folder",
  "Find files I haven't touched in 30 days",
];

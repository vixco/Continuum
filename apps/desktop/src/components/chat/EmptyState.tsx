"use client";

// EmptyState — shown when a conversation has no messages yet, or before
// the user picks one. The design-system rule: "one-line copy + one CTA.
// No illustrations." We give a single helpful nudge + a kbd hint so the
// user knows about / and @.

// (Kept in chat/ rather than ui/primitives because the rules in the
// design doc are very chat-specific — slash menu, skill hints, voice —
// and the primitive EmptyState is the generic placeholder used by other
// tabs.)

import { clsx } from "clsx";
import { ArrowUp, Mic, AtSign, Slash } from "lucide-react";

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
      <div className="max-w-lg space-y-5 text-center">
        <div className="space-y-1.5">
          <div className="text-sm font-medium text-ink">Ask Continuum anything</div>
          <div className="text-xs text-ink-muted">
            I have access to your local tools, skills, and ambient state. I can read files, search
            memory, and call any MCP tool.
          </div>
        </div>
        <div className="grid grid-cols-2 gap-2 text-left sm:grid-cols-3">
          <Hint icon={<Slash size={11} />} title="Skills">
            Type <Kbd>/</Kbd> to browse installed skills.
          </Hint>
          <Hint icon={<AtSign size={11} />} title="Tools">
            Type <Kbd>@</Kbd> to invoke a specific tool by name.
          </Hint>
          <Hint icon={<Mic size={11} />} title="Voice">
            Press <Kbd>⌘⇧Space</Kbd> for hands-free input.
          </Hint>
        </div>
        <div className="space-y-1.5">
          <div className="text-[10px] font-semibold uppercase tracking-wider text-ink-dim">
            Try one of these
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
                <ArrowUp size={11} className="text-amber-400/80" />
                <span className="min-w-0 truncate">{s}</span>
              </button>
            ))}
          </div>
        </div>
        {hasSkills && (
          <div className="text-[10px] text-ink-dim">
            Tip: installed skills (file-organizer, code-review, daily-briefing) auto-attach when
            your request matches their description.
          </div>
        )}
      </div>
    </div>
  );
}

function Hint({
  icon,
  title,
  children,
}: {
  icon: React.ReactNode;
  title: string;
  children: React.ReactNode;
}) {
  return (
    <div className="rounded-md border border-bg-border bg-bg-elevated px-2.5 py-2">
      <div className="mb-1 flex items-center gap-1.5 text-[10px] font-semibold uppercase tracking-wider text-ink-dim">
        {icon}
        {title}
      </div>
      <div className="text-[11.5px] leading-snug text-ink-muted">{children}</div>
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

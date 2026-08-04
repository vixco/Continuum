"use client";

// SkillChip — pill shown in the input bar for each skill the user toggled
// on for *this* turn. Stays compact: a small "x" clears it; click to
// deactivate via the store. Tap to remove. Long-term "always on" lives in
// the Skills config tab; this is per-turn ambient state.

import { clsx } from "clsx";
import { X, Sparkles } from "lucide-react";

import type { ChatSkillChip as Skill } from "./types";

interface SkillChipProps {
  skill: Skill;
  onRemove: () => void;
  compact?: boolean;
}

export function SkillChip({ skill, onRemove, compact }: SkillChipProps) {
  return (
    <span
      className={clsx(
        "press inline-flex items-center gap-1 rounded-md border border-amber-500/25 bg-amber-500/[0.08] text-amber-200/90",
        compact ? "px-1.5 py-0.5 text-[10px]" : "px-2 py-0.5 text-[11px]"
      )}
    >
      <Sparkles size={compact ? 9 : 10} strokeWidth={1.8} />
      <span className="font-medium">{skill.name}</span>
      <button
        type="button"
        onClick={onRemove}
        aria-label={`Remove ${skill.name}`}
        className="press -mr-0.5 ml-0.5 rounded-sm p-0.5 text-amber-200/60 hover:bg-amber-500/15 hover:text-amber-100"
      >
        <X size={9} strokeWidth={2} />
      </button>
    </span>
  );
}

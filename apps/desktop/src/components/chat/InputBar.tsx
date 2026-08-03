"use client";

// InputBar — the composer row at the bottom of the chat pane.
//
// Responsibilities:
//   - Multiline auto-grow textarea (Enter sends, Shift+Enter newline).
//   - Slash menu pop-up (`/`) for skills/tools/built-in commands.
//   - Skill chip row for active (per-turn) skills.
//   - Attachment chips with remove buttons.
//   - Voice toggle (mic icon) for ambient capture.
//   - Send / Stop button (label flips based on `isStreaming`).
//
// The component is fully controlled by the chat store; it never owns
// composer text, attachments, or skill chips. This keeps the ambient
// state machine (composing → streaming → done → composing) entirely in
// the store and lets the slash menu / skill chips stay in sync with
// every other surface that might edit them in the future.

import { useEffect, useMemo, useRef } from "react";
import { clsx } from "clsx";
import { ArrowUp, Mic, Square, Paperclip, X } from "lucide-react";

import { Kbd } from "@/components/ui/primitives";
import { useChatStore } from "./state";
import { SkillChip } from "./SkillChip";
import { SlashCommandMenu, BUILTIN_COMMANDS } from "./SlashCommandMenu";
import type { ChatAttachment, SlashCommand } from "./types";

interface InputBarProps {
  disabled?: boolean;
  placeholder?: string;
  /** Global Cmd+Shift+Space listener is set up by the tab; InputBar
   *  also exposes a mic button. */
  onToggleVoice: () => void;
  voiceListening: boolean;
}

const MAX_HEIGHT_PX = 220;

export function InputBar({ disabled, placeholder, onToggleVoice, voiceListening }: InputBarProps) {
  const composerText = useChatStore((s) => s.composerText);
  const setComposer = useChatStore((s) => s.setComposer);
  const attachments = useChatStore((s) => s.composerAttachments);
  const addAttachment = useChatStore((s) => s.addAttachment);
  const removeAttachment = useChatStore((s) => s.removeAttachment);
  const activeSkills = useChatStore((s) => s.activeSkills);
  const toggleSkill = useChatStore((s) => s.toggleSkill);
  const send = useChatStore((s) => s.send);
  const cancel = useChatStore((s) => s.cancel);
  const sendingId = useChatStore((s) => s.sendingId);
  const activeId = useChatStore((s) => s.activeId);
  const slashOpen = useChatStore((s) => s.slashMenuOpen);
  const slashQuery = useChatStore((s) => s.slashMenuQuery);
  const setSlashMenu = useChatStore((s) => s.setSlashMenu);
  const skills = useChatStore((s) => s.skills);

  const ref = useRef<HTMLTextAreaElement | null>(null);
  const fileInputRef = useRef<HTMLInputElement | null>(null);

  // Build the slash menu options from the catalog of skills + tools +
  // built-in commands. Tools aren't loaded yet (the chat tab can derive
  // them from the same MCP_NAMESPACES the Tools tab uses), so we keep
  // a small list of well-known names for now.
  const slashOptions = useMemo<SlashCommand[]>(() => {
    const opts: SlashCommand[] = [...BUILTIN_COMMANDS];
    for (const s of skills) {
      opts.push({
        id: `skill_${s.name}`,
        label: `/${s.name}`,
        hint: s.description,
        kind: "skill",
        insert: `/${s.name}`,
        searchHaystack: `${s.name} ${s.description}`,
      });
    }
    return opts;
  }, [skills]);

  const isStreaming = sendingId != null && sendingId === activeId;
  const canSend = !disabled && !isStreaming && (composerText.trim().length > 0 || attachments.length > 0);

  // Auto-grow the textarea up to MAX_HEIGHT_PX. We deliberately don't use
  // `rows` because the browser resets it on every input event.
  useEffect(() => {
    const el = ref.current;
    if (!el) return;
    el.style.height = "auto";
    el.style.height = `${Math.min(MAX_HEIGHT_PX, el.scrollHeight)}px`;
  }, [composerText]);

  // Focus on tab activation so the user can type immediately.
  useEffect(() => {
    if (disabled) return;
    ref.current?.focus();
  }, [disabled, activeId]);

  function handleKeyDown(e: React.KeyboardEvent<HTMLTextAreaElement>) {
    if (e.key === "Enter" && !e.shiftKey) {
      e.preventDefault();
      if (canSend) void send();
      return;
    }
    if (e.key === "Escape") {
      if (slashOpen) setSlashMenu(false);
    }
  }

  function pickSlash(cmd: SlashCommand) {
    if (cmd.id === "cmd_clear") {
      setComposer("");
      setSlashMenu(false);
      return;
    }
    if (cmd.id === "cmd_voice") {
      setSlashMenu(false);
      onToggleVoice();
      return;
    }
    if (cmd.id === "cmd_cancel") {
      setSlashMenu(false);
      void cancel();
      return;
    }
    if (cmd.id === "cmd_help") {
      // Surface a help note into the composer; user can extend if needed.
      setComposer("Show me the available skills, tools, and keyboard shortcuts.");
      setSlashMenu(false);
      return;
    }
    if (cmd.kind === "skill") {
      // Skill activation: insert the skill name into active chips rather
      // than the composer text. The model will see the active-skills
      // header prefix from the store on send.
      const name = cmd.insert.replace(/^\//, "").trim();
      if (name) {
        const exists = activeSkills.some((s) => s.name === name);
        if (!exists) void toggleSkill(name);
      }
      setComposer("");
      setSlashMenu(false);
      return;
    }
    // Generic command: drop the inserted text into the composer.
    setComposer(cmd.insert + " ");
    setSlashMenu(false);
  }

  function pickFile() {
    fileInputRef.current?.click();
  }

  function onFiles(e: React.ChangeEvent<HTMLInputElement>) {
    const files = Array.from(e.target.files ?? []);
    for (const f of files) {
      const att: ChatAttachment = {
        kind: f.type.startsWith("image/") ? "image" : "file",
        name: f.name,
        path: (f as File & { path?: string }).path,
        size: f.size,
      };
      addAttachment(att);
    }
    e.target.value = "";
  }

  return (
    <div className="border-t border-bg-border bg-bg-surface/40 px-3 py-2.5">
      <div className="relative mx-auto max-w-3xl">
        <SlashCommandMenu
          open={slashOpen}
          query={slashQuery}
          commands={slashOptions}
          onPick={pickSlash}
          onClose={() => setSlashMenu(false)}
        />

        {(activeSkills.length > 0 || attachments.length > 0) && (
          <div className="mb-1.5 flex flex-wrap items-center gap-1.5">
            {activeSkills.map((s) => (
              <SkillChip key={s.name} skill={s} onRemove={() => void toggleSkill(s.name)} />
            ))}
            {attachments.map((a, i) => (
              <span
                key={`${a.name}-${i}`}
                className="press inline-flex items-center gap-1 rounded-md border border-bg-border bg-bg-elevated px-1.5 py-0.5 text-[10px] text-ink-muted"
              >
                <Paperclip size={9} className="text-ink-dim" />
                <span className="max-w-[160px] truncate">{a.name}</span>
                <button
                  type="button"
                  onClick={() => removeAttachment(i)}
                  className="press -mr-0.5 ml-0.5 rounded-sm p-0.5 text-ink-dim hover:bg-bg-hover hover:text-ink"
                  aria-label={`Remove ${a.name}`}
                >
                  <X size={9} />
                </button>
              </span>
            ))}
          </div>
        )}

        <div
          className={clsx(
            "flex items-end gap-1.5 rounded-md border bg-bg-elevated px-2 py-1.5 transition-colors",
            "border-bg-border focus-within:border-amber-500/40 focus-within:bg-bg-surface"
          )}
        >
          <button
            type="button"
            onClick={pickFile}
            aria-label="Attach file"
            className="press shrink-0 rounded p-1.5 text-ink-dim hover:bg-bg-hover hover:text-ink"
            title="Attach file (pasted screenshots also work)"
          >
            <Paperclip size={13} />
          </button>
          <input
            ref={fileInputRef}
            type="file"
            multiple
            hidden
            onChange={onFiles}
          />
          <textarea
            ref={ref}
            value={composerText}
            onChange={(e) => setComposer(e.target.value)}
            onKeyDown={handleKeyDown}
            rows={1}
            placeholder={
              disabled
                ? "Select or create a conversation to start…"
                : placeholder ?? "Ask Continuum…"
            }
            disabled={disabled}
            spellCheck
            className={clsx(
              "min-h-[28px] max-h-[220px] flex-1 resize-none bg-transparent px-1.5 py-1.5 text-[13.5px] leading-snug text-ink outline-none placeholder:text-ink-dim",
              "disabled:opacity-50"
            )}
          />
          <button
            type="button"
            onClick={onToggleVoice}
            aria-label={voiceListening ? "Stop voice" : "Start voice"}
            className={clsx(
              "press shrink-0 rounded p-1.5 transition-colors",
              voiceListening
                ? "bg-amber-500/20 text-amber-300"
                : "text-ink-dim hover:bg-bg-hover hover:text-ink"
            )}
            title="Voice input (⌘⇧Space)"
          >
            <Mic size={13} />
          </button>
          {isStreaming ? (
            <button
              type="button"
              onClick={() => void cancel()}
              aria-label="Stop generation"
              className="press shrink-0 rounded-md border border-state-error/40 bg-state-error/[0.12] px-2.5 py-1.5 text-[11px] font-medium text-state-error hover:bg-state-error/[0.18]"
              title="Stop (esc)"
            >
              <Square size={10} className="mr-0.5 inline" />
              Stop
            </button>
          ) : (
            <button
              type="button"
              onClick={() => void send()}
              disabled={!canSend}
              aria-label="Send message"
              className={clsx(
                "press shrink-0 rounded-md border border-amber-500/40 bg-amber-500 px-2.5 py-1.5 text-[11px] font-medium text-black transition-all",
                "hover:brightness-105 disabled:cursor-not-allowed disabled:opacity-40 disabled:saturate-50"
              )}
            >
              <ArrowUp size={11} className="mr-0.5 inline" />
              Send
              <Kbd className="ml-1.5 !bg-black/20 !text-black/70 !border-black/20">⏎</Kbd>
            </button>
          )}
        </div>
        <div className="mt-1 flex items-center justify-between px-0.5 text-[10px] text-ink-dim">
          <span>
            <Kbd>enter</Kbd> to send · <Kbd>shift+enter</Kbd> for newline · <Kbd>/</Kbd> for skills
          </span>
          <span className="font-mono tabular-nums">
            {composerText.length > 0 ? `${composerText.length} chars` : ""}
          </span>
        </div>
      </div>
    </div>
  );
}

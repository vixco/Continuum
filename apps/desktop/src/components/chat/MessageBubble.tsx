"use client";

// MessageBubble — role-aware render of a single chat turn. Assistant
// messages route through MarkdownText (so streaming deltas get the same
// code/list treatment as persisted ones); user messages are a clean
// right-aligned block. Tool calls and thinking blocks are interleaved via
// `parts`, so a single message can render text + collapsible thinking + a
// stack of tool cards in its row.

import { clsx } from "clsx";
import { Bot, User, AlertTriangle } from "lucide-react";

import { ThinkingBlock } from "./ThinkingBlock";
import { ToolInvocationCard } from "./ToolInvocationCard";
import { ToolGroup } from "./ToolGroup";
import { StreamingText } from "./StreamingText";
import type { ChatMessage, ContentPart } from "./types";

interface MessageBubbleProps {
  message: ChatMessage;
  /** True if the message is the last assistant turn and is currently
   *  streaming — used to skip markdown caching on the live tail. */
  isStreaming?: boolean;
  /** True if this is the last user message in the conversation. Used for
   *  the ambient "stop" affordance below the input. */
  isLast?: boolean;
}

export function MessageBubble({ message, isStreaming }: MessageBubbleProps) {
  if (message.role === "user") {
    return <UserBubble message={message} />;
  }
  if (message.role === "tool") {
    return <ToolBubble message={message} />;
  }
  return <AssistantBubble message={message} isStreaming={isStreaming} />;
}

function UserBubble({ message }: { message: ChatMessage }) {
  return (
    <div className="flex animate-[msg-in_180ms_ease-out] justify-end">
      <div className="max-w-[78%] space-y-2">
        {message.attachments && message.attachments.length > 0 && (
          <div className="flex flex-wrap justify-end gap-1.5">
            {message.attachments.map((a, i) => (
              <AttachmentChip key={i} attachment={a} />
            ))}
          </div>
        )}
        {message.text && (
          <div className="rounded-lg border border-amber-500/25 bg-amber-500/[0.07] px-3.5 py-2.5 text-sm leading-relaxed text-ink shadow-[inset_0_1px_0_rgba(255,255,255,0.02)]">
            {message.text}
          </div>
        )}
        <div className="flex items-center justify-end gap-1.5 text-[10px] text-ink-dim">
          <User size={10} strokeWidth={1.8} />
          <span>{formatTime(message.ts)}</span>
        </div>
      </div>
    </div>
  );
}

function AssistantBubble({
  message,
  isStreaming,
}: {
  message: ChatMessage;
  isStreaming?: boolean;
}) {
  const isError = message.status === "error";
  // "Thinking" = the assistant turn is live but has produced no visible
  // content yet (no streamed text, no tool calls, no thinking block). This
  // is the window between send and the first token — show breathing dots
  // so the user sees the AI is busy, not a frozen empty bubble. As soon as
  // the first delta or tool_call lands, this flips off and the real content
  // (with the streaming cursor) takes over.
  const isThinking =
    Boolean(isStreaming) &&
    !isError &&
    (message.text?.length ?? 0) === 0 &&
    !message.parts.some(
      (p) => p.kind === "tool" || p.kind === "tool_group" || p.kind === "thinking"
    );
  return (
    <div className="flex animate-[msg-in_180ms_ease-out] justify-start">
      <div className="w-full max-w-full space-y-2">
        <div className="flex items-center gap-2 text-[10px] text-ink-dim">
          <Bot size={11} strokeWidth={1.8} className="text-amber-400/80" />
          <span className="font-medium text-ink-muted">Continuum</span>
          {message.model && (
            <>
              <span className="text-ink-dim/60">·</span>
              <span className="font-mono">{message.model}</span>
            </>
          )}
          {message.durationMs != null && (
            <>
              <span className="text-ink-dim/60">·</span>
              <span className="font-mono tabular-nums">
                {(message.durationMs / 1000).toFixed(1)}s
              </span>
            </>
          )}
          {message.usage?.input != null && message.usage?.output != null && (
            <>
              <span className="text-ink-dim/60">·</span>
              <span className="font-mono tabular-nums">
                {message.usage.input}↑ {message.usage.output}↓
              </span>
            </>
          )}
          {isStreaming && (
            <span className="ml-1 inline-flex h-1.5 w-1.5 animate-pulse rounded-full bg-amber-400" />
          )}
        </div>
        {isError && (
          <div className="flex items-center gap-2 rounded-md border border-state-error/30 bg-state-error/[0.08] px-3 py-2 text-xs text-state-error">
            <AlertTriangle size={12} strokeWidth={1.8} />
            <span>{message.text || "Generation failed"}</span>
          </div>
        )}
        {!isError && (
          <div className="rounded-lg border border-bg-border bg-bg-surface px-3.5 py-3 text-sm text-ink shadow-[inset_0_1px_0_rgba(255,255,255,0.015)]">
            {isThinking ? (
              <ThinkingDots />
            ) : (
              <Parts parts={message.parts} isStreaming={isStreaming} />
            )}
          </div>
        )}
        {message.status === "aborted" && <div className="text-[10px] text-ink-dim">stopped</div>}
      </div>
    </div>
  );
}

/// Three breathing dots shown while the model is working but has not yet
/// emitted its first token (or tool call). Replaced by streaming text the
/// moment content arrives. `prefers-reduced-motion` collapses the animation
/// to a static dim row so it never fights a motion-sensitive user.
function ThinkingDots() {
  return (
    <div className="flex items-center gap-1" aria-label="Continuum is thinking" role="status">
      {[0, 1, 2].map((i) => (
        <span
          key={i}
          className="inline-block h-1.5 w-1.5 rounded-full bg-amber-400/70 motion-safe:animate-[chat-thinking-dot_1.2s_ease-in-out_infinite]"
          style={{ animationDelay: `${i * 0.18}s` }}
        />
      ))}
    </div>
  );
}

function ToolBubble({ message }: { message: ChatMessage }) {
  // Standalone system/tool messages (rare; tool cards normally live inside
  // an assistant turn). Rendered as a quiet monospace line.
  return (
    <div className="flex animate-[msg-in_180ms_ease-out] justify-start">
      <div className="rounded-md border border-bg-border bg-bg-elevated px-3 py-2 font-mono text-[11px] text-ink-muted">
        {message.text}
      </div>
    </div>
  );
}

function Parts({ parts, isStreaming }: { parts: ContentPart[]; isStreaming?: boolean }) {
  return (
    <div className="space-y-2.5">
      {parts.map((part, i) => {
        if (part.kind === "text") {
          return (
            <StreamingText
              key={i}
              text={part.text}
              isStreaming={Boolean(isStreaming) && i === parts.length - 1}
            />
          );
        }
        if (part.kind === "thinking") {
          return <ThinkingBlock key={i} block={part.block} />;
        }
        if (part.kind === "tool") {
          return <ToolInvocationCard key={i} invocation={part.invocation} />;
        }
        // tool_group
        return <ToolGroup key={i} group={part} />;
      })}
    </div>
  );
}

function AttachmentChip({
  attachment,
}: {
  attachment: { name: string; kind: "file" | "image"; size?: number };
}) {
  return (
    <div
      className={clsx(
        "flex items-center gap-1.5 rounded-md border border-bg-border bg-bg-elevated px-2 py-1 text-[10px] text-ink-muted"
      )}
    >
      <span className="font-mono">{attachment.kind === "image" ? "img" : "file"}</span>
      <span className="max-w-[160px] truncate">{attachment.name}</span>
      {attachment.size != null && (
        <span className="text-ink-dim/70">{formatBytes(attachment.size)}</span>
      )}
    </div>
  );
}

function formatTime(ts: string): string {
  try {
    return new Date(ts).toLocaleTimeString([], { hour: "2-digit", minute: "2-digit" });
  } catch {
    return ts;
  }
}

function formatBytes(b: number): string {
  if (b < 1024) return `${b}B`;
  if (b < 1024 * 1024) return `${(b / 1024).toFixed(1)}KB`;
  return `${(b / 1024 / 1024).toFixed(1)}MB`;
}

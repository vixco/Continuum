"use client";

// MessageList — virtualized message rendering.
//
// Why virtualize: a long session with the orchestrator can produce 200+
// tool calls in a single turn. A flat map of MessageBubble components
// would re-mount and re-parse the entire list on every delta. With
// react-virtuoso, only the visible window + a small buffer is in the
// DOM, so streaming stays smooth even with hundreds of turns.
//
// The streaming tail follows the bottom of the scroll region by default
// (followOutput="smooth") — if the user scrolls up to read an earlier
// tool card, Virtuoso pauses autoscroll and they can scroll back down
// without the live tail grabbing focus.

import { useEffect, useMemo, useRef } from "react";
import { Virtuoso, type VirtuosoHandle } from "react-virtuoso";

import { MessageBubble } from "./MessageBubble";
import type { ChatMessage } from "./types";

interface MessageListProps {
  messages: ChatMessage[];
  /** The "ephemeral" tail message currently streaming. When present, the
   *  live cursor stays pinned to it and the list autoscrolls as it grows.
   *  When absent (stream finished or never started), the last persisted
   *  message is the tail and the user can scroll freely. */
  streamingMessage?: ChatMessage | null;
  isStreaming: boolean;
}

export function MessageList({ messages, streamingMessage, isStreaming }: MessageListProps) {
  const ref = useRef<VirtuosoHandle | null>(null);

  // The list we render = persisted messages + (optional) ephemeral tail.
  const data = useMemo<ChatMessage[]>(
    () => (streamingMessage ? [...messages, streamingMessage] : messages),
    [messages, streamingMessage]
  );

  // Snap to bottom on first mount (after the persisted list loads).
  useEffect(() => {
    if (data.length > 0) {
      ref.current?.scrollToIndex({ index: data.length - 1, align: "end" });
    }
    // eslint-disable-next-line react-hooks/exhaustive-deps
  }, []);

  return (
    <Virtuoso
      ref={ref}
      data={data}
      followOutput={isStreaming ? "smooth" : false}
      initialTopMostItemIndex={Math.max(0, data.length - 1)}
      itemContent={(index, message) => (
        <div className="px-1 pb-5">
          <MessageBubble
            message={message}
            isStreaming={Boolean(streamingMessage) && index === data.length - 1}
            isLast={index === data.length - 1}
          />
        </div>
      )}
      components={{
        // Empty list placeholder is rendered by the tab itself, not here.
        Footer: () => (isStreaming ? <div className="h-2" /> : null),
      }}
      className="h-full"
    />
  );
}

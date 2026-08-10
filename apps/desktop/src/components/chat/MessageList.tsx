"use client";

// MessageList — virtualized message rendering with user-intent-aware
// stick-to-bottom behavior. A streaming response grows inside one Virtuoso
// item, so physical "at bottom" state is not enough: item resize can move the
// bottom away without the user scrolling. The scroll-state helper keeps that
// resize drift separate from a deliberate upward scroll.

import { useCallback, useEffect, useLayoutEffect, useMemo, useRef, useState } from "react";
import { ArrowDown } from "lucide-react";
import { Virtuoso, type VirtuosoHandle } from "react-virtuoso";

import { MessageBubble } from "./MessageBubble";
import {
  createChatScrollSnapshot,
  observeAtBottomSignal,
  observeChatContent,
  observeChatScroll,
  requestLatest,
  shouldFollowChatOutput,
  type ChatScrollSnapshot,
} from "./scrollState";
import type { ChatMessage } from "./types";

interface MessageListProps {
  messages: ChatMessage[];
  /** The ephemeral assistant tail that grows during streaming. */
  streamingMessage?: ChatMessage | null;
  isStreaming: boolean;
}

interface ScrollUiState {
  atBottom: boolean;
  hasUnseenContent: boolean;
}

export function MessageList({ messages, streamingMessage, isStreaming }: MessageListProps) {
  const virtuosoRef = useRef<VirtuosoHandle | null>(null);
  const scrollerCleanupRef = useRef<(() => void) | null>(null);
  const followFrameRef = useRef<number | null>(null);
  const conversationRef = useRef<string | null>(null);
  const scrollRef = useRef<ChatScrollSnapshot>(createChatScrollSnapshot());
  const [scrollUi, setScrollUi] = useState<ScrollUiState>({
    atBottom: true,
    hasUnseenContent: false,
  });

  const conversationId = streamingMessage?.conversationId ?? messages[0]?.conversationId ?? "empty";

  const data = useMemo<ChatMessage[]>(
    () => (streamingMessage ? [...messages, streamingMessage] : messages),
    [messages, streamingMessage]
  );

  const publishScrollState = useCallback((next: ChatScrollSnapshot) => {
    scrollRef.current = next;
    setScrollUi((current) => {
      if (
        current.atBottom === next.atBottom &&
        current.hasUnseenContent === next.hasUnseenContent
      ) {
        return current;
      }
      return { atBottom: next.atBottom, hasUnseenContent: next.hasUnseenContent };
    });
  }, []);

  const scrollToLatest = useCallback(
    (behavior: "auto" | "smooth") => {
      if (data.length === 0) return;
      if (followFrameRef.current !== null) cancelAnimationFrame(followFrameRef.current);
      followFrameRef.current = requestAnimationFrame(() => {
        followFrameRef.current = null;
        virtuosoRef.current?.scrollToIndex({
          index: data.length - 1,
          align: "end",
          behavior,
        });
      });
    },
    [data.length]
  );

  const setScrollerRef = useCallback(
    (target: HTMLElement | Window | null) => {
      scrollerCleanupRef.current?.();
      scrollerCleanupRef.current = null;
      if (!(target instanceof HTMLElement)) return;

      const readMetrics = () => ({
        scrollTop: target.scrollTop,
        scrollHeight: target.scrollHeight,
        clientHeight: target.clientHeight,
      });
      const onScroll = () => {
        publishScrollState(observeChatScroll(scrollRef.current, readMetrics()));
      };
      const onWheel = (event: WheelEvent) => {
        if (event.deltaY >= 0) return;
        publishScrollState({
          ...scrollRef.current,
          atBottom: false,
          pinnedToBottom: false,
        });
      };

      target.addEventListener("scroll", onScroll, { passive: true });
      target.addEventListener("wheel", onWheel, { passive: true });
      onScroll();

      scrollerCleanupRef.current = () => {
        target.removeEventListener("scroll", onScroll);
        target.removeEventListener("wheel", onWheel);
      };
    },
    [publishScrollState]
  );

  useEffect(
    () => () => {
      scrollerCleanupRef.current?.();
      if (followFrameRef.current !== null) cancelAnimationFrame(followFrameRef.current);
    },
    []
  );

  // Follow both appended messages and growth of the existing streaming tail.
  // The latter is the case `followOutput` alone cannot distinguish from a
  // user scrolling away. One RAF coalesces rapid token/tool updates and avoids
  // stacking smooth-scroll animations while text is streaming.
  useLayoutEffect(() => {
    const changedConversation = conversationRef.current !== conversationId;
    if (changedConversation) {
      conversationRef.current = conversationId;
      const reset = createChatScrollSnapshot(scrollRef.current.lastScrollTop);
      publishScrollState(reset);
      if (data.length > 0) scrollToLatest("auto");
      return;
    }
    if (data.length === 0) return;

    const next = observeChatContent(scrollRef.current);
    publishScrollState(next);
    if (shouldFollowChatOutput(next)) scrollToLatest("auto");
  }, [
    conversationId,
    data.length,
    messages.length,
    publishScrollState,
    scrollToLatest,
    streamingMessage,
  ]);

  const jumpToLatest = () => {
    publishScrollState(requestLatest(scrollRef.current));
    scrollToLatest("smooth");
  };

  const showJump = (!scrollUi.atBottom || scrollUi.hasUnseenContent) && data.length > 0;

  return (
    <div className="relative h-full">
      <Virtuoso
        ref={virtuosoRef}
        data={data}
        scrollerRef={setScrollerRef}
        followOutput={false}
        computeItemKey={(index, message) => message.id || index}
        atBottomStateChange={(atBottom) =>
          publishScrollState(observeAtBottomSignal(scrollRef.current, atBottom))
        }
        itemContent={(index, message) => (
          <div className="mx-auto w-full max-w-3xl px-4 pb-5">
            <MessageBubble
              message={message}
              isStreaming={Boolean(streamingMessage) && index === data.length - 1}
              isLast={index === data.length - 1}
            />
          </div>
        )}
        components={{
          Footer: () => (isStreaming ? <div className="h-2" /> : null),
        }}
        className="h-full"
      />
      {showJump && (
        <button
          type="button"
          onClick={jumpToLatest}
          aria-label={scrollUi.hasUnseenContent ? "Show new messages" : "Jump to latest"}
          className="press absolute bottom-4 left-1/2 z-10 flex -translate-x-1/2 items-center gap-1.5 rounded-full border border-bg-border bg-bg-elevated px-3 py-1.5 text-[11px] font-medium text-ink shadow-lg transition-colors hover:border-amber-500/40 hover:text-ink"
        >
          <ArrowDown size={12} className="text-amber-400/80" />
          {scrollUi.hasUnseenContent ? "New messages" : "Latest"}
        </button>
      )}
    </div>
  );
}

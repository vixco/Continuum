/**
 * User-intent state for the virtualized chat transcript.
 *
 * A streaming assistant response grows inside the last Virtuoso item. That
 * increases scrollHeight without necessarily emitting a user scroll. We keep
 * "pinned" separate from the physical at-bottom flag so content growth cannot
 * accidentally opt the user out of following the stream.
 */

export const CHAT_BOTTOM_THRESHOLD_PX = 48;
export const CHAT_SCROLL_DIRECTION_EPSILON_PX = 1;

export interface ChatScrollMetrics {
  scrollTop: number;
  scrollHeight: number;
  clientHeight: number;
}

export interface ChatScrollSnapshot {
  /** The viewport is physically within the bottom threshold. */
  atBottom: boolean;
  /** New output may move the viewport to the bottom. */
  pinnedToBottom: boolean;
  /** Output arrived while the user intentionally read older content. */
  hasUnseenContent: boolean;
  /** Last observed scroll offset, used to detect an upward user gesture. */
  lastScrollTop: number;
}

export function createChatScrollSnapshot(lastScrollTop = 0): ChatScrollSnapshot {
  return {
    atBottom: true,
    pinnedToBottom: true,
    hasUnseenContent: false,
    lastScrollTop,
  };
}

export function distanceFromBottom(metrics: ChatScrollMetrics): number {
  return Math.max(0, metrics.scrollHeight - metrics.clientHeight - metrics.scrollTop);
}

/**
 * Update scroll intent from a native scroll event.
 *
 * Only a real upward movement disables pinning. A resize of the streaming
 * item can make `atBottom` false while `scrollTop` stays unchanged; treating
 * that as user intent is the root of the jump-away-from-latest regression.
 */
export function observeChatScroll(
  snapshot: ChatScrollSnapshot,
  metrics: ChatScrollMetrics,
  threshold = CHAT_BOTTOM_THRESHOLD_PX
): ChatScrollSnapshot {
  const atBottom = distanceFromBottom(metrics) <= threshold;
  const movedUp = metrics.scrollTop < snapshot.lastScrollTop - CHAT_SCROLL_DIRECTION_EPSILON_PX;

  return {
    atBottom,
    pinnedToBottom: atBottom ? true : movedUp ? false : snapshot.pinnedToBottom,
    hasUnseenContent: atBottom ? false : snapshot.hasUnseenContent,
    lastScrollTop: metrics.scrollTop,
  };
}

/** Record output without confusing content growth with a user scroll. */
export function observeChatContent(snapshot: ChatScrollSnapshot): ChatScrollSnapshot {
  if (snapshot.pinnedToBottom) {
    return { ...snapshot, hasUnseenContent: false };
  }
  return { ...snapshot, hasUnseenContent: true };
}

/** A positive bottom signal is safe; a negative one may only be resize drift. */
export function observeAtBottomSignal(
  snapshot: ChatScrollSnapshot,
  atBottom: boolean
): ChatScrollSnapshot {
  if (!atBottom) return { ...snapshot, atBottom: false };
  return {
    ...snapshot,
    atBottom: true,
    pinnedToBottom: true,
    hasUnseenContent: false,
  };
}

export function requestLatest(snapshot: ChatScrollSnapshot): ChatScrollSnapshot {
  return {
    ...snapshot,
    atBottom: true,
    pinnedToBottom: true,
    hasUnseenContent: false,
  };
}

export function shouldFollowChatOutput(snapshot: ChatScrollSnapshot): boolean {
  return snapshot.pinnedToBottom;
}

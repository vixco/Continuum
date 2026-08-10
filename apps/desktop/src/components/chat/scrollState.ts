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
 * Explicit movement wins over the physical bottom threshold. A small upward
 * gesture can remain inside that threshold, but it is still user intent and
 * must disable following. Conversely, an unpinned reader resumes following
 * only after actually moving down into the bottom region. Streaming-item
 * resize drift leaves the prior intent unchanged.
 */
export function observeChatScroll(
  snapshot: ChatScrollSnapshot,
  metrics: ChatScrollMetrics,
  threshold = CHAT_BOTTOM_THRESHOLD_PX
): ChatScrollSnapshot {
  const atBottom = distanceFromBottom(metrics) <= threshold;
  const movedUp = metrics.scrollTop < snapshot.lastScrollTop - CHAT_SCROLL_DIRECTION_EPSILON_PX;
  const movedDown = metrics.scrollTop > snapshot.lastScrollTop + CHAT_SCROLL_DIRECTION_EPSILON_PX;

  let pinnedToBottom = snapshot.pinnedToBottom;
  if (movedUp) {
    pinnedToBottom = false;
  } else if (movedDown && atBottom) {
    pinnedToBottom = true;
  }

  return {
    atBottom,
    pinnedToBottom,
    hasUnseenContent: pinnedToBottom ? false : snapshot.hasUnseenContent,
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

/**
 * A negative Virtuoso signal may be resize drift. A positive signal updates
 * physical position but cannot override an explicit unpinned reader intent;
 * native downward movement or the latest-message action performs that change.
 */
export function observeAtBottomSignal(
  snapshot: ChatScrollSnapshot,
  atBottom: boolean
): ChatScrollSnapshot {
  if (!atBottom) return { ...snapshot, atBottom: false };
  if (!snapshot.pinnedToBottom) return { ...snapshot, atBottom: true };
  return {
    ...snapshot,
    atBottom: true,
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

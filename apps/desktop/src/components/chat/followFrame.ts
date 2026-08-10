export interface AnimationFrameScheduler {
  requestFrame(callback: () => void): number;
  cancelFrame(handle: number): void;
}

export interface FollowFrameController {
  schedule(task: () => void): void;
  cancel(): void;
  hasPending(): boolean;
}

/**
 * Coalesces streaming follow requests and makes cancellation authoritative.
 * The identity guard also blocks a canceled callback if an implementation
 * delivers it after `cancelFrame`, which closes the user-scroll race.
 */
export function createFollowFrameController(
  scheduler: AnimationFrameScheduler
): FollowFrameController {
  let pendingHandle: number | null = null;

  const cancel = () => {
    if (pendingHandle === null) return;
    scheduler.cancelFrame(pendingHandle);
    pendingHandle = null;
  };

  return {
    schedule(task) {
      cancel();
      let handle = -1;
      handle = scheduler.requestFrame(() => {
        if (pendingHandle !== handle) return;
        pendingHandle = null;
        task();
      });
      pendingHandle = handle;
    },
    cancel,
    hasPending() {
      return pendingHandle !== null;
    },
  };
}

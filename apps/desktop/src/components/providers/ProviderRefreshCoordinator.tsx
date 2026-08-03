"use client";

import { useEffect } from "react";

import { refreshAllProviderModels } from "@/lib/providers";
import { continuum, isTauri } from "@/lib/tauri";

/** Keeps cached provider model lists fresh while the desktop shell is open. */
export function ProviderRefreshCoordinator() {
  useEffect(() => {
    let cancelled = false;
    let timer: ReturnType<typeof setInterval> | undefined;

    void (async () => {
      if (!(await isTauri()) || cancelled) return;
      const config = await continuum.getConfig().catch(() => null);
      const seconds = config?.chat.model_refresh_interval_secs ?? 300;
      if (seconds === 0 || cancelled) return;
      timer = setInterval(
        () => {
          void refreshAllProviderModels().catch(() => {
            // Automatic refresh is best-effort. Manual refresh surfaces failures inline.
          });
        },
        Math.max(30, seconds) * 1000
      );
    })();

    return () => {
      cancelled = true;
      if (timer) clearInterval(timer);
    };
  }, []);

  return null;
}

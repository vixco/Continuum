"use client";

import { useEffect, useState } from "react";
import { clsx } from "clsx";

interface StartupIntroProps {
  onComplete: () => void;
}

/**
 * Brief launch treatment shown above the already-mounting application.
 *
 * It is intentionally decorative: the intro never reports progress or blocks
 * application bootstrap work, and reduced-motion users get a near-instant exit.
 */
export function StartupIntro({ onComplete }: StartupIntroProps) {
  const [leaving, setLeaving] = useState(false);

  useEffect(() => {
    const reducedMotion = window.matchMedia("(prefers-reduced-motion: reduce)").matches;
    let completionTimer: number | undefined;
    const leaveTimer = window.setTimeout(
      () => {
        setLeaving(true);
        completionTimer = window.setTimeout(onComplete, reducedMotion ? 20 : 420);
      },
      reducedMotion ? 180 : 1_050
    );

    return () => {
      window.clearTimeout(leaveTimer);
      if (completionTimer !== undefined) window.clearTimeout(completionTimer);
    };
  }, [onComplete]);

  return (
    <div
      className={clsx("startup-intro", leaving && "is-leaving")}
      role="status"
      aria-label="Opening Continuum"
    >
      <div className="startup-aurora" aria-hidden="true" />
      <div className="startup-emblem" aria-hidden="true">
        <span className="startup-ring startup-ring-outer" />
        <span className="startup-ring startup-ring-inner" />
        <span className="startup-mark-shell">
          {/* Static-exported Tauri asset; Next's image loader requires a server. */}
          {/* eslint-disable-next-line @next/next/no-img-element */}
          <img src="/continuum-mark.png" alt="" width={72} height={72} draggable={false} />
        </span>
      </div>
    </div>
  );
}

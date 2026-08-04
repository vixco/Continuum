"use client";

// Bottom scrub bar: buckets today's memory events into 48 fixed slots and
// renders each as a density bar. Click or drag across bars to scrub -
// `onScrub` reports the bucket's `[since, until)` bounds so the parent can
// derive `dimIds` for MemoryGraph. Fetching is entirely the parent's job
// (`MemoryTab.refresh()`); this component only buckets whatever `events`
// it's handed. See task-14-brief.md.

import { useEffect, useMemo, useRef, useState } from "react";
import { clsx } from "clsx";

import type { MemoryEvent } from "@/lib/types";

const BUCKET_COUNT = 48;

interface TimelineStripProps {
  events: MemoryEvent[];
  window: { since: string; until: string } | null;
  onScrub: (w: { since: string; until: string } | null) => void;
}

interface Bucket {
  since: string;
  until: string;
  count: number;
  hasError: boolean;
  events: MemoryEvent[];
}

/** Local midnight, as an ISO string - the start of "today" that both this
 * component's buckets and MemoryTab's `memoryEvents` fetch range agree on. */
export function todayStartIso(): string {
  const d = new Date();
  return new Date(d.getFullYear(), d.getMonth(), d.getDate()).toISOString();
}

function buildBuckets(events: MemoryEvent[]): Bucket[] {
  const start = new Date(todayStartIso()).getTime();
  const end = start + 24 * 60 * 60 * 1000;
  const bucketMs = (end - start) / BUCKET_COUNT;
  const buckets: Bucket[] = Array.from({ length: BUCKET_COUNT }, (_, i) => {
    const bStart = start + i * bucketMs;
    const bEnd = bStart + bucketMs;
    return {
      since: new Date(bStart).toISOString(),
      until: new Date(bEnd).toISOString(),
      count: 0,
      hasError: false,
      events: [],
    };
  });
  for (const e of events) {
    const t = new Date(e.ts).getTime();
    if (Number.isNaN(t) || t < start || t >= end) continue;
    const idx = Math.min(BUCKET_COUNT - 1, Math.floor((t - start) / bucketMs));
    const bucket = buckets[idx];
    bucket.count += 1;
    bucket.events.push(e);
    if (e.kind === "error") bucket.hasError = true;
  }
  return buckets;
}

function formatBucketLabel(iso: string): string {
  const d = new Date(iso);
  if (Number.isNaN(d.getTime())) return iso;
  return d.toLocaleTimeString(undefined, { hour: "2-digit", minute: "2-digit" });
}

// The prop is named `window` per the brief's contract, which would shadow
// the global `window` object for the rest of this component - destructured
// under a local alias instead so the mouseup listener below still reaches it.
export function TimelineStrip({ events, window: scrubWindow, onScrub }: TimelineStripProps) {
  const buckets = useMemo(() => buildBuckets(events), [events]);
  const maxCount = Math.max(1, ...buckets.map((b) => b.count));
  const [openIdx, setOpenIdx] = useState<number | null>(null);
  const draggingRef = useRef(false);
  const lastIdxRef = useRef<number | null>(null);

  useEffect(() => {
    function onMouseUp() {
      if (draggingRef.current && lastIdxRef.current !== null) {
        setOpenIdx(lastIdxRef.current);
      }
      draggingRef.current = false;
    }
    window.addEventListener("mouseup", onMouseUp);
    return () => window.removeEventListener("mouseup", onMouseUp);
  }, []);

  const activeIdx = scrubWindow
    ? buckets.findIndex((b) => b.since === scrubWindow.since && b.until === scrubWindow.until)
    : -1;

  function scrubTo(idx: number) {
    lastIdxRef.current = idx;
    const b = buckets[idx];
    onScrub({ since: b.since, until: b.until });
  }

  function clear() {
    onScrub(null);
    setOpenIdx(null);
    lastIdxRef.current = null;
  }

  const openBucket = openIdx !== null ? buckets[openIdx] : null;

  return (
    <div className="relative flex h-10 shrink-0 items-center gap-2 border-t border-bg-border bg-bg-surface px-2">
      <button
        type="button"
        onClick={clear}
        className={clsx(
          "shrink-0 rounded-md border px-2 py-1 text-[11px] transition-colors",
          !scrubWindow
            ? "border-accent-amber/60 text-ink"
            : "border-bg-border text-ink-muted hover:text-ink"
        )}
      >
        now &#9654;
      </button>
      <div className="relative flex h-7 flex-1 items-end gap-px">
        {buckets.map((b, i) => (
          <button
            key={b.since}
            type="button"
            aria-label={`${formatBucketLabel(b.since)}: ${b.count} event(s)`}
            className="group relative h-full flex-1"
            onMouseDown={() => {
              draggingRef.current = true;
              scrubTo(i);
              setOpenIdx(i);
            }}
            onMouseEnter={() => {
              if (draggingRef.current) {
                scrubTo(i);
                setOpenIdx(i);
              }
            }}
          >
            <span
              className={clsx(
                "absolute bottom-0 left-0 right-0 rounded-sm transition-colors",
                b.count > 0
                  ? i === activeIdx
                    ? "bg-accent-amber"
                    : "bg-accent-amber/60 group-hover:bg-accent-amber/80"
                  : "bg-transparent group-hover:bg-bg-hover"
              )}
              style={{ height: b.count > 0 ? `${(b.count / maxCount) * 100}%` : "100%" }}
            />
            {b.hasError && (
              <span className="absolute -top-1 left-1/2 h-1.5 w-1.5 -translate-x-1/2 rounded-full bg-state-error" />
            )}
          </button>
        ))}
        {openBucket && (
          <div className="absolute bottom-full left-0 z-20 mb-2 max-h-64 w-72 overflow-y-auto rounded-lg border border-bg-border bg-bg-elevated p-2 shadow-xl">
            <div className="mb-1 flex items-center justify-between">
              <span className="text-[11px] font-medium text-ink-muted">
                {formatBucketLabel(openBucket.since)}&ndash;{formatBucketLabel(openBucket.until)}
              </span>
              <button
                type="button"
                onClick={() => setOpenIdx(null)}
                className="text-ink-dim hover:text-ink"
                aria-label="Close"
              >
                &times;
              </button>
            </div>
            {openBucket.events.length === 0 ? (
              <div className="py-2 text-center text-xs text-ink-dim">No events in this window.</div>
            ) : (
              <ul className="space-y-1">
                {openBucket.events.map((e) => (
                  <li key={e.id} className="text-xs">
                    <span className="text-ink-dim">{formatBucketLabel(e.ts)}</span>{" "}
                    <span
                      className={clsx(
                        "rounded border px-1 py-0.5 text-[10px]",
                        e.kind === "error"
                          ? "border-state-error/40 text-state-error"
                          : "border-bg-border text-ink-muted"
                      )}
                    >
                      {e.kind}
                    </span>{" "}
                    <span className="text-ink-muted">{e.text}</span>
                  </li>
                ))}
              </ul>
            )}
          </div>
        )}
      </div>
    </div>
  );
}

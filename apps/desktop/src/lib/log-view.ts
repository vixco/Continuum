import type { LogEntry } from "./types";

export type LogSeverity = "error" | "warn" | "info" | "debug" | "trace" | "other";

export interface LogDisplayGroup {
  key: string;
  entries: LogEntry[];
}

/** Normalize backend or adapter-specific level spelling for consistent presentation. */
export function logSeverity(level: string): LogSeverity {
  switch (level.trim().toLowerCase()) {
    case "error":
    case "fatal":
      return "error";
    case "warn":
    case "warning":
      return "warn";
    case "info":
      return "info";
    case "debug":
      return "debug";
    case "trace":
      return "trace";
    default:
      return "other";
  }
}

/**
 * Condense adjacent entries only when every displayed operational value matches.
 * Each original entry remains in the group so IDs, timestamps, order, and export
 * data are never discarded.
 */
export function groupAdjacentLogs(entries: LogEntry[]): LogDisplayGroup[] {
  const groups: LogDisplayGroup[] = [];
  let previousSignature: string | null = null;

  for (const entry of entries) {
    const signature = displaySignature(entry);
    const previous = groups.at(-1);

    if (previous && signature === previousSignature) {
      previous.entries.push(entry);
      continue;
    }

    groups.push({
      key: String(entry.id),
      entries: [entry],
    });
    previousSignature = signature;
  }

  return groups;
}

function displaySignature(entry: LogEntry): string {
  return JSON.stringify([
    logSeverity(entry.level),
    entry.layer,
    entry.component,
    entry.target,
    entry.message,
    entry.fields,
  ]);
}

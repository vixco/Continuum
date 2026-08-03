// Canvas palette for the memory graph. The one file allowed to hold raw hex:
// canvas 2D can't consume Tailwind classes. Keep in sync with tailwind tokens.
import type { MemoryNodeType } from "./types";

export const NODE_COLORS: Record<MemoryNodeType, string> = {
  project: "#7c5cff",
  goal: "#5cc8ff",
  task: "#ffd166",
  decision: "#c792ea",
  person: "#7ee787",
  preference: "#64d8cb",
  fact: "#8ab4ff",
  error: "#ff7b72",
  session: "#9a9ab0",
  note: "#c9c9d9",
};

export const NODE_TYPE_LABELS: Record<MemoryNodeType, string> = {
  project: "Project",
  goal: "Goal",
  task: "Task",
  decision: "Decision",
  person: "Person",
  preference: "Preference",
  fact: "Fact",
  error: "Error",
  session: "Session",
  note: "Note",
};

export const GHOST_COLOR = "#55556e";
export const EDGE_COLOR = "rgba(120,120,150,0.25)";
export const LABEL_COLOR = "#b8b8d0";

// Saved memory-graph filter presets, persisted to localStorage so a
// user's "active project only" or "candidates only" view survives a restart.

"use client";

import { create } from "zustand";
import { persist } from "zustand/middleware";

import type { MemoryGraphFilter } from "./types";

export interface SavedView {
  name: string;
  filter: MemoryGraphFilter;
}

interface MemoryViewsStore {
  views: SavedView[];
  addView: (name: string, filter: MemoryGraphFilter) => void;
  removeView: (name: string) => void;
}

export const useMemoryViews = create<MemoryViewsStore>()(
  persist(
    (set) => ({
      views: [],
      addView: (name, filter) =>
        set((s) => ({
          views: [...s.views.filter((v) => v.name !== name), { name, filter }],
        })),
      removeView: (name) => set((s) => ({ views: s.views.filter((v) => v.name !== name) })),
    }),
    { name: "continuum-memory-views" }
  )
);

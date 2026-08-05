// Theme system for the Continuum desktop shell.
// One source of truth: the `data-theme` attribute on <html>. Tokens.css
// branches every color/stroke/shadow on that attribute, so switching themes
// is a single DOM write — no component re-renders required for the recolor.
// React state here only exists to drive the picker UI's selected state.

"use client";

import { useSyncExternalStore } from "react";

export type Theme = "light" | "dark";

export const THEME_KEY = "continuum.theme";

let current: Theme = resolveInitial();
const listeners = new Set<() => void>();

function resolveInitial(): Theme {
  if (typeof document !== "undefined") {
    const attr = document.documentElement.getAttribute("data-theme");
    if (attr === "light" || attr === "dark") return attr;
  }
  if (typeof localStorage !== "undefined") {
    const stored = localStorage.getItem(THEME_KEY);
    if (stored === "light" || stored === "dark") return stored;
  }
  return "dark";
}

function emit() {
  for (const l of listeners) l();
}

/** Apply a theme to <html> and persist it. Safe to call during render-free
 *  event handlers; does not trigger React re-renders except for picker UI. */
export function setTheme(theme: Theme) {
  current = theme;
  if (typeof document !== "undefined") {
    document.documentElement.setAttribute("data-theme", theme);
  }
  if (typeof localStorage !== "undefined") {
    try {
      localStorage.setItem(THEME_KEY, theme);
    } catch {
      /* private mode / disabled storage — ignore */
    }
  }
  emit();
}

/** Read current theme without subscribing. */
export function getTheme(): Theme {
  return current;
}

/** React hook: [theme, setTheme]. Re-renders only the subscribing component. */
export function useTheme(): [Theme, (t: Theme) => void] {
  const theme = useSyncExternalStore(
    (cb) => {
      listeners.add(cb);
      return () => listeners.delete(cb);
    },
    () => current,
    () => "dark" as Theme
  );
  return [theme, setTheme];
}

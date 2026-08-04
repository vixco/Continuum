"use client";

import { continuum } from "./tauri";
import type { ProviderConnection } from "./types";

const PROVIDERS_CHANGED_EVENT = "continuum:providers-changed";

let refreshInFlight: Promise<ProviderRefreshResult> | null = null;

export interface ProviderRefreshResult {
  providers: ProviderConnection[];
  refreshed: number;
  failed: number;
}

export function notifyProvidersChanged(): void {
  if (typeof window !== "undefined") window.dispatchEvent(new Event(PROVIDERS_CHANGED_EVENT));
}

export function onProvidersChanged(handler: () => void): () => void {
  if (typeof window === "undefined") return () => {};
  window.addEventListener(PROVIDERS_CHANGED_EVENT, handler);
  return () => window.removeEventListener(PROVIDERS_CHANGED_EVENT, handler);
}

/** Refresh every configured provider once and coalesce overlapping timer/manual requests. */
export function refreshAllProviderModels(): Promise<ProviderRefreshResult> {
  if (refreshInFlight) return refreshInFlight;
  refreshInFlight = (async () => {
    const providers = await continuum.providersList();
    const results = await Promise.allSettled(
      providers.map((provider) => continuum.providerRefreshModels(provider.id))
    );
    const next = await continuum.providersList();
    notifyProvidersChanged();
    return {
      providers: next,
      refreshed: results.filter((result) => result.status === "fulfilled").length,
      failed: results.filter((result) => result.status === "rejected").length,
    };
  })().finally(() => {
    refreshInFlight = null;
  });
  return refreshInFlight;
}

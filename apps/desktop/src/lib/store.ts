// Zustand store: single source of truth for the dashboard. Hydrated on
// mount by useDashboardBootstrap, then kept in sync via Tauri's `continuum:*`
// events. Components select thin slices with useStore(selector) so a
// perception-only change doesn't re-render the Voice tab.

"use client";

import { create } from "zustand";
import {
  DEFAULT_CONFIG,
  DEFAULT_STATE,
  continuum,
  subscribeLogs,
  subscribeRepair,
  subscribeState,
} from "./tauri";
import type {
  ComponentHealth,
  ContinuumConfig,
  ContinuumState,
  LogEntry,
  RepairEvent,
} from "./types";

interface Store {
  state: ContinuumState;
  config: ContinuumConfig;
  logs: LogEntry[];
  logLimit: number;
  repairEvents: RepairEvent[];
  components: ComponentHealth[];
  bootstrapped: boolean;
  setState: (state: ContinuumState) => void;
  setConfig: (config: ContinuumConfig) => void;
  setComponents: (components: ComponentHealth[]) => void;
  pushLog: (entry: LogEntry) => void;
  setLogs: (entries: LogEntry[]) => void;
  pushRepair: (entry: RepairEvent) => void;
  clearRepair: () => void;
  setBootstrapped: (b: boolean) => void;
}

export const useStore = create<Store>((set) => ({
  state: DEFAULT_STATE,
  config: DEFAULT_CONFIG,
  logs: [],
  logLimit: 1000,
  repairEvents: [],
  components: [],
  bootstrapped: false,
  setState: (state) => set({ state }),
  setConfig: (config) => set({ config }),
  setComponents: (components) => set({ components }),
  pushLog: (entry) =>
    set((s) => {
      const next = [entry, ...s.logs];
      if (next.length > s.logLimit) next.length = s.logLimit;
      return { logs: next };
    }),
  setLogs: (entries) => set({ logs: entries }),
  pushRepair: (entry) => set((s) => ({ repairEvents: [...s.repairEvents, entry] })),
  clearRepair: () => set({ repairEvents: [] }),
  setBootstrapped: (b) => set({ bootstrapped: b }),
}));

let bootstrapped = false;
let unsubs: Array<() => void> = [];

export async function bootstrapStore() {
  if (bootstrapped) return;
  bootstrapped = true;

  const [state, config, logs, components] = await Promise.all([
    continuum.getState(),
    continuum.getConfig(),
    continuum.getLogs({ limit: 500 }),
    continuum.getHealth(),
  ]);

  useStore.setState({
    state,
    config,
    logs,
    components,
    bootstrapped: true,
  });

  const stateUnsub = await subscribeState((s) => {
    useStore.getState().setState(s);
    useStore.getState().setComponents(s.health.components);
  });
  const logUnsub = await subscribeLogs((e) => {
    useStore.getState().pushLog(e);
  });
  const repairUnsub = await subscribeRepair((e) => {
    useStore.getState().pushRepair(e);
  });

  unsubs = [stateUnsub, logUnsub, repairUnsub];
}

export function teardownStore() {
  unsubs.forEach((u) => {
    try {
      u();
    } catch {
      /* ignore */
    }
  });
  unsubs = [];
  bootstrapped = false;
}

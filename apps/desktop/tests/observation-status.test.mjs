import assert from "node:assert/strict";
import test from "node:test";

import { deriveObservationSummary, requestObservationToggle } from "../src/lib/observation.ts";

const healthy = (detail = null) => ({
  healthy: true,
  enabled: true,
  should_restart: false,
  detail,
});

function fixture() {
  const state = {
    perception: {
      last_capture_at: "2026-08-10T12:00:00Z",
    },
    health: { last_check_ts: "2026-08-10T12:00:00Z" },
    memory: { raw_log_rows: 4 },
    system: {
      paused: false,
      vision_model_loaded: true,
      stt_loaded: true,
    },
    context: {
      session: {
        active_project: "continuum",
        current_goal: "Improve desktop trust",
        current_task: "Fix chat scrolling",
        active_app: "Code",
        window_title: "MessageList.tsx",
        confidence: 0.82,
        local_only: false,
        updated: "2026-08-10T12:00:00Z",
      },
      engine: {
        idle: false,
        context_watcher: healthy(),
        live_context: healthy(),
        git_watcher: healthy(),
        file_watcher: healthy(),
        process_watcher: healthy(),
        events_writer: healthy(),
        triage: healthy(),
      },
      page: {
        toggles: { mic: true, screen: true, files: true, git: true, pause_all: false },
        recent_events: [],
      },
    },
  };
  const config = {
    resources: { vision_enabled: true },
    storage: { retention_days: 30 },
    screen: { save_screenshots: false },
  };
  return { state, config };
}

function summarize(overrides = {}) {
  const { state, config } = fixture();
  return deriveObservationSummary({
    state,
    config,
    runtimeAvailable: true,
    pauseStatus: { paused: false, until: null },
    ...overrides,
  });
}

test("healthy live sources render a grounded observing summary", () => {
  const summary = summarize();

  assert.equal(summary.kind, "observing");
  assert.equal(summary.label, "Observing");
  assert.ok(summary.activeCount >= 4);
  assert.equal(summary.currentActivity.title, "Fix chat scrolling");
  assert.equal(summary.currentActivity.confidence, 0.82);
  assert.match(summary.currentActivity.evidence, /Code/);
});

test("paused is distinct from disabled individual sources", () => {
  const summary = summarize({ pauseStatus: { paused: true, until: null } });

  assert.equal(summary.kind, "paused");
  assert.equal(summary.label, "Paused");
  assert.equal(summary.sources.find((source) => source.id === "screen").enabled, true);
});

test("any positive live pause signal keeps the global state fail-closed", () => {
  const { state, config } = fixture();
  state.system.paused = true;
  const summary = deriveObservationSummary({
    state,
    config,
    runtimeAvailable: true,
    pauseStatus: { paused: false, until: null },
  });

  assert.equal(summary.kind, "paused");
});

test("permission-required state preserves the runtime reason", () => {
  const { state, config } = fixture();
  state.context.engine.live_context = {
    healthy: true,
    enabled: false,
    should_restart: false,
    detail: "Screen recording permission required",
  };
  const summary = deriveObservationSummary({ state, config, runtimeAvailable: true });

  assert.equal(summary.kind, "permission_required");
  assert.match(summary.reason, /permission required/i);
});

test("missing vision model is not mislabeled as an idle source", () => {
  const { state, config } = fixture();
  state.system.vision_model_loaded = false;
  const summary = deriveObservationSummary({ state, config, runtimeAvailable: true });

  assert.equal(summary.kind, "vision_unavailable");
  assert.equal(summary.sources.find((source) => source.id === "screen").state, "unavailable");
});

test("an unhealthy enabled watcher makes the global state degraded", () => {
  const { state, config } = fixture();
  state.context.page.toggles.screen = false;
  state.context.engine.file_watcher = {
    healthy: false,
    enabled: true,
    should_restart: true,
    detail: "Watcher stopped after repeated access errors",
  };
  const summary = deriveObservationSummary({ state, config, runtimeAvailable: true });

  assert.equal(summary.kind, "degraded");
  assert.match(summary.reason, /File activity/);
  assert.match(summary.reason, /access errors/);
});

test("idle remains enabled and does not collapse into off", () => {
  const { state, config } = fixture();
  state.context.engine.idle = true;
  const summary = deriveObservationSummary({ state, config, runtimeAvailable: true });

  assert.equal(summary.kind, "observing");
  assert.equal(summary.label, "Observing · idle");
  assert.equal(summary.sources.find((source) => source.id === "files").state, "idle");
});

test("retention zero surfaces historical context off while live sources remain active", () => {
  const { state, config } = fixture();
  config.storage.retention_days = 0;
  const summary = deriveObservationSummary({ state, config, runtimeAvailable: true });

  assert.equal(summary.kind, "historical_context_off");
  assert.match(summary.reason, /not retained/);
});

test("runtime startup and missing heartbeat remain distinct", () => {
  const starting = summarize({ runtimeAvailable: false, runtimeStarting: true });
  const unavailable = summarize({ runtimeAvailable: false, runtimeStarting: false });

  assert.equal(starting.kind, "processing");
  assert.equal(unavailable.kind, "unavailable");
});

test("toggle success waits for the runtime contract instead of mutating UI state", async () => {
  const calls = [];
  const result = await requestObservationToggle(
    async (intent) => {
      calls.push(intent);
    },
    "screen",
    false,
    "Screen & vision"
  );

  assert.equal(result.ok, true);
  assert.deepEqual(calls, [{ kind: "set_toggle", name: "screen", value: false }]);
  assert.match(result.message, /Waiting for the runtime to confirm/);
});

test("toggle failure keeps the backend reason visible", async () => {
  const result = await requestObservationToggle(
    async () => {
      throw new Error("permission service unavailable");
    },
    "files",
    true,
    "File activity"
  );

  assert.equal(result.ok, false);
  assert.match(result.message, /permission service unavailable/);
});

test("unpublished process and triage health stay unavailable rather than looking disabled", () => {
  const { state, config } = fixture();
  state.context.engine.process_watcher = undefined;
  state.context.engine.triage = undefined;
  const summary = deriveObservationSummary({ state, config, runtimeAvailable: true });

  assert.equal(summary.sources.find((source) => source.id === "processes").state, "unavailable");
  assert.equal(summary.sources.find((source) => source.id === "triage").state, "unavailable");
});

test("healthy history writer is live and reports the latest retained event", () => {
  const { state, config } = fixture();
  state.context.page.recent_events = [{ ts: "2026-08-10T11:59:00Z" }];
  const summary = deriveObservationSummary({ state, config, runtimeAvailable: true });
  const history = summary.sources.find((source) => source.id === "history");

  assert.equal(history.state, "active");
  assert.match(history.reason, /writer is healthy/i);
  assert.match(history.reason, /Latest retained event/);
});

test("published history without writer health is explicitly last-known", () => {
  const { state, config } = fixture();
  state.context.page.recent_events = [{ ts: "2026-08-10T11:58:00Z" }];
  state.context.engine.events_writer = null;
  const summary = deriveObservationSummary({ state, config, runtimeAvailable: true });
  const history = summary.sources.find((source) => source.id === "history");

  assert.equal(history.enabled, true);
  assert.equal(history.state, "last_known");
  assert.match(history.reason, /last-known/i);
});

test("unhealthy history writer is degraded instead of looking enabled", () => {
  const { state, config } = fixture();
  state.context.page.recent_events = [{ ts: "2026-08-10T11:57:00Z" }];
  state.context.engine.events_writer = {
    healthy: false,
    enabled: true,
    should_restart: true,
    detail: "History writer stopped after repeated write failures",
  };
  const summary = deriveObservationSummary({ state, config, runtimeAvailable: true });
  const history = summary.sources.find((source) => source.id === "history");

  assert.equal(history.state, "degraded");
  assert.equal(summary.kind, "degraded");
  assert.match(summary.reason, /Historical context/);
  assert.match(summary.reason, /write failures/);
});

test("configured retention with a disabled writer is unavailable, not off by choice", () => {
  const { state, config } = fixture();
  state.context.engine.events_writer = {
    healthy: true,
    enabled: false,
    should_restart: false,
    detail: "History writer is unavailable in this runtime",
  };
  const summary = deriveObservationSummary({ state, config, runtimeAvailable: true });
  const history = summary.sources.find((source) => source.id === "history");

  assert.equal(history.enabled, true);
  assert.equal(history.state, "unavailable");
  assert.match(history.reason, /unavailable/i);
});

test("history and triage alone never make the UI claim the user is being observed", () => {
  const { state, config } = fixture();
  state.context.page.toggles = {
    mic: false,
    screen: false,
    files: false,
    git: false,
    pause_all: false,
  };
  state.context.engine.process_watcher = {
    healthy: true,
    enabled: false,
    should_restart: false,
    detail: "Off by policy",
  };
  const summary = deriveObservationSummary({ state, config, runtimeAvailable: true });

  assert.equal(summary.activeCount, 0);
  assert.equal(summary.kind, "off");
});

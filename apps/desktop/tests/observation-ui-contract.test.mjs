import assert from "node:assert/strict";
import { readFile } from "node:fs/promises";
import test from "node:test";

const read = (path) => readFile(new URL(`../${path}`, import.meta.url), "utf8");

test("global observation control is backed by runtime state and commands", async () => {
  const source = await read("src/components/observation/ObservationStatusControl.tsx");

  assert.match(source, /deriveObservationSummary/);
  assert.match(source, /getRuntimeStatus/);
  assert.match(source, /getObservationPause/);
  assert.match(source, /pauseObservation/);
  assert.match(source, /resumeObservation/);
  assert.match(source, /contextWriteIntent/);
  assert.match(source, /updateLiveContextConfig/);
  assert.match(source, /Live runtime state, not sample data/);
  assert.doesNotMatch(source, /fixture/i);
});

test("observation controls explain privacy, unavailable reasons, history and confidence", async () => {
  const source = await read("src/components/observation/ObservationStatusControl.tsx");

  assert.match(source, /Privacy details/);
  assert.match(source, /source\.reason/);
  assert.match(source, /Historical retention/);
  assert.match(source, /Current activity/);
  assert.match(source, /Confidence:/);
  assert.match(source, /Context & history/);
  assert.match(source, /Diagnose/);
});

test("the explanatory observation control replaces the ambiguous titlebar power button", async () => {
  const shell = await read("src/components/layout/Shell.tsx");
  const page = await read("src/app/page.tsx");

  assert.match(shell, /ObservationStatusControl/);
  assert.doesNotMatch(shell, /ObservationPowerButton/);
  assert.doesNotMatch(page, /ObservationStatusControl/);
});

test("history state vocabulary includes live and last-known without treating either as a fixture", async () => {
  const source = await read("src/components/observation/ObservationStatusControl.tsx");

  assert.match(source, /active: "Live"/);
  assert.match(source, /last_known: "Last known"/);
});

test("runtime loss labels source and activity freshness instead of claiming live state", async () => {
  const source = await read("src/components/observation/ObservationStatusControl.tsx");

  assert.match(source, /Runtime unavailable; last-known data is labeled/);
  assert.match(source, /activityFreshnessLabel/);
  assert.doesNotMatch(source, />\s*Live runtime state, not sample data\.\s*</);
});

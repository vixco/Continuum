import assert from "node:assert/strict";
import test from "node:test";

import { groupAdjacentLogs, logSeverity } from "../src/lib/log-view.ts";

function entry(id, overrides = {}) {
  return {
    id,
    ts: `2026-08-04T12:00:0${id}.000Z`,
    level: "info",
    layer: "system",
    component: "runtime",
    target: "continuum_core",
    message: "ready",
    fields: [],
    ...overrides,
  };
}

test("groups adjacent content-identical entries without losing event data", () => {
  const source = [entry(1), entry(2), entry(3, { message: "working" })];
  const groups = groupAdjacentLogs(source);

  assert.equal(groups.length, 2);
  assert.deepEqual(
    groups[0].entries.map((item) => item.id),
    [1, 2]
  );
  assert.equal(groups[0].entries[0].ts, source[0].ts);
  assert.equal(groups[0].entries[1].ts, source[1].ts);
  assert.ok(groups.flatMap((group) => group.entries).every((item, index) => item === source[index]));
});

test("does not combine matching entries separated by another operational event", () => {
  const groups = groupAdjacentLogs([
    entry(1),
    entry(2, { message: "intervening" }),
    entry(3),
  ]);

  assert.equal(groups.length, 3);
});

test("keeps entries distinct when target, fields, component, or severity differ", () => {
  const groups = groupAdjacentLogs([
    entry(1),
    entry(2, { target: "desktop" }),
    entry(3, { fields: [["attempt", "2"]] }),
    entry(4, { component: "worker" }),
    entry(5, { level: "warn" }),
  ]);

  assert.equal(groups.length, 5);
});

test("normalizes warning and fatal aliases while retaining an other fallback", () => {
  assert.equal(logSeverity("ERROR"), "error");
  assert.equal(logSeverity("fatal"), "error");
  assert.equal(logSeverity("warning"), "warn");
  assert.equal(logSeverity("notice"), "other");
});

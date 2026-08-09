import assert from "node:assert/strict";
import { readFile } from "node:fs/promises";
import test from "node:test";

const read = (path) => readFile(new URL(`../${path}`, import.meta.url), "utf8");

test("push-to-talk makes the runtime ready before arming voice", async () => {
  const source = await read("src/components/PushToTalkButton.tsx");

  const runtimeStatus = source.indexOf("continuum.getRuntimeStatus()");
  const startRuntime = source.indexOf("continuum.startRuntime()");
  const talkNow = source.indexOf("continuum.talkNow()");

  assert.ok(runtimeStatus >= 0, "PTT must check whether the runtime is alive");
  assert.ok(startRuntime > runtimeStatus, "PTT must start an offline runtime");
  assert.ok(talkNow > startRuntime, "PTT must arm voice only after runtime startup");
  assert.match(source, /await waitForRuntime\(\)/, "PTT must wait for runtime readiness");
});

test("explicit push-to-talk recovers paused or muted voice and surfaces failure", async () => {
  const source = await read("src/components/PushToTalkButton.tsx");

  assert.match(source, /state\.system\.paused[\s\S]*continuum\.setPaused\(false\)/);
  assert.match(source, /state\.voice\.muted[\s\S]*continuum\.setVoiceMuted\(false\)/);
  assert.match(source, /setError\(toErrorMessage\(err\)\)/);
  assert.match(source, /Voice unavailable/);
  assert.match(source, /Speak now…/);
});

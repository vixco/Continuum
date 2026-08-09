import assert from "node:assert/strict";
import { readFile } from "node:fs/promises";
import test from "node:test";

const read = (path) => readFile(new URL(`../${path}`, import.meta.url), "utf8");

test("live voice waits for the automatically managed runtime before arming a turn", async () => {
  const source = await read("src/components/PushToTalkButton.tsx");

  const runtimeStatus = source.indexOf("continuum.getRuntimeStatus()");
  const waitForRuntime = source.indexOf("waitForRuntime()");
  const voiceReady = source.indexOf("waitForVoiceReady()");
  const talkNow = source.indexOf("continuum.talkNow()");

  assert.ok(runtimeStatus >= 0, "voice must check whether the runtime is alive");
  assert.ok(waitForRuntime > runtimeStatus, "voice must wait for automatic startup");
  assert.ok(voiceReady > waitForRuntime, "voice must wait for native voice components");
  assert.ok(talkNow > voiceReady, "voice must arm only after runtime + voice readiness");
  assert.doesNotMatch(source, /continuum\.startRuntime\(\)/);
  assert.match(source, /state\.system\.stt_loaded/);
  assert.match(source, /state\.system\.tts_loaded/);
  assert.match(source, /state\.system\.orchestrator_ready/);
});

test("one click keeps voice conversational by re-arming after each completed turn", async () => {
  const source = await read("src/components/PushToTalkButton.tsx");

  assert.match(source, /previousWasActive/);
  assert.match(source, /mode === "idle"/);
  assert.match(source, /setTimeout\(\(\) => \{[\s\S]*void armNextTurn\(\)/);
  assert.match(source, /aria-pressed=\{liveActive\}/);
  assert.match(source, /Stop live voice/);
  assert.match(source, /next turn starts automatically/);
});

test("live voice controls the headless runtime and surfaces concrete failures", async () => {
  const source = await read("src/components/PushToTalkButton.tsx");

  assert.match(
    source,
    /continuum\.contextWriteIntent\(\{\s*kind: "set_toggle",\s*name: "pause_all",\s*value: false,?\s*\}\)/
  );
  assert.match(
    source,
    /continuum\.contextWriteIntent\(\{\s*kind: "set_toggle",\s*name: "mic",\s*value: true,?\s*\}\)/
  );
  assert.match(source, /config\.audio\.enabled/);
  assert.match(source, /config\.voice\.enabled/);
  assert.match(source, /role="alert"/);
  assert.match(source, /Live voice is not ready:/);
  assert.match(source, /speech-to-text \(Whisper\)/);
  assert.match(source, /text-to-speech \(Piper\/Kokoros\)/);
  assert.match(source, /voice model\/orchestrator/);
  assert.match(source, /No speech reached Whisper within 25 seconds/);
  assert.doesNotMatch(
    source,
    /setPaused\(false\)/,
    "dashboard-only pause state must not pretend to control the daemon"
  );
  assert.doesNotMatch(
    source,
    /setVoiceMuted\(false\)/,
    "dashboard-only mute state must not pretend to control the daemon"
  );
});

import assert from "node:assert/strict";
import { readFile } from "node:fs/promises";
import test from "node:test";

const read = (path) => readFile(new URL(`../${path}`, import.meta.url), "utf8");

test("live voice makes the native runtime ready before arming a turn", async () => {
  const source = await read("src/components/PushToTalkButton.tsx");

  const runtimeStatus = source.indexOf("continuum.getRuntimeStatus()");
  const startRuntime = source.indexOf("continuum.startRuntime()");
  const voiceReady = source.indexOf("waitForVoiceReady()");
  const talkNow = source.indexOf("continuum.talkNow()");

  assert.ok(runtimeStatus >= 0, "voice must check whether the runtime is alive");
  assert.ok(startRuntime > runtimeStatus, "voice must start an offline runtime");
  assert.ok(voiceReady > startRuntime, "voice must wait for native voice components");
  assert.ok(talkNow > voiceReady, "voice must arm only after runtime + voice readiness");
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

test("explicit live voice recovers pause or mute and never hides setup errors", async () => {
  const source = await read("src/components/PushToTalkButton.tsx");

  assert.match(source, /state\.system\.paused[\s\S]*continuum\.setPaused\(false\)/);
  assert.match(source, /state\.voice\.muted[\s\S]*continuum\.setVoiceMuted\(false\)/);
  assert.match(source, /role="alert"/);
  assert.match(source, /Live voice is not ready:/);
  assert.match(source, /speech-to-text \(Whisper\)/);
  assert.match(source, /text-to-speech \(Piper\/Kokoros\)/);
  assert.match(source, /voice model\/orchestrator/);
  assert.doesNotMatch(source, /catch \{\s*\/\*.*ignore/s, "voice errors must not be swallowed");
});

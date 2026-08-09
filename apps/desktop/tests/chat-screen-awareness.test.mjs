import assert from "node:assert/strict";
import { readFile } from "node:fs/promises";
import test from "node:test";

const read = (path) => readFile(new URL(`../${path}`, import.meta.url), "utf8");
const readRoot = (path) => readFile(new URL(`../../../${path}`, import.meta.url), "utf8");

test("chat prompt knows Continuum can inspect live screens", async () => {
  const prompt = await read("src-tauri/assets/chat-system-prompt.md");

  assert.doesNotMatch(prompt, /You have no tools and no access/i);
  assert.match(prompt, /context_screen/);
  assert.match(prompt, /screen 3[\s\S]*display-3/i);
  assert.match(prompt, /scherm 3/);
  assert.match(prompt, /Never answer[\s\S]*I can't see your screen/i);
  assert.match(prompt, /unavailable, disabled by a privacy toggle, stale, or a tool call fails/i);
});

test("desktop chat keeps live screen and window tools wired", async () => {
  const tools = await read("src-tauri/src/chat_tools.rs");
  const permissions = await readRoot("config/default-permissions.toml");

  assert.match(tools, /mcp__continuum__context_screen/);
  assert.match(tools, /mcp__continuum__context_window/);
  assert.match(permissions, /context_screen\s*=\s*"auto"/);
  assert.match(permissions, /context_window\s*=\s*"auto"/);
});

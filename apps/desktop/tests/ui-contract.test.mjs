import assert from "node:assert/strict";
import { readFile } from "node:fs/promises";
import test from "node:test";

const read = (path) => readFile(new URL(`../${path}`, import.meta.url), "utf8");

test("Hallmark polish loads after the base token stylesheet", async () => {
  const layout = await read("src/app/layout.tsx");
  const globalsIndex = layout.indexOf('import "./globals.css"');
  const hermesIndex = layout.indexOf('import "./hermes.css"');

  assert.ok(globalsIndex >= 0, "base globals stylesheet must be loaded");
  assert.ok(hermesIndex > globalsIndex, "polish layer must load after globals.css");
});

test("Hermes-inspired layer stays token-driven and reduced-motion safe", async () => {
  const css = await read("src/app/hermes.css");

  assert.match(css, /var\(--color-paper\)/);
  assert.match(css, /var\(--ui-stroke-tertiary\)/);
  assert.match(css, /@media \(prefers-reduced-motion: reduce\)/);
  assert.doesNotMatch(css, /#[0-9a-f]{3,8}\b/i, "do not introduce raw hex colors");
  assert.doesNotMatch(css, /transition-all/, "hot UI must not use transition-all");
});

test("shell keeps explicit navigation and window-control accessibility labels", async () => {
  const shell = await read("src/components/layout/Shell.tsx");

  assert.match(shell, /<aside className="sidebar" aria-label="Main navigation">/);
  assert.match(shell, /aria-current=\{active === id \? "page" : undefined\}/);
  assert.match(shell, /aria-label="Minimize"/);
  assert.match(shell, /aria-label=\{maximized \? "Restore" : "Maximize"\}/);
  assert.match(shell, /aria-label="Close"/);
});

test("chat remains virtualized for long agent transcripts", async () => {
  const list = await read("src/components/chat/MessageList.tsx");
  assert.match(list, /react-virtuoso/);
  assert.match(list, /<Virtuoso/);
  assert.match(list, /atBottomStateChange=\{setAtBottom\}/);
});

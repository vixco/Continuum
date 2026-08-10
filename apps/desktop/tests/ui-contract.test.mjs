import assert from "node:assert/strict";
import { readFile } from "node:fs/promises";
import path from "node:path";
import test from "node:test";
import { fileURLToPath } from "node:url";

const here = path.dirname(fileURLToPath(import.meta.url));
const desktopRoot = path.resolve(here, "..");
const repoRoot = path.resolve(desktopRoot, "../..");

async function read(relativePath) {
  return readFile(path.join(desktopRoot, relativePath), "utf8");
}

async function readRoot(relativePath) {
  return readFile(path.join(repoRoot, relativePath), "utf8");
}

test("desktop styles load shared tokens before Tailwind", async () => {
  const globals = await read("src/app/globals.css");
  const tokenImport = '@import "../../../../tokens.css";';
  assert.match(globals, /tokens\.css/);
  assert.ok(globals.indexOf(tokenImport) < globals.indexOf("@tailwind base;"));
});

test("desktop styles are token-driven and reduced-motion safe", async () => {
  const globals = await read("src/app/globals.css");
  assert.match(globals, /var\(--color-accent\)/);
  assert.match(globals, /var\(--color-paper\)/);
  assert.match(globals, /@media \(prefers-reduced-motion: reduce\)/);
});

test("shell keeps navigation and window controls accessible", async () => {
  const shell = await read("src/components/layout/Shell.tsx");
  assert.match(shell, /aria-label="Main navigation"/);
  assert.match(shell, /aria-label="Minimize"/);
  assert.match(shell, /"Restore" : "Maximize"/);
  assert.match(shell, /aria-label="Close"/);
});

test("runtime startup is automatic and Settings owns the model directory", async () => {
  const main = await read("src-tauri/src/main.rs");
  const settings = await read("src/components/layout/SettingsPage.tsx");
  const observation = await read("src/components/observation/ObservationStatusControl.tsx");
  assert.match(main, /spawn_automatic_runtime_start/);
  assert.match(settings, /continuum\.getModelsDirectory\(\)/);
  assert.match(settings, /continuum\.updateModelsDirectory\(selected\)/);
  assert.match(settings, /next automatic runtime start/i);
  assert.doesNotMatch(observation, /start_runtime/);
});

test("chat stays virtualized and respects explicit scroll intent", async () => {
  const messages = await read("src/components/chat/MessageList.tsx");
  assert.match(messages, /VirtuosoHandle/);
  assert.match(messages, /followOutput=\{false\}/);
  assert.match(messages, /shouldFollowChatOutput\(next\)/);
  assert.match(messages, /atBottomStateChange=\{\(atBottom\) =>/);
});

test("MCP permission controls read and persist the enforced native policy", async () => {
  const tools = await read("src/components/tabs/ToolsTab.tsx");
  const tauriMain = await read("src-tauri/src/main.rs");
  const broker = await readRoot("crates/continuum-mcp/src/permission_broker.rs");
  const chatTools = await read("src-tauri/src/chat_tools.rs");

  assert.match(tools, /invoke<ToolPermissionView\[]>\("list_tool_permissions"\)/);
  assert.match(tools, /invoke<ToolPermissionView\[]>\("set_tool_permission"/);
  assert.doesNotMatch(tools, /in-memory only/i);
  assert.match(tauriMain, /permissions::list_tool_permissions/);
  assert.match(tauriMain, /permissions::set_tool_permission/);
  assert.match(broker, /unwrap_or\(ToolPermission::AlwaysConfirm\)/);
  assert.match(broker, /self\.broker\.authorize\(&tool, &arguments\)\.await/);
  assert.match(chatTools, /authorize_in_process_tool\(name, input\)\.await/);
  assert.match(chatTools, /Sensitive memory body withheld/);
});

test("release publication requires Windows and both macOS architectures", async () => {
  const workflow = await readRoot(".github/workflows/release.yml");

  assert.match(workflow, /workflow_call:/);
  assert.match(workflow, /pnpm tauri build --bundles nsis/);
  assert.match(workflow, /runner: macos-15[\s\S]*?expected_uname: arm64/);
  assert.match(workflow, /runner: macos-15-intel[\s\S]*?expected_uname: x86_64/);
  assert.match(workflow, /pnpm tauri build --bundles app,dmg --target/);
  assert.match(workflow, /continuum-agent-os/);
  assert.match(workflow, /continuum-\$version-macos-\$arch\.dmg/);
  assert.match(workflow, /continuum-\$version-macos-\$arch\.app\.tar\.gz/);
  assert.match(workflow, /test -s "\$updater\.sig"/);
  assert.match(workflow, /release-manifest\.json/);
  assert.match(workflow, /Verify the draft release has every asset/);
  assert.match(workflow, /Publish the fully assembled draft atomically/);
  assert.match(workflow, /fail_on_unmatched_files: true/);
});

test("Agent OS mutations are journaled and typed before execution", async () => {
  const reliable = await readRoot("crates/continuum-mcp/src/reliable_agent_v2.rs");

  assert.match(reliable, /StepState::Dispatched/);
  assert.match(reliable, /automatic replay is blocked/i);
  assert.match(reliable, /json_pointer_exists:/);
  assert.match(reliable, /window_title_contains:/);
});

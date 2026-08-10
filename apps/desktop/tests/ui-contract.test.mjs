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

test("Hallmark polish loads after the base token stylesheet", async () => {
  const globals = await read("src/app/globals.css");
  assert.match(globals, /@import "\.\/hallmark\.css";/);
  assert.ok(
    globals.indexOf('@import "./hallmark.css";') > globals.indexOf("@tailwind utilities;"),
    "Hallmark polish should load after Tailwind utilities so it can refine the shell",
  );
});

test("Hermes-inspired layer stays token-driven and reduced-motion safe", async () => {
  const hallmark = await read("src/app/hallmark.css");
  assert.match(hallmark, /var\(--continuum-cyan\)/);
  assert.match(hallmark, /var\(--continuum-accent\)/);
  assert.match(hallmark, /@media \(prefers-reduced-motion: reduce\)/);
});

test("shell keeps explicit navigation and window-control accessibility labels", async () => {
  const sidebar = await read("src/components/sidebar/Sidebar.tsx");
  const header = await read("src/components/header/Header.tsx");

  assert.match(sidebar, /aria-label="Primary navigation"/);
  assert.match(header, /aria-label="Minimize window"/);
  assert.match(header, /aria-label="Maximize window"/);
  assert.match(header, /aria-label="Close window"/);
});

test("runtime startup is automatic and Settings owns the model directory", async () => {
  const main = await read("src-tauri/src/main.rs");
  const settings = await read("src/components/tabs/SettingsTab.tsx");
  const observation = await read("src/components/observation/ObservationStatusControl.tsx");

  assert.match(main, /spawn_automatic_runtime_start/);
  assert.match(settings, /get_models_directory/);
  assert.match(settings, /set_models_directory/);
  assert.match(settings, /restart required/i);
  assert.doesNotMatch(observation, /start_runtime/);
});

test("chat remains virtualized and follows only explicit scroll intent", async () => {
  const chat = await read("src/components/tabs/ChatTab.tsx");
  assert.match(chat, /Virtuoso/);
  assert.match(chat, /followOutput=\{followOutput\}/);
  assert.match(chat, /atBottomStateChange=\{setIsAtBottom\}/);
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

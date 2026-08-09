import assert from "node:assert/strict";
import { readFile } from "node:fs/promises";
import test from "node:test";

const read = (path) => readFile(new URL(`../${path}`, import.meta.url), "utf8");
const readRoot = (path) => readFile(new URL(`../../../${path}`, import.meta.url), "utf8");

test("Workspace refinement loads after the base token stylesheet", async () => {
  const layout = await read("src/app/layout.tsx");
  const globalsIndex = layout.indexOf('import "./globals.css"');
  const workspaceIndex = layout.indexOf('import "./workspace.css"');

  assert.ok(globalsIndex >= 0, "base globals stylesheet must be loaded");
  assert.ok(workspaceIndex > globalsIndex, "workspace refinement layer must load after globals.css");
});

test("Workspace refinement layer stays token-driven and reduced-motion safe", async () => {
  const css = await read("src/app/workspace.css");

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
  const workflow = await readRoot(".github/workflows/publish.yml");

  assert.match(workflow, /runner: macos-15[\s\S]*?uname: arm64/);
  assert.match(workflow, /runner: macos-15-intel[\s\S]*?uname: x86_64/);
  assert.match(workflow, /pnpm tauri build --bundles app,dmg/);
  assert.match(workflow, /continuum-agent-os/);
  assert.match(workflow, /continuum-\$version-macos-aarch64\.dmg/);
  assert.match(workflow, /continuum-\$version-macos-x86_64\.dmg/);
  assert.match(workflow, /continuum-\$version-macos-aarch64\.app\.tar\.gz\.sig/);
  assert.match(workflow, /continuum-\$version-macos-x86_64\.app\.tar\.gz\.sig/);
  assert.match(workflow, /fail_on_unmatched_files: true/);
});

test("Agent OS mutations are journaled and typed before execution", async () => {
  const reliable = await readRoot("crates/continuum-mcp/src/reliable_agent_v2.rs");

  assert.match(reliable, /StepState::Dispatched/);
  assert.match(reliable, /automatic replay is blocked/i);
  assert.match(reliable, /json_pointer_exists:/);
  assert.match(reliable, /window_title_contains:/);
  assert.match(reliable, /element_present:/);
  assert.match(reliable, /Direct Agent OS mutations are disabled/);
});

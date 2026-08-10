import assert from "node:assert/strict";
import { readFile } from "node:fs/promises";
import path from "node:path";
import test from "node:test";
import { fileURLToPath } from "node:url";

import { runtimeBinaryName, runtimePaths } from "../../../scripts/dev-runtime.mjs";

const here = path.dirname(fileURLToPath(import.meta.url));
const repoRoot = path.resolve(here, "../../..");

test("runtime bootstrap resolves the Windows release binary into Tauri resources", () => {
  assert.equal(runtimeBinaryName("win32"), "continuum.exe");
  const paths = runtimePaths({ repoRoot, platform: "win32" });
  assert.equal(paths.source, path.join(repoRoot, "target", "release", "continuum.exe"));
  assert.equal(
    paths.staged,
    path.join(repoRoot, "apps", "desktop", "src-tauri", "resources", "bin", "continuum.exe")
  );
});

test("runtime bootstrap resolves the Unix release binary into Tauri resources", () => {
  assert.equal(runtimeBinaryName("linux"), "continuum");
  const paths = runtimePaths({ repoRoot, platform: "darwin" });
  assert.equal(paths.source, path.join(repoRoot, "target", "release", "continuum"));
  assert.equal(
    paths.staged,
    path.join(repoRoot, "apps", "desktop", "src-tauri", "resources", "bin", "continuum")
  );
});

test("Tauri dev always runs the live runtime bootstrap before Next.js", async () => {
  const configPath = path.join(repoRoot, "apps", "desktop", "src-tauri", "tauri.conf.json");
  const config = JSON.parse(await readFile(configPath, "utf8"));
  assert.equal(config.build.beforeDevCommand, "node ../../scripts/dev-runtime.mjs");
  assert.equal(config.bundle.resources["resources/bin"], "bin");
});

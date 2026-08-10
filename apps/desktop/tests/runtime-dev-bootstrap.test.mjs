import assert from "node:assert/strict";
import { mkdir, mkdtemp, readFile, rm, writeFile } from "node:fs/promises";
import os from "node:os";
import path from "node:path";
import test from "node:test";
import { fileURLToPath } from "node:url";

import {
  runtimeBinaryName,
  runtimePaths,
  stageRuntime,
} from "../../../scripts/dev-runtime.mjs";

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

test("runtime bootstrap stages the built runtime binary for desktop discovery", async () => {
  const tempRoot = await mkdtemp(path.join(os.tmpdir(), "continuum-runtime-bootstrap-"));
  try {
    const paths = runtimePaths({ repoRoot: tempRoot, platform: "win32" });
    await mkdir(path.dirname(paths.source), { recursive: true });
    await writeFile(paths.source, "synthetic-continuum-runtime", "utf8");

    let buildInvocations = 0;
    const staged = stageRuntime({
      repoRoot: tempRoot,
      platform: "win32",
      build(command, args, options) {
        buildInvocations += 1;
        assert.equal(command, "cargo");
        assert.deepEqual(args, ["build", "--release", "--locked", "--bin", "continuum"]);
        assert.equal(options.cwd, tempRoot);
        return { status: 0 };
      },
    });

    assert.equal(buildInvocations, 1);
    assert.equal(staged.staged, paths.staged);
    assert.equal(await readFile(paths.staged, "utf8"), "synthetic-continuum-runtime");
  } finally {
    await rm(tempRoot, { recursive: true, force: true });
  }
});

test("runtime bootstrap fails closed when the runtime build fails", () => {
  assert.throws(
    () =>
      stageRuntime({
        repoRoot,
        platform: "win32",
        build() {
          return { status: 101 };
        },
      }),
    /runtime build failed with exit code 101/i
  );
});

test("Tauri dev always runs the live runtime bootstrap before Next.js", async () => {
  const configPath = path.join(repoRoot, "apps", "desktop", "src-tauri", "tauri.conf.json");
  const config = JSON.parse(await readFile(configPath, "utf8"));
  assert.equal(config.build.beforeDevCommand, "node ../../scripts/dev-runtime.mjs");
  assert.equal(config.bundle.resources["resources/bin"], "bin");
});

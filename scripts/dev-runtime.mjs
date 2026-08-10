import { spawn, spawnSync } from "node:child_process";
import { chmodSync, copyFileSync, mkdirSync } from "node:fs";
import path from "node:path";
import { fileURLToPath } from "node:url";

const scriptPath = fileURLToPath(import.meta.url);
const defaultRepoRoot = path.resolve(path.dirname(scriptPath), "..");

export function runtimeBinaryName(platform = process.platform) {
  return platform === "win32" ? "continuum.exe" : "continuum";
}

export function runtimePaths({ repoRoot = defaultRepoRoot, platform = process.platform } = {}) {
  const binary = runtimeBinaryName(platform);
  return {
    source: path.join(repoRoot, "target", "release", binary),
    staged: path.join(repoRoot, "apps", "desktop", "src-tauri", "resources", "bin", binary),
    desktop: path.join(repoRoot, "apps", "desktop"),
  };
}

export function stageRuntime({
  repoRoot = defaultRepoRoot,
  platform = process.platform,
  build = spawnSync,
} = {}) {
  const paths = runtimePaths({ repoRoot, platform });
  console.log("\n== Preparing Continuum observation runtime");

  const result = build(
    "cargo",
    ["build", "--release", "--locked", "--bin", "continuum"],
    {
      cwd: repoRoot,
      stdio: "inherit",
      shell: false,
    }
  );

  if (result.error) {
    throw new Error(`Could not start cargo to build the Continuum runtime: ${result.error.message}`);
  }
  if (result.status !== 0) {
    throw new Error(`Continuum runtime build failed with exit code ${result.status ?? "unknown"}`);
  }

  mkdirSync(path.dirname(paths.staged), { recursive: true });
  copyFileSync(paths.source, paths.staged);
  if (platform !== "win32") chmodSync(paths.staged, 0o755);

  console.log(`== Runtime staged at ${paths.staged}`);
  return paths;
}

export function runDesktopDev({ repoRoot = defaultRepoRoot, platform = process.platform } = {}) {
  const paths = stageRuntime({ repoRoot, platform });
  const child = spawn("pnpm", ["dev"], {
    cwd: paths.desktop,
    env: process.env,
    stdio: "inherit",
    shell: platform === "win32",
  });

  const stop = () => {
    if (!child.killed) child.kill();
  };
  process.once("SIGINT", stop);
  process.once("SIGTERM", stop);

  child.on("error", (error) => {
    console.error(`Could not start the desktop frontend: ${error.message}`);
    process.exitCode = 1;
  });
  child.on("exit", (code, signal) => {
    if (signal && platform !== "win32") process.kill(process.pid, signal);
    else process.exitCode = code ?? (signal ? 1 : 0);
  });
}

if (process.argv[1] && path.resolve(process.argv[1]) === scriptPath) {
  try {
    runDesktopDev();
  } catch (error) {
    console.error(error instanceof Error ? error.message : String(error));
    process.exitCode = 1;
  }
}

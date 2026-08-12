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

export function devPort(value = process.env.CONTINUUM_DEV_PORT ?? process.env.PORT ?? "3000") {
  const port = Number(value);
  if (!Number.isInteger(port) || port < 1 || port > 65535) {
    throw new Error(`CONTINUUM_DEV_PORT must be a valid TCP port; received ${JSON.stringify(value)}`);
  }
  return port;
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
  const paths = runtimePaths({ repoRoot, platform });
  const port = devPort();
  // Start Next directly and pin its port. `next dev` otherwise silently moves
  // from a busy port (for example 3000 to 3001), while Tauri keeps polling the
  // devUrl selected by scripts/dev.ps1. Running Node directly also avoids the
  // Windows shell argument warning from spawning pnpm through `cmd.exe`.
  const nextCli = path.join(paths.desktop, "node_modules", "next", "dist", "bin", "next");
  const child = spawn(process.execPath, [nextCli, "dev", "--port", String(port)], {
    cwd: paths.desktop,
    env: process.env,
    stdio: "inherit",
    shell: false,
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

  // Tauri waits only 180 seconds for build.devUrl. A cold release-runtime
  // build can take longer than that, so stage it after Next is already
  // listening. The desktop dashboard can open immediately; the staged binary
  // is then ready for the next development launch.
  try {
    stageRuntime({ repoRoot, platform });
  } catch (error) {
    console.error(error instanceof Error ? error.message : String(error));
  }
}

if (process.argv[1] && path.resolve(process.argv[1]) === scriptPath) {
  try {
    runDesktopDev();
  } catch (error) {
    console.error(error instanceof Error ? error.message : String(error));
    process.exitCode = 1;
  }
}

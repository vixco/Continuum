#!/usr/bin/env bash
# Continuum local dev - one command to run the dashboard locally.
# No CI, no push, no release artifacts. Pure local loop.
#
# Usage:
#   ./scripts/dev.sh                  # Tauri desktop app (default) - the real frameless UI
#   ./scripts/dev.sh --frontend-only  # Next.js only, auto free port (no Rust/Tauri)
#   ./scripts/dev.sh --with-runtime   # also start `continuum` (release) for live data
#   ./scripts/dev.sh --check          # just verify prerequisites, don't run anything
#
# The default opens the real frameless dashboard with working window controls.
# Live perception/triage/voice data only flows when the runtime (the
# `continuum` binary) is running - pass --with-runtime for that, or click
# "Start runtime" in the titlebar.
#
# This is the macOS / Linux twin of scripts/dev.ps1. It deliberately
# mirrors the same flags and behaviour so contributors can switch OS
# without learning a different workflow. Where the platforms diverge
# (ONNX library name, port-collision probe, how to source corepack) the
# code branches inline with a comment explaining why.
#
# Requirements handled here:
#   - pnpm on PATH (corepack is treated as a first-class citizen, not a
#     Windows shim)
#   - cargo on PATH (only required for the Tauri / --with-runtime paths)
#   - a compatible ONNX Runtime shared library on disk
#   - a free TCP port to hand to Next.js + Tauri's devUrl
#
# What is NOT handled here (and intentionally left to dev-setup.ps1 /
# dev-setup.sh):
#   - LLVM/libclang, protoc, cmake, ninja
#   - Rust toolchain pinning
#   - Xcode CLT (mac) / build-essential (linux)

set -Eeuo pipefail

SCRIPT_DIR=$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)
REPO_ROOT=$(cd "$SCRIPT_DIR/.." && pwd)
DESKTOP_DIR="$REPO_ROOT/apps/desktop"

# shellcheck source=lib/onnx-runtime.sh
. "$SCRIPT_DIR/lib/onnx-runtime.sh"

# --- flag parsing --------------------------------------------------------
# Long-only options to keep muscle-memory consistent with the PowerShell
# script. We accept -FrontendOnly too (PowerShell is case-insensitive and
# drops the leading dashes on the call site, so the docs use both shapes).
FRONTEND_ONLY=0
WITH_RUNTIME=0
CHECK_ONLY=0
for arg in "$@"; do
  case "$arg" in
    --frontend-only|FrontendOnly|-FrontendOnly) FRONTEND_ONLY=1 ;;
    --with-runtime|WithRuntime|-WithRuntime)   WITH_RUNTIME=1 ;;
    --check|Check|-Check)                      CHECK_ONLY=1 ;;
    -h|--help)
      sed -n '2,16p' "$0"
      exit 0
      ;;
    *)
      echo "Unknown flag: $arg" >&2
      echo "Run with --help for usage." >&2
      exit 64
      ;;
  esac
done

# --- output helpers ------------------------------------------------------
# Coloured output without dragging in `tput` (which is missing on minimal
# Linux containers and varies between BSD/macOS). We just write ANSI
# sequences directly and let CI strip them if needed.
if [[ -t 1 ]]; then
  C_CYAN=$'\033[36m'
  C_GREEN=$'\033[32m'
  C_YELLOW=$'\033[33m'
  C_RED=$'\033[31m'
  C_GREY=$'\033[90m'
  C_RESET=$'\033[0m'
else
  C_CYAN=""; C_GREEN=""; C_YELLOW=""; C_RED=""; C_GREY=""; C_RESET=""
fi

write_step() { printf '\n%s== %s%s\n' "$C_CYAN" "$1" "$C_RESET"; }
write_ok()   { printf '  %sOK%s  %s\n' "$C_GREEN" "$C_RESET" "$1"; }
write_warn() { printf '  %s!%s   %s\n' "$C_YELLOW" "$C_RESET" "$1"; }
write_err()  { printf '  %sX%s   %s\n' "$C_RED" "$C_RESET" "$1"; }

# --- port discovery ------------------------------------------------------
# Find the first free TCP port starting at $1. We probe by trying to bind
# a listener on 127.0.0.1:port. The dev.ps1 version uses
# Get-NetTCPConnection instead because it inspects existing OS listeners
# without racing; on POSIX the analogue is lsof / ss, but those aren't
# always installed, so a quick bind-and-close is the most portable probe.
# The race window between probe and Next.js bind is benign on a dev box
# (the next attempt finds the next free port).
find_free_port() {
  local start="${1:-3000}"
  local p
  for (( p = start; p < start + 200; p++ )); do
    if port_is_free "$p"; then
      echo "$p"
      return 0
    fi
  done
  echo "No free dev port found in ${start}..$((start + 199)). Free a port and retry." >&2
  exit 1
}

# Returns 0 if the port can be bound, 1 if it is in use. Prefers python3
# because it ships with macOS and most Linux distros and uses the same
# socket interface everywhere. Falls back to `nc -z` (netcat), then to
# `lsof -iTCP:port -sTCP:LISTEN` (macOS). If nothing is available, assume
# the port is free; we'd rather race than refuse to start.
port_is_free() {
  local port="$1"
  if command -v python3 >/dev/null 2>&1; then
    python3 - "$port" <<'PY' >/dev/null 2>&1
import socket, sys
s = socket.socket()
try:
    s.bind(("127.0.0.1", int(sys.argv[1])))
    s.close()
except OSError:
    sys.exit(1)
sys.exit(0)
PY
    return $?
  fi
  if command -v nc >/dev/null 2>&1; then
    if nc -z 127.0.0.1 "$port" >/dev/null 2>&1; then
      return 1
    fi
    return 0
  fi
  if command -v lsof >/dev/null 2>&1; then
    if lsof -iTCP:"$port" -sTCP:LISTEN >/dev/null 2>&1; then
      return 1
    fi
    return 0
  fi
  return 0
}

# --- prerequisite checks -------------------------------------------------
# `need_rust=1` is passed for the Tauri / --with-runtime paths. Frontend
# alone only needs pnpm.
test_prereqs() {
  local need_rust="${1:-0}"
  local ok=1

  if ! command -v pnpm >/dev/null 2>&1; then
    # pnpm is published via corepack on most mac/linux setups. Enabling
    # corepack in the user's chosen package manager shim dir is enough to
    # put `pnpm` on PATH for this shell.
    if command -v corepack >/dev/null 2>&1; then
      write_warn "pnpm not on PATH - enabling via corepack"
      if corepack enable >/dev/null 2>&1; then
        write_ok "corepack enabled"
      else
        write_err "corepack enable failed - install pnpm manually (https://pnpm.io/installation)"
        ok=0
      fi
    else
      write_err "pnpm not on PATH and corepack missing - install Node 22+ and 'corepack enable'"
      ok=0
    fi
  else
    write_ok "pnpm found"
  fi

  if (( need_rust )); then
    if ! command -v cargo >/dev/null 2>&1; then
      write_err "cargo not on PATH - install Rust stable from https://rustup.rs"
      ok=0
    else
      write_ok "cargo found"
    fi
  fi

  if (( ! ok )); then
    return 1
  fi
  return 0
}

# --- --check: just validate ----------------------------------------------
if (( CHECK_ONLY )); then
  write_step "Checking prerequisites"
  prereqs_ok=1
  if ! test_prereqs 1; then prereqs_ok=0; fi
  if onnx_info=$(resolve_continuum_onnx_runtime "$REPO_ROOT" 2>/dev/null); then
    onnx_path="${onnx_info%%|*}"
    onnx_version="${onnx_info##*|}"
    write_ok "ONNX Runtime $onnx_version found at $onnx_path"
  else
    write_err "ONNX Runtime >= $CONTINUUM_MIN_ONNX_RUNTIME_VERSION not found"
    prereqs_ok=0
  fi
  printf '\n%sRun without flags to launch the Tauri dashboard.%s\n' "$C_GREY" "$C_RESET"
  if (( ! prereqs_ok )); then exit 1; fi
  exit 0
fi

# --- --frontend-only: Next.js, no Rust/Tauri ----------------------------
if (( FRONTEND_ONLY )); then
  port=$(find_free_port 3000)
  export PORT="$port"
  write_step "Frontend-only mode -> http://localhost:$port"
  if ! test_prereqs 0; then exit 1; fi
  ( cd "$DESKTOP_DIR" && exec pnpm dev )
fi

# --- default + --with-runtime: Tauri desktop app ------------------------
write_step "Tauri desktop app (frameless dashboard)"
if ! test_prereqs 1; then exit 1; fi

# ort uses dynamic loading. Without an explicit path, the OS would silently
# pick whatever ships with the platform (often 1.17 or older on macOS),
# while ort rc.11 requires 1.23+. Resolve and validate before Tauri starts
# so both the dashboard and the runtime process spawned by its button
# inherit the safe library.
if ! onnx_info=$(resolve_continuum_onnx_runtime "$REPO_ROOT" 2>&1); then
  write_err "$onnx_info"
  exit 1
fi
onnx_path="${onnx_info%%|*}"
onnx_version="${onnx_info##*|}"
export ORT_DYLIB_PATH="$onnx_path"
write_ok "ONNX Runtime $onnx_version -> $onnx_path"

if [[ ! -d "$DESKTOP_DIR/node_modules" ]]; then
  write_step "Installing desktop dependencies (first run only)"
  ( cd "$DESKTOP_DIR" && pnpm install )
fi

runtime_pid=""
if (( WITH_RUNTIME )); then
  write_step "Starting Continuum runtime (continuum, release)"
  # On mac/linux the artifact has no .exe suffix; cargo puts it directly
  # under target/release/ with the crate's binary name.
  bin="$REPO_ROOT/target/release/continuum"
  if [[ ! -x "$bin" ]]; then
    write_warn "Runtime binary not built - building (release; ~9 min first time)..."
    ( cd "$REPO_ROOT" && cargo build --release --bin continuum )
    bin="$REPO_ROOT/target/release/continuum"
  fi
  if [[ -x "$bin" ]]; then
    "$bin" &
    runtime_pid=$!
    write_ok "Runtime started (PID $runtime_pid)"
  else
    write_err "Runtime build failed - continuing without it (dashboard still runs)."
  fi
fi

# Pick a free port and point both Next.js (via $PORT) and Tauri's devUrl
# (via $TAURI_CONFIG) at it, so a stale dev server or any other app on
# 3000 can never wedge the dashboard - it just uses the next free port.
port=$(find_free_port 3000)
export PORT="$port"
export TAURI_CONFIG='{"build":{"devUrl":"http://localhost:'"$port"'"}}'
write_step "Launching Tauri dev on http://localhost:$port (compiles the Rust backend, then opens the window)"
printf '%s  Ctrl+C to stop.%s\n' "$C_GREY" "$C_RESET"

cleanup() {
  local exit_code=$?
  if [[ -n "$runtime_pid" ]] && kill -0 "$runtime_pid" 2>/dev/null; then
    write_step "Stopping runtime"
    kill "$runtime_pid" 2>/dev/null || true
    # Give it a beat to exit cleanly before SIGKILL. The PowerShell
    # version uses Stop-Process -Force, which is the equivalent of kill -9.
    for _ in 1 2 3 4 5; do
      kill -0 "$runtime_pid" 2>/dev/null || break
      sleep 0.2
    done
    kill -9 "$runtime_pid" 2>/dev/null || true
  fi
  exit "$exit_code"
}
trap cleanup EXIT INT TERM

( cd "$DESKTOP_DIR" && exec pnpm tauri dev )

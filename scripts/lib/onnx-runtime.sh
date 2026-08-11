#!/usr/bin/env bash
# Shared ONNX Runtime discovery for Continuum's macOS / Linux development scripts.
#
# Mirrors scripts/lib/onnx-runtime.ps1: search a fixed list of locations,
# accept the first candidate whose version is >= the minimum, and report
# everything rejected so the user can clean up. ort uses dynamic loading;
# without an explicit path the OS would silently pick whatever ships with
# the platform (often 1.17 or older on macOS), while ort rc.11 requires
# 1.23+.

set -u

# Minimum version Continuum supports. Kept in sync with the PowerScript.
CONTINUUM_MIN_ONNX_RUNTIME_VERSION="1.23"

# Resolve a library version from its install_name / soname. macOS exposes
# the version via otool -L (LC_ID_DYLIB load command) for a real dylib, but
# the file we have is often a symlink/fat lib without LC_ID_DYLIB set. We
# fall back to the filename (`libonnxruntime.<version>.dylib`), then to
# `otool -L` of the first dependent, then to `strings`. macOS doesn't put
# the version in Info.plist for dylibs the way Windows puts it in
# VERSIONINFO, so this is the most reliable cross-platform heuristic.
discover_onnx_runtime_version() {
  local lib_path="$1"
  if [[ ! -f "$lib_path" ]]; then
    return 1
  fi

  # 1. Filename convention: libonnxruntime.1.23.0.dylib or .so.1.23.0
  local from_name
  from_name=$(basename "$lib_path" | sed -nE 's/.*onnxruntime\.([0-9]+(\.[0-9]+)+).*/\1/p')
  if [[ -n "$from_name" ]]; then
    echo "$from_name"
    return 0
  fi

  # 1b. If the candidate is a symlink, follow it once and re-check the
  # target's basename. Homebrew (and most distro package managers) install
  # libonnxruntime.<X>.dylib and expose a canonical symlink with the bare
  # name; the actual version lives on the symlink's target.
  if [[ -L "$lib_path" ]]; then
    local target
    target=$(readlink "$lib_path" 2>/dev/null || true)
    if [[ -n "$target" ]]; then
      local target_dir
      target_dir=$(dirname "$lib_path")
      local resolved
      if [[ "$target" = /* ]]; then
        resolved="$target"
      else
        resolved="$target_dir/$target"
      fi
      from_name=$(basename "$resolved" | sed -nE 's/.*onnxruntime\.([0-9]+(\.[0-9]+)+).*/\1/p')
      if [[ -n "$from_name" ]]; then
        echo "$from_name"
        return 0
      fi
    fi
  fi

  # 2. otool -L: the install name often encodes the version on macOS.
  if command -v otool >/dev/null 2>&1; then
    local via_otool
    via_otool=$(otool -L "$lib_path" 2>/dev/null \
      | sed -nE 's/.*onnxruntime\.([0-9]+(\.[0-9]+)+).*/\1/p' \
      | head -n 1)
    if [[ -n "$via_otool" ]]; then
      echo "$via_otool"
      return 0
    fi
  fi

  # 3. strings: last-ditch; many builds put "Version: 1.23.0" in the binary.
  if command -v strings >/dev/null 2>&1; then
    local via_strings
    via_strings=$(strings "$lib_path" 2>/dev/null \
      | sed -nE 's/.*([0-9]+\.[0-9]+(\.[0-9]+)+).*/\1/p' \
      | awk -F. '{ printf("%d.%d\n", $1, $2) }' \
      | awk -v min="$CONTINUUM_MIN_ONNX_RUNTIME_VERSION" \
             '$1 > min || ($1 == min && $2 >= 0)' \
      | head -n 1)
    if [[ -n "$via_strings" ]]; then
      echo "$via_strings"
      return 0
    fi
  fi

  return 1
}

# Compare two dotted version strings. Echoes 1 if $1 >= $2, 0 otherwise.
# Pre-MVP: macOS ships BSD awk which doesn't have `gsub` semantics issues
# here, but we keep the logic trivial so the call site stays portable.
version_gte() {
  local actual="$1"
  local minimum="$2"
  local a_major a_minor b_major b_minor
  IFS=. read -r a_major a_minor _ <<<"$actual"
  IFS=. read -r b_major b_minor _ <<<"$minimum"
  a_major=${a_major:-0}; a_minor=${a_minor:-0}
  b_major=${b_major:-0}; b_minor=${b_minor:-0}
  if (( a_major > b_major )); then return 0; fi
  if (( a_major < b_major )); then return 1; fi
  (( a_minor >= b_minor ))
}

# Pick the right library filename for the current OS. The shape mirrors
# what ort looks for via `load-dynamic` and what brew / apt actually
# install: `libonnxruntime.dylib` on macOS (with a versioned sibling as
# fallback), `libonnxruntime.so` on Linux.
onnx_runtime_lib_name() {
  case "$(uname -s)" in
    Darwin) echo "libonnxruntime.dylib" ;;
    Linux)  echo "libonnxruntime.so" ;;
    *)      echo "libonnxruntime.so" ;;
  esac
}

# Resolve the highest-versioned compatible ONNX Runtime library. Echoes
# "path|version" on success, exits non-zero (with a message on stderr) if
# nothing acceptable was found. Candidate sources, in order:
#
#   1. $ORT_DYLIB_PATH (env var, may be a file or a directory)
#   2. $repo/.deps/onnxruntime/   (mirrors Windows layout)
#   3. macOS: ~/Library/Application Support/Continuum/onnxruntime/
#      Linux: ~/.local/share/Continuum/onnxruntime/
#   4. macOS: Homebrew prefixes (/opt/homebrew/opt/onnxruntime/lib,
#                              /usr/local/opt/onnxruntime/lib)
#   5. Linux: /usr/lib/onnxruntime, /usr/local/lib
#
# Anything we find but reject is reported in the error so the user can
# remove the stale copy and stop wondering why the dashboard's vision tab
# keeps falling back to the system one.
resolve_continuum_onnx_runtime() {
  local repo_root="$1"
  local lib_name
  lib_name=$(onnx_runtime_lib_name)

  local -a candidates=()
  local -a sources=()

  if [[ -n "${ORT_DYLIB_PATH:-}" ]]; then
    if [[ -d "$ORT_DYLIB_PATH" ]]; then
      candidates+=("$ORT_DYLIB_PATH/$lib_name" "$ORT_DYLIB_PATH/lib/$lib_name")
      sources+=("ORT_DYLIB_PATH" "ORT_DYLIB_PATH")
    elif [[ -f "$ORT_DYLIB_PATH" ]]; then
      candidates+=("$ORT_DYLIB_PATH")
      sources+=("ORT_DYLIB_PATH")
    fi
  fi

  candidates+=(
    "$repo_root/.deps/onnxruntime/$lib_name"
    "$repo_root/.deps/onnxruntime/lib/$lib_name"
  )
  sources+=("repo .deps" "repo .deps")

  case "$(uname -s)" in
    Darwin)
      candidates+=(
        "$HOME/Library/Application Support/Continuum/onnxruntime/$lib_name"
        "/opt/homebrew/opt/onnxruntime/lib/$lib_name"
        "/usr/local/opt/onnxruntime/lib/$lib_name"
        "/opt/homebrew/lib/$lib_name"
        "/usr/local/lib/$lib_name"
      )
      sources+=(
        "user dir"
        "Homebrew (Apple Silicon)"
        "Homebrew (Intel)"
        "Homebrew (Apple Silicon, flat)"
        "Homebrew (Intel, flat)"
      )
      ;;
    Linux)
      candidates+=(
        "$HOME/.local/share/Continuum/onnxruntime/$lib_name"
        "/usr/lib/onnxruntime/$lib_name"
        "/usr/lib/x86_64-linux-gnu/$lib_name"
        "/usr/local/lib/$lib_name"
      )
      sources+=(
        "user dir"
        "system /usr/lib/onnxruntime"
        "system multiarch"
        "system /usr/local/lib"
      )
      ;;
  esac

  local rejected=()
  local seen=""
  local i
  for i in "${!candidates[@]}"; do
    local path="${candidates[$i]}"
    local src="${sources[$i]}"
    # Skip duplicates: env-var candidates can overlap with repo ones when
    # the user pointed ORT_DYLIB_PATH at the same place.
    case "|$seen|" in
      *"|$path|"*) continue ;;
    esac
    seen+="|$path"

    [[ -f "$path" ]] || continue

    local version=""
    if version=$(discover_onnx_runtime_version "$path"); then
      if version_gte "$version" "$CONTINUUM_MIN_ONNX_RUNTIME_VERSION"; then
        echo "$path|$version"
        return 0
      fi
      rejected+=("$path ($version; from $src)")
    else
      rejected+=("$path (unknown version; from $src)")
    fi
  done

  {
    echo "Compatible ONNX Runtime not found. Continuum requires $lib_name >= $CONTINUUM_MIN_ONNX_RUNTIME_VERSION."
    if (( ${#rejected[@]} > 0 )); then
      echo "Found incompatible:"
      printf '  - %s\n' "${rejected[@]}"
    fi
    echo "Set ORT_DYLIB_PATH to the full compatible library path, or place it under the repo .deps/ dir."
  } >&2
  return 1
}

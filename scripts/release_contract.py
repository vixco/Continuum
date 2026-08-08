#!/usr/bin/env python3
"""Deterministic release planning and artifact validation for Continuum.

The workflow deliberately keeps version selection, updater metadata generation,
and the asset contract in reviewable Python instead of fragile shell globs.
Only the Python standard library is used so CI can run this on every platform.
"""

from __future__ import annotations

import argparse
import hashlib
import json
import os
import re
import subprocess
import sys
from dataclasses import dataclass
from pathlib import Path
from typing import Iterable, Sequence

SEMVER_RE = re.compile(
    r"^(?P<major>0|[1-9]\d*)\."
    r"(?P<minor>0|[1-9]\d*)\."
    r"(?P<patch>0|[1-9]\d*)"
    r"(?:-(?P<prerelease>[0-9A-Za-z-]+(?:\.[0-9A-Za-z-]+)*))?"
    r"(?:\+(?P<build>[0-9A-Za-z-]+(?:\.[0-9A-Za-z-]+)*))?$"
)
WORKSPACE_VERSION_RE = re.compile(
    r"(?ms)^\[workspace\.package\]\s*.*?^version\s*=\s*\"([^\"]+)\""
)
TAURI_TS_VERSION_RE = re.compile(r'version:\s*"([^"]+)"')
CONTINUUM_LOCK_PACKAGES = {
    "continuum-core",
    "continuum-desktop",
    "continuum-gateway",
    "continuum-llm",
    "continuum-mcp",
    "continuum-memory",
    "continuum-vision",
}

REQUIRED_ASSET_SUFFIXES = (
    "windows-x64-setup.exe",
    "windows-x64-setup.exe.sig",
    "windows-x64.zip",
    "macos-aarch64.dmg",
    "macos-aarch64.app.tar.gz",
    "macos-aarch64.app.tar.gz.sig",
    "macos-aarch64.tar.gz",
    "macos-x86_64.dmg",
    "macos-x86_64.app.tar.gz",
    "macos-x86_64.app.tar.gz.sig",
    "macos-x86_64.tar.gz",
)


class ContractError(RuntimeError):
    """A release invariant was violated."""


@dataclass(frozen=True, order=True)
class SemVer:
    major: int
    minor: int
    patch: int
    prerelease: tuple[str, ...] = ()
    build: tuple[str, ...] = ()

    @classmethod
    def parse(cls, raw: str) -> "SemVer":
        value = raw.strip()
        match = SEMVER_RE.fullmatch(value)
        if not match:
            raise ContractError(f"Unsupported SemVer: {raw!r}")
        prerelease = tuple((match.group("prerelease") or "").split("."))
        build = tuple((match.group("build") or "").split("."))
        return cls(
            int(match.group("major")),
            int(match.group("minor")),
            int(match.group("patch")),
            tuple(part for part in prerelease if part),
            tuple(part for part in build if part),
        )

    def __str__(self) -> str:
        core = f"{self.major}.{self.minor}.{self.patch}"
        pre = f"-{'.'.join(self.prerelease)}" if self.prerelease else ""
        build = f"+{'.'.join(self.build)}" if self.build else ""
        return f"{core}{pre}{build}"

    @property
    def tag(self) -> str:
        return f"v{self}"

    @property
    def numbered_prerelease(self) -> tuple[tuple[str, ...], int] | None:
        if len(self.prerelease) < 2 or not self.prerelease[-1].isdigit():
            return None
        return self.prerelease[:-1], int(self.prerelease[-1])


@dataclass(frozen=True)
class ReleasePlan:
    version: SemVer
    recovering: bool
    release_commit: str | None

    @property
    def tag(self) -> str:
        return self.version.tag


def run_git(repo_root: Path, *args: str, check: bool = True) -> str:
    process = subprocess.run(
        ["git", "-C", str(repo_root), *args],
        check=False,
        stdout=subprocess.PIPE,
        stderr=subprocess.PIPE,
        text=True,
        encoding="utf-8",
    )
    if check and process.returncode != 0:
        raise ContractError(
            f"git {' '.join(args)} failed: {process.stderr.strip() or process.stdout.strip()}"
        )
    return process.stdout.strip()


def read_workspace_version(repo_root: Path) -> SemVer:
    cargo_toml = (repo_root / "Cargo.toml").read_text(encoding="utf-8")
    match = WORKSPACE_VERSION_RE.search(cargo_toml)
    if not match:
        raise ContractError("Cargo.toml has no [workspace.package] version")
    return SemVer.parse(match.group(1))


def normalize_tag(raw: str) -> str:
    value = raw.strip()
    return value if value.startswith("v") else f"v{value}"


def read_tag_file(path: Path | None) -> set[str]:
    if path is None or not path.exists():
        return set()
    return {
        normalize_tag(line)
        for line in path.read_text(encoding="utf-8").splitlines()
        if line.strip()
    }


def list_git_tags(repo_root: Path) -> set[str]:
    output = run_git(repo_root, "tag", "--list")
    return {line.strip() for line in output.splitlines() if line.strip()}


def parse_version_tags(tags: Iterable[str]) -> dict[str, SemVer]:
    parsed: dict[str, SemVer] = {}
    for tag in tags:
        normalized = normalize_tag(tag)
        try:
            parsed[normalized] = SemVer.parse(normalized.removeprefix("v"))
        except ContractError:
            continue
    return parsed


def first_parent(repo_root: Path, commit: str) -> str | None:
    output = run_git(repo_root, "rev-list", "--parents", "-n", "1", commit)
    parts = output.split()
    return parts[1] if len(parts) > 1 else None


def resolve_tag_commit(repo_root: Path, tag: str) -> str:
    return run_git(repo_root, "rev-list", "-n", "1", tag)


def same_release_series(left: SemVer, right: SemVer) -> bool:
    if (left.major, left.minor, left.patch) != (right.major, right.minor, right.patch):
        return False
    left_numbered = left.numbered_prerelease
    right_numbered = right.numbered_prerelease
    if left_numbered is None or right_numbered is None:
        return left.prerelease == right.prerelease
    return left_numbered[0] == right_numbered[0]


def next_available_version(source: SemVer, used: Sequence[SemVer]) -> SemVer:
    used_strings = {str(version) for version in used}
    numbered = source.numbered_prerelease
    if numbered is not None:
        prefix, source_number = numbered
        matching_numbers = [
            candidate[1]
            for version in used
            if (candidate := version.numbered_prerelease) is not None
            and (version.major, version.minor, version.patch)
            == (source.major, source.minor, source.patch)
            and candidate[0] == prefix
        ]
        if str(source) not in used_strings and not any(
            number >= source_number for number in matching_numbers
        ):
            return source
        return SemVer(
            source.major,
            source.minor,
            source.patch,
            (*prefix, str(max([source_number, *matching_numbers]) + 1)),
        )

    if str(source) not in used_strings:
        return source

    if source.prerelease:
        raise ContractError(
            "Automatic increments require a numeric prerelease suffix, "
            f"for example {source}-1"
        )

    patches = [
        version.patch
        for version in used
        if not version.prerelease
        and (version.major, version.minor) == (source.major, source.minor)
    ]
    return SemVer(source.major, source.minor, max([source.patch, *patches]) + 1)


def plan_release(
    repo_root: Path,
    source_sha: str,
    published_tags: set[str],
    explicit_version: str | None = None,
) -> ReleasePlan:
    source_sha = run_git(repo_root, "rev-parse", source_sha)
    all_tags = list_git_tags(repo_root)
    parsed_tags = parse_version_tags(all_tags)
    source_version = read_workspace_version(repo_root)
    requested = SemVer.parse(explicit_version) if explicit_version else source_version

    # Recover only a tag whose release commit is exactly this source commit or
    # has this source commit as its first parent. This prevents stale orphaned
    # tags from older code from being attached to a new release.
    recoverable: list[tuple[SemVer, str]] = []
    for tag, version in parsed_tags.items():
        if tag in published_tags or not same_release_series(version, requested):
            continue
        commit = resolve_tag_commit(repo_root, tag)
        if commit == source_sha or first_parent(repo_root, commit) == source_sha:
            recoverable.append((version, commit))

    if explicit_version:
        tag = requested.tag
        if tag in published_tags:
            raise ContractError(f"Release {tag} is already published")
        if tag in parsed_tags:
            commit = resolve_tag_commit(repo_root, tag)
            if commit == source_sha or first_parent(repo_root, commit) == source_sha:
                return ReleasePlan(requested, True, commit)
            raise ContractError(
                f"Tag {tag} already points to unrelated commit {commit}; choose another version"
            )
        return ReleasePlan(requested, False, None)

    if recoverable:
        version, commit = max(recoverable, key=lambda item: semver_sort_key(item[0]))
        return ReleasePlan(version, True, commit)

    used_versions = list(parsed_tags.values())
    used_versions.extend(
        SemVer.parse(tag.removeprefix("v"))
        for tag in published_tags
        if SEMVER_RE.fullmatch(tag.removeprefix("v"))
    )
    return ReleasePlan(next_available_version(requested, used_versions), False, None)


def prerelease_part_key(part: str) -> tuple[int, int | str]:
    if part.isdigit():
        return (0, int(part))
    return (1, part)


def semver_sort_key(version: SemVer) -> tuple[object, ...]:
    # Stable is newer than prerelease for the same core.
    pre_key: tuple[object, ...]
    if version.prerelease:
        pre_key = (0, *(prerelease_part_key(part) for part in version.prerelease))
    else:
        pre_key = (1,)
    return (version.major, version.minor, version.patch, pre_key)


def write_github_output(path: Path | None, values: dict[str, str]) -> None:
    if path is None:
        for key, value in values.items():
            print(f"{key}={value}")
        return
    with path.open("a", encoding="utf-8", newline="\n") as handle:
        for key, value in values.items():
            if "\n" in value or "\r" in value:
                raise ContractError(f"GitHub output {key!r} contains a newline")
            handle.write(f"{key}={value}\n")


def validate_release_config(repo_root: Path) -> None:
    tauri_path = repo_root / "apps/desktop/src-tauri/tauri.conf.json"
    config = json.loads(tauri_path.read_text(encoding="utf-8"))
    bundle = config.get("bundle") or {}
    targets = set(bundle.get("targets") or [])
    missing = {"nsis", "dmg"} - targets
    if missing:
        raise ContractError(f"Tauri bundle targets are missing: {sorted(missing)}")
    if bundle.get("active") is not True:
        raise ContractError("Tauri bundling must be active")
    if bundle.get("createUpdaterArtifacts") is not True:
        raise ContractError("Tauri createUpdaterArtifacts must be true")

    updater = (config.get("plugins") or {}).get("updater") or {}
    if not str(updater.get("pubkey") or "").strip():
        raise ContractError("Tauri updater pubkey is missing")
    endpoints = updater.get("endpoints") or []
    expected = "https://github.com/vixco/Continuum/releases/latest/download/latest.json"
    if expected not in endpoints:
        raise ContractError(
            "Tauri updater must read the latest.json asset from the latest GitHub Release"
        )


def cargo_lock_versions(repo_root: Path) -> dict[str, str]:
    text = (repo_root / "Cargo.lock").read_text(encoding="utf-8")
    packages: dict[str, str] = {}
    for block in text.split("[[package]]"):
        name_match = re.search(r'(?m)^name = "([^"]+)"$', block)
        version_match = re.search(r'(?m)^version = "([^"]+)"$', block)
        if name_match and version_match and name_match.group(1) in CONTINUUM_LOCK_PACKAGES:
            packages[name_match.group(1)] = version_match.group(1)
    return packages


def validate_version(repo_root: Path, expected: str) -> None:
    SemVer.parse(expected)
    observed: dict[str, str] = {
        "Cargo.toml": str(read_workspace_version(repo_root)),
        "apps/desktop/package.json": json.loads(
            (repo_root / "apps/desktop/package.json").read_text(encoding="utf-8")
        ).get("version", ""),
        "apps/desktop/src-tauri/tauri.conf.json": json.loads(
            (repo_root / "apps/desktop/src-tauri/tauri.conf.json").read_text(
                encoding="utf-8"
            )
        ).get("version", ""),
    }
    tauri_ts = (repo_root / "apps/desktop/src/lib/tauri.ts").read_text(encoding="utf-8")
    versions = set(TAURI_TS_VERSION_RE.findall(tauri_ts))
    if expected not in versions:
        observed["apps/desktop/src/lib/tauri.ts"] = ", ".join(sorted(versions)) or "<missing>"

    for package, version in cargo_lock_versions(repo_root).items():
        observed[f"Cargo.lock::{package}"] = version

    mismatches = {
        location: version for location, version in observed.items() if version != expected
    }
    missing_lock = CONTINUUM_LOCK_PACKAGES - set(cargo_lock_versions(repo_root))
    if missing_lock:
        mismatches["Cargo.lock::missing"] = ", ".join(sorted(missing_lock))
    if mismatches:
        rendered = "; ".join(f"{key}={value!r}" for key, value in sorted(mismatches.items()))
        raise ContractError(f"Version {expected} is not synchronized: {rendered}")


def sha256(path: Path) -> str:
    digest = hashlib.sha256()
    with path.open("rb") as handle:
        for chunk in iter(lambda: handle.read(1024 * 1024), b""):
            digest.update(chunk)
    return digest.hexdigest()


def read_signature(path: Path) -> str:
    signature = "".join(path.read_text(encoding="utf-8").split())
    if not signature:
        raise ContractError(f"Updater signature is empty: {path.name}")
    return signature


def expected_asset_names(version: str) -> list[str]:
    return [f"continuum-{version}-{suffix}" for suffix in REQUIRED_ASSET_SUFFIXES]


def assemble_release(
    assets_dir: Path,
    version: str,
    tag: str,
    repository: str,
    source_sha: str,
) -> None:
    SemVer.parse(version)
    if tag != f"v{version}":
        raise ContractError(f"Tag {tag!r} does not match version {version!r}")
    if not re.fullmatch(r"[A-Za-z0-9_.-]+/[A-Za-z0-9_.-]+", repository):
        raise ContractError(f"Invalid GitHub repository: {repository!r}")
    if not re.fullmatch(r"[0-9a-fA-F]{40}", source_sha):
        raise ContractError("source_sha must be a full 40-character Git commit SHA")

    assets_dir.mkdir(parents=True, exist_ok=True)
    expected = expected_asset_names(version)
    missing = [name for name in expected if not (assets_dir / name).is_file()]
    if missing:
        raise ContractError(f"Release is missing required assets: {', '.join(missing)}")
    empty = [name for name in expected if (assets_dir / name).stat().st_size == 0]
    if empty:
        raise ContractError(f"Release contains empty assets: {', '.join(empty)}")

    win_sig = read_signature(
        assets_dir / f"continuum-{version}-windows-x64-setup.exe.sig"
    )
    arm_sig = read_signature(
        assets_dir / f"continuum-{version}-macos-aarch64.app.tar.gz.sig"
    )
    intel_sig = read_signature(
        assets_dir / f"continuum-{version}-macos-x86_64.app.tar.gz.sig"
    )
    release_base = f"https://github.com/{repository}/releases/download/{tag}"

    latest = {
        "version": version,
        "notes": f"Continuum {tag}. See the GitHub release notes for details.",
        "pub_date": os.environ.get("CONTINUUM_RELEASE_PUB_DATE", ""),
        "platforms": {
            "windows-x86_64": {
                "signature": win_sig,
                "url": f"{release_base}/continuum-{version}-windows-x64-setup.exe",
            },
            "darwin-aarch64": {
                "signature": arm_sig,
                "url": f"{release_base}/continuum-{version}-macos-aarch64.app.tar.gz",
            },
            "darwin-x86_64": {
                "signature": intel_sig,
                "url": f"{release_base}/continuum-{version}-macos-x86_64.app.tar.gz",
            },
        },
    }
    if not latest["pub_date"]:
        from datetime import datetime, timezone

        latest["pub_date"] = datetime.now(timezone.utc).replace(microsecond=0).isoformat().replace(
            "+00:00", "Z"
        )

    asset_rows = []
    for name in sorted(expected):
        path = assets_dir / name
        asset_rows.append(
            {"name": name, "bytes": path.stat().st_size, "sha256": sha256(path)}
        )

    manifest = {
        "schema_version": 1,
        "product": "Continuum",
        "version": version,
        "tag": tag,
        "source_sha": source_sha.lower(),
        "repository": repository,
        "required_platforms": ["windows-x86_64", "darwin-aarch64", "darwin-x86_64"],
        "assets": asset_rows,
    }

    (assets_dir / "latest.json").write_text(
        json.dumps(latest, indent=2, sort_keys=True) + "\n", encoding="utf-8"
    )
    (assets_dir / "release-manifest.json").write_text(
        json.dumps(manifest, indent=2, sort_keys=True) + "\n", encoding="utf-8"
    )

    checksum_targets = sorted(
        [*expected, "latest.json", "release-manifest.json"]
    )
    checksum_lines = [
        f"{sha256(assets_dir / name)}  {name}" for name in checksum_targets
    ]
    (assets_dir / "SHA256SUMS.txt").write_text(
        "\n".join(checksum_lines) + "\n", encoding="utf-8"
    )

    final_names = {path.name for path in assets_dir.iterdir() if path.is_file()}
    required_final = {
        *expected,
        "latest.json",
        "release-manifest.json",
        "SHA256SUMS.txt",
    }
    missing_final = required_final - final_names
    if missing_final:
        raise ContractError(f"Failed to assemble: {sorted(missing_final)}")


def verify_published_release(release_json: Path, version: str) -> None:
    SemVer.parse(version)
    payload = json.loads(release_json.read_text(encoding="utf-8"))
    if payload.get("draft") is True:
        raise ContractError("GitHub Release is still a draft")
    tag = payload.get("tag_name")
    if tag != f"v{version}":
        raise ContractError(
            f"Published release tag {tag!r} does not match version {version!r}"
        )
    names = {
        str(asset.get("name"))
        for asset in payload.get("assets", [])
        if isinstance(asset, dict) and asset.get("name")
    }
    required = {
        *expected_asset_names(version),
        "latest.json",
        "release-manifest.json",
        "SHA256SUMS.txt",
    }
    missing = sorted(required - names)
    if missing:
        raise ContractError(
            "Published GitHub Release is missing required assets: " + ", ".join(missing)
        )
    empty = sorted(
        str(asset.get("name"))
        for asset in payload.get("assets", [])
        if isinstance(asset, dict)
        and asset.get("name") in required
        and int(asset.get("size") or 0) <= 0
    )
    if empty:
        raise ContractError(
            "Published GitHub Release contains empty assets: " + ", ".join(empty)
        )


def build_parser() -> argparse.ArgumentParser:
    parser = argparse.ArgumentParser()
    subparsers = parser.add_subparsers(dest="command", required=True)

    next_parser = subparsers.add_parser("next-version")
    next_parser.add_argument("--repo-root", type=Path, default=Path("."))
    next_parser.add_argument("--source-sha", required=True)
    next_parser.add_argument("--published-tags-file", type=Path)
    next_parser.add_argument("--version")
    next_parser.add_argument("--github-output", type=Path)

    config_parser = subparsers.add_parser("validate-config")
    config_parser.add_argument("--repo-root", type=Path, default=Path("."))

    version_parser = subparsers.add_parser("validate-version")
    version_parser.add_argument("--repo-root", type=Path, default=Path("."))
    version_parser.add_argument("--version", required=True)

    assemble_parser = subparsers.add_parser("assemble")
    assemble_parser.add_argument("--assets-dir", type=Path, required=True)
    assemble_parser.add_argument("--version", required=True)
    assemble_parser.add_argument("--tag", required=True)
    assemble_parser.add_argument("--repository", required=True)
    assemble_parser.add_argument("--source-sha", required=True)

    verify_parser = subparsers.add_parser("verify-published")
    verify_parser.add_argument("--release-json", type=Path, required=True)
    verify_parser.add_argument("--version", required=True)

    return parser


def main(argv: Sequence[str] | None = None) -> int:
    args = build_parser().parse_args(argv)
    try:
        if args.command == "next-version":
            published = read_tag_file(args.published_tags_file)
            plan = plan_release(
                args.repo_root.resolve(),
                args.source_sha,
                published,
                args.version,
            )
            write_github_output(
                args.github_output,
                {
                    "version": str(plan.version),
                    "tag": plan.tag,
                    "recovering": str(plan.recovering).lower(),
                    "release_commit": plan.release_commit or "",
                },
            )
        elif args.command == "validate-config":
            validate_release_config(args.repo_root.resolve())
        elif args.command == "validate-version":
            validate_version(args.repo_root.resolve(), args.version)
        elif args.command == "assemble":
            assemble_release(
                args.assets_dir.resolve(),
                args.version,
                args.tag,
                args.repository,
                args.source_sha,
            )
        elif args.command == "verify-published":
            verify_published_release(args.release_json.resolve(), args.version)
        else:
            raise ContractError(f"Unknown command: {args.command}")
    except (ContractError, OSError, json.JSONDecodeError) as error:
        print(f"release contract failed: {error}", file=sys.stderr)
        return 1
    return 0


if __name__ == "__main__":
    raise SystemExit(main())

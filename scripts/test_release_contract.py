#!/usr/bin/env python3
from __future__ import annotations

import importlib.util
import json
import os
import subprocess
import sys
import tempfile
import unittest
from pathlib import Path

MODULE_PATH = Path(__file__).with_name("release_contract.py")
SPEC = importlib.util.spec_from_file_location("release_contract", MODULE_PATH)
assert SPEC and SPEC.loader
release_contract = importlib.util.module_from_spec(SPEC)
sys.modules[SPEC.name] = release_contract
SPEC.loader.exec_module(release_contract)


def git(root: Path, *args: str) -> str:
    result = subprocess.run(
        ["git", "-C", str(root), *args],
        check=True,
        stdout=subprocess.PIPE,
        stderr=subprocess.PIPE,
        text=True,
        encoding="utf-8",
    )
    return result.stdout.strip()


def initialize_repo(root: Path, version: str = "0.1.0-alpha.11") -> str:
    (root / "apps/desktop/src-tauri").mkdir(parents=True)
    (root / "apps/desktop/src/lib").mkdir(parents=True)
    (root / "Cargo.toml").write_text(
        f'[workspace]\nmembers = []\n\n[workspace.package]\nversion = "{version}"\n',
        encoding="utf-8",
    )
    (root / "apps/desktop/package.json").write_text(
        json.dumps({"version": version}), encoding="utf-8"
    )
    (root / "apps/desktop/src-tauri/tauri.conf.json").write_text(
        json.dumps(
            {
                "version": version,
                "bundle": {
                    "active": True,
                    "createUpdaterArtifacts": True,
                    "targets": ["nsis", "dmg"],
                },
                "plugins": {
                    "updater": {
                        "pubkey": "public-key",
                        "endpoints": [
                            "https://github.com/vixco/Continuum/releases/latest/download/latest.json"
                        ],
                    }
                },
            }
        ),
        encoding="utf-8",
    )
    (root / "apps/desktop/src/lib/tauri.ts").write_text(
        f'const state = {{ version: "{version}" }};\n', encoding="utf-8"
    )
    lock = []
    for package in sorted(release_contract.CONTINUUM_LOCK_PACKAGES):
        lock.append(f'[[package]]\nname = "{package}"\nversion = "{version}"\n')
    (root / "Cargo.lock").write_text("\n".join(lock), encoding="utf-8")

    git(root, "init")
    git(root, "config", "user.name", "Continuum Test")
    git(root, "config", "user.email", "test@example.invalid")
    git(root, "add", ".")
    git(root, "commit", "-m", "source")
    return git(root, "rev-parse", "HEAD")


class SemVerTests(unittest.TestCase):
    def test_numbered_prerelease_increments_past_existing_tags(self) -> None:
        source = release_contract.SemVer.parse("0.1.0-alpha.11")
        used = [
            release_contract.SemVer.parse("0.1.0-alpha.9"),
            release_contract.SemVer.parse("0.1.0-alpha.12"),
        ]
        self.assertEqual(
            str(release_contract.next_available_version(source, used)),
            "0.1.0-alpha.13",
        )

    def test_untagged_source_version_is_used_without_skipping(self) -> None:
        source = release_contract.SemVer.parse("0.2.0-beta.4")
        used = [release_contract.SemVer.parse("0.2.0-beta.3")]
        self.assertEqual(
            str(release_contract.next_available_version(source, used)),
            "0.2.0-beta.4",
        )


class PlanTests(unittest.TestCase):
    def test_current_orphan_tags_from_other_commits_are_not_reused(self) -> None:
        with tempfile.TemporaryDirectory() as directory:
            root = Path(directory)
            source_sha = initialize_repo(root)
            git(root, "tag", "v0.1.0-alpha.11")
            git(root, "commit", "--allow-empty", "-m", "new source")
            current_sha = git(root, "rev-parse", "HEAD")
            plan = release_contract.plan_release(root, current_sha, set())
            self.assertEqual(str(plan.version), "0.1.0-alpha.12")
            self.assertFalse(plan.recovering)
            self.assertNotEqual(source_sha, current_sha)

    def test_tagged_release_commit_for_same_source_is_recovered(self) -> None:
        with tempfile.TemporaryDirectory() as directory:
            root = Path(directory)
            source_sha = initialize_repo(root)
            git(root, "commit", "--allow-empty", "-m", "release snapshot")
            release_commit = git(root, "rev-parse", "HEAD")
            git(root, "tag", "-a", "v0.1.0-alpha.11", "-m", "release")
            plan = release_contract.plan_release(root, source_sha, set())
            self.assertEqual(str(plan.version), "0.1.0-alpha.11")
            self.assertTrue(plan.recovering)
            self.assertEqual(plan.release_commit, release_commit)

    def test_complete_release_for_same_source_is_a_noop(self) -> None:
        with tempfile.TemporaryDirectory() as directory:
            root = Path(directory)
            source_sha = initialize_repo(root)
            git(root, "commit", "--allow-empty", "-m", "release snapshot")
            release_commit = git(root, "rev-parse", "HEAD")
            git(root, "tag", "v0.1.0-alpha.11")
            plan = release_contract.plan_release(
                root, source_sha, {"v0.1.0-alpha.11"}
            )
            self.assertEqual(str(plan.version), "0.1.0-alpha.11")
            self.assertFalse(plan.recovering)
            self.assertTrue(plan.already_published)
            self.assertEqual(plan.release_commit, release_commit)


class ContractTests(unittest.TestCase):
    def test_version_and_tauri_contract_validate(self) -> None:
        with tempfile.TemporaryDirectory() as directory:
            root = Path(directory)
            initialize_repo(root)
            release_contract.validate_release_config(root)
            release_contract.validate_version(root, "0.1.0-alpha.11")

    def test_release_config_rejects_raw_main_updater_endpoint(self) -> None:
        with tempfile.TemporaryDirectory() as directory:
            root = Path(directory)
            initialize_repo(root)
            config_path = root / "apps/desktop/src-tauri/tauri.conf.json"
            config = json.loads(config_path.read_text(encoding="utf-8"))
            config["plugins"]["updater"]["endpoints"] = [
                "https://raw.githubusercontent.com/vixco/Continuum/main/latest.json"
            ]
            config_path.write_text(json.dumps(config), encoding="utf-8")
            with self.assertRaises(release_contract.ContractError):
                release_contract.validate_release_config(root)

    def test_assemble_requires_all_three_platforms(self) -> None:
        with tempfile.TemporaryDirectory() as directory:
            assets = Path(directory)
            version = "0.1.0-alpha.12"
            for name in release_contract.expected_asset_names(version):
                (assets / name).write_bytes(b"asset")
            for name in (
                f"continuum-{version}-windows-x64-setup.exe.sig",
                f"continuum-{version}-macos-aarch64.app.tar.gz.sig",
                f"continuum-{version}-macos-x86_64.app.tar.gz.sig",
            ):
                (assets / name).write_text("signature\n", encoding="utf-8")

            old = os.environ.get("CONTINUUM_RELEASE_PUB_DATE")
            os.environ["CONTINUUM_RELEASE_PUB_DATE"] = "2026-08-09T12:00:00Z"
            try:
                release_contract.assemble_release(
                    assets,
                    version,
                    f"v{version}",
                    "vixco/Continuum",
                    "a" * 40,
                )
            finally:
                if old is None:
                    os.environ.pop("CONTINUUM_RELEASE_PUB_DATE", None)
                else:
                    os.environ["CONTINUUM_RELEASE_PUB_DATE"] = old

            latest = json.loads((assets / "latest.json").read_text(encoding="utf-8"))
            self.assertEqual(
                set(latest["platforms"]),
                {"windows-x86_64", "darwin-aarch64", "darwin-x86_64"},
            )
            manifest = json.loads(
                (assets / "release-manifest.json").read_text(encoding="utf-8")
            )
            self.assertEqual(len(manifest["assets"]), 11)
            checksums = (assets / "SHA256SUMS.txt").read_text(encoding="utf-8")
            self.assertIn(f"continuum-{version}-macos-x86_64.dmg", checksums)

    def test_verify_published_requires_every_platform_asset(self) -> None:
        with tempfile.TemporaryDirectory() as directory:
            root = Path(directory)
            version = "0.1.0-alpha.12"
            release_json = root / "release.json"
            release_json.write_text(
                json.dumps(
                    {
                        "tag_name": f"v{version}",
                        "draft": False,
                        "assets": [
                            {"name": name, "size": 10}
                            for name in [
                                *release_contract.expected_asset_names(version),
                                "latest.json",
                                "release-manifest.json",
                                "SHA256SUMS.txt",
                            ]
                        ],
                    }
                ),
                encoding="utf-8",
            )
            release_contract.verify_published_release(release_json, version)
            payload = json.loads(release_json.read_text(encoding="utf-8"))
            payload["assets"] = [
                asset
                for asset in payload["assets"]
                if not asset["name"].endswith("macos-x86_64.dmg")
            ]
            release_json.write_text(json.dumps(payload), encoding="utf-8")
            with self.assertRaises(release_contract.ContractError):
                release_contract.verify_published_release(release_json, version)

    def test_complete_tags_excludes_partial_release(self) -> None:
        with tempfile.TemporaryDirectory() as directory:
            root = Path(directory)
            version = "0.1.0-alpha.12"
            complete_assets = [
                {"name": name, "size": 10}
                for name in [
                    *release_contract.expected_asset_names(version),
                    "latest.json",
                    "release-manifest.json",
                    "SHA256SUMS.txt",
                ]
            ]
            lines = root / "releases.ndjson"
            lines.write_text(
                "\n".join(
                    [
                        json.dumps(
                            {
                                "tag_name": f"v{version}",
                                "draft": False,
                                "assets": complete_assets,
                            }
                        ),
                        json.dumps(
                            {
                                "tag_name": "v0.1.0-alpha.13",
                                "draft": False,
                                "assets": complete_assets[:-1],
                            }
                        ),
                    ]
                )
                + "\n",
                encoding="utf-8",
            )
            output = root / "complete.txt"
            release_contract.write_complete_release_tags(lines, output)
            self.assertEqual(output.read_text(encoding="utf-8"), f"v{version}\n")

    def test_assemble_fails_when_intel_dmg_is_missing(self) -> None:
        with tempfile.TemporaryDirectory() as directory:
            assets = Path(directory)
            version = "0.1.0-alpha.12"
            names = release_contract.expected_asset_names(version)
            for name in names:
                if not name.endswith("macos-x86_64.dmg"):
                    (assets / name).write_bytes(b"asset")
            with self.assertRaises(release_contract.ContractError):
                release_contract.assemble_release(
                    assets,
                    version,
                    f"v{version}",
                    "vixco/Continuum",
                    "b" * 40,
                )


if __name__ == "__main__":
    unittest.main()

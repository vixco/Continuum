#!/usr/bin/env python3
"""Generate one deterministic CycloneDX inventory for Rust and pnpm packages."""

from __future__ import annotations

import json
import subprocess
import urllib.parse
import uuid
from pathlib import Path


ROOT = Path(__file__).resolve().parents[1]
OUTPUT = ROOT / "docs" / "sbom" / "continuum.cdx.json"


def command_json(*command: str) -> object:
    result = subprocess.run(
        command,
        cwd=ROOT,
        check=True,
        stdout=subprocess.PIPE,
        text=True,
        encoding="utf-8",
    )
    return json.loads(result.stdout)


def npm_purl(name: str, version: str) -> str:
    return f"pkg:npm/{urllib.parse.quote(name, safe='')}@{version}"


def collect_npm(
    node: dict[str, object],
    components: dict[str, dict[str, object]],
    dependencies: dict[str, set[str]],
    name_hint: str | None = None,
) -> str | None:
    name = node.get("name", name_hint)
    version = node.get("version")
    current_ref = None
    if isinstance(name, str) and isinstance(version, str):
        current_ref = npm_purl(name, version)
        components.setdefault(
            current_ref,
            {
                "type": "library",
                "group": name.split("/", 1)[0][1:] if name.startswith("@") else "",
                "name": name.split("/", 1)[-1],
                "version": version,
                "purl": current_ref,
                "bom-ref": current_ref,
                "properties": [{"name": "continuum:ecosystem", "value": "npm"}],
            },
        )

    child_refs: set[str] = set()
    for bucket in ("dependencies", "devDependencies", "optionalDependencies"):
        children = node.get(bucket)
        if not isinstance(children, dict):
            continue
        for child_name, child in children.items():
            if isinstance(child, dict):
                child_ref = collect_npm(
                    child,
                    components,
                    dependencies,
                    str(child_name),
                )
                if child_ref:
                    child_refs.add(child_ref)
    if current_ref:
        dependencies.setdefault(current_ref, set()).update(child_refs)
    return current_ref


def main() -> None:
    cargo = command_json("cargo", "metadata", "--format-version", "1", "--locked")
    pnpm = command_json("pnpm", "list", "--recursive", "--depth", "Infinity", "--json")

    components: dict[str, dict[str, object]] = {}
    dependencies: dict[str, set[str]] = {}
    cargo_refs: dict[str, str] = {}

    assert isinstance(cargo, dict)
    for package in cargo.get("packages", []):
        if not isinstance(package, dict):
            continue
        name = str(package["name"])
        version = str(package["version"])
        ref = f"pkg:cargo/{name}@{version}"
        cargo_refs[str(package["id"])] = ref
        component: dict[str, object] = {
            "type": "library",
            "name": name,
            "version": version,
            "purl": ref,
            "bom-ref": ref,
            "properties": [{"name": "continuum:ecosystem", "value": "cargo"}],
        }
        license_expression = package.get("license")
        if isinstance(license_expression, str):
            component["licenses"] = [{"expression": license_expression}]
        components.setdefault(ref, component)

    resolve = cargo.get("resolve")
    if isinstance(resolve, dict):
        for node in resolve.get("nodes", []):
            if not isinstance(node, dict):
                continue
            ref = cargo_refs.get(str(node.get("id")))
            if not ref:
                continue
            dependencies.setdefault(ref, set()).update(
                cargo_refs[dependency]
                for dependency in map(str, node.get("dependencies", []))
                if dependency in cargo_refs
            )

    if isinstance(pnpm, list):
        for workspace in pnpm:
            if isinstance(workspace, dict):
                collect_npm(workspace, components, dependencies)

    bom = {
        "bomFormat": "CycloneDX",
        "specVersion": "1.6",
        "serialNumber": f"urn:uuid:{uuid.uuid5(uuid.NAMESPACE_URL, 'https://github.com/vixco/Continuum')}",
        "version": 1,
        "metadata": {
            "component": {
                "type": "application",
                "name": "Continuum",
                "bom-ref": "pkg:github/vixco/Continuum",
            },
            "tools": {
                "components": [
                    {
                        "type": "application",
                        "name": "scripts/generate-sbom.py",
                        "version": "1",
                    }
                ]
            },
        },
        "components": [components[key] for key in sorted(components)],
        "dependencies": [
            {"ref": key, "dependsOn": sorted(values)}
            for key, values in sorted(dependencies.items())
        ],
    }
    OUTPUT.parent.mkdir(parents=True, exist_ok=True)
    OUTPUT.write_text(json.dumps(bom, indent=2) + "\n", encoding="utf-8")
    print(f"Wrote {len(components)} components to {OUTPUT}")


if __name__ == "__main__":
    main()

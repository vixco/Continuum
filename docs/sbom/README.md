# Software bill of materials

`continuum.cdx.json` is a CycloneDX 1.6 inventory generated from the locked
Cargo and pnpm dependency graphs:

```powershell
python scripts/generate-sbom.py
```

`javascript-licenses.json` is the matching pnpm license report:

```powershell
pnpm licenses list --json | Out-File -Encoding utf8 docs/sbom/javascript-licenses.json
```

Run it only after `pnpm install --frozen-lockfile`. The report covers package
dependencies; release engineering must still compare native binaries, models,
voices and installer contents against `docs/PROVENANCE.md`.

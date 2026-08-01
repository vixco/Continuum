# Continuum provenance and release licensing

Last audited: 2026-08-01 at commit `edf95d8077c427cf05e55b32acd07c0bcab92827`.

This file records repository evidence and release checks. It is not legal advice and does not guarantee that any use or distribution is compliant. When a source is unclear, this document treats it as unresolved rather than assuming permission.

## Repository lineage

Continuum currently preserves the complete Git history of the Kairo donor repository.

- The current `origin` is `https://github.com/vixco/Continuum.git`.
- The first local commit is `1d144f31fd7b7ddd5c84e6b12f9f57b86a7d4e8e` (2026-04-10), titled `chore: initial repository scaffolding`.
- That first commit already identifies the project as Kairo, sets the Cargo repository to `https://github.com/vixco/kairo-ai`, declares `Apache-2.0`, and adds the root `LICENSE` with `Copyright 2026 Toshan (vixco)`.
- Commit `dc799d738c88d0d4040bdbfb6b42728942d149d3` explicitly records `Merge branch 'main' of https://github.com/vixco/kairo-ai`.
- `git shortlog -sne --all` reports 77 commits as `Toshan` and 2 as `T`; both aliases use the same GitHub noreply address, `93612321+vixco@users.noreply.github.com`. No other author email appears in the preserved history.
- The root Cargo metadata still points to `vixco/kairo-ai`, and current source, docs, package names, prompts, and crate names still contain Kairo identity and design.

These facts establish repository lineage, not ownership. The local evidence does **not** prove:

- who owns every contribution or whether any work was contributed under a separate agreement;
- the terms under which the repository moved from `vixco/kairo-ai` to `vixco/Continuum`;
- the provenance and permitted use of AI-assisted output beyond the commit attribution;
- that the GitHub account identity and the named copyright holder are legally the same person.

Before a public Continuum release, the maintainer should record the donor-to-Continuum transition in a signed or otherwise durable project record. Do not squash away the donor history until that record and all required attributions are preserved elsewhere.

## Apache-2.0 material to preserve

The workspace and all five first-party Rust packages currently declare `Apache-2.0`. The root `LICENSE` has existed since the first commit. No inherited `NOTICE` file exists in Git history. Continuum now adds its own `NOTICE` to preserve the donor attribution without implying that it came from upstream.

For source or binary redistribution of the inherited Kairo work, the project release process must at minimum preserve the conditions stated in section 4 of the checked-in Apache-2.0 text:

1. Ship a copy of the applicable license.
2. Mark files that Continuum modifies with prominent change notices where required.
3. Retain relevant copyright, patent, trademark, and attribution notices from the source form.
4. If a relevant upstream dependency or future donor adds a `NOTICE`, reproduce its applicable notices in a permitted location.
5. Add Continuum attribution alongside inherited attribution; do not replace `Copyright 2026 Toshan (vixco)` without documented authority.

Apache-2.0 does not grant trademark rights. The Kairo name, Continuum name, logos, and third-party product names need a separate naming and trademark review before release.

### Root license integrity repair

The inherited `LICENSE` contained a corrupted appendix sentence: `Please also get an "Alarm or alarm" page or equivalent of your project`. With maintainer authorization during Phase 0, that appendix was restored to the canonical Apache License 2.0 wording published by the Apache Software Foundation. The inherited `Copyright 2026 Toshan (vixco)` attribution was moved to Continuum's new `NOTICE` instead of being discarded. This repair does not resolve ownership questions described above.

## Code dependencies

Lockfiles identify exact dependency versions, but a package's own license and notices remain authoritative. Dependency license conclusions must be regenerated for every release.

### Rust

`Cargo.lock` contains 1,054 package records: 1,049 registry packages and no Git-sourced packages. A locked `cargo metadata` run on the Phase 0 test server is archived in CycloneDX form at `docs/sbom/continuum.cdx.json`; it contains 1,054 Cargo components and their resolved dependency edges and declared license expressions.

The SBOM is package-manager evidence, not automatic legal clearance. It does not inspect generated native code, downloaded models or the final installer contents.

Notable native dependency families requiring release-artifact review include `llama-cpp-2`, `whisper-rs`, `ort`, `tokenizers`, `lancedb`, Tauri, and their native/transitive components. Registry metadata alone is not proof that every compiled artifact carries all required notices.

### JavaScript

The root and app packages are marked `private`; their package manifests do not declare a package license. The applications are still covered by the repository license unless the maintainers document otherwise.

After a clean frozen install on the Phase 0 test server, `pnpm licenses list --json` completed successfully. Its report is archived at `docs/sbom/javascript-licenses.json`; the combined CycloneDX inventory contains 843 npm components. A manifest-level check found 831 unique third-party package/version manifests with the following declared metadata:

| Declared license metadata                                                                    | Count |
| -------------------------------------------------------------------------------------------- | ----: |
| MIT                                                                                          |   674 |
| ISC                                                                                          |    59 |
| Apache-2.0                                                                                   |    55 |
| BSD-2-Clause or BSD-3-Clause                                                                 |    21 |
| MIT/Apache alternatives or combinations                                                      |    10 |
| Other permissive identifiers (`0BSD`, `CC0-1.0`, `Unlicense`, `BlueOak-1.0.0`, `Python-2.0`) |     5 |
| MPL/Apache alternatives                                                                      |     2 |
| CC-BY-4.0                                                                                    |     2 |
| MPL-2.0                                                                                      |     1 |
| Apache-2.0 AND LGPL-3.0-or-later                                                             |     1 |
| Missing license field                                                                        |     1 |

Packages needing explicit artifact and notice review:

- `@img/sharp-win32-x64@0.34.5` declares `Apache-2.0 AND LGPL-3.0-or-later` and is reached through the production `next -> sharp` chain.
- `caniuse-lite` declares `CC-BY-4.0` and is reached through production Next.js and build tooling.
- `axe-core@4.12.1` declares `MPL-2.0` and is reached through desktop development lint tooling.
- `dompurify` declares `(MPL-2.0 OR Apache-2.0)` and is reached by the docs through Nextra/Mermaid; the selected licensing path and notices must be recorded.
- `khroma@2.1.0`, also reached through Nextra/Mermaid, has no license field in its installed `package.json`; verify its source license before shipping the docs bundle.

These reports describe the locked development workspace. A production release still needs an installer-level inventory so dev-only and platform-specific packages can be separated from shipped files.

## Models, voices, binaries, and assets

No file matching the audited common model, executable, library, font, image, audio, or video extensions is tracked at `HEAD`. `scripts/download-models.ps1` downloads external artifacts to the user's data directory. Download-on-install is still distribution behavior that needs source, license, integrity, and notice review.

| Artifact referenced by the downloader                                         | Source recorded in the repository                                                                                                                                                                                        | Upstream license evidence                                                                                                                           | Release status                                                                                                                   |
| ----------------------------------------------------------------------------- | ------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------ | --------------------------------------------------------------------------------------------------------------------------------------------------- | -------------------------------------------------------------------------------------------------------------------------------- |
| SmolVLM-256M-Instruct ONNX files and tokenizer                                | [Hugging Face](https://huggingface.co/HuggingFaceTB/SmolVLM-256M-Instruct)                                                                                                                                               | Model card declares Apache-2.0                                                                                                                      | Record revision, hashes, and license copy before bundling.                                                                       |
| Qwen3 8B and 4B GGUF                                                          | [8B](https://huggingface.co/Qwen/Qwen3-8B-GGUF), [4B](https://huggingface.co/Qwen/Qwen3-4B-GGUF)                                                                                                                         | Both model cards declare Apache-2.0                                                                                                                 | Record exact revisions, hashes, and license copies.                                                                              |
| Whisper medium and small GGML                                                 | [Hugging Face mirror](https://huggingface.co/ggerganov/whisper.cpp)                                                                                                                                                      | Repository metadata declares MIT and identifies converted OpenAI Whisper models                                                                     | Verify the exact model-file notices and hashes used by the release.                                                              |
| Piper `nl_NL-mls-medium` voice                                                | [voice repository](https://huggingface.co/rhasspy/piper-voices/tree/main/nl/nl_NL/mls/medium), trained from [MLS](https://openslr.org/94/)                                                                               | Repository metadata says MIT; the voice card identifies its dataset as CC-BY-4.0                                                                    | Include required attribution and verify whether model-weight distribution adds obligations beyond the repository-level metadata. |
| Piper `en_US-norman-medium` voice                                             | [voice repository](https://huggingface.co/rhasspy/piper-voices/tree/v1.0.0/en/en_US/norman/medium), trained from LibriVox recordings                                                                                      | Repository metadata says MIT; the model card says the recordings are public domain and the model was trained from scratch                            | Replaces the Lessac-derived default. Archive the exact model card and repository license with release attribution.                |
| Archived Piper Windows binary                                                 | [rhasspy/piper release](https://github.com/rhasspy/piper/releases/tag/2023.11.14-2)                                                                                                                                      | Archived repository declares MIT                                                                                                                    | Preserve its license and verify every file in the downloaded archive.                                                            |
| `espeak-ng-data` included with Piper or fetched from the sherpa-onnx fallback | Piper archive or [k2-fsa/sherpa-onnx release](https://github.com/k2-fsa/sherpa-onnx/releases/tag/tts-models)                                                                                                             | eSpeak NG states GPL-3.0-or-later for the synthesizer, with additional licenses for some files; the exact data/archive composition was not verified | **Blocked from bundling until file-level licenses, source obligations, and notices are reviewed.**                               |

The downloader uses mutable Hugging Face `resolve/main` URLs and checks only minimum file size. It does not pin revisions or verify cryptographic hashes. That is both a provenance and supply-chain gap. The voice repository's top-level MIT label must not be treated as overriding dataset or per-voice terms.

Claude Code, optional ElevenLabs access, Hugging Face hosting, and other network services are external services rather than vendored code. Their service/account terms are separate from this repository's Apache-2.0 license and must be reviewed for each supported integration.

## Release checklist

1. **Freeze provenance:** tag the exact Git commit; preserve donor history; record the authorized Kairo-to-Continuum transition and all contributor identities/agreements known to the maintainer.
2. **Validate first-party licensing:** compare `LICENSE` with canonical Apache-2.0 text, confirm the donor notice, mark modified files where required, and include applicable `LICENSE`/`NOTICE` materials in source and installers.
3. **Generate dependency evidence:** regenerate the checked-in CycloneDX and pnpm license reports; add installer-level output; review every missing, copyleft, custom, dual-license, native, and binary result.
4. **Clear external artifacts:** keep Lessac-derived voices excluded; resolve Piper/eSpeak archive obligations; pin every model/voice/binary by immutable revision and SHA-256; archive its exact license and attribution text.
5. **Inspect the built release:** list every shipped file from MSI/NSIS/portable/docs artifacts, match it to the SBOM and notices, scan for untracked assets/secrets, and block publication on any unmatched file or unresolved license.

## Reproducible evidence commands

Run from the repository root:

```powershell
git remote -v
git log --reverse --date=iso-strict --format="%H`t%ad`t%an <%ae>`t%s"
git shortlog -sne --all
git log --all -- NOTICE
git ls-tree -r --name-only HEAD
rg -n "https://" scripts/download-models.ps1
cargo metadata --format-version 1 --locked
pnpm install --frozen-lockfile
pnpm licenses list --json
```

The two final inventory commands must succeed in a clean release environment; the fallback results in this document are not substitutes for that gate.

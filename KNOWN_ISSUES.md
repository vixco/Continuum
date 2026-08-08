# Known issues

Continuum is in alpha. This file lists current product limitations and
operational risks; it is not a roadmap or a place to hide failing tests.

Last updated: 2026-08-09.

## Release and distribution

- **The automatic cross-platform Release pipeline is new.** A release is not
  considered complete until the GitHub Release contains validated Windows x64,
  macOS Apple Silicon, and macOS Intel assets plus updater metadata, manifest,
  and checksums. Tags without those assets are incomplete release state.
- **Windows binaries are not Authenticode-signed.** SmartScreen can warn on
  first launch until an OV/EV signing certificate is configured.
- **macOS packages are not yet Developer ID signed or notarized.** The workflow
  produces DMGs for Apple Silicon and Intel, but Gatekeeper can warn until Apple
  signing, hardened runtime, notarization, and stapling credentials are added.
- **Tauri updater signatures are not publisher certificates.** They protect the
  updater artifact and are verified by installed Continuum clients; they do not
  remove SmartScreen or Gatekeeper warnings.
- **Linux has no supported desktop package.** The Rust workspace may compile on
  Linux in selected configurations, but no Linux installer or full desktop
  support contract exists.

## Platform parity

- **Computer use is Windows-first.** Window observation, UI Automation,
  semantic element targeting, mouse/keyboard input, screenshots, and verified
  foreground focus are implemented for Windows. The macOS desktop can be built
  and distributed, but it does not yet have feature-equivalent native computer
  control.
- **Several ambient/voice integrations remain Windows-specific.** Global
  hotkeys, some foreground-window sensing, and legacy capture paths still rely
  on Win32 APIs.
- **Apple Silicon and Intel are separate packages.** Continuum does not yet ship
  a universal macOS binary.

## Agent OS control plane

- **There is no first-class per-run pause/cancel/emergency-stop MCP tool yet.**
  Runs are bounded, resumable, policy-gated, and protected against concurrent
  duplicate execution, but a dedicated user-visible kill switch is still
  required for high-confidence unattended operation.
- **Execution leases are process-local.** They prevent concurrent tasks inside
  one Agent OS server from replaying a run. A future multi-process execution
  topology will need an OS/file/database-backed distributed lease.
- **A lost transport response remains an unknown outcome.** The bundled skill
  instructs agents to inspect evidence and the destination state before retrying,
  but not every external service offers an idempotency key or definitive read
  after write.
- **Post-action computer verification is structural, not semantic proof.** A
  changed accessibility tree or foreground window is useful evidence, but it
  does not prove that a human-level goal was achieved. Important workflows
  still need explicit observable expectations.

## Connected apps and Composio

- **Composio requires a user-supplied API key and account connections.** The key
  is not accepted through an MCP tool; on Windows it can be stored with DPAPI.
- **Live provider behavior can drift.** Composio tool schemas and response
  envelopes are external contracts. Continuum rejects explicit/malformed
  failures and classifies destructive actions conservatively, but a maintained
  mocked contract suite and a dedicated live test tenant are still needed.
- **Tool-name risk classification is conservative but heuristic.** Unknown
  mutations default to write authority, and money/account-security verbs are
  destructive. Newly introduced vendor terminology may still require an
  explicit regression test.
- **Remote workbench and remote bash surfaces remain deny-by-default.** They are
  intentionally not a fallback for missing schemas or denied app actions.

## Checkpoints, evidence, and privacy

- **Windows resumable run checkpoints use current-user DPAPI.** They cannot be
  moved to another Windows user or machine and expected to decrypt. Legacy
  plaintext checkpoints are migrated when next saved.
- **Unix checkpoints are permission-restricted, not encrypted.** Files are
  owner-only (`0600`) under an owner-only directory (`0700`), but full at-rest
  encryption for macOS/Linux is not implemented.
- **Evidence is minimized but not cryptographically chained.** Sensitive
  Composio arguments, typed text, account identifiers, OAuth URLs, and upstream
  responses are redacted/minimized. The JSONL evidence log is append-only by
  convention, but it is not yet tamper-evident with a hash chain or signature.
- **Approved screenshots are sensitive local artifacts.** They are stored under
  the Agent OS data directory when requested. Automatic retention limits,
  encryption, and a user-facing purge control still need to be added.
- **Goals and expectations can still contain user-supplied sensitive text.** The
  skill explicitly forbids putting secrets there; stronger structured
  sensitivity labels and field-level encryption are future work.

## Memory and world model

- **Memory poisoning is not fully solved.** Provenance, status, confidence, and
  supersession are core design requirements, but every ingestion source still
  needs consistent trust scoring and confirmation rules.
- **Long-term memory is local-user scoped.** There is no household/multi-user
  isolation model beyond the operating-system account boundary.
- **Export, selective deletion, and retention controls are incomplete.** Power
  users can inspect local stores, but the desktop does not yet expose a complete
  privacy lifecycle for every memory/evidence artifact.
- **Embeddings improve retrieval but cannot prove truth or freshness.** Durable
  decisions must continue to use source references, validity windows, and
  confirmation rather than vector similarity alone.

## Desktop and UX

- **Some panels remain hybrid or fixture-backed.** `docs/CURRENT_TABS.md` is the
  source of truth for each tab's live/fixture status. Fixture data must never be
  presented as live state.
- **The permissions experience is still developer-oriented.** Capability names,
  risk levels, evidence IDs, and integration state need a simpler user-facing
  explanation without hiding the underlying authority boundary.
- **Computer-use approvals are native dialogs, not a complete activity center.**
  A persistent queue with diffable scope, timeout, cancellation, and grouped
  approvals is still needed.
- **Accessibility and reduced-motion contracts need continuous end-to-end UI
  testing.** Static UI contract tests exist, but they do not replace screen
  reader and keyboard-only validation on each desktop platform.

## Models, voice, and orchestration

- **Model/provider behavior is not deterministic.** Continuum can enforce tool
  schemas, permissions, budgets, and evidence, but cannot guarantee that every
  provider will produce the same plan or explanation.
- **Local-model quality depends heavily on hardware and quantization.** Smaller
  models can produce weaker routing, planning, and verification decisions.
- **Voice quality and wake behavior vary by language and model.** Dutch and
  multilingual paths still need broader evaluation, especially around false
  wakes and proper nouns.
- **A real verifier must remain independent from the planner's confidence.**
  Current structural verification is a foundation; higher-level outcome
  verification and adversarial evaluation are still required.

## Security boundaries that are not bugs

- MCP alone cannot constrain an agent's native shell or filesystem tools.
  Continuum can claim enforced permissions only when it launches the agent with
  matching sandbox restrictions or routes execution through its own broker.
- Access by another process already executing as the same OS user is outside the
  protection offered by ordinary user-level file permissions and DPAPI use.
- Deliberately relaxing a policy or adding a broad filesystem path expands the
  authority the user granted. The UI should make that clear, but it cannot make
  an intentionally broad grant narrow.

## Reporting

For ordinary bugs, open a GitHub issue with reproduction steps and logs that do
not contain secrets. For permission bypasses, credential exposure, updater
integrity issues, or other security-sensitive findings, follow
[SECURITY.md](./SECURITY.md) and report privately.

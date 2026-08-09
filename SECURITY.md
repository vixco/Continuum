# Security Policy

Continuum is a local AI execution and context layer. Security reports can involve
computer input, connected accounts, updater integrity, local memory, credentials,
or permission enforcement. Please handle suspected vulnerabilities privately.

## Supported versions

Continuum is currently pre-release. Security fixes are made against the latest
commit on `main` and shipped in the next signed updater release. Older alpha
releases are not maintained as separate security branches.

| Version | Supported |
| --- | --- |
| Latest release | Yes |
| `main` | Yes |
| Older alpha releases | No |

## Reporting a vulnerability

Do **not** open a public issue for a vulnerability that could put users,
credentials, connected services, or local files at risk.

Use one of these private channels:

1. **GitHub private vulnerability reporting / Security Advisory** (preferred):
   open a private draft under the repository's Security tab.
2. **Email:** `t.soekar@tovix.nl`, with subject
   `[continuum-security] <short description>`.

Include, where possible:

- affected commit or release;
- operating system and architecture;
- exact capability, MCP tool, provider, or integration involved;
- minimal reproduction steps;
- observed and expected permission decisions;
- whether user interaction or an approval dialog was required;
- potential impact and data exposed;
- logs with secrets removed;
- a suggested fix or regression test, when available.

Do not include real API keys, OAuth links, cookies, private messages, passwords,
or customer data. Use test accounts and synthetic payloads.

## High-priority vulnerability classes

Please report these privately even when impact is uncertain:

- bypassing an `allow` / `ask` / `deny` capability decision;
- executing a denied action through another tool or plan step;
- approval-dialog spoofing or approval reuse outside its approved scope;
- duplicate side effects after a retry, timeout, crash, or concurrent run;
- arbitrary command, script, URL, path, or process execution;
- unsafe computer input, window targeting, or accessibility-tree confusion;
- credential, token, OAuth URL, clipboard, typed-text, screenshot, or memory
  disclosure;
- Composio tool misclassification that lowers a destructive action to read or
  write authority;
- tampering with resumable run checkpoints or evidence;
- updater signature bypass, wrong-platform updates, incomplete Releases, or
  replacement of published binaries;
- untrusted project content escaping a sandbox or changing durable policy;
- remote content causing silent scope expansion or cross-account actions.

## Security boundaries

The following boundaries are intentional and should remain explicit:

- Audit logging after execution is not permission enforcement.
- MCP cannot constrain an agent's native shell or filesystem tools unless the
  agent is launched with matching sandbox restrictions.
- A transport success is not proof that an external side effect succeeded.
- A timeout or lost response is an unknown outcome, not permission to retry.
- Destructive connected-app actions default to deny.
- Relaxing a persistent policy requires independent native user approval.
- Sensitive tool arguments must not be copied into goals, expectations, logs,
  evidence, or error messages.
- Windows resumable run checkpoints are protected with current-user DPAPI;
  Unix checkpoint files are restricted to the owner.
- Tauri updater signatures protect update artifacts, but do not replace Windows
  Authenticode or Apple Developer ID signing/notarization.

## Response targets

| Stage | Target |
| --- | --- |
| Acknowledge a report | 3 business days |
| Initial severity assessment | 7 days |
| Critical/high mitigation or fix | 30 days |
| Medium/low mitigation or fix | 90 days |

These are targets, not guarantees. Complex upstream or certificate-related
issues may require coordination with another maintainer or vendor.

## Local security testing

Security changes should include a regression test whenever practical. Relevant
gates include:

```text
cargo fmt --all -- --check
cargo clippy --workspace --all-targets -- -D warnings
cargo test --workspace
python -m unittest -v scripts/test_release_contract.py
```

Use synthetic secrets and ensure the test fails when the protected value appears
in raw checkpoint bytes, evidence, logs, release metadata, or error output.

## Disclosure and credit

We aim to acknowledge valid reports and coordinate disclosure after a fix is
available. Public disclosure timing should be agreed through the private
advisory. Reporters who want credit should include the preferred name and link.

## Out of scope

The following are generally not vulnerabilities by themselves:

- expected Windows SmartScreen or macOS Gatekeeper warnings while publisher
  signing/notarization credentials are not configured;
- actions the user explicitly approved with accurate scope and risk information;
- access by another process already running with the same user privileges to
  data that the operating system intentionally exposes to that user;
- unsupported older alpha builds when the issue is fixed in the latest release;
- social engineering without a product or permission-boundary bypass;
- vulnerabilities that exist entirely in an upstream project and cannot be
  mitigated in Continuum. Report them upstream as well, but contact us privately
  when Continuum users need a coordinated dependency update.

When in doubt, report privately. A cautious report is preferable to publishing a
potential permission or credential issue before it can be assessed.

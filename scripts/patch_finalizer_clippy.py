from pathlib import Path

path = Path("scripts/finalize_swarm_integration.sh")
text = path.read_text(encoding="utf-8")
marker = """PY_INTEGRATION_FIXES

cargo fmt --all
git add \\
  crates/continuum-core/src/context/project.rs \\
  crates/continuum-core/src/senses/process_watch.rs
git commit -m \"fix(integration): close cross-platform runtime state gaps\"
"""
if text.count(marker) != 1:
    raise SystemExit(f"expected one integration-fix commit marker, found {text.count(marker)}")

replacement = r'''PY_INTEGRATION_FIXES

# Satisfy the pinned toolchain's strict Clippy gate without suppressing any
# warning or weakening any assertion.
python - <<'PY_CLIPPY_FIXES'
from pathlib import Path

path = Path("crates/continuum-core/src/context/temporal.rs")
text = path.read_text(encoding="utf-8")
replacements = {
    ".since.map_or(true, |since| row.ended_at >= since)":
        ".since.is_none_or(|since| row.ended_at >= since)",
    ".until.map_or(true, |until| row.started_at <= until)":
        ".until.is_none_or(|until| row.started_at <= until)",
    ".map_or(true, |project| row.project.as_deref() == Some(project));":
        ".is_none_or(|project| row.project.as_deref() == Some(project));",
}
for old, new in replacements.items():
    count = text.count(old)
    if count != 1:
        raise SystemExit(f"expected one temporal Clippy match for {old!r}, found {count}")
    text = text.replace(old, new, 1)
path.write_text(text, encoding="utf-8")

path = Path("crates/continuum-core/src/health/verified.rs")
text = path.read_text(encoding="utf-8")
old = ".unwrap_or_else(ComponentDiagnostic::default)"
if text.count(old) != 1:
    raise SystemExit(f"expected one verified-probe default match, found {text.count(old)}")
path.write_text(text.replace(old, ".unwrap_or_default()", 1), encoding="utf-8")

path = Path("crates/continuum-core/src/senses/process_watch.rs")
text = path.read_text(encoding="utf-8")
old = '''        let mut health = ProcessWatchHealth::default();
        health.enabled = true;
        health.state = OperationalState::Running;
        health.activated_at = Some(Utc::now() - chrono::Duration::minutes(2));'''
new = '''        let health = ProcessWatchHealth {
            enabled: true,
            state: OperationalState::Running,
            activated_at: Some(Utc::now() - chrono::Duration::minutes(2)),
            ..ProcessWatchHealth::default()
        };'''
if text.count(old) != 1:
    raise SystemExit(f"expected one stale-health initializer match, found {text.count(old)}")
path.write_text(text.replace(old, new, 1), encoding="utf-8")
PY_CLIPPY_FIXES

cargo fmt --all
git add \
  crates/continuum-core/src/context/project.rs \
  crates/continuum-core/src/context/temporal.rs \
  crates/continuum-core/src/health/verified.rs \
  crates/continuum-core/src/senses/process_watch.rs
git commit -m "fix(integration): close cross-platform runtime state gaps"
'''

path.write_text(text.replace(marker, replacement, 1), encoding="utf-8")

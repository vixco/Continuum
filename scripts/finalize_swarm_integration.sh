#!/usr/bin/env bash
set -euo pipefail

EXPECTED_HEAD="fc68a3e4625227d59e08813b30e067532951fb6a"
A8_COMMIT="ab81a0b81a94f193bf45b803c16edc4e87fe8193"
A5_COMMIT="6d73e86ff0c5bc5678679c72c085a868c71e488b"

git config user.name "Continuum integration bot"
git config user.email "93612321+vixco@users.noreply.github.com"

actual="$(git rev-parse HEAD)"
if [[ "$actual" != "$EXPECTED_HEAD" ]]; then
  echo "Integration branch moved: expected $EXPECTED_HEAD, found $actual" >&2
  exit 1
fi

# A8 was authored directly on the current integration head. Apply its durable
# provenance/privacy/chat hardening first, while preserving the stronger pre-A8
# scroll-race regression coverage.
cp apps/desktop/tests/chat-scroll-behavior.test.mjs /tmp/chat-scroll-before-a8.mjs
git fetch origin swarm/a8-evaluation-and-ci
git cherry-pick "$A8_COMMIT"
python - <<'PY'
from pathlib import Path

path = Path("apps/desktop/tests/chat-scroll-behavior.test.mjs")
text = Path("/tmp/chat-scroll-before-a8.mjs").read_text(encoding="utf-8")
marker = 'test("scrolling back near the bottom resumes following and clears unseen output", () => {'
addition = '''test("a small upward scroll inside the bottom threshold preserves reader intent", () => {
  const initial = createChatScrollSnapshot(600);
  const scrolledUp = observeChatScroll(initial, {
    scrollTop: 580,
    scrollHeight: 1200,
    clientHeight: 600,
  });

  assert.equal(scrolledUp.atBottom, true, "the viewport remains physically near the bottom");
  assert.equal(scrolledUp.pinnedToBottom, false, "upward intent must win over the threshold");

  const afterPositiveSignal = observeAtBottomSignal(scrolledUp, true);
  assert.equal(
    afterPositiveSignal.pinnedToBottom,
    false,
    "a stale positive Virtuoso signal must not re-enable following"
  );

  const withOutput = observeChatContent(afterPositiveSignal);
  assert.equal(shouldFollowChatOutput(withOutput), false);
  assert.equal(withOutput.hasUnseenContent, true);
});

'''
if addition.strip() not in text:
    if marker not in text:
        raise SystemExit("chat test insertion marker missing")
    text = text.replace(marker, addition + marker, 1)
path.write_text(text, encoding="utf-8")
PY
cargo fmt --all
git add -A
git commit --amend --no-edit

# A5's product commit is selected, but its temporary export workflow is not.
# Resolve the sole expected conflict in favor of the newer shared settings
# backend already present in the integration branch.
git fetch origin swarm/a5-health-and-watchers
set +e
git cherry-pick "$A5_COMMIT"
status=$?
set -e
if [[ "$status" -ne 0 ]]; then
  mapfile -t conflicts < <(git diff --name-only --diff-filter=U)
  if [[ "${#conflicts[@]}" -ne 1 || "${conflicts[0]}" != "apps/desktop/src-tauri/src/settings_tools.rs" ]]; then
    printf 'Unexpected A5 conflicts:\n%s\n' "${conflicts[*]:-none}" >&2
    exit 1
  fi
  git checkout --ours -- apps/desktop/src-tauri/src/settings_tools.rs
  git add apps/desktop/src-tauri/src/settings_tools.rs
  GIT_EDITOR=true git cherry-pick --continue
fi

python - <<'PY'
from pathlib import Path

# Add the new services projection to the sole explicit ContextPageSnapshot test
# initializer that predates A5's additive contract.
path = Path("crates/continuum-core/src/runtime_publish.rs")
lines = path.read_text(encoding="utf-8").splitlines()
candidates = []
for index, line in enumerate(lines):
    if line.strip() != "continuation: vec![ContinuationCandidateView {":
        continue
    nearby = lines[max(0, index - 8):index]
    if any("files: false" in item for item in nearby):
        candidates.append(index)
if len(candidates) != 1:
    raise SystemExit(f"expected one ContextPageSnapshot insertion point, found {len(candidates)}")
index = candidates[0]
if index == 0 or "services:" not in lines[index - 1]:
    indent = lines[index][: len(lines[index]) - len(lines[index].lstrip())]
    lines.insert(index, f"{indent}services: RuntimeServiceSnapshot::default(),")
path.write_text("\n".join(lines) + "\n", encoding="utf-8")

# Return directly from the verified-repair retry loop, eliminating overwritten
# initial assignments while preserving bounded retry semantics.
path = Path("crates/continuum-core/src/health/verified.rs")
text = path.read_text(encoding="utf-8")
start_marker = "    let mut backoff = RetryBackoff::new(plan.retry);"
end_marker = "\n}\n\n/// Converts a legacy health status"
start = text.index(start_marker)
end = text.index(end_marker, start)
replacement = "\n".join([
    "    let mut backoff = RetryBackoff::new(plan.retry);",
    "    let mut attempts = 0;",
    "",
    "    loop {",
    "        attempts += 1;",
    "        let execution = executor.execute(plan).await;",
    "        let retryable = execution.retryable;",
    "        let after = probe.inspect().await;",
    "",
    "        let command_ok = execution.exited_successfully;",
    "        let state_ok = plan.success_states.contains(&after.state);",
    "        if command_ok && state_ok {",
    "            return VerifiedRepairResult {",
    "                component: plan.component.clone(),",
    "                action: plan.action.clone(),",
    "                outcome: VerifiedRepairOutcome::VerifiedSuccess,",
    "                before,",
    "                after,",
    "                execution: Some(execution),",
    "                attempts,",
    "                started_at,",
    "                verified_at: Utc::now(),",
    "                explanation: \"The action completed and a fresh health probe verified recovery.\"",
    "                    .to_string(),",
    "            };",
    "        }",
    "",
    "        let delay = if retryable {",
    "            backoff.next_delay()",
    "        } else {",
    "            None",
    "        };",
    "        if let Some(delay) = delay {",
    "            tokio::time::sleep(delay).await;",
    "            continue;",
    "        }",
    "",
    "        return VerifiedRepairResult {",
    "            component: plan.component.clone(),",
    "            action: plan.action.clone(),",
    "            outcome: VerifiedRepairOutcome::VerifiedFailure,",
    "            before,",
    "            after,",
    "            execution: Some(execution),",
    "            attempts,",
    "            started_at,",
    "            verified_at: Utc::now(),",
    "            explanation: \"The action did not produce an accepted post-repair health state.\"",
    "                .to_string(),",
    "        };",
    "    }",
    "",
])
path.write_text(text[:start] + replacement + text[end:], encoding="utf-8")

# Keep the small header discriminator, but remove the duplicate unused field
# from the full repair-intent envelope.
path = Path("crates/continuum-core/src/health/runtime_repair.rs")
lines = path.read_text(encoding="utf-8").splitlines()
inside_envelope = False
removed_kind = 0
for index, line in enumerate(lines):
    if line.strip() == "struct RepairIntentEnvelope {":
        inside_envelope = True
        continue
    if inside_envelope and line.strip() == "}":
        inside_envelope = False
    if inside_envelope and line.strip() == "kind: String,":
        lines[index] = None
        removed_kind += 1
if removed_kind != 1:
    raise SystemExit(f"expected one envelope kind field, removed {removed_kind}")
lines = [line for line in lines if line is not None]

# Collapse the nested deadline guard without changing its fail-closed behavior.
start_index = None
for index, line in enumerate(lines):
    if line.strip().startswith("if !activated || (!accepted_state"):
        start_index = index
        break
if start_index is None:
    raise SystemExit("verification deadline guard not found")
indent = lines[start_index][: len(lines[start_index]) - len(lines[start_index].lstrip())]
actual_tail = [item.strip() for item in lines[start_index + 1:start_index + 5]]
expected_tail = ["if now < pending.deadline {", "continue;", "}", "}"]
if actual_tail != expected_tail:
    raise SystemExit(f"unexpected verification deadline guard: {actual_tail}")
lines[start_index:start_index + 5] = [
    f"{indent}if now < pending.deadline",
    f"{indent}    && (!activated || (!accepted_state && !terminal_failure))",
    f"{indent}{{",
    f"{indent}    continue;",
    f"{indent}}}",
]
path.write_text("\n".join(lines) + "\n", encoding="utf-8")

# This internal constructor deliberately receives the complete event record.
path = Path("crates/continuum-core/src/operational_state.rs")
lines = path.read_text(encoding="utf-8").splitlines()
matches = [index for index, line in enumerate(lines) if line == "fn push_event("]
if len(matches) != 1:
    raise SystemExit(f"expected one push_event helper, found {len(matches)}")
index = matches[0]
if index == 0 or lines[index - 1] != "#[allow(clippy::too_many_arguments)]":
    lines.insert(index, "#[allow(clippy::too_many_arguments)]")
path.write_text("\n".join(lines) + "\n", encoding="utf-8")
PY

cargo fmt --all
git add -A
git commit --amend --no-edit

# Fix two integration defects exposed only by the complete cross-platform suite:
# project discovery rejected POSIX absolute paths, and the process watcher left
# its public state enabled after a clean runtime shutdown.
python - <<'PY_INTEGRATION_FIXES'
from pathlib import Path

project_path = Path("crates/continuum-core/src/context/project.rs")
lines = project_path.read_text(encoding="utf-8").splitlines()
function_index = next(
    (index for index, line in enumerate(lines) if line == "fn extract_title_paths(title: &str) -> Vec<PathBuf> {"),
    None,
)
if function_index is None:
    raise SystemExit("extract_title_paths function not found")
doc_index = function_index
while doc_index > 0 and lines[doc_index - 1].startswith("///"):
    doc_index -= 1
brace_depth = 0
function_end = None
for index in range(function_index, len(lines)):
    brace_depth += lines[index].count("{")
    brace_depth -= lines[index].count("}")
    if brace_depth == 0:
        function_end = index + 1
        break
if function_end is None:
    raise SystemExit("extract_title_paths function end not found")
replacement = r'''/// Extracts absolute paths from a window title. Windows drive-letter paths are
/// accepted from anywhere inside an editor segment; POSIX absolute paths are
/// accepted when the trimmed segment itself starts with `/`. Relative paths,
/// URLs, UNC paths and `~`-scrubbed paths are deliberately not candidates.
fn extract_title_paths(title: &str) -> Vec<PathBuf> {
    let mut paths = Vec::new();
    for segment in editor_title_segments(title) {
        let bytes = segment.as_bytes();
        let windows_start = (0..bytes.len().saturating_sub(2)).find(|&i| {
            bytes[i].is_ascii_alphabetic()
                && bytes[i + 1] == b':'
                && (bytes[i + 2] == b'\\' || bytes[i + 2] == b'/')
                && (i == 0 || !bytes[i - 1].is_ascii_alphanumeric())
        });
        let raw = if let Some(start) = windows_start {
            Some(segment[start..].trim_end_matches(['"', '\'', ')', ']', '.', ',']))
        } else if segment.starts_with('/') && !segment.starts_with("//") {
            Some(segment.trim_end_matches(['"', '\'', ')', ']', '.', ',']))
        } else {
            None
        };
        if let Some(raw) = raw {
            if raw.len() > 1 {
                paths.push(PathBuf::from(raw));
            }
        }
    }
    paths
}'''.splitlines()
lines[doc_index:function_end] = replacement

test_marker = "    // --- Resolution matrix ---"
marker_index = next((index for index, line in enumerate(lines) if line == test_marker), None)
if marker_index is None:
    raise SystemExit("project test insertion marker not found")
test_block = r'''    #[test]
    fn title_path_extraction_is_cross_platform_and_rejects_untrusted_shapes() {
        assert_eq!(
            extract_title_paths("notes.md - /tmp/continuum/project - Code"),
            vec![PathBuf::from("/tmp/continuum/project")]
        );
        assert_eq!(
            extract_title_paths("notes.md - D:\\Dev\\Continuum - Code"),
            vec![PathBuf::from("D:\\Dev\\Continuum")]
        );
        assert!(extract_title_paths("https://example.invalid/project").is_empty());
        assert!(extract_title_paths("notes.md - relative/project - Code").is_empty());
        assert!(extract_title_paths("notes.md - //server/share - Code").is_empty());
    }
'''.splitlines()
if not any("title_path_extraction_is_cross_platform" in line for line in lines):
    lines[marker_index:marker_index] = test_block + [""]
project_path.write_text("\n".join(lines) + "\n", encoding="utf-8")

process_path = Path("crates/continuum-core/src/senses/process_watch.rs")
process_text = process_path.read_text(encoding="utf-8")
old_shutdown = '''        let mut health = self.health.write();
        health.current_instances = 0;
        health.activated_at = None;
    }
}'''
new_shutdown = '''        self.set_state(
            OperationalState::DisabledByPolicy,
            "runtime_shutdown",
            "Background activity observation stopped because the runtime is shutting down.",
            Some("runtime shutdown"),
        );
    }
}'''
if process_text.count(old_shutdown) != 1:
    raise SystemExit(f"expected one process watcher shutdown block, found {process_text.count(old_shutdown)}")
process_text = process_text.replace(old_shutdown, new_shutdown, 1)
old_assertion = '''        assert!(snapshot.active.is_empty());
        assert!(!health.read().enabled);
        assert_eq!(health.read().polls, 0);'''
new_assertion = '''        assert!(snapshot.active.is_empty());
        let health = health.read();
        assert!(!health.enabled);
        assert_eq!(health.state, OperationalState::DisabledByPolicy);
        assert_eq!(health.reason_code, "runtime_shutdown");
        assert_eq!(health.polls, 0);'''
if process_text.count(old_assertion) != 1:
    raise SystemExit(f"expected one master-pause assertion block, found {process_text.count(old_assertion)}")
process_path.write_text(process_text.replace(old_assertion, new_assertion, 1), encoding="utf-8")
PY_INTEGRATION_FIXES

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

for path in \
  .a8-minimal-temporal-fix.py \
  .a8-minimal-temporal-test.log \
  .github/workflows/a8-minimal-temporal-fix.yml \
  .github/workflows/a5-export-integration-bundle.yml; do
  if [[ -e "$path" ]]; then
    echo "Temporary swarm artifact would ship: $path" >&2
    exit 1
  fi
done

pnpm install --frozen-lockfile
cargo fmt --all -- --check
cargo test -p continuum-core --no-default-features --lib
cargo clippy -p continuum-core --no-default-features --lib --tests -- -D warnings
cargo test -p continuum-mcp --lib
python -m unittest -v scripts/test_persistent_intelligence_eval.py
python scripts/persistent_intelligence_eval.py \
  --suite evals/persistent-intelligence/reference-suite.json \
  --report /tmp/persistent-intelligence-report.json
pnpm test:desktop
pnpm typecheck
pnpm lint
pnpm build

test -z "$(git status --porcelain)"
git push origin HEAD:swarm/agi-integration

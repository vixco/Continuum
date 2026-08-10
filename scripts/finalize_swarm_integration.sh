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

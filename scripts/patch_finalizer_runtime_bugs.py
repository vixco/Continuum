from pathlib import Path

path = Path("scripts/finalize_swarm_integration.sh")
text = path.read_text(encoding="utf-8")
needle = """cargo fmt --all
git add -A
git commit --amend --no-edit

for path in \\
"""
if text.count(needle) != 1:
    raise SystemExit(f"expected one A5 finalization marker, found {text.count(needle)}")

addition = r'''cargo fmt --all
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

cargo fmt --all
git add \
  crates/continuum-core/src/context/project.rs \
  crates/continuum-core/src/senses/process_watch.rs
git commit -m "fix(integration): close cross-platform runtime state gaps"

for path in \
'''

path.write_text(text.replace(needle, addition, 1), encoding="utf-8")

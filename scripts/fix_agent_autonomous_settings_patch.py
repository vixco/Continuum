#!/usr/bin/env python3
from pathlib import Path

path = Path("scripts/agent_autonomous_settings_patch.py")
text = path.read_text(encoding="utf-8")
old = '''        .map(|(path, value)| {
            let sensitive = is_sensitive_path(&path);
            let default = default_paths.get(&path).cloned().unwrap_or(Value::Null);
            json!({
                "path": path,
                "value": redact_value(&path, value),
                "default": redact_value(&path, default),
                "value_type": value_type(&value),
                "sensitive": sensitive,
                "mutable": true,
                "ui_location": ui_location(&path),
                "restart_recommended": true,
            })
        })'''
new = '''        .map(|(path, value)| {
            let sensitive = is_sensitive_path(&path);
            let default = default_paths.get(&path).cloned().unwrap_or(Value::Null);
            let kind = value_type(&value);
            let current = redact_value(&path, value);
            let default = redact_value(&path, default);
            let ui_location = ui_location(&path);
            json!({
                "path": path,
                "value": current,
                "default": default,
                "value_type": kind,
                "sensitive": sensitive,
                "mutable": true,
                "ui_location": ui_location,
                "restart_recommended": true,
            })
        })'''
count = text.count(old)
if count != 1:
    raise SystemExit(f"expected exactly one settings-list closure, found {count}")
path.write_text(text.replace(old, new, 1), encoding="utf-8")
print("Generator preflight fix applied.")

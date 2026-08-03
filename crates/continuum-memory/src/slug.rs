//! Slug generation: lowercase ASCII alnum + '-', stable after creation.

/// Turn a title into a filesystem slug. Falls back to "note" when nothing
/// survives filtering.
pub fn slugify(title: &str) -> String {
    let mut out = String::with_capacity(title.len());
    let mut last_dash = true; // suppress leading dash
    for c in title.chars() {
        if c.is_ascii_alphanumeric() {
            out.push(c.to_ascii_lowercase());
            last_dash = false;
        } else if !last_dash {
            out.push('-');
            last_dash = true;
        }
    }
    let trimmed = out.trim_matches('-').to_string();
    if trimmed.is_empty() {
        "note".to_string()
    } else {
        trimmed
    }
}

/// Append `-2`, `-3`, … until `exists` returns false.
pub fn unique_slug(base: &str, exists: impl Fn(&str) -> bool) -> String {
    if !exists(base) {
        return base.to_string();
    }
    for n in 2u32.. {
        let candidate = format!("{base}-{n}");
        if !exists(&candidate) {
            return candidate;
        }
    }
    unreachable!("u32 exhausted")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn slugify_basics() {
        assert_eq!(
            slugify("Lobby creation MUST be manual!"),
            "lobby-creation-must-be-manual"
        );
        assert_eq!(slugify("Café déjà-vu ×3"), "caf-d-j-vu-3");
        assert_eq!(slugify("!!!"), "note");
    }

    #[test]
    fn unique_slug_appends_counter() {
        let existing = ["a", "a-2"];
        let s = unique_slug("a", |c| existing.contains(&c));
        assert_eq!(s, "a-3");
        assert_eq!(unique_slug("fresh", |_| false), "fresh");
    }
}

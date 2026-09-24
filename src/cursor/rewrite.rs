//! Boundary-safe path, URI, and hash replacement.

#[derive(Debug, Clone)]
pub struct Replacement {
    pub from: String,
    pub to: String,
}

impl Replacement {
    pub fn new(from: impl Into<String>, to: impl Into<String>) -> Self {
        Self {
            from: from.into(),
            to: to.into(),
        }
    }

    pub fn is_active(&self) -> bool {
        !self.from.is_empty() && self.from != self.to
    }
}

pub fn replace_scoped(value: &str, replacements: &[Replacement]) -> String {
    let active: Vec<&Replacement> = replacements
        .iter()
        .filter(|item| item.is_active())
        .collect();
    if active.is_empty() {
        return value.to_string();
    }

    let mut staged = value.to_string();
    let mut tokens = Vec::new();
    let mut seed = 0usize;
    for replacement in &active {
        let mut token = format!("__CREPATH_TOKEN_{seed}__");
        while staged.contains(&token) || tokens.iter().any(|(existing, _)| existing == &token) {
            seed += 1;
            token = format!("__CREPATH_TOKEN_{seed}__");
        }
        staged = replace_matches(&staged, &replacement.from, &token);
        tokens.push((token, replacement.from.as_str()));
        seed += 1;
    }

    let mut normalized = staged;
    for (token, from) in tokens {
        if let Some(replacement) = active.iter().find(|item| item.from == from) {
            normalized = normalized.replace(&token, &replacement.to);
        }
    }
    normalized
}

pub fn contains_scoped(value: &str, pattern: &str) -> bool {
    if pattern.is_empty() {
        return false;
    }
    let mut offset = 0usize;
    while let Some(pos) = value[offset..].find(pattern) {
        let absolute = offset + pos;
        let suffix = value[absolute + pattern.len()..].chars().next();
        if is_terminator(suffix) {
            return true;
        }
        offset = absolute + 1;
    }
    false
}

fn replace_matches(value: &str, pattern: &str, replacement: &str) -> String {
    if pattern.is_empty() {
        return value.to_string();
    }
    let mut offset = 0usize;
    let mut out = String::with_capacity(value.len());
    while let Some(pos) = value[offset..].find(pattern) {
        let absolute = offset + pos;
        out.push_str(&value[offset..absolute]);
        let next = absolute + pattern.len();
        let suffix = value[next..].chars().next();
        if is_terminator(suffix) {
            out.push_str(replacement);
        } else {
            out.push_str(pattern);
        }
        offset = next;
    }
    out.push_str(&value[offset..]);
    out
}

fn is_terminator(suffix: Option<char>) -> bool {
    match suffix {
        None => true,
        Some(ch) => !ch.is_ascii_alphanumeric() && ch != '_' && ch != '-' && ch != '.' && ch != '%',
    }
}

pub fn key_range_end(prefix: &str) -> String {
    let mut bytes = prefix.as_bytes().to_vec();
    if let Some(last) = bytes.last_mut() {
        *last = last.saturating_add(1);
    }
    String::from_utf8(bytes).unwrap_or_else(|_| format!("{prefix};"))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn sibling_prefix_is_left_alone() {
        let replacements = [Replacement::new(
            "/home/user/project",
            "/home/user/project-copy",
        )];
        let value = r#"{"a":"/home/user/project","b":"/home/user/projects/foo"}"#;
        let updated = replace_scoped(value, &replacements);
        assert!(updated.contains("/home/user/project-copy"));
        assert!(updated.contains("/home/user/projects/foo"));
        assert!(!updated.contains("project-copy-copy"));
    }
}

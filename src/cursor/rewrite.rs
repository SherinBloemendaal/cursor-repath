//! Boundary-safe, single-pass replacement of paths, URIs, hashes, and chat ids.

use aho_corasick::{AhoCorasick, MatchKind};
use std::borrow::Cow;
use std::collections::HashSet;

use super::uri::{Platform, file_uri};

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

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Boundary {
    Path,
    Token,
}

#[derive(Debug, Clone)]
pub struct Rewriter {
    matcher: Option<AhoCorasick>,
    targets: Vec<String>,
    boundaries: Vec<Boundary>,
}

impl Default for Rewriter {
    fn default() -> Self {
        Self::build(Vec::new())
    }
}

impl Rewriter {
    pub fn paths(replacements: &[Replacement]) -> Self {
        Self::build(
            replacements
                .iter()
                .map(|item| (item.clone(), Boundary::Path))
                .collect(),
        )
    }

    pub fn tokens(replacements: &[Replacement]) -> Self {
        Self::build(
            replacements
                .iter()
                .map(|item| (item.clone(), Boundary::Token))
                .collect(),
        )
    }

    pub fn build(entries: Vec<(Replacement, Boundary)>) -> Self {
        let mut seen = HashSet::new();
        let mut patterns = Vec::new();
        let mut targets = Vec::new();
        let mut boundaries = Vec::new();
        for (item, boundary) in entries {
            if !item.is_active() || !seen.insert(item.from.clone()) {
                continue;
            }
            patterns.push(item.from);
            targets.push(item.to);
            boundaries.push(boundary);
        }
        let matcher = if patterns.is_empty() {
            None
        } else {
            Some(
                AhoCorasick::builder()
                    .match_kind(MatchKind::Standard)
                    .build(&patterns)
                    .expect("literal patterns always build"),
            )
        };
        Self {
            matcher,
            targets,
            boundaries,
        }
    }

    pub fn is_empty(&self) -> bool {
        self.matcher.is_none()
    }

    fn accepts(&self, value: &str, start: usize, end: usize, pattern: usize) -> bool {
        let next = value[end..].chars().next();
        match self.boundaries[pattern] {
            Boundary::Path => is_path_terminator(next),
            Boundary::Token => {
                let prev = value[..start].chars().next_back();
                !is_token_char(prev) && !is_token_char(next)
            }
        }
    }

    fn selected(&self, value: &str) -> Vec<(usize, usize, usize)> {
        let Some(matcher) = &self.matcher else {
            return Vec::new();
        };
        let mut found: Vec<(usize, usize, usize)> = matcher
            .find_overlapping_iter(value)
            .map(|hit| (hit.start(), hit.end(), hit.pattern().as_usize()))
            .collect();
        found.sort_by(|a, b| a.0.cmp(&b.0).then(b.1.cmp(&a.1)));
        let mut picked = Vec::new();
        let mut cursor = 0usize;
        for (start, end, pattern) in found {
            if start < cursor || !self.accepts(value, start, end, pattern) {
                continue;
            }
            picked.push((start, end, pattern));
            cursor = end;
        }
        picked
    }

    pub fn rewrite<'a>(&self, value: &'a str) -> Cow<'a, str> {
        let picked = self.selected(value);
        if picked.is_empty() {
            return Cow::Borrowed(value);
        }
        let mut out = String::with_capacity(value.len());
        let mut offset = 0usize;
        for (start, end, pattern) in picked {
            out.push_str(&value[offset..start]);
            out.push_str(&self.targets[pattern]);
            offset = end;
        }
        out.push_str(&value[offset..]);
        Cow::Owned(out)
    }

    pub fn contains(&self, value: &str) -> bool {
        !self.selected(value).is_empty()
    }

    pub fn hits(&self, value: &str) -> Vec<&str> {
        self.selected(value)
            .into_iter()
            .map(|(_, _, pattern)| self.targets[pattern].as_str())
            .collect()
    }
}

const OLD_MARK: &str = "\u{1}old";
const NEW_MARK: &str = "\u{1}new";

/// Finds `from` forms that survived a rewrite. A `to` form at the same position wins, so an
/// old path that is a prefix of the new one is not reported.
#[derive(Debug, Clone)]
pub struct Leftovers {
    rewriter: Rewriter,
}

impl Leftovers {
    pub fn new(entries: &[(Replacement, Boundary)]) -> Self {
        let mut marks = Vec::new();
        for (item, boundary) in entries.iter().filter(|(item, _)| item.is_active()) {
            marks.push((Replacement::new(&item.to, NEW_MARK), *boundary));
        }
        for (item, boundary) in entries.iter().filter(|(item, _)| item.is_active()) {
            marks.push((Replacement::new(&item.from, OLD_MARK), *boundary));
        }
        Self {
            rewriter: Rewriter::build(marks),
        }
    }

    pub fn found(&self, value: &str) -> bool {
        self.rewriter.hits(value).contains(&OLD_MARK)
    }
}

pub fn replace_scoped(value: &str, replacements: &[Replacement]) -> String {
    Rewriter::paths(replacements).rewrite(value).into_owned()
}

pub fn contains_scoped(value: &str, pattern: &str) -> bool {
    Rewriter::paths(&[Replacement::new(pattern, "\u{0}")]).contains(value)
}

fn is_path_terminator(suffix: Option<char>) -> bool {
    match suffix {
        None => true,
        Some(ch) => !ch.is_ascii_alphanumeric() && ch != '_' && ch != '-' && ch != '.' && ch != '%',
    }
}

fn is_token_char(ch: Option<char>) -> bool {
    ch.is_some_and(|ch| ch.is_ascii_alphanumeric() || ch == '_' || ch == '-')
}

fn json_escaped(value: &str) -> String {
    let quoted = serde_json::to_string(value).unwrap_or_else(|_| format!("\"{value}\""));
    quoted[1..quoted.len() - 1].to_string()
}

fn with_drive_case(path: &str, upper: bool) -> Option<String> {
    let bytes = path.as_bytes();
    if bytes.len() >= 2 && bytes[0].is_ascii_alphabetic() && bytes[1] == b':' {
        let drive = if upper {
            path[..1].to_ascii_uppercase()
        } else {
            path[..1].to_ascii_lowercase()
        };
        Some(format!("{drive}{}", &path[1..]))
    } else {
        None
    }
}

fn legacy_file_url(path: &str) -> Option<String> {
    url::Url::from_file_path(path)
        .ok()
        .map(|url| url.to_string())
}

/// Every textual form Cursor stores a path in: raw, JSON-escaped, `file://` URI, and on
/// Windows both drive-letter cases and forward-slash URI paths.
pub fn path_replacements(platform: Platform, old: &str, new: &str) -> Vec<Replacement> {
    let mut forms: Vec<(String, String)> = Vec::new();
    if platform == Platform::Windows {
        for upper in [false, true] {
            if let (Some(old_case), Some(new_case)) =
                (with_drive_case(old, upper), with_drive_case(new, upper))
            {
                forms.push((old_case, new_case));
            } else {
                forms.push((old.to_string(), new.to_string()));
            }
        }
    } else {
        forms.push((old.to_string(), new.to_string()));
    }
    let mut out = Vec::new();
    for (old_form, new_form) in &forms {
        out.push(Replacement::new(old_form, new_form));
        let escaped_old = json_escaped(old_form);
        if &escaped_old != old_form {
            out.push(Replacement::new(escaped_old, json_escaped(new_form)));
        }
        if platform == Platform::Windows {
            out.push(Replacement::new(
                old_form.replace('\\', "/"),
                new_form.replace('\\', "/"),
            ));
        }
    }
    out.push(Replacement::new(
        file_uri(platform, old),
        file_uri(platform, new),
    ));
    if platform == Platform::current()
        && let (Some(old_url), Some(new_url)) = (legacy_file_url(old), legacy_file_url(new))
    {
        out.push(Replacement::new(old_url, new_url));
    }
    out
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
    use std::time::{Duration, Instant};

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

    #[test]
    fn longest_valid_match_wins_and_output_is_not_rescanned() {
        let rewriter = Rewriter::paths(&[
            Replacement::new("/a/p", "/a/p/x"),
            Replacement::new("/a/p/x", "/z"),
        ]);
        assert_eq!(rewriter.rewrite("/a/p/x /a/p /a/p-1"), "/z /a/p/x /a/p-1");
    }

    #[test]
    fn token_boundaries_protect_neighbours() {
        let rewriter = Rewriter::tokens(&[Replacement::new("abc", "xyz")]);
        assert_eq!(
            rewriter.rewrite("abc:abc-1 xabc abc_ \"abc\" inlineDiff:abc:"),
            "xyz:abc-1 xabc abc_ \"xyz\" inlineDiff:xyz:"
        );
    }

    #[test]
    fn windows_variants_cover_escaped_json_uris_and_drive_case() {
        let rewriter = Rewriter::paths(&path_replacements(
            Platform::Windows,
            "C:\\Users\\me\\old",
            "C:\\Users\\me\\new",
        ));
        let value = r#"{"fsPath":"c:\\Users\\me\\old\\a.rs","external":"file:///c%3A/Users/me/old/a.rs","path":"/c:/Users/me/old/a.rs","raw":"C:\Users\me\old","sib":"c:\\Users\\me\\older"}"#;
        assert_eq!(
            rewriter.rewrite(value),
            r#"{"fsPath":"c:\\Users\\me\\new\\a.rs","external":"file:///c%3A/Users/me/new/a.rs","path":"/c:/Users/me/new/a.rs","raw":"C:\Users\me\new","sib":"c:\\Users\\me\\older"}"#
        );
    }

    #[test]
    fn posix_variants_cover_encoded_uris() {
        let rewriter = Rewriter::paths(&path_replacements(
            Platform::Macos,
            "/Users/me/my app (1)",
            "/Users/me/app",
        ));
        assert_eq!(
            rewriter.rewrite("file:///Users/me/my%20app%20%281%29/x /Users/me/my app (1)/y"),
            "file:///Users/me/app/x /Users/me/app/y"
        );
    }

    #[test]
    fn leftovers_ignore_new_paths_that_extend_the_old_one() {
        let entries = [(Replacement::new("/a/p", "/a/p/sub"), Boundary::Path)];
        let check = Leftovers::new(&entries);
        assert!(!check.found("/a/p/sub/file.rs"));
        assert!(check.found("/a/p/file.rs"));
        assert!(!check.found("/a/p-other"));
    }

    fn uuid(index: usize) -> String {
        format!("{index:08x}-0000-4000-8000-{index:012x}")
    }

    fn time_ids(count: usize) -> Duration {
        let reps: Vec<Replacement> = (0..count)
            .map(|index| Replacement::new(uuid(index), uuid(index + 1_000_000)))
            .collect();
        let value: String = (0..count)
            .map(|index| format!("{{\"id\":\"{}\"}},", uuid(index)))
            .collect();
        let mut best = Duration::MAX;
        for _ in 0..3 {
            let started = Instant::now();
            let rewriter = Rewriter::tokens(&reps);
            let out = rewriter.rewrite(&value);
            best = best.min(started.elapsed());
            assert!(out.contains(&uuid(1_000_000 + count - 1)));
            assert!(!out.contains(&format!("\"{}\"", uuid(0))));
        }
        best
    }

    #[test]
    fn chat_id_rewrite_scales_linearly() {
        let small = time_ids(2_000);
        let large = time_ids(8_000);
        let ratio = large.as_secs_f64() / small.as_secs_f64().max(1e-6);
        assert!(
            ratio < 8.0,
            "4x ids took {ratio:.1}x longer ({small:?} -> {large:?})"
        );
    }
}

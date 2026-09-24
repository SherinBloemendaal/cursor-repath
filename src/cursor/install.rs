//! Every Cursor installation on the machine and the VS Code profiles inside each.
//!
//! An installation is one Electron user data directory: the platform default
//! (`Cursor` under Application Support, `~/.config`, or `%APPDATA%`), any sibling
//! `Cursor*` directory there (`Cursor Nightly`), and every `~/.cursor-<name>`
//! directory that Cursor was started on with `--user-data-dir`.

use anyhow::{Context, Result};
use serde_json::Value;
use std::collections::HashSet;
use std::fs;
use std::path::{Path, PathBuf};

pub const DEFAULT: &str = "default";
pub const DEFAULT_PROFILE: &str = "__default__profile__";

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Found {
    pub name: String,
    pub root: PathBuf,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Roots {
    pub home: PathBuf,
    pub app_data: PathBuf,
}

impl Roots {
    pub fn system() -> Result<Self> {
        Ok(Self {
            home: dirs::home_dir().context("Could not determine home directory")?,
            app_data: crate::config::app_data_dir()?,
        })
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct UserProfile {
    pub id: String,
    pub name: String,
}

pub fn is_user_data_dir(path: &Path) -> bool {
    let user = path.join("User");
    user.join("globalStorage").is_dir() || user.join("workspaceStorage").is_dir()
}

pub fn readable(raw: &str) -> String {
    let mut out = String::new();
    for ch in raw.trim().chars() {
        if ch.is_whitespace() || ch == '/' || ch == '\\' {
            out.push('-');
        } else {
            out.extend(ch.to_lowercase());
        }
    }
    out.trim_matches(|ch| matches!(ch, '-' | '_' | '.'))
        .to_string()
}

fn sorted_dirs(parent: &Path) -> Vec<(String, PathBuf)> {
    let Ok(entries) = fs::read_dir(parent) else {
        return Vec::new();
    };
    let mut found: Vec<(String, PathBuf)> = entries
        .flatten()
        .filter(|entry| entry.path().is_dir())
        .map(|entry| {
            (
                entry.file_name().to_string_lossy().to_string(),
                entry.path(),
            )
        })
        .collect();
    found.sort();
    found
}

fn identity(path: &Path) -> PathBuf {
    fs::canonicalize(path).unwrap_or_else(|_| path.to_path_buf())
}

pub fn discover(roots: &Roots) -> Vec<Found> {
    let mut candidates = vec![(DEFAULT.to_string(), roots.app_data.join("Cursor"))];
    for (name, path) in sorted_dirs(&roots.app_data) {
        let lower = name.to_lowercase();
        if lower == "cursor" || !lower.starts_with("cursor") || !is_user_data_dir(&path) {
            continue;
        }
        let label = readable(&name["cursor".len()..]);
        if !label.is_empty() {
            candidates.push((label, path));
        }
    }
    for (name, path) in sorted_dirs(&roots.home) {
        let Some(rest) = name.strip_prefix(".cursor-") else {
            continue;
        };
        if !is_user_data_dir(&path) {
            continue;
        }
        let label = readable(rest);
        if !label.is_empty() {
            candidates.push((label, path));
        }
    }
    let mut seen = HashSet::new();
    candidates.retain(|(_, path)| seen.insert(identity(path)));
    let wanted: HashSet<String> = candidates.iter().map(|(name, _)| name.clone()).collect();
    let mut taken: HashSet<String> = HashSet::new();
    let mut found = Vec::new();
    for (name, root) in candidates {
        let mut unique = name.clone();
        let mut index = 2;
        while taken.contains(&unique) || (unique != name && wanted.contains(&unique)) {
            unique = format!("{name}-{index}");
            index += 1;
        }
        taken.insert(unique.clone());
        found.push(Found { name: unique, root });
    }
    found.sort_by(|a, b| {
        (a.name != DEFAULT)
            .cmp(&(b.name != DEFAULT))
            .then_with(|| a.name.cmp(&b.name))
    });
    found
}

pub fn user_profiles(storage: &Value) -> Vec<UserProfile> {
    storage
        .get("userDataProfiles")
        .and_then(Value::as_array)
        .map(|items| {
            items
                .iter()
                .filter_map(|item| {
                    let id = item.get("location").and_then(Value::as_str)?;
                    let name = item.get("name").and_then(Value::as_str).unwrap_or(id);
                    Some(UserProfile {
                        id: id.to_string(),
                        name: name.to_string(),
                    })
                })
                .collect()
        })
        .unwrap_or_default()
}

pub fn read_storage(path: &Path) -> Result<Value> {
    if !path.exists() {
        return Ok(Value::Null);
    }
    let raw =
        fs::read_to_string(path).with_context(|| format!("Failed to read: {}", path.display()))?;
    Ok(serde_json::from_str(&raw).unwrap_or(Value::Null))
}

pub fn profile_name(profiles: &[UserProfile], id: &str) -> String {
    if id == DEFAULT_PROFILE {
        return DEFAULT.to_string();
    }
    profiles
        .iter()
        .find(|profile| profile.id == id)
        .map(|profile| profile.name.clone())
        .unwrap_or_else(|| id.to_string())
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn user_data(path: &Path) {
        fs::create_dir_all(path.join("User/globalStorage")).unwrap();
        fs::create_dir_all(path.join("User/workspaceStorage")).unwrap();
    }

    fn roots(tmp: &Path) -> Roots {
        let roots = Roots {
            home: tmp.join("home"),
            app_data: tmp.join("app"),
        };
        fs::create_dir_all(&roots.home).unwrap();
        fs::create_dir_all(&roots.app_data).unwrap();
        roots
    }

    fn names(found: &[Found]) -> Vec<&str> {
        found.iter().map(|item| item.name.as_str()).collect()
    }

    #[test]
    fn finds_default_siblings_and_home_user_data_dirs() {
        let tmp = tempfile::tempdir().unwrap();
        let roots = roots(tmp.path());
        user_data(&roots.app_data.join("Cursor"));
        user_data(&roots.app_data.join("Cursor Nightly"));
        user_data(&roots.home.join(".cursor-resolved"));
        user_data(&roots.home.join(".cursor-debtt-1"));
        fs::create_dir_all(roots.home.join(".cursor/projects")).unwrap();
        fs::create_dir_all(roots.home.join(".cursor-tutor/projects")).unwrap();
        fs::create_dir_all(roots.app_data.join("Code/User/globalStorage")).unwrap();
        let found = discover(&roots);
        assert_eq!(names(&found), ["default", "debtt-1", "nightly", "resolved"]);
        assert_eq!(found[0].root, roots.app_data.join("Cursor"));
        assert_eq!(found[1].root, roots.home.join(".cursor-debtt-1"));
        assert_eq!(found[2].root, roots.app_data.join("Cursor Nightly"));
        assert_eq!(found[3].root, roots.home.join(".cursor-resolved"));
    }

    #[test]
    fn default_is_listed_before_cursor_has_created_it() {
        let tmp = tempfile::tempdir().unwrap();
        let roots = roots(tmp.path());
        let found = discover(&roots);
        assert_eq!(names(&found), ["default"]);
        assert_eq!(found[0].root, roots.app_data.join("Cursor"));
    }

    #[test]
    fn colliding_names_stay_unique_and_readable() {
        let tmp = tempfile::tempdir().unwrap();
        let roots = roots(tmp.path());
        user_data(&roots.app_data.join("Cursor-Resolved"));
        user_data(&roots.home.join(".cursor-resolved"));
        user_data(&roots.home.join(".cursor-resolved-2"));
        let found = discover(&roots);
        assert_eq!(
            names(&found),
            ["default", "resolved", "resolved-2", "resolved-3"]
        );
        assert_eq!(found[1].root, roots.app_data.join("Cursor-Resolved"));
        assert_eq!(found[2].root, roots.home.join(".cursor-resolved-2"));
        assert_eq!(found[3].root, roots.home.join(".cursor-resolved"));
    }

    #[cfg(unix)]
    #[test]
    fn a_symlinked_user_data_dir_is_counted_once() {
        let tmp = tempfile::tempdir().unwrap();
        let roots = roots(tmp.path());
        user_data(&roots.app_data.join("Cursor"));
        user_data(&tmp.path().join("elsewhere"));
        std::os::unix::fs::symlink(
            roots.app_data.join("Cursor"),
            roots.home.join(".cursor-alias"),
        )
        .unwrap();
        std::os::unix::fs::symlink(
            tmp.path().join("elsewhere"),
            roots.home.join(".cursor-linked"),
        )
        .unwrap();
        assert_eq!(names(&discover(&roots)), ["default", "linked"]);
    }

    #[test]
    fn readable_names_are_lowercase_and_trimmed() {
        assert_eq!(readable(" Nightly"), "nightly");
        assert_eq!(readable("-resolved"), "resolved");
        assert_eq!(readable("Team Work"), "team-work");
        assert_eq!(readable("debtt-1"), "debtt-1");
        assert_eq!(readable("--"), "");
    }

    #[test]
    fn user_profiles_come_from_storage_json() {
        let storage = json!({
            "userDataProfiles": [
                {"location": "-5a3b1c", "name": "Work"},
                {"location": "7f2e"}
            ]
        });
        let profiles = user_profiles(&storage);
        assert_eq!(
            profiles,
            [
                UserProfile {
                    id: "-5a3b1c".into(),
                    name: "Work".into()
                },
                UserProfile {
                    id: "7f2e".into(),
                    name: "7f2e".into()
                }
            ]
        );
        assert_eq!(profile_name(&profiles, "-5a3b1c"), "Work");
        assert_eq!(profile_name(&profiles, DEFAULT_PROFILE), "default");
        assert_eq!(profile_name(&profiles, "gone"), "gone");
        assert!(user_profiles(&Value::Null).is_empty());
    }
}

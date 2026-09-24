//! Global storage operations
//!
//! Handles updates to ~/Library/Application Support/Cursor/User/globalStorage/storage.json

use anyhow::{Context, Result};
use serde::Deserialize;
use serde_json::{Map, Value};
use std::collections::HashMap;
use std::fs;
use std::path::Path;

use super::rewrite::{Replacement, replace_scoped};

/// Rewrite every string and object key in storage.json.
///
/// Covers folderUri, workspace configPath, profileAssociations.workspaces keys,
/// backupWorkspaces entries, windowsState, windowSplashWorkspaceOverride, and exact hashes.
pub fn rewrite_storage_json<P: AsRef<Path>>(
    storage_path: P,
    replacements: &[Replacement],
    dry_run: bool,
) -> Result<Vec<String>> {
    let storage_path = storage_path.as_ref();
    if !storage_path.exists() {
        return Ok(Vec::new());
    }
    let content = fs::read_to_string(storage_path)
        .with_context(|| format!("Failed to read: {}", storage_path.display()))?;
    let mut json: Value = serde_json::from_str(&content).context("Failed to parse storage.json")?;
    let rewritten = rewrite_storage_value(&mut json, replacements);
    if !rewritten.is_empty() && !dry_run {
        let new_content = serde_json::to_string_pretty(&json)?;
        fs::write(storage_path, new_content)
            .with_context(|| format!("Failed to write: {}", storage_path.display()))?;
    }
    Ok(rewritten)
}

pub fn rewrite_storage_value(value: &mut Value, replacements: &[Replacement]) -> Vec<String> {
    let mut rewritten = Vec::new();
    walk_json(value, "", replacements, &mut rewritten);
    rewritten
}

fn walk_json(
    value: &mut Value,
    path: &str,
    replacements: &[Replacement],
    rewritten: &mut Vec<String>,
) {
    match value {
        Value::Object(map) => walk_object(map, path, replacements, rewritten),
        Value::Array(items) => {
            for (index, item) in items.iter_mut().enumerate() {
                let child = format!("{path}[{index}]");
                walk_json(item, &child, replacements, rewritten);
            }
        }
        Value::String(text) => {
            let updated = replace_scoped(text, replacements);
            if updated != *text {
                *text = updated;
                rewritten.push(path.to_string());
            }
        }
        _ => {}
    }
}

fn walk_object(
    map: &mut Map<String, Value>,
    path: &str,
    replacements: &[Replacement],
    rewritten: &mut Vec<String>,
) {
    let keys: Vec<String> = map.keys().cloned().collect();
    let mut rebuilt = Map::new();
    for key in keys {
        let mut child = map.remove(&key).unwrap_or(Value::Null);
        let child_path = if path.is_empty() {
            key.clone()
        } else {
            format!("{path}.{key}")
        };
        walk_json(&mut child, &child_path, replacements, rewritten);
        let new_key = replace_scoped(&key, replacements);
        if new_key != key {
            rewritten.push(format!("{child_path}#key"));
        }
        rebuilt.insert(new_key, child);
    }
    *map = rebuilt;
}

/// Update workspace references in storage.json.
pub fn update_storage_json<P: AsRef<Path>>(
    storage_path: P,
    old_uri: &str,
    new_uri: &str,
    dry_run: bool,
) -> Result<bool> {
    let rewritten =
        rewrite_storage_json(storage_path, &[Replacement::new(old_uri, new_uri)], dry_run)?;
    Ok(!rewritten.is_empty())
}

/// A simpler representation of storage.json for reading
#[derive(Debug, Deserialize)]
#[allow(dead_code)]
pub struct StorageJson {
    #[serde(rename = "backupWorkspaces")]
    pub backup_workspaces: Option<BackupWorkspaces>,

    #[serde(rename = "profileAssociations")]
    pub profile_associations: Option<ProfileAssociations>,
}

#[derive(Debug, Deserialize)]
#[allow(dead_code)]
pub struct BackupWorkspaces {
    pub folders: Option<Vec<FolderEntry>>,
}

#[derive(Debug, Deserialize)]
#[allow(dead_code)]
pub struct FolderEntry {
    #[serde(rename = "folderUri")]
    pub folder_uri: String,
}

#[derive(Debug, Deserialize)]
#[allow(dead_code)]
pub struct ProfileAssociations {
    pub workspaces: Option<HashMap<String, String>>,
}

impl StorageJson {
    /// Read storage.json from a file
    #[allow(dead_code)]
    pub fn read<P: AsRef<Path>>(path: P) -> Result<Self> {
        let content = fs::read_to_string(path.as_ref())
            .with_context(|| format!("Failed to read: {}", path.as_ref().display()))?;
        serde_json::from_str(&content).context("Failed to parse storage.json")
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::Write;
    use tempfile::NamedTempFile;

    #[test]
    fn test_update_storage_json() {
        let mut file = NamedTempFile::new().unwrap();
        write!(
            file,
            r#"{{
    "backupWorkspaces": {{
        "folders": [
            {{ "folderUri": "file:///old/path" }},
            {{ "folderUri": "file:///other/path" }}
        ]
    }},
    "profileAssociations": {{
        "workspaces": {{
            "file:///old/path": "__default__profile__"
        }}
    }}
}}"#
        )
        .unwrap();

        let modified =
            update_storage_json(file.path(), "file:///old/path", "file:///new/path", false)
                .unwrap();

        assert!(modified);

        // Verify changes
        let content = fs::read_to_string(file.path()).unwrap();
        assert!(content.contains("file:///new/path"));
        assert!(!content.contains("file:///old/path"));
    }
}

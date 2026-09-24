//! Global storage.json rewrites.

use anyhow::{Context, Result};
use serde_json::{Map, Value};
use std::fs;
use std::path::Path;

use super::rewrite::{Replacement, Rewriter};

/// Rewrite every string and object key in storage.json.
///
/// Covers folderUri, workspace configPath, profileAssociations.workspaces keys,
/// backupWorkspaces entries, windowsState, windowSplashWorkspaceOverride, and exact hashes.
pub fn rewrite_storage_json<P: AsRef<Path>>(
    storage_path: P,
    replacements: &[Replacement],
    dry_run: bool,
) -> Result<Vec<String>> {
    rewrite_storage_file(
        storage_path.as_ref(),
        &Rewriter::paths(replacements),
        dry_run,
    )
}

pub fn rewrite_storage_file(
    storage_path: &Path,
    rewriter: &Rewriter,
    dry_run: bool,
) -> Result<Vec<String>> {
    if !storage_path.exists() {
        return Ok(Vec::new());
    }
    let content = fs::read_to_string(storage_path)
        .with_context(|| format!("Failed to read: {}", storage_path.display()))?;
    let mut json: Value = serde_json::from_str(&content).context("Failed to parse storage.json")?;
    let rewritten = rewrite_storage_value(&mut json, rewriter);
    if !rewritten.is_empty() && !dry_run {
        let new_content = serde_json::to_string_pretty(&json)?;
        let parent = storage_path
            .parent()
            .context("storage.json has no parent")?;
        let mut temp = tempfile::NamedTempFile::new_in(parent)?;
        std::io::Write::write_all(&mut temp, new_content.as_bytes())?;
        temp.as_file().sync_all()?;
        temp.persist(storage_path)
            .map_err(|err| err.error)
            .with_context(|| format!("Failed to write: {}", storage_path.display()))?;
    }
    Ok(rewritten)
}

pub fn rewrite_storage_value(value: &mut Value, rewriter: &Rewriter) -> Vec<String> {
    let mut rewritten = Vec::new();
    walk_json(value, "", rewriter, &mut rewritten);
    rewritten
}

fn walk_json(value: &mut Value, path: &str, rewriter: &Rewriter, rewritten: &mut Vec<String>) {
    match value {
        Value::Object(map) => walk_object(map, path, rewriter, rewritten),
        Value::Array(items) => {
            for (index, item) in items.iter_mut().enumerate() {
                let child = format!("{path}[{index}]");
                walk_json(item, &child, rewriter, rewritten);
            }
        }
        Value::String(text) => {
            let updated = rewriter.rewrite(text).into_owned();
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
    rewriter: &Rewriter,
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
        walk_json(&mut child, &child_path, rewriter, rewritten);
        let new_key = rewriter.rewrite(&key).into_owned();
        if new_key != key {
            rewritten.push(format!("{child_path}#key"));
        }
        rebuilt.insert(new_key, child);
    }
    *map = rebuilt;
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::Write;
    use tempfile::NamedTempFile;

    #[test]
    fn rewrites_backup_folders_and_profile_keys() {
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
        let rewritten = rewrite_storage_json(
            file.path(),
            &[Replacement::new("file:///old/path", "file:///new/path")],
            false,
        )
        .unwrap();
        assert!(!rewritten.is_empty());
        let content = fs::read_to_string(file.path()).unwrap();
        assert!(content.contains("file:///new/path"));
        assert!(!content.contains("file:///old/path"));
        assert!(content.contains("file:///other/path"));
    }
}

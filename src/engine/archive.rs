//! `.crepath` gzip archives.

use anyhow::{Context, Result, bail};
use flate2::Compression;
use flate2::read::GzDecoder;
use flate2::write::GzEncoder;
use rusqlite::params;
use serde_json::{Value, json};
use sha2::{Digest, Sha256};
use std::fs::{self, File};
use std::path::{Path, PathBuf};
use tar::{Archive, Builder};

use super::{Runtime, Workspace};
use crate::cursor::folder_id::path_to_folder_id;
use crate::cursor::registry::{self, COMPOSER_EXACT_PREFIXES, COMPOSER_PREFIXES};
use crate::cursor::rewrite::key_range_end;
use crate::engine::db;

pub fn write_export(rt: &Runtime, workspace: &Workspace, file: &Path) -> Result<()> {
    if let Some(parent) = file.parent()
        && !parent.as_os_str().is_empty()
    {
        fs::create_dir_all(parent)?;
    }
    let staging = rt.layout.crepath_home.join("export-staging");
    if staging.exists() {
        fs::remove_dir_all(&staging)?;
    }
    fs::create_dir_all(&staging)?;
    let storage_rel = PathBuf::from("workspaceStorage").join(&workspace.id);
    copy_tree(&workspace.dir, &staging.join(&storage_rel))?;
    if let Some(path) = &workspace.path {
        let slug = path_to_folder_id(path);
        let projects = rt.layout.projects_dir.join(&slug);
        if projects.exists() {
            copy_tree(&projects, &staging.join("projects").join(&slug))?;
        }
    }
    let conn = db::open_rw(&rt.layout.global_db())?;
    let headers = db::headers(&conn, &workspace.id)?;
    fs::write(
        staging.join("composer-headers.json"),
        serde_json::to_string_pretty(&headers_json(&headers))?,
    )?;
    let mut kv = Vec::new();
    for header in &headers {
        collect_composer_rows(&conn, &header.composer_id, &mut kv)?;
    }
    fs::write(
        staging.join("cursor-disk.json"),
        serde_json::to_string_pretty(&kv)?,
    )?;
    let excerpt = storage_excerpt(rt, workspace)?;
    fs::write(
        staging.join("storage-excerpt.json"),
        serde_json::to_string_pretty(&excerpt)?,
    )?;
    let mut files = Vec::new();
    for entry in walkdir::WalkDir::new(&staging) {
        let entry = entry?;
        if entry.file_type().is_file() {
            let rel = entry.path().strip_prefix(&staging)?.to_path_buf();
            let bytes = fs::read(entry.path())?;
            files.push(json!({
                "path": rel.to_string_lossy(),
                "sha256": sha256_hex(&bytes),
            }));
        }
    }
    let manifest = json!({
        "version": 1,
        "workspaceId": workspace.id,
        "files": files,
    });
    fs::write(
        staging.join("manifest.json"),
        serde_json::to_string_pretty(&manifest)?,
    )?;
    let gz = GzEncoder::new(File::create(file)?, Compression::default());
    let mut builder = Builder::new(gz);
    builder.append_dir_all(".", &staging)?;
    builder.finish()?;
    fs::remove_dir_all(staging)?;
    Ok(())
}

pub fn read_import(rt: &Runtime, file: &Path, dest: Option<&Path>) -> Result<()> {
    let staging = rt.layout.crepath_home.join("import-staging");
    if staging.exists() {
        fs::remove_dir_all(&staging)?;
    }
    fs::create_dir_all(&staging)?;
    let decoder = GzDecoder::new(File::open(file)?);
    let mut archive = Archive::new(decoder);
    archive.unpack(&staging)?;
    let manifest: Value =
        serde_json::from_str(&fs::read_to_string(staging.join("manifest.json"))?)?;
    verify_checksums(&staging, &manifest)?;
    let workspace_id = manifest
        .get("workspaceId")
        .and_then(|v| v.as_str())
        .context("manifest missing workspaceId")?;
    let packed = staging.join("workspaceStorage").join(workspace_id);
    let dest_dir = rt.layout.workspace_storage().join(workspace_id);
    if dest_dir.exists() {
        bail!(
            "collision: {} already holds another workspace; refusing overwrite",
            dest_dir.display()
        );
    }
    copy_tree(&packed, &dest_dir)?;
    if let Some(projects) = staging
        .join("projects")
        .exists()
        .then_some(staging.join("projects"))
        && projects.is_dir()
    {
        for entry in fs::read_dir(projects)?.flatten() {
            let target = rt.layout.projects_dir.join(entry.file_name());
            copy_tree(&entry.path(), &target)?;
        }
    }
    let headers: Vec<Value> =
        serde_json::from_str(&fs::read_to_string(staging.join("composer-headers.json"))?)?;
    let rows: Vec<Value> =
        serde_json::from_str(&fs::read_to_string(staging.join("cursor-disk.json"))?)?;
    let conn = db::ensure_db(&rt.layout.global_db())?;
    for header in headers {
        conn.execute(
            "INSERT OR REPLACE INTO composerHeaders (composerId, workspaceId, createdAt, lastUpdatedAt, isArchived, isSubagent, recency, checkpointAt, value, subagentTypeName) \
             VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10)",
            params![
                header.get("composerId").and_then(|v| v.as_str()).unwrap_or(""),
                header.get("workspaceId").and_then(|v| v.as_str()).unwrap_or(workspace_id),
                header.get("createdAt").and_then(|v| v.as_i64()),
                header.get("lastUpdatedAt").and_then(|v| v.as_i64()),
                header.get("isArchived").and_then(|v| v.as_i64()).unwrap_or(0),
                header.get("isSubagent").and_then(|v| v.as_i64()).unwrap_or(0),
                header.get("recency").and_then(|v| v.as_i64()),
                header.get("checkpointAt").and_then(|v| v.as_i64()),
                header.get("value").and_then(|v| v.as_str()).unwrap_or("{}"),
                header.get("subagentTypeName").and_then(|v| v.as_str()),
            ],
        )?;
    }
    for row in rows {
        let key = row.get("key").and_then(|v| v.as_str()).unwrap_or("");
        if key.starts_with("agentKv:") {
            continue;
        }
        let value = row.get("value").and_then(|v| v.as_str()).unwrap_or("");
        conn.execute(
            "INSERT OR REPLACE INTO cursorDiskKV (key, value) VALUES (?1, ?2)",
            params![key, value],
        )?;
    }
    if let Some(dest) = dest
        && dest.exists()
    {
        let uri = url::Url::from_file_path(dest)
            .map_err(|_| anyhow::anyhow!("failed to convert path to uri: {}", dest.display()))?;
        let body = if dest.extension().and_then(|ext| ext.to_str()) == Some("code-workspace") {
            json!({ "workspace": uri.to_string() })
        } else {
            json!({ "folder": uri.to_string() })
        };
        fs::write(
            dest_dir.join("workspace.json"),
            serde_json::to_string_pretty(&body)?,
        )?;
    }
    fs::remove_dir_all(staging)?;
    Ok(())
}

fn verify_checksums(staging: &Path, manifest: &Value) -> Result<()> {
    let Some(files) = manifest.get("files").and_then(|v| v.as_array()) else {
        bail!("manifest has no checksums");
    };
    for file in files {
        let rel = file.get("path").and_then(|v| v.as_str()).unwrap_or("");
        if rel == "manifest.json" {
            continue;
        }
        let expected = file.get("sha256").and_then(|v| v.as_str()).unwrap_or("");
        let bytes =
            fs::read(staging.join(rel)).with_context(|| format!("missing archived file {rel}"))?;
        let actual = sha256_hex(&bytes);
        if actual != expected {
            bail!("checksum mismatch for {rel}");
        }
    }
    Ok(())
}

fn headers_json(headers: &[registry::ComposerHeader]) -> Vec<Value> {
    headers
        .iter()
        .map(|header| {
            json!({
                "composerId": header.composer_id,
                "workspaceId": header.workspace_id,
                "createdAt": header.created_at,
                "lastUpdatedAt": header.last_updated_at,
                "isArchived": if header.is_archived { 1 } else { 0 },
                "isSubagent": if header.is_subagent { 1 } else { 0 },
                "recency": header.recency,
                "checkpointAt": header.checkpoint_at,
                "value": header.value,
                "subagentTypeName": header.subagent_type_name,
            })
        })
        .collect()
}

fn collect_composer_rows(
    conn: &rusqlite::Connection,
    composer_id: &str,
    out: &mut Vec<Value>,
) -> Result<()> {
    for prefix in COMPOSER_EXACT_PREFIXES {
        let key = format!("{prefix}{composer_id}");
        if let Some(value) = db::read_text(conn, &key)? {
            out.push(json!({"key": key, "value": value}));
        }
    }
    for prefix in COMPOSER_PREFIXES {
        let start = format!("{prefix}{composer_id}:");
        let end = key_range_end(&start);
        let mut stmt =
            conn.prepare("SELECT key, value FROM cursorDiskKV WHERE key >= ?1 AND key < ?2")?;
        let mut rows = stmt.query(params![start, end])?;
        while let Some(row) = rows.next()? {
            let key: String = row.get(0)?;
            if key.starts_with("agentKv:") {
                continue;
            }
            let value: String = row.get(1)?;
            out.push(json!({"key": key, "value": value}));
        }
    }
    Ok(())
}

fn storage_excerpt(rt: &Runtime, workspace: &Workspace) -> Result<Value> {
    let path = rt.layout.storage_json();
    if !path.exists() {
        return Ok(json!({}));
    }
    let json: Value = serde_json::from_str(&fs::read_to_string(path)?)?;
    let needle = workspace.uri.clone().unwrap_or_default();
    Ok(json!({
        "workspaceId": workspace.id,
        "uri": needle,
        "profileAssociations": json.get("profileAssociations").cloned().unwrap_or(Value::Null),
    }))
}

fn copy_tree(source: &Path, dest: &Path) -> Result<()> {
    if !source.exists() {
        return Ok(());
    }
    let options = fs_extra::dir::CopyOptions::new().copy_inside(true);
    if let Some(parent) = dest.parent() {
        fs::create_dir_all(parent)?;
    }
    fs_extra::dir::copy(source, dest, &options)?;
    Ok(())
}

fn sha256_hex(bytes: &[u8]) -> String {
    let digest = Sha256::digest(bytes);
    digest.iter().map(|byte| format!("{byte:02x}")).collect()
}

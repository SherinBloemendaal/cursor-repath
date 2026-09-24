//! `.crepath` gzip archives.
//!
//! Version 2 layout: `manifest.json`, `headers.jsonl` (full `composerHeaders` rows),
//! `rows.jsonl` (composer-keyed `cursorDiskKV` rows), `blobs.jsonl` (`agentKv:blob:*` rows
//! referenced by those chats), `storage-excerpt.json`, `workspaceStorage/<id>/`, and
//! `projects/<slug>/`. Every value keeps its SQLite type; BLOBs are base64.

use anyhow::{Context, Result, bail};
use base64::Engine as _;
use base64::engine::general_purpose::{STANDARD, STANDARD_NO_PAD};
use flate2::Compression;
use flate2::read::GzDecoder;
use flate2::write::GzEncoder;
use rusqlite::Connection;
use rusqlite::types::Value;
use serde_json::{Value as Json, json};
use sha2::{Digest, Sha256};
use std::collections::{HashMap, HashSet, VecDeque};
use std::fs::{self, File};
use std::io::{BufRead, BufReader, BufWriter, Read, Write};
use std::path::{Component, Path, PathBuf};
use tar::{Archive, Builder, EntryType};

use super::db::{self, HeaderRow, Target};
use super::fsops::{SpaceNeeds, copy_tree, exists, write_atomic};
use super::journal::Table;
use super::local;
use super::session;
use super::{Kind, Report, Runtime, Workspace, open_global_ro, require_workspace};
use crate::cursor::registry::{
    BLOB_PREFIX, COMPOSER_EXACT_PREFIXES, COMPOSER_PREFIXES, composer_headers_table,
};
use crate::cursor::rewrite::{Boundary, Replacement, Rewriter, path_replacements};
use crate::cursor::uri::{Platform, normalize_path, uri_path};

const VERSION: u64 = 2;

pub fn export_workspace(rt: &Runtime, target: &str, file: &Path) -> Result<Report> {
    rt.check()?;
    let workspace = require_workspace(rt, target)?;
    let conn = open_global_ro(rt)?;
    let ids = match &conn {
        Some(conn) => {
            db::expand_chat_ids(conn, &db::composer_ids_for_workspace(conn, &workspace.id)?)?
        }
        None => Vec::new(),
    };
    let file = normalize_path(file);
    if exists(&file) {
        bail!("refusing to overwrite {}", file.display());
    }
    let mut report = Report::default();
    if rt.dry_run {
        report.applied.push(format!(
            "dry-run export {} ({} chats) -> {}",
            workspace.id,
            ids.len(),
            file.display()
        ));
        return Ok(report);
    }
    let parent = file.parent().context("archive path has no parent")?;
    fs::create_dir_all(parent)?;
    let staging = tempfile::tempdir_in(parent)?;
    let mut files = Vec::new();
    let mut blobs = 0usize;
    if let Some(conn) = &conn {
        files.push(write_lines(staging.path(), "headers.jsonl", |out| {
            write_headers(conn, &ids, out)
        })?);
        let mut refs = Vec::new();
        files.push(write_lines(staging.path(), "rows.jsonl", |out| {
            write_rows(conn, &ids, out, &mut refs)
        })?);
        files.push(write_lines(staging.path(), "blobs.jsonl", |out| {
            blobs = write_blobs(conn, refs, out)?;
            Ok(())
        })?);
    }
    let excerpt = storage_excerpt(rt, &workspace)?;
    let excerpt_path = staging.path().join("storage-excerpt.json");
    fs::write(&excerpt_path, serde_json::to_vec_pretty(&excerpt)?)?;
    files.push(file_entry("storage-excerpt.json", &excerpt_path)?);
    let storage_prefix = format!("workspaceStorage/{}", workspace.id);
    files.extend(tree_entries(&workspace.dir, &storage_prefix)?);
    let slug = workspace.slug();
    let project_dir = slug.as_ref().map(|slug| rt.layout.projects_dir.join(slug));
    let project_prefix = slug.as_ref().map(|slug| format!("projects/{slug}"));
    if let (Some(dir), Some(prefix)) = (&project_dir, &project_prefix)
        && dir.is_dir()
    {
        files.extend(tree_entries(dir, prefix)?);
    }
    let manifest = json!({
        "version": VERSION,
        "workspace": {
            "id": workspace.id,
            "kind": workspace.kind.label(),
            "uri": workspace.uri,
            "path": workspace.path.as_ref().map(|path| path.to_string_lossy().to_string()),
            "slug": slug,
        },
        "chats": ids,
        "blobs": blobs,
        "files": files,
    });
    let manifest_path = staging.path().join("manifest.json");
    fs::write(&manifest_path, serde_json::to_vec_pretty(&manifest)?)?;
    let partial = tempfile::NamedTempFile::new_in(parent)?;
    {
        let gz = GzEncoder::new(BufWriter::new(partial.as_file()), Compression::default());
        let mut builder = Builder::new(gz);
        builder.follow_symlinks(false);
        for name in [
            "headers.jsonl",
            "rows.jsonl",
            "blobs.jsonl",
            "storage-excerpt.json",
        ] {
            let path = staging.path().join(name);
            if path.exists() {
                builder.append_path_with_name(&path, name)?;
            }
        }
        if workspace.dir.is_dir() {
            builder.append_dir_all(&storage_prefix, &workspace.dir)?;
        }
        if let (Some(dir), Some(prefix)) = (&project_dir, &project_prefix)
            && dir.is_dir()
        {
            builder.append_dir_all(prefix, dir)?;
        }
        builder.append_path_with_name(&manifest_path, "manifest.json")?;
        builder.into_inner()?.finish()?.flush()?;
    }
    partial.as_file().sync_all()?;
    partial
        .persist_noclobber(&file)
        .map_err(|err| err.error)
        .with_context(|| format!("failed to write {}", file.display()))?;
    report.applied.push(format!(
        "exported {} ({} chats, {blobs} blobs) -> {}",
        workspace.id,
        ids.len(),
        file.display()
    ));
    Ok(report)
}

struct HashingWriter<W: Write> {
    inner: W,
    hasher: Sha256,
}

impl<W: Write> Write for HashingWriter<W> {
    fn write(&mut self, buf: &[u8]) -> std::io::Result<usize> {
        let written = self.inner.write(buf)?;
        self.hasher.update(&buf[..written]);
        Ok(written)
    }

    fn flush(&mut self) -> std::io::Result<()> {
        self.inner.flush()
    }
}

type LineWriter = HashingWriter<BufWriter<File>>;

fn write_lines(
    dir: &Path,
    name: &str,
    fill: impl FnOnce(&mut LineWriter) -> Result<()>,
) -> Result<Json> {
    let mut out = HashingWriter {
        inner: BufWriter::new(File::create(dir.join(name))?),
        hasher: Sha256::new(),
    };
    fill(&mut out)?;
    out.flush()?;
    Ok(json!({"path": name, "sha256": hex(&out.hasher.finalize())}))
}

fn hex(bytes: &[u8]) -> String {
    bytes.iter().map(|byte| format!("{byte:02x}")).collect()
}

fn sha256_file(path: &Path) -> Result<String> {
    let mut hasher = Sha256::new();
    let mut reader = BufReader::new(File::open(path)?);
    let mut buf = vec![0u8; 1 << 16];
    loop {
        let read = reader.read(&mut buf)?;
        if read == 0 {
            break;
        }
        hasher.update(&buf[..read]);
    }
    Ok(hex(&hasher.finalize()))
}

fn file_entry(name: &str, path: &Path) -> Result<Json> {
    Ok(json!({"path": name, "sha256": sha256_file(path)?}))
}

fn tree_entries(root: &Path, prefix: &str) -> Result<Vec<Json>> {
    let mut out = Vec::new();
    if !root.is_dir() {
        return Ok(out);
    }
    for entry in walkdir::WalkDir::new(root).follow_links(false) {
        let entry = entry?;
        if entry.file_type().is_symlink() {
            bail!("refusing to export symlink {}", entry.path().display());
        }
        if !entry.file_type().is_file() {
            continue;
        }
        let rel = entry.path().strip_prefix(root)?;
        let rel: Vec<String> = rel
            .components()
            .map(|part| part.as_os_str().to_string_lossy().to_string())
            .collect();
        out.push(file_entry(
            &format!("{prefix}/{}", rel.join("/")),
            entry.path(),
        )?);
    }
    Ok(out)
}

fn encode_value(value: &Value) -> Json {
    match value {
        Value::Null => json!({"t": "null"}),
        Value::Integer(number) => json!({"t": "integer", "v": number}),
        Value::Real(number) => json!({"t": "real", "v": number}),
        Value::Text(text) => json!({"t": "text", "v": text}),
        Value::Blob(bytes) => json!({"t": "blob", "v": STANDARD.encode(bytes)}),
    }
}

fn decode_value(json: &Json) -> Result<Value> {
    let kind = json
        .get("t")
        .and_then(|v| v.as_str())
        .context("value without type")?;
    Ok(match kind {
        "null" => Value::Null,
        "integer" => Value::Integer(
            json.get("v")
                .and_then(|v| v.as_i64())
                .context("bad integer")?,
        ),
        "real" => Value::Real(json.get("v").and_then(|v| v.as_f64()).context("bad real")?),
        "text" => Value::Text(
            json.get("v")
                .and_then(|v| v.as_str())
                .context("bad text")?
                .to_string(),
        ),
        "blob" => Value::Blob(
            STANDARD
                .decode(json.get("v").and_then(|v| v.as_str()).context("bad blob")?)
                .context("bad blob encoding")?,
        ),
        other => bail!("unknown value type {other}"),
    })
}

fn write_json_line(out: &mut impl Write, line: &Json) -> Result<()> {
    serde_json::to_writer(&mut *out, line)?;
    out.write_all(b"\n")?;
    Ok(())
}

fn write_headers(conn: &Connection, ids: &[String], out: &mut impl Write) -> Result<()> {
    if !composer_headers_table(conn)? {
        for header in crate::cursor::registry::load_headers(conn)? {
            if ids.contains(&header.composer_id) {
                write_json_line(
                    out,
                    &json!({"legacy": serde_json::from_str::<Json>(&header.value)?}),
                )?;
            }
        }
        return Ok(());
    }
    for id in ids {
        if let Some(row) = db::header_row(conn, id)? {
            let columns: Vec<Json> = row
                .columns
                .iter()
                .map(|(name, value)| json!([name, encode_value(value)]))
                .collect();
            write_json_line(out, &json!({"columns": columns}))?;
        }
    }
    Ok(())
}

fn write_rows(
    conn: &Connection,
    ids: &[String],
    out: &mut impl Write,
    refs: &mut Vec<String>,
) -> Result<()> {
    for id in ids {
        for key in db::composer_keys(conn, id)? {
            let Some(value) = db::read_value(conn, Table::Disk, &key)? else {
                continue;
            };
            collect_refs(&key, &value, refs);
            write_json_line(out, &json!({"key": key, "value": encode_value(&value)}))?;
        }
    }
    Ok(())
}

fn write_blobs(conn: &Connection, refs: Vec<String>, out: &mut impl Write) -> Result<usize> {
    let mut seen = HashSet::new();
    let mut queue: VecDeque<String> = refs.into();
    let mut count = 0usize;
    while let Some(hash) = queue.pop_front() {
        if !seen.insert(hash.clone()) {
            continue;
        }
        let key = format!("{BLOB_PREFIX}{hash}");
        let Some(value) = db::read_value(conn, Table::Disk, &key)? else {
            continue;
        };
        let mut nested = Vec::new();
        collect_refs(&key, &value, &mut nested);
        queue.extend(nested);
        write_json_line(out, &json!({"key": key, "value": encode_value(&value)}))?;
        count += 1;
    }
    Ok(count)
}

fn is_hex(byte: u8) -> bool {
    byte.is_ascii_digit() || (b'a'..=b'f').contains(&byte)
}

fn hex_tokens(bytes: &[u8], out: &mut Vec<String>) {
    let mut index = 0usize;
    while index < bytes.len() {
        if !is_hex(bytes[index]) {
            index += 1;
            continue;
        }
        let start = index;
        while index < bytes.len() && is_hex(bytes[index]) {
            index += 1;
        }
        let bounded = (start == 0 || !bytes[start - 1].is_ascii_alphanumeric())
            && bytes
                .get(index)
                .is_none_or(|next| !next.is_ascii_alphanumeric());
        if index - start == 64 && bounded {
            out.push(String::from_utf8_lossy(&bytes[start..index]).to_string());
        }
    }
}

fn varint(bytes: &[u8], index: &mut usize) -> Option<u64> {
    let mut value = 0u64;
    for shift in (0..64).step_by(7) {
        let byte = *bytes.get(*index)?;
        *index += 1;
        value |= u64::from(byte & 0x7f) << shift;
        if byte < 0x80 {
            return Some(value);
        }
    }
    None
}

fn protobuf_hashes(bytes: &[u8], depth: usize, out: &mut Vec<String>) -> bool {
    if depth > 8 {
        return false;
    }
    let mut index = 0usize;
    let mut found = Vec::new();
    while index < bytes.len() {
        let Some(key) = varint(bytes, &mut index) else {
            return false;
        };
        match key & 7 {
            0 => {
                if varint(bytes, &mut index).is_none() {
                    return false;
                }
            }
            1 => index += 8,
            5 => index += 4,
            2 => {
                let Some(len) = varint(bytes, &mut index) else {
                    return false;
                };
                let Ok(len) = usize::try_from(len) else {
                    return false;
                };
                let Some(chunk) = index.checked_add(len).and_then(|end| bytes.get(index..end))
                else {
                    return false;
                };
                index += len;
                if len == 32 {
                    found.push(hex(chunk));
                } else {
                    protobuf_hashes(chunk, depth + 1, &mut found);
                }
            }
            _ => return false,
        }
        if index > bytes.len() {
            return false;
        }
    }
    out.extend(found);
    true
}

/// Blob hashes a row points at: 64-hex tokens, and 32-byte fields of the base64 protobuf
/// `conversationState` in `composerData`.
fn collect_refs(key: &str, value: &Value, out: &mut Vec<String>) {
    let bytes: &[u8] = match value {
        Value::Text(text) => text.as_bytes(),
        Value::Blob(bytes) => bytes,
        Value::Null | Value::Integer(_) | Value::Real(_) => return,
    };
    hex_tokens(bytes, out);
    if key.starts_with("composerData:")
        && let Ok(json) = serde_json::from_slice::<Json>(bytes)
        && let Some(state) = json.get("conversationState").and_then(|v| v.as_str())
    {
        let encoded = state.strip_prefix('~').unwrap_or(state);
        let decoded = STANDARD
            .decode(encoded)
            .or_else(|_| STANDARD_NO_PAD.decode(encoded.trim_end_matches('=')));
        if let Ok(decoded) = decoded {
            protobuf_hashes(&decoded, 0, out);
        }
    }
    if key.starts_with(BLOB_PREFIX) && std::str::from_utf8(bytes).is_err() {
        protobuf_hashes(bytes, 0, out);
    }
}

fn storage_excerpt(rt: &Runtime, workspace: &Workspace) -> Result<Json> {
    let path = rt.layout.storage_json();
    let uri = workspace.uri.clone().unwrap_or_default();
    if !path.exists() {
        return Ok(json!({"workspaceId": workspace.id, "uri": uri}));
    }
    let storage: Json = serde_json::from_str(&fs::read_to_string(path)?)?;
    Ok(json!({
        "workspaceId": workspace.id,
        "uri": uri,
        "profile": storage.pointer("/profileAssociations/workspaces").and_then(|v| v.get(&uri)).cloned(),
    }))
}

fn is_uuid(id: &str) -> bool {
    let bytes = id.as_bytes();
    bytes.len() == 36
        && bytes.iter().enumerate().all(|(index, byte)| match index {
            8 | 13 | 18 | 23 => *byte == b'-',
            _ => byte.is_ascii_hexdigit(),
        })
}

/// Workspace ids Cursor creates: md5 hex, untitled timestamps, or UUIDs.
pub fn valid_workspace_id(id: &str) -> bool {
    (id.len() == 32 && id.bytes().all(|byte| byte.is_ascii_hexdigit()))
        || ((10..=16).contains(&id.len()) && id.bytes().all(|byte| byte.is_ascii_digit()))
        || is_uuid(id)
}

fn valid_chat_id(id: &str) -> bool {
    is_uuid(id)
}

fn valid_slug(slug: &str) -> bool {
    !slug.is_empty() && slug != "." && slug != ".." && !slug.contains(['/', '\\', ':', '\0'])
}

fn chat_of_key(key: &str) -> Option<&str> {
    for prefix in COMPOSER_EXACT_PREFIXES {
        if let Some(rest) = key.strip_prefix(prefix)
            && valid_chat_id(rest)
        {
            return Some(rest);
        }
    }
    for prefix in COMPOSER_PREFIXES {
        if let Some(rest) = key.strip_prefix(prefix)
            && let Some((id, _)) = rest.split_once(':')
        {
            return Some(id);
        }
    }
    None
}

fn valid_blob_key(key: &str) -> bool {
    key.strip_prefix(BLOB_PREFIX)
        .is_some_and(|hash| hash.len() == 64 && hash.bytes().all(is_hex))
}

fn safe_relative(path: &Path) -> bool {
    path.components()
        .all(|part| matches!(part, Component::Normal(_) | Component::CurDir))
}

fn unpack(file: &Path, staging: &Path) -> Result<()> {
    let mut archive = Archive::new(GzDecoder::new(BufReader::new(File::open(file)?)));
    for entry in archive.entries()? {
        let mut entry = entry?;
        let path = entry.path()?.into_owned();
        if !safe_relative(&path) {
            bail!("archive entry escapes the archive: {}", path.display());
        }
        match entry.header().entry_type() {
            EntryType::Regular | EntryType::Directory => {}
            other => bail!(
                "archive entry {} has unsupported type {other:?}",
                path.display()
            ),
        }
        if !entry.unpack_in(staging)? {
            bail!("archive entry escapes the archive: {}", path.display());
        }
    }
    Ok(())
}

fn verify_checksums(staging: &Path, manifest: &Json) -> Result<()> {
    let files = manifest
        .get("files")
        .and_then(|v| v.as_array())
        .context("manifest has no checksums")?;
    let mut listed = HashSet::new();
    for file in files {
        let rel = file.get("path").and_then(|v| v.as_str()).unwrap_or("");
        if rel == "manifest.json" {
            continue;
        }
        let rel_path = Path::new(rel);
        if rel.is_empty() || !safe_relative(rel_path) {
            bail!("manifest lists an unsafe path: {rel}");
        }
        let expected = file.get("sha256").and_then(|v| v.as_str()).unwrap_or("");
        let actual = sha256_file(&staging.join(rel_path))
            .with_context(|| format!("missing archived file {rel}"))?;
        if actual != expected {
            bail!("checksum mismatch for {rel}");
        }
        listed.insert(staging.join(rel_path));
    }
    for entry in walkdir::WalkDir::new(staging) {
        let entry = entry?;
        if entry.file_type().is_file()
            && entry.file_name() != "manifest.json"
            && !listed.contains(entry.path())
        {
            bail!(
                "archive contains a file the manifest does not list: {}",
                entry.path().strip_prefix(staging)?.display()
            );
        }
    }
    Ok(())
}

struct ArchivedRow {
    key: String,
    value: Value,
}

fn read_lines(path: &Path, mut visit: impl FnMut(Json) -> Result<()>) -> Result<()> {
    if !path.exists() {
        return Ok(());
    }
    for line in BufReader::new(File::open(path)?).lines() {
        let line = line?;
        if line.trim().is_empty() {
            continue;
        }
        visit(serde_json::from_str(&line).context("corrupt archive line")?)?;
    }
    Ok(())
}

fn archived_row(json: &Json) -> Result<ArchivedRow> {
    Ok(ArchivedRow {
        key: json
            .get("key")
            .and_then(|v| v.as_str())
            .context("row without key")?
            .to_string(),
        value: decode_value(json.get("value").context("row without value")?)?,
    })
}

enum ArchivedHeader {
    Row(HeaderRow),
    Legacy(Json),
}

impl ArchivedHeader {
    fn id(&self) -> Option<String> {
        match self {
            Self::Row(row) => row.text("composerId").map(str::to_string),
            Self::Legacy(json) => json
                .get("composerId")
                .and_then(|v| v.as_str())
                .map(str::to_string),
        }
    }
}

fn archived_header(json: Json) -> Result<ArchivedHeader> {
    if let Some(legacy) = json.get("legacy") {
        return Ok(ArchivedHeader::Legacy(legacy.clone()));
    }
    let columns = json
        .get("columns")
        .and_then(|v| v.as_array())
        .context("header without columns")?;
    let mut row = Vec::new();
    for column in columns {
        let name = column
            .get(0)
            .and_then(|v| v.as_str())
            .context("header column without name")?;
        if !name
            .bytes()
            .all(|byte| byte.is_ascii_alphanumeric() || byte == b'_')
        {
            bail!("invalid header column name {name}");
        }
        row.push((
            name.to_string(),
            decode_value(column.get(1).context("header column without value")?)?,
        ));
    }
    Ok(ArchivedHeader::Row(HeaderRow { columns: row }))
}

fn legacy_header(value: &Json) -> Result<HeaderRow> {
    let text = |field: &str| {
        value
            .get(field)
            .and_then(|v| v.as_str())
            .map(|v| Value::Text(v.to_string()))
            .unwrap_or(Value::Null)
    };
    let int = |field: &str| {
        value
            .get(field)
            .and_then(|v| v.as_i64())
            .map(Value::Integer)
            .unwrap_or(Value::Null)
    };
    Ok(HeaderRow {
        columns: vec![
            ("composerId".to_string(), text("composerId")),
            ("workspaceId".to_string(), text("workspaceId")),
            ("createdAt".to_string(), int("createdAt")),
            ("lastUpdatedAt".to_string(), int("lastUpdatedAt")),
            (
                "isArchived".to_string(),
                Value::Integer(
                    value
                        .get("isArchived")
                        .and_then(|v| v.as_i64())
                        .unwrap_or(0),
                ),
            ),
            (
                "isSubagent".to_string(),
                Value::Integer(
                    value
                        .get("isSubagent")
                        .and_then(|v| v.as_i64())
                        .unwrap_or(0),
                ),
            ),
            ("recency".to_string(), int("recency")),
            ("checkpointAt".to_string(), int("checkpointAt")),
            (
                "value".to_string(),
                Value::Text(
                    value
                        .get("value")
                        .and_then(|v| v.as_str())
                        .unwrap_or("{}")
                        .to_string(),
                ),
            ),
            ("subagentTypeName".to_string(), text("subagentTypeName")),
        ],
    })
}

struct Source {
    id: String,
    kind: Kind,
    uri: Option<String>,
    path: Option<PathBuf>,
    slug: Option<String>,
}

fn source_of(manifest: &Json, staging: &Path) -> Result<Source> {
    let version = manifest
        .get("version")
        .and_then(|v| v.as_u64())
        .unwrap_or(0);
    let (id, uri, slug, kind) = match version {
        1 => {
            let id = manifest
                .get("workspaceId")
                .and_then(|v| v.as_str())
                .context("manifest missing workspaceId")?
                .to_string();
            let slug = fs::read_dir(staging.join("projects"))
                .ok()
                .and_then(|mut entries| entries.next())
                .and_then(|entry| entry.ok())
                .map(|entry| entry.file_name().to_string_lossy().to_string());
            (id, None, slug, None)
        }
        2 => {
            let workspace = manifest
                .get("workspace")
                .context("manifest missing workspace")?;
            (
                workspace
                    .get("id")
                    .and_then(|v| v.as_str())
                    .context("manifest missing workspace id")?
                    .to_string(),
                workspace
                    .get("uri")
                    .and_then(|v| v.as_str())
                    .map(str::to_string),
                workspace
                    .get("slug")
                    .and_then(|v| v.as_str())
                    .map(str::to_string),
                workspace
                    .get("kind")
                    .and_then(|v| v.as_str())
                    .map(str::to_string),
            )
        }
        other => bail!("unsupported archive version {other}"),
    };
    if !valid_workspace_id(&id) {
        bail!("archive has an invalid workspace id {id:?}");
    }
    if let Some(slug) = &slug
        && !valid_slug(slug)
    {
        bail!("archive has an invalid project slug {slug:?}");
    }
    let dir = staging.join("workspaceStorage").join(&id);
    let uri = match uri {
        Some(uri) => Some(uri),
        None => crate::cursor::workspace::read_workspace_target_uri(&dir)?,
    };
    let path = uri
        .as_deref()
        .and_then(uri_path)
        .map(|path| normalize_path(&path));
    let kind = match kind.as_deref() {
        Some("code-workspace") => Kind::CodeWorkspace,
        Some("unsaved") => Kind::Unsaved,
        Some("empty-window") => Kind::EmptyWindow,
        Some("remote") => Kind::Remote,
        _ if path
            .as_deref()
            .is_some_and(crate::cursor::workspace::is_workspace_file) =>
        {
            Kind::CodeWorkspace
        }
        _ => Kind::Folder,
    };
    Ok(Source {
        id,
        kind,
        uri,
        path,
        slug,
    })
}

fn legacy_v1_rows(
    staging: &Path,
    headers: &mut Vec<ArchivedHeader>,
    rows: &mut Vec<ArchivedRow>,
) -> Result<()> {
    let path = staging.join("composer-headers.json");
    if path.exists() {
        let items: Vec<Json> = serde_json::from_str(&fs::read_to_string(path)?)?;
        for item in items {
            headers.push(ArchivedHeader::Row(legacy_header(&item)?));
        }
    }
    let path = staging.join("cursor-disk.json");
    if path.exists() {
        let items: Vec<Json> = serde_json::from_str(&fs::read_to_string(path)?)?;
        for item in items {
            rows.push(ArchivedRow {
                key: item
                    .get("key")
                    .and_then(|v| v.as_str())
                    .context("row without key")?
                    .to_string(),
                value: item
                    .get("value")
                    .and_then(|v| v.as_str())
                    .map(|text| Value::Text(text.to_string()))
                    .unwrap_or(Value::Null),
            });
        }
    }
    Ok(())
}

pub fn import_archive(
    rt: &Runtime,
    file: &Path,
    dest: Option<&Path>,
    overwrite: bool,
) -> Result<Report> {
    rt.check()?;
    let staging = tempfile::tempdir()?;
    unpack(file, staging.path())?;
    let manifest: Json = serde_json::from_str(
        &fs::read_to_string(staging.path().join("manifest.json"))
            .context("archive has no manifest")?,
    )?;
    verify_checksums(staging.path(), &manifest)?;
    let source = source_of(&manifest, staging.path())?;
    let version = manifest
        .get("version")
        .and_then(|v| v.as_u64())
        .unwrap_or(0);

    let mut headers = Vec::new();
    let mut legacy_rows = Vec::new();
    if version == 1 {
        legacy_v1_rows(staging.path(), &mut headers, &mut legacy_rows)?;
    } else {
        read_lines(&staging.path().join("headers.jsonl"), |line| {
            headers.push(archived_header(line)?);
            Ok(())
        })?;
    }
    let mut chat_ids = Vec::new();
    for header in &headers {
        let id = header.id().context("archived header without composerId")?;
        if !valid_chat_id(&id) {
            bail!("archive has an invalid chat id {id:?}");
        }
        chat_ids.push(id);
    }
    let archived: HashSet<String> = chat_ids.iter().cloned().collect();
    let rows_path = staging.path().join("rows.jsonl");
    let blobs_path = staging.path().join("blobs.jsonl");
    let mut row_bytes = 0u64;
    let mut check_row = |row: &ArchivedRow| -> Result<()> {
        match chat_of_key(&row.key) {
            Some(id) if archived.contains(id) => {}
            _ => bail!(
                "archive row {} does not belong to an archived chat",
                row.key
            ),
        }
        row_bytes += super::sql::size(&row.value);
        Ok(())
    };
    for row in &legacy_rows {
        check_row(row)?;
    }
    read_lines(&rows_path, |line| check_row(&archived_row(&line)?))?;
    read_lines(&blobs_path, |line| {
        let row = archived_row(&line)?;
        if !valid_blob_key(&row.key) {
            bail!("archive blob has an invalid key {}", row.key);
        }
        row_bytes += super::sql::size(&row.value);
        Ok(())
    })?;

    let target = match dest {
        Some(dest) => {
            let dest = normalize_path(dest);
            if !dest.exists() {
                bail!("import destination does not exist: {}", dest.display());
            }
            let mut planned = Workspace::planned(&rt.layout, &dest)?;
            if let Some(existing) = super::find_workspace(rt, &planned.id)? {
                planned = existing;
            }
            planned
        }
        None => {
            let id = source.id.clone();
            super::find_workspace(rt, &id)?.unwrap_or(Workspace {
                dir: rt.layout.workspace_storage().join(&id),
                id,
                kind: source.kind.clone(),
                uri: source.uri.clone(),
                path: source.path.clone(),
                profile: "default".to_string(),
                destination_missing: false,
            })
        }
    };
    if !valid_workspace_id(&target.id) {
        bail!("invalid target workspace id {:?}", target.id);
    }
    let target_exists = target.dir.exists();
    let conn = open_global_ro(rt)?.with_context(|| {
        format!(
            "Cursor global database not found: {}",
            rt.layout.global_db().display()
        )
    })?;
    let mut existing = Vec::new();
    for id in &chat_ids {
        if crate::cursor::registry::load_header(&conn, id)?.is_some()
            || db::read_value(&conn, Table::Disk, &format!("composerData:{id}"))?.is_some()
        {
            existing.push(id.clone());
        }
    }
    drop(conn);
    let mut report = Report::default();
    let skip: HashSet<String> = if overwrite {
        HashSet::new()
    } else {
        existing.iter().cloned().collect()
    };
    for id in &existing {
        if overwrite {
            report
                .warnings
                .push(format!("replacing existing chat {id}"));
        } else {
            report.warnings.push(format!(
                "skipped {id}: chat already exists (pass --overwrite to replace)"
            ));
            report.skipped.push(id.clone());
        }
    }
    let mut needs = SpaceNeeds::default();
    needs.add(
        &rt.layout.global_storage(),
        row_bytes.saturating_mul(2),
        "database log",
    );
    if !target_exists {
        needs.add(
            &rt.layout.workspace_storage(),
            super::fsops::dir_size(&staging.path().join("workspaceStorage").join(&source.id))?,
            "workspace storage",
        );
    }
    needs.check()?;
    let imported: Vec<String> = chat_ids
        .iter()
        .filter(|id| !skip.contains(*id))
        .cloned()
        .collect();
    if rt.dry_run {
        report.applied.push(format!(
            "dry-run import {} chats into {}{}",
            imported.len(),
            target.id,
            if target_exists {
                " (existing workspace)"
            } else {
                ""
            }
        ));
        return Ok(report);
    }

    let mut entries: Vec<(Replacement, Boundary)> = Vec::new();
    if let (Some(old), Some(new)) = (&source.path, &target.path)
        && old != new
    {
        for item in path_replacements(
            Platform::current(),
            &old.to_string_lossy(),
            &new.to_string_lossy(),
        ) {
            entries.push((item, Boundary::Path));
        }
    }
    if let (Some(old), Some(new)) = (&source.uri, &target.uri) {
        entries.push((Replacement::new(old, new), Boundary::Path));
    }
    entries.push((Replacement::new(&source.id, &target.id), Boundary::Token));
    let rewriter = Rewriter::build(entries);
    let identity = target.identity();
    let wanted: HashSet<String> = imported.iter().cloned().collect();
    let staging_dir = staging.path().to_path_buf();
    let replaced: Vec<String> = existing
        .iter()
        .filter(|id| overwrite && wanted.contains(*id))
        .cloned()
        .collect();

    session::run(
        rt,
        "import",
        |session| {
            session.step()?;
            if !target_exists {
                session.journal.created(&target.dir)?;
                let packed = staging_dir.join("workspaceStorage").join(&source.id);
                if packed.is_dir() {
                    copy_tree(&packed, &target.dir)?;
                } else {
                    fs::create_dir_all(&target.dir)?;
                }
                if target.uri.is_some() {
                    write_atomic(
                        &target.dir.join("workspace.json"),
                        serde_json::to_string_pretty(&target.workspace_json()?)?.as_bytes(),
                    )?;
                }
                if !target.dir.join("state.vscdb").exists() {
                    local::create_local_db(&target.dir)?;
                }
            }
            session.step()?;
            {
                let mut writer = session.db()?;
                writer.delete_chats(&replaced)?;
                let t = Target {
                    id: &target.id,
                    identity: &identity,
                };
                let write_row = |writer: &mut db::Writer<'_>, row: ArchivedRow| -> Result<()> {
                    let Some(id) = chat_of_key(&row.key) else {
                        return Ok(());
                    };
                    if !wanted.contains(id) {
                        return Ok(());
                    }
                    let value = match super::sql::text(&row.value) {
                        Some(text) => {
                            let rewritten = rewriter.rewrite(text).into_owned();
                            let rewritten = if row.key.starts_with("composerData:") {
                                db::with_identity(&rewritten, t.identity, false)
                                    .unwrap_or(rewritten)
                            } else {
                                rewritten
                            };
                            super::sql::like(&row.value, rewritten)
                        }
                        None => row.value.clone(),
                    };
                    writer.put(Table::Disk, &row.key, &value)
                };
                for row in legacy_rows.drain(..) {
                    write_row(&mut writer, row)?;
                }
                read_lines(&staging_dir.join("rows.jsonl"), |line| {
                    write_row(&mut writer, archived_row(&line)?)
                })?;
                read_lines(&staging_dir.join("blobs.jsonl"), |line| {
                    let row = archived_row(&line)?;
                    if db::read_value(writer.conn(), Table::Disk, &row.key)?.is_none() {
                        writer.put(Table::Disk, &row.key, &row.value)?;
                    }
                    Ok(())
                })?;
                let table = composer_headers_table(writer.conn())?;
                let mut legacy_entries = Vec::new();
                for header in headers.drain(..) {
                    let Some(id) = header.id() else { continue };
                    if !wanted.contains(&id) {
                        continue;
                    }
                    let mut row = match header {
                        ArchivedHeader::Row(row) => row,
                        ArchivedHeader::Legacy(json) => legacy_header(&json)?,
                    };
                    row.set("workspaceId", Value::Text(target.id.clone()));
                    if let Some(value) = row.text("value").map(str::to_string) {
                        let rewritten = rewriter.rewrite(&value).into_owned();
                        let rewritten =
                            db::with_identity(&rewritten, &identity, true).unwrap_or(rewritten);
                        row.set("value", Value::Text(rewritten));
                    }
                    if table {
                        writer.put_header(&row)?;
                    } else {
                        let mut entry: Json = row
                            .text("value")
                            .and_then(|value| serde_json::from_str(value).ok())
                            .unwrap_or_else(|| json!({}));
                        if let Some(object) = entry.as_object_mut() {
                            object.insert("composerId".to_string(), Json::String(id.clone()));
                            object.insert("workspaceIdentifier".to_string(), identity.clone());
                        }
                        legacy_entries.push(entry);
                    }
                }
                writer.upsert_legacy_headers(legacy_entries)?;
            }
            session.step()?;
            if target_exists {
                let packed = staging_dir.join("workspaceStorage").join(&source.id);
                let map: HashMap<String, String> =
                    imported.iter().map(|id| (id.clone(), id.clone())).collect();
                local::transfer_selection(
                    &mut session.journal,
                    &packed,
                    &target.dir,
                    &imported,
                    Some(&map),
                    false,
                )?;
            }
            session.step()?;
            if let (Some(from), Some(to)) = (&source.slug, target.slug()) {
                let from = staging_dir.join("projects").join(from);
                let to = session.rt.layout.projects_dir.join(to);
                let map: HashMap<String, String> =
                    imported.iter().map(|id| (id.clone(), id.clone())).collect();
                local::copy_project(&mut session.journal, &from, &to, &map, true)?;
            }
            Ok(())
        },
        |session, ()| {
            let conn = session.conn()?;
            for id in &imported {
                if composer_headers_table(conn)? {
                    let header = crate::cursor::registry::load_header(conn, id)?
                        .with_context(|| format!("verify failed: chat {id} was not imported"))?;
                    if header.workspace_id != target.id {
                        bail!(
                            "verify failed: chat {id} is owned by {}",
                            header.workspace_id
                        );
                    }
                }
            }
            if target.uri.is_some() && !target_exists {
                let raw = fs::read_to_string(target.dir.join("workspace.json"))?;
                if serde_json::from_str::<Json>(&raw)? != target.workspace_json()? {
                    bail!("verify failed: workspace.json does not point at the import target");
                }
            }
            Ok(())
        },
    )?;
    report.applied.push(format!(
        "imported {} chats into {}",
        imported.len(),
        target.id
    ));
    Ok(report)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn validates_ids_strictly() {
        assert!(valid_workspace_id("5af0e30872454aba2290760e07cae157"));
        assert!(valid_workspace_id("1765558213752"));
        assert!(valid_workspace_id("1b68e383-f36e-463a-afc8-5c55ed6519bd"));
        for bad in [
            "../../etc",
            "a/b",
            "",
            "empty-window",
            "..",
            "5af0e308/2454aba2290760e07cae15",
        ] {
            assert!(!valid_workspace_id(bad), "{bad}");
        }
        assert!(valid_chat_id("1b68e383-f36e-463a-afc8-5c55ed6519bd"));
        assert!(!valid_chat_id("../1b68e383-f36e-463a-afc8-5c55ed6519"));
        assert!(!valid_slug(".."));
        assert!(!valid_slug("a/b"));
        assert!(valid_slug("Users-me-app"));
    }

    #[test]
    fn finds_hex_and_protobuf_blob_refs() {
        let hash = [7u8; 32];
        let mut proto = vec![0x0a, 0x20];
        proto.extend_from_slice(&hash);
        let nested = {
            let mut inner = vec![0x0a, 0x20];
            inner.extend_from_slice(&[9u8; 32]);
            let mut outer = vec![0x42, inner.len() as u8];
            outer.extend(inner);
            outer
        };
        proto.extend(nested);
        let state = format!("~{}", STANDARD.encode(&proto));
        let text = json!({"conversationState": state, "other": "a".repeat(64)}).to_string();
        let mut refs = Vec::new();
        collect_refs("composerData:x", &Value::Text(text), &mut refs);
        assert!(refs.contains(&hex(&hash)));
        assert!(refs.contains(&hex(&[9u8; 32])));
        assert!(refs.contains(&"a".repeat(64)));
        let mut none = Vec::new();
        collect_refs("bubbleId:x:y", &Value::Text("b".repeat(65)), &mut none);
        assert!(none.is_empty());
    }
}

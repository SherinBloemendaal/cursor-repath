//! Indexed global database reads and chat ownership changes.

use anyhow::{Context, Result};
use rusqlite::{Connection, OpenFlags, OptionalExtension, params};
use serde_json::{Map, Value};
use std::collections::{HashMap, HashSet};

use crate::cursor::registry::{
    COMPOSER_EXACT_PREFIXES, COMPOSER_PREFIXES, ComposerHeader, LEGACY_HEADERS_KEY,
    composer_headers_table, ensure_global_schema, load_headers_for_workspace,
};
use crate::cursor::rewrite::{Replacement, contains_scoped, key_range_end, replace_scoped};
use crate::cursor::sqlite_value::Utf8SqlValue;

pub const PLAN_KEYS: &[&str] = &["composer.planRegistry", "composer.planRedirects"];

#[derive(Debug, Clone)]
pub struct KvRow {
    pub key: String,
    pub value: Option<Utf8SqlValue>,
}

pub fn open_rw(path: &std::path::Path) -> Result<Connection> {
    let conn =
        Connection::open(path).with_context(|| format!("failed to open {}", path.display()))?;
    conn.pragma_update(None, "foreign_keys", "OFF")?;
    Ok(conn)
}

pub fn open_ro(path: &std::path::Path) -> Result<Connection> {
    let conn = Connection::open_with_flags(
        path,
        OpenFlags::SQLITE_OPEN_READ_ONLY | OpenFlags::SQLITE_OPEN_NO_MUTEX,
    )
    .with_context(|| format!("failed to open {} read-only", path.display()))?;
    conn.pragma_update(None, "query_only", "ON")?;
    Ok(conn)
}

pub fn composer_ids_for_workspace(conn: &Connection, workspace_id: &str) -> Result<Vec<String>> {
    Ok(load_headers_for_workspace(conn, workspace_id)?
        .into_iter()
        .map(|header| header.composer_id)
        .collect())
}

pub fn rewrite_workspace(
    conn: &Connection,
    workspace_id: &str,
    replacements: &[Replacement],
    old_hash: &str,
    new_hash: &str,
    dry_run: bool,
) -> Result<usize> {
    let ids = composer_ids_for_workspace(conn, workspace_id)?;
    let mut touched = 0usize;
    let tx = conn.unchecked_transaction()?;
    for id in &ids {
        touched += rewrite_composer_rows(&tx, id, replacements, dry_run)?;
    }
    touched += rekey_prefix(
        &tx,
        &format!("inlineDiff:{old_hash}:"),
        new_hash,
        old_hash,
        replacements,
        dry_run,
    )?;
    touched += rekey_prefix(
        &tx,
        &format!("patch-graph:{old_hash}:"),
        new_hash,
        old_hash,
        replacements,
        dry_run,
    )?;
    touched += rewrite_plan_keys(&tx, replacements, dry_run)?;
    if !dry_run && old_hash != new_hash {
        retarget_headers(&tx, workspace_id, new_hash, replacements)?;
    } else if !dry_run {
        rewrite_header_values(&tx, workspace_id, replacements)?;
    }
    if dry_run {
        tx.rollback().ok();
    } else {
        tx.commit().context("failed to commit workspace rewrite")?;
    }
    Ok(touched)
}

fn rewrite_composer_rows(
    conn: &Connection,
    composer_id: &str,
    replacements: &[Replacement],
    dry_run: bool,
) -> Result<usize> {
    let mut count = 0usize;
    for prefix in COMPOSER_EXACT_PREFIXES {
        let key = format!("{prefix}{composer_id}");
        count += rewrite_exact_key(conn, &key, replacements, dry_run)?;
    }
    for prefix in COMPOSER_PREFIXES {
        let start = format!("{prefix}{composer_id}:");
        let end = key_range_end(&start);
        let rows = scan_range(conn, &start, &end)?;
        for row in rows {
            if row.key.starts_with("agentKv:") {
                continue;
            }
            count += write_row_value(conn, &row, replacements, dry_run)?;
        }
    }
    Ok(count)
}

fn rewrite_exact_key(
    conn: &Connection,
    key: &str,
    replacements: &[Replacement],
    dry_run: bool,
) -> Result<usize> {
    let Some(row) = read_row(conn, key)? else {
        return Ok(0);
    };
    write_row_value(conn, &row, replacements, dry_run)
}

fn write_row_value(
    conn: &Connection,
    row: &KvRow,
    replacements: &[Replacement],
    dry_run: bool,
) -> Result<usize> {
    let Some(value) = &row.value else {
        return Ok(0);
    };
    let updated = replace_scoped(value.as_str(), replacements);
    if updated == value.as_str() {
        return Ok(0);
    }
    if dry_run {
        return Ok(1);
    }
    match value {
        Utf8SqlValue::Text(_) => {
            conn.execute(
                "UPDATE cursorDiskKV SET value = ?1 WHERE key = ?2",
                params![updated, row.key],
            )?;
        }
        Utf8SqlValue::Blob(_) => {
            conn.execute(
                "UPDATE cursorDiskKV SET value = ?1 WHERE key = ?2",
                params![updated.as_bytes(), row.key],
            )?;
        }
    }
    Ok(1)
}

fn rekey_prefix(
    conn: &Connection,
    start_prefix: &str,
    new_hash: &str,
    old_hash: &str,
    replacements: &[Replacement],
    dry_run: bool,
) -> Result<usize> {
    let end = key_range_end(start_prefix);
    let rows = scan_range(conn, start_prefix, &end)?;
    let mut count = 0usize;
    for row in rows {
        let Some(rest) = row.key.strip_prefix(start_prefix) else {
            continue;
        };
        let family = start_prefix.split(':').next().unwrap_or("inlineDiff");
        let new_key = format!("{family}:{new_hash}:{rest}");
        let new_value = row
            .value
            .as_ref()
            .map(|value| replace_scoped(value.as_str(), replacements));
        let changed = new_key != row.key
            || new_value.as_deref() != row.value.as_ref().map(Utf8SqlValue::as_str);
        if !changed {
            continue;
        }
        count += 1;
        if dry_run {
            continue;
        }
        if new_key != row.key {
            conn.execute("DELETE FROM cursorDiskKV WHERE key = ?1", params![new_key])?;
            conn.execute(
                "UPDATE cursorDiskKV SET key = ?1 WHERE key = ?2",
                params![new_key, row.key],
            )?;
        }
        if let Some(text) = new_value {
            let blob = matches!(row.value, Some(Utf8SqlValue::Blob(_)));
            if blob {
                conn.execute(
                    "UPDATE cursorDiskKV SET value = ?1 WHERE key = ?2",
                    params![text.as_bytes(), new_key],
                )?;
            } else {
                conn.execute(
                    "UPDATE cursorDiskKV SET value = ?1 WHERE key = ?2",
                    params![text, new_key],
                )?;
            }
        }
        let _ = old_hash;
    }
    Ok(count)
}

fn rewrite_plan_keys(
    conn: &Connection,
    replacements: &[Replacement],
    dry_run: bool,
) -> Result<usize> {
    if !crate::cursor::registry::table_exists(conn, "ItemTable")? {
        return Ok(0);
    }
    let mut count = 0usize;
    for key in PLAN_KEYS {
        let Some(raw) = item_value(conn, key)? else {
            continue;
        };
        let updated = replace_scoped(&raw, replacements);
        if updated == raw {
            continue;
        }
        count += 1;
        if !dry_run {
            conn.execute(
                "UPDATE ItemTable SET value = ?1 WHERE key = ?2",
                params![updated, key],
            )?;
        }
    }
    Ok(count)
}

fn retarget_headers(
    conn: &Connection,
    old_workspace: &str,
    new_workspace: &str,
    replacements: &[Replacement],
) -> Result<()> {
    if composer_headers_table(conn)? {
        let headers = load_headers_for_workspace(conn, old_workspace)?;
        for header in headers {
            let value = rewrite_header_json(&header.value, new_workspace, replacements);
            conn.execute(
                "UPDATE composerHeaders SET workspaceId = ?1, value = ?2 WHERE composerId = ?3",
                params![new_workspace, value, header.composer_id],
            )?;
        }
        return Ok(());
    }
    rewrite_legacy_headers(conn, old_workspace, new_workspace, replacements)
}

fn rewrite_header_values(
    conn: &Connection,
    workspace_id: &str,
    replacements: &[Replacement],
) -> Result<()> {
    if !composer_headers_table(conn)? {
        return Ok(());
    }
    let headers = load_headers_for_workspace(conn, workspace_id)?;
    for header in headers {
        let value = replace_scoped(&header.value, replacements);
        if value == header.value {
            continue;
        }
        conn.execute(
            "UPDATE composerHeaders SET value = ?1 WHERE composerId = ?2",
            params![value, header.composer_id],
        )?;
    }
    Ok(())
}

pub fn rewrite_header_json(
    value: &str,
    new_workspace: &str,
    replacements: &[Replacement],
) -> String {
    let mut json: Value = serde_json::from_str(value).unwrap_or(Value::String(value.to_string()));
    if let Some(object) = json.as_object_mut()
        && let Some(ident) = object
            .get_mut("workspaceIdentifier")
            .and_then(|v| v.as_object_mut())
    {
        ident.insert("id".to_string(), Value::String(new_workspace.to_string()));
        rewrite_identifier_paths(ident, replacements);
    }
    let rendered = json.to_string();
    replace_scoped(&rendered, replacements)
}

fn rewrite_identifier_paths(ident: &mut Map<String, Value>, replacements: &[Replacement]) {
    for key in ["uri", "configPath", "folderUri"] {
        if let Some(Value::String(text)) = ident.get_mut(key) {
            *text = replace_scoped(text, replacements);
        }
    }
    if let Some(uri) = ident.get_mut("uri").and_then(|v| v.as_object_mut()) {
        for key in ["external", "path", "fsPath"] {
            if let Some(Value::String(text)) = uri.get_mut(key) {
                *text = replace_scoped(text, replacements);
            }
        }
    }
}

fn rewrite_legacy_headers(
    conn: &Connection,
    old_workspace: &str,
    new_workspace: &str,
    replacements: &[Replacement],
) -> Result<()> {
    let Some(raw) = item_value(conn, LEGACY_HEADERS_KEY)? else {
        return Ok(());
    };
    let mut json: Value = serde_json::from_str(&raw).unwrap_or(Value::Null);
    let Some(composers) = json.get_mut("allComposers").and_then(|v| v.as_array_mut()) else {
        return Ok(());
    };
    for composer in composers {
        let id = composer
            .pointer("/workspaceIdentifier/id")
            .and_then(|v| v.as_str())
            .unwrap_or("");
        if id != old_workspace {
            continue;
        }
        if let Some(ident) = composer
            .get_mut("workspaceIdentifier")
            .and_then(|v| v.as_object_mut())
        {
            ident.insert("id".to_string(), Value::String(new_workspace.to_string()));
            rewrite_identifier_paths(ident, replacements);
        }
    }
    conn.execute(
        "UPDATE ItemTable SET value = ?1 WHERE key = ?2",
        params![json.to_string(), LEGACY_HEADERS_KEY],
    )?;
    Ok(())
}

pub fn expand_chat_ids(conn: &Connection, seeds: &[String]) -> Result<Vec<String>> {
    let mut seen = HashSet::new();
    let mut ordered = Vec::new();
    let mut queue = seeds.to_vec();
    while let Some(id) = queue.pop() {
        if !seen.insert(id.clone()) {
            continue;
        }
        if let Some(raw) = read_text(conn, &format!("composerData:{id}"))?
            && let Ok(json) = serde_json::from_str::<Value>(&raw)
        {
            for field in ["subComposerIds", "subagentComposerIds"] {
                if let Some(items) = json.get(field).and_then(|v| v.as_array()) {
                    for item in items {
                        if let Some(child) = item.as_str() {
                            queue.push(child.to_string());
                        }
                    }
                }
            }
        }
        ordered.push(id);
    }
    Ok(ordered)
}

pub fn clone_chats(
    conn: &Connection,
    ids: &[String],
    target_workspace: &str,
    replacements: &[Replacement],
) -> Result<HashMap<String, String>> {
    let mut map = HashMap::new();
    for id in ids {
        map.insert(id.clone(), uuid::Uuid::new_v4().to_string());
    }
    let id_reps: Vec<Replacement> = map
        .iter()
        .map(|(old, new)| Replacement::new(old, new))
        .collect();
    let mut ordered: Vec<&Replacement> = id_reps.iter().collect();
    ordered.sort_by_key(|item| std::cmp::Reverse(item.from.len()));
    let id_reps: Vec<Replacement> = ordered.into_iter().cloned().collect();

    for (old, new) in &map {
        clone_rows_for(conn, old, new, &id_reps, replacements)?;
        clone_header(conn, old, new, target_workspace, &id_reps, replacements)?;
    }
    Ok(map)
}

fn clone_rows_for(
    conn: &Connection,
    old: &str,
    new: &str,
    id_reps: &[Replacement],
    path_reps: &[Replacement],
) -> Result<()> {
    let mut rows = Vec::new();
    for prefix in COMPOSER_EXACT_PREFIXES {
        let key = format!("{prefix}{old}");
        if let Some(row) = read_row(conn, &key)? {
            rows.push(row);
        }
    }
    for prefix in COMPOSER_PREFIXES {
        let start = format!("{prefix}{old}:");
        let end = key_range_end(&start);
        rows.extend(scan_range(conn, &start, &end)?);
    }
    for row in rows {
        if row.key.starts_with("agentKv:") {
            continue;
        }
        let new_key = retarget_composer_key(&row.key, old, new);
        let new_value = row.value.as_ref().map(|value| {
            let with_ids = replace_scoped(value.as_str(), id_reps);
            replace_scoped(&with_ids, path_reps)
        });
        insert_kv(
            conn,
            &new_key,
            new_value.as_deref(),
            matches!(row.value, Some(Utf8SqlValue::Blob(_))),
        )?;
    }
    Ok(())
}

fn clone_header(
    conn: &Connection,
    old: &str,
    new: &str,
    target_workspace: &str,
    id_reps: &[Replacement],
    path_reps: &[Replacement],
) -> Result<()> {
    if !composer_headers_table(conn)? {
        return Ok(());
    }
    let header = conn
        .query_row(
            "SELECT workspaceId, createdAt, lastUpdatedAt, isArchived, isSubagent, recency, checkpointAt, value, subagentTypeName \
             FROM composerHeaders WHERE composerId = ?1",
            params![old],
            |row| {
                Ok((
                    row.get::<_, Option<String>>(0)?,
                    row.get::<_, Option<i64>>(1)?,
                    row.get::<_, Option<i64>>(2)?,
                    row.get::<_, Option<i64>>(3)?,
                    row.get::<_, Option<i64>>(4)?,
                    row.get::<_, Option<i64>>(5)?,
                    row.get::<_, Option<i64>>(6)?,
                    row.get::<_, String>(7)?,
                    row.get::<_, Option<String>>(8)?,
                ))
            },
        )
        .optional()?;
    let Some((_, created, updated, archived, subagent, recency, checkpoint, value, type_name)) =
        header
    else {
        return Ok(());
    };
    let rewritten = replace_scoped(&replace_scoped(&value, id_reps), path_reps);
    let rewritten = rewrite_header_json(&rewritten, target_workspace, path_reps);
    let rewritten = replace_scoped(&rewritten, id_reps);
    conn.execute(
        "INSERT INTO composerHeaders (composerId, workspaceId, createdAt, lastUpdatedAt, isArchived, isSubagent, recency, checkpointAt, value, subagentTypeName) \
         VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10)",
        params![
            new,
            target_workspace,
            created,
            updated,
            archived.unwrap_or(0),
            subagent.unwrap_or(0),
            recency,
            checkpoint,
            rewritten,
            type_name
        ],
    )?;
    Ok(())
}

pub fn reassign_chats(
    conn: &Connection,
    ids: &[String],
    source_workspace: &str,
    target_workspace: &str,
    replacements: &[Replacement],
) -> Result<()> {
    if !composer_headers_table(conn)? {
        return Ok(());
    }
    for id in ids {
        let value: Option<String> = conn
            .query_row(
                "SELECT value FROM composerHeaders WHERE composerId = ?1 AND workspaceId = ?2",
                params![id, source_workspace],
                |row| row.get(0),
            )
            .optional()?;
        let Some(value) = value else {
            continue;
        };
        let value = rewrite_header_json(&value, target_workspace, replacements);
        conn.execute(
            "UPDATE composerHeaders SET workspaceId = ?1, value = ?2 WHERE composerId = ?3",
            params![target_workspace, value, id],
        )?;
        if let Some(raw) = read_text(conn, &format!("composerData:{id}"))? {
            let updated = rewrite_header_json(&raw, target_workspace, replacements);
            conn.execute(
                "UPDATE cursorDiskKV SET value = ?1 WHERE key = ?2",
                params![updated, format!("composerData:{id}")],
            )?;
        }
    }
    Ok(())
}

pub fn delete_chats(conn: &Connection, ids: &[String]) -> Result<()> {
    for id in ids {
        if composer_headers_table(conn)? {
            conn.execute(
                "DELETE FROM composerHeaders WHERE composerId = ?1",
                params![id],
            )?;
        }
        for prefix in COMPOSER_EXACT_PREFIXES {
            conn.execute(
                "DELETE FROM cursorDiskKV WHERE key = ?1",
                params![format!("{prefix}{id}")],
            )?;
        }
        for prefix in COMPOSER_PREFIXES {
            let start = format!("{prefix}{id}:");
            let end = key_range_end(&start);
            conn.execute(
                "DELETE FROM cursorDiskKV WHERE key >= ?1 AND key < ?2",
                params![start, end],
            )?;
        }
    }
    Ok(())
}

pub fn owned_ids(conn: &Connection, workspace_id: &str) -> Result<HashSet<String>> {
    Ok(load_headers_for_workspace(conn, workspace_id)?
        .into_iter()
        .map(|header| header.composer_id)
        .collect())
}

pub fn headers(conn: &Connection, workspace_id: &str) -> Result<Vec<ComposerHeader>> {
    load_headers_for_workspace(conn, workspace_id)
}

pub fn read_text(conn: &Connection, key: &str) -> Result<Option<String>> {
    Ok(read_row(conn, key)?.and_then(|row| row.value.map(|value| value.as_str().to_string())))
}

fn read_row(conn: &Connection, key: &str) -> Result<Option<KvRow>> {
    if !crate::cursor::registry::table_exists(conn, "cursorDiskKV")? {
        return Ok(None);
    }
    let mut stmt = conn.prepare("SELECT key, value FROM cursorDiskKV WHERE key = ?1")?;
    let mut rows = stmt.query(params![key])?;
    let Some(row) = rows.next()? else {
        return Ok(None);
    };
    Ok(Some(KvRow {
        key: row.get(0)?,
        value: Utf8SqlValue::from_row(row, 1)?,
    }))
}

fn scan_range(conn: &Connection, start: &str, end: &str) -> Result<Vec<KvRow>> {
    if !crate::cursor::registry::table_exists(conn, "cursorDiskKV")? {
        return Ok(Vec::new());
    }
    let mut stmt =
        conn.prepare("SELECT key, value FROM cursorDiskKV WHERE key >= ?1 AND key < ?2")?;
    let mut rows = stmt.query(params![start, end])?;
    let mut out = Vec::new();
    while let Some(row) = rows.next()? {
        out.push(KvRow {
            key: row.get(0)?,
            value: Utf8SqlValue::from_row(row, 1)?,
        });
    }
    Ok(out)
}

fn insert_kv(conn: &Connection, key: &str, value: Option<&str>, blob: bool) -> Result<()> {
    conn.execute("DELETE FROM cursorDiskKV WHERE key = ?1", params![key])?;
    match value {
        Some(text) if blob => {
            conn.execute(
                "INSERT INTO cursorDiskKV (key, value) VALUES (?1, ?2)",
                params![key, text.as_bytes()],
            )?;
        }
        Some(text) => {
            conn.execute(
                "INSERT INTO cursorDiskKV (key, value) VALUES (?1, ?2)",
                params![key, text],
            )?;
        }
        None => {
            conn.execute(
                "INSERT INTO cursorDiskKV (key, value) VALUES (?1, NULL)",
                params![key],
            )?;
        }
    }
    Ok(())
}

fn item_value(conn: &Connection, key: &str) -> Result<Option<String>> {
    conn.query_row(
        "SELECT value FROM ItemTable WHERE key = ?1",
        params![key],
        |row| row.get(0),
    )
    .optional()
    .context("failed to read ItemTable")
}

pub fn retarget_composer_key(key: &str, old: &str, new: &str) -> String {
    for prefix in COMPOSER_EXACT_PREFIXES
        .iter()
        .chain(COMPOSER_PREFIXES.iter())
    {
        let needle = format!("{prefix}{old}");
        if key == needle || key.starts_with(&format!("{needle}:")) {
            return format!("{prefix}{new}{}", &key[needle.len()..]);
        }
    }
    key.to_string()
}

pub fn local_selected(conn: &Connection) -> Result<(Vec<String>, Vec<String>)> {
    let Some(raw) = item_value(conn, "composer.composerData")? else {
        return Ok((Vec::new(), Vec::new()));
    };
    let json: Value = serde_json::from_str(&raw).unwrap_or(Value::Null);
    Ok((
        string_array(json.get("selectedComposerIds")),
        string_array(json.get("lastFocusedComposerIds")),
    ))
}

pub fn write_local_selected(
    conn: &Connection,
    selected: &[String],
    focused: &[String],
) -> Result<()> {
    let mut json = if let Some(raw) = item_value(conn, "composer.composerData")? {
        serde_json::from_str::<Value>(&raw).unwrap_or(Value::Object(Map::new()))
    } else {
        Value::Object(Map::new())
    };
    if !json.is_object() {
        json = Value::Object(Map::new());
    }
    if let Some(object) = json.as_object_mut() {
        object.insert(
            "selectedComposerIds".to_string(),
            Value::Array(selected.iter().cloned().map(Value::String).collect()),
        );
        object.insert(
            "lastFocusedComposerIds".to_string(),
            Value::Array(focused.iter().cloned().map(Value::String).collect()),
        );
    }
    conn.execute(
        "INSERT INTO ItemTable (key, value) VALUES (?1, ?2) \
         ON CONFLICT(key) DO UPDATE SET value = excluded.value",
        params!["composer.composerData", json.to_string()],
    )?;
    Ok(())
}

fn string_array(value: Option<&Value>) -> Vec<String> {
    value
        .and_then(|v| v.as_array())
        .map(|items| {
            items
                .iter()
                .filter_map(|item| item.as_str().map(str::to_string))
                .collect()
        })
        .unwrap_or_default()
}

pub fn row_contains_old(conn: &Connection, workspace_id: &str, old: &str) -> Result<bool> {
    let ids = composer_ids_for_workspace(conn, workspace_id)?;
    for id in ids {
        for prefix in COMPOSER_EXACT_PREFIXES {
            if let Some(text) = read_text(conn, &format!("{prefix}{id}"))?
                && contains_scoped(&text, old)
            {
                return Ok(true);
            }
        }
        for prefix in COMPOSER_PREFIXES {
            let start = format!("{prefix}{id}:");
            let end = key_range_end(&start);
            for row in scan_range(conn, &start, &end)? {
                if let Some(value) = &row.value
                    && contains_scoped(value.as_str(), old)
                {
                    return Ok(true);
                }
            }
        }
        if composer_headers_table(conn)? {
            let value: Option<String> = conn
                .query_row(
                    "SELECT value FROM composerHeaders WHERE composerId = ?1",
                    params![id],
                    |row| row.get(0),
                )
                .optional()?;
            if let Some(value) = value
                && contains_scoped(&value, old)
            {
                return Ok(true);
            }
        }
    }
    Ok(false)
}

pub fn touched_paths(conn: &Connection, composer_id: &str) -> Result<Vec<String>> {
    let mut paths = Vec::new();
    let start = format!("ofsContent:{composer_id}:");
    let end = key_range_end(&start);
    for row in scan_range(conn, &start, &end)? {
        if let Some(rest) = row.key.strip_prefix(&start) {
            paths.push(rest.to_string());
        }
    }
    if let Some(raw) = read_text(conn, &format!("composerData:{composer_id}"))?
        && let Ok(json) = serde_json::from_str::<Value>(&raw)
    {
        collect_file_paths(&json, &mut paths, true);
    }
    let bubble_start = format!("bubbleId:{composer_id}:");
    let bubble_end = key_range_end(&bubble_start);
    for row in scan_range(conn, &bubble_start, &bubble_end)? {
        if let Some(value) = &row.value
            && let Ok(json) = serde_json::from_str::<Value>(value.as_str())
        {
            collect_file_paths(&json, &mut paths, false);
        }
    }
    Ok(paths)
}

fn collect_file_paths(value: &Value, out: &mut Vec<String>, honor_ignore: bool) {
    match value {
        Value::Object(map) => {
            for (key, child) in map {
                if honor_ignore && key == "trackedGitRepos" {
                    continue;
                }
                if key == "newlyCreatedFiles" || key == "codeBlockData" {
                    collect_file_paths(child, out, false);
                    continue;
                }
                if matches!(
                    key.as_str(),
                    "uri" | "path" | "fsPath" | "external" | "fileUri" | "targetFile"
                ) && let Some(text) = child.as_str()
                {
                    out.push(text.to_string());
                }
                collect_file_paths(child, out, honor_ignore);
            }
        }
        Value::Array(items) => {
            for item in items {
                if let Some(text) = item.as_str() {
                    out.push(text.to_string());
                } else {
                    collect_file_paths(item, out, honor_ignore);
                }
            }
        }
        Value::String(text)
            if text.contains("://") || text.starts_with('/') || text.contains(":\\") =>
        {
            out.push(text.clone());
        }
        Value::String(_) => {}
        _ => {}
    }
}

pub fn ensure_db(path: &std::path::Path) -> Result<Connection> {
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent)?;
    }
    let conn = open_rw(path)?;
    ensure_global_schema(&conn)?;
    Ok(conn)
}

pub fn count_headers(conn: &Connection, workspace_id: &str) -> Result<usize> {
    Ok(load_headers_for_workspace(conn, workspace_id)?.len())
}

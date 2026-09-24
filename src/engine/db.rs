//! Indexed global database reads and journaled chat ownership changes.

use anyhow::{Context, Result};
use rusqlite::types::Value;
use rusqlite::{Connection, OptionalExtension, params, params_from_iter};
use serde_json::Value as Json;
use std::collections::{HashMap, HashSet};

use super::journal::{Journal, RowImage, Table};
use super::sql::{self, like, table_exists};
use crate::cursor::registry::{
    COMPOSER_EXACT_PREFIXES, COMPOSER_PREFIXES, ComposerHeader, LEGACY_HEADERS_KEY, PLAN_KEYS,
    WORKSPACE_PREFIXES, composer_headers_table, load_headers_for_workspace,
};
use crate::cursor::rewrite::{Boundary, Replacement, Rewriter, key_range_end};

pub fn read_value(conn: &Connection, table: Table, key: &str) -> Result<Option<Value>> {
    if !table_exists(conn, table.name())? {
        return Ok(None);
    }
    conn.prepare_cached(&format!(
        "SELECT value FROM {} WHERE key = ?1",
        table.name()
    ))?
    .query_row([key], |row| row.get::<_, Value>(0))
    .optional()
    .with_context(|| format!("failed to read {key}"))
}

pub fn read_text(conn: &Connection, key: &str) -> Result<Option<String>> {
    Ok(read_value(conn, Table::Disk, key)?
        .as_ref()
        .and_then(sql::text)
        .map(str::to_string))
}

pub fn read_item(conn: &Connection, key: &str) -> Result<Option<String>> {
    Ok(read_value(conn, Table::Item, key)?
        .as_ref()
        .and_then(sql::text)
        .map(str::to_string))
}

pub fn keys_in(conn: &Connection, start: &str, end: &str) -> Result<Vec<String>> {
    if !table_exists(conn, "cursorDiskKV")? {
        return Ok(Vec::new());
    }
    let mut stmt =
        conn.prepare_cached("SELECT key FROM cursorDiskKV WHERE key >= ?1 AND key < ?2")?;
    let rows = stmt.query_map(params![start, end], |row| row.get::<_, String>(0))?;
    rows.collect::<Result<Vec<_>, _>>()
        .context("failed to scan cursorDiskKV")
}

pub fn scan(
    conn: &Connection,
    start: &str,
    end: &str,
    mut visit: impl FnMut(&str, &Value) -> Result<()>,
) -> Result<()> {
    if !table_exists(conn, "cursorDiskKV")? {
        return Ok(());
    }
    let mut stmt =
        conn.prepare_cached("SELECT key, value FROM cursorDiskKV WHERE key >= ?1 AND key < ?2")?;
    let mut rows = stmt.query(params![start, end])?;
    while let Some(row) = rows.next()? {
        let key: String = row.get(0)?;
        let value: Value = row.get(1)?;
        visit(&key, &value)?;
    }
    Ok(())
}

pub fn composer_keys(conn: &Connection, id: &str) -> Result<Vec<String>> {
    let mut keys = Vec::new();
    for prefix in COMPOSER_EXACT_PREFIXES {
        let key = format!("{prefix}{id}");
        if read_value(conn, Table::Disk, &key)?.is_some() {
            keys.push(key);
        }
    }
    for prefix in COMPOSER_PREFIXES {
        let start = format!("{prefix}{id}:");
        keys.extend(keys_in(conn, &start, &key_range_end(&start))?);
    }
    Ok(keys)
}

pub fn workspace_keys(conn: &Connection, workspace_id: &str) -> Result<Vec<String>> {
    let mut keys = Vec::new();
    for prefix in WORKSPACE_PREFIXES {
        let start = format!("{prefix}{workspace_id}:");
        keys.extend(keys_in(conn, &start, &key_range_end(&start))?);
    }
    Ok(keys)
}

pub fn retarget_composer_key(key: &str, old: &str, new: &str) -> String {
    for prefix in COMPOSER_EXACT_PREFIXES
        .iter()
        .chain(COMPOSER_PREFIXES.iter())
        .chain(WORKSPACE_PREFIXES.iter())
    {
        let needle = format!("{prefix}{old}");
        if key == needle || key.starts_with(&format!("{needle}:")) {
            return format!("{prefix}{new}{}", &key[needle.len()..]);
        }
    }
    key.to_string()
}

pub fn composer_ids_for_workspace(conn: &Connection, workspace_id: &str) -> Result<Vec<String>> {
    Ok(load_headers_for_workspace(conn, workspace_id)?
        .into_iter()
        .map(|header| header.composer_id)
        .collect())
}

pub fn headers(conn: &Connection, workspace_id: &str) -> Result<Vec<ComposerHeader>> {
    load_headers_for_workspace(conn, workspace_id)
}

pub fn owned_ids(conn: &Connection, workspace_id: &str) -> Result<HashSet<String>> {
    Ok(composer_ids_for_workspace(conn, workspace_id)?
        .into_iter()
        .collect())
}

pub fn count_headers(conn: &Connection, workspace_id: &str) -> Result<usize> {
    Ok(load_headers_for_workspace(conn, workspace_id)?.len())
}

fn child_ids(json: &Json) -> Vec<String> {
    let mut out = Vec::new();
    for field in ["subComposerIds", "subagentComposerIds"] {
        if let Some(items) = json.get(field).and_then(|v| v.as_array()) {
            out.extend(
                items
                    .iter()
                    .filter_map(|item| item.as_str())
                    .map(str::to_string),
            );
        }
    }
    out
}

/// Seeds plus every nested subagent and subComposer id.
pub fn expand_chat_ids(conn: &Connection, seeds: &[String]) -> Result<Vec<String>> {
    let mut seen = HashSet::new();
    let mut ordered = Vec::new();
    let mut queue: Vec<String> = seeds.iter().rev().cloned().collect();
    while let Some(id) = queue.pop() {
        if !seen.insert(id.clone()) {
            continue;
        }
        if let Some(raw) = read_text(conn, &format!("composerData:{id}"))?
            && let Ok(json) = serde_json::from_str::<Json>(&raw)
        {
            for child in child_ids(&json).into_iter().rev() {
                queue.push(child);
            }
        }
        ordered.push(id);
    }
    Ok(ordered)
}

/// Seeds that are not nested inside another seed.
pub fn top_level_ids(conn: &Connection, ids: &[String]) -> Result<Vec<String>> {
    let set: HashSet<&String> = ids.iter().collect();
    let mut children = HashSet::new();
    for id in ids {
        if let Some(raw) = read_text(conn, &format!("composerData:{id}"))?
            && let Ok(json) = serde_json::from_str::<Json>(&raw)
        {
            for child in child_ids(&json) {
                if set.contains(&child) {
                    children.insert(child);
                }
            }
        }
    }
    Ok(ids
        .iter()
        .filter(|id| !children.contains(*id))
        .cloned()
        .collect())
}

/// Bytes of every composer-keyed row and header of `ids`.
pub fn chat_bytes(conn: &Connection, ids: &[String]) -> Result<u64> {
    if !table_exists(conn, "cursorDiskKV")? {
        return Ok(0);
    }
    let mut total = 0u64;
    let mut range = conn.prepare_cached(
        "SELECT COALESCE(SUM(LENGTH(value)), 0) FROM cursorDiskKV WHERE key >= ?1 AND key < ?2",
    )?;
    let mut exact = conn.prepare_cached(
        "SELECT COALESCE(SUM(LENGTH(value)), 0) FROM cursorDiskKV WHERE key = ?1",
    )?;
    for id in ids {
        for prefix in COMPOSER_EXACT_PREFIXES {
            let bytes: i64 = exact.query_row([format!("{prefix}{id}")], |row| row.get(0))?;
            total += bytes.max(0) as u64;
        }
        for prefix in COMPOSER_PREFIXES {
            let start = format!("{prefix}{id}:");
            let bytes: i64 =
                range.query_row(params![start, key_range_end(&start)], |row| row.get(0))?;
            total += bytes.max(0) as u64;
        }
    }
    if composer_headers_table(conn)? {
        let mut header = conn.prepare_cached(
            "SELECT COALESCE(SUM(LENGTH(value)), 0) FROM composerHeaders WHERE composerId = ?1",
        )?;
        for id in ids {
            let bytes: i64 = header.query_row([id], |row| row.get(0))?;
            total += bytes.max(0) as u64;
        }
    }
    Ok(total)
}

pub fn workspace_bytes(conn: &Connection, workspace_id: &str) -> Result<u64> {
    if !table_exists(conn, "cursorDiskKV")? {
        return Ok(0);
    }
    let mut total = 0u64;
    for prefix in WORKSPACE_PREFIXES {
        let start = format!("{prefix}{workspace_id}:");
        let bytes: i64 = conn.query_row(
            "SELECT COALESCE(SUM(LENGTH(value)), 0) FROM cursorDiskKV WHERE key >= ?1 AND key < ?2",
            params![start, key_range_end(&start)],
            |row| row.get(0),
        )?;
        total += bytes.max(0) as u64;
    }
    for key in PLAN_KEYS {
        if let Some(value) = read_value(conn, Table::Item, key)? {
            total += sql::size(&value);
        }
    }
    Ok(total)
}

/// Replace `workspaceIdentifier` in a JSON object. `force` adds it when missing.
pub fn with_identity(text: &str, identity: &Json, force: bool) -> Option<String> {
    let mut json: Json = serde_json::from_str(text).ok()?;
    let object = json.as_object_mut()?;
    if !force && !object.contains_key("workspaceIdentifier") {
        return None;
    }
    if object.get("workspaceIdentifier") == Some(identity) {
        return None;
    }
    object.insert("workspaceIdentifier".to_string(), identity.clone());
    Some(json.to_string())
}

#[derive(Debug, Clone)]
pub struct HeaderRow {
    pub columns: RowImage,
}

impl HeaderRow {
    pub fn get(&self, name: &str) -> Option<&Value> {
        self.columns
            .iter()
            .find(|(column, _)| column == name)
            .map(|(_, value)| value)
    }

    pub fn set(&mut self, name: &str, value: Value) {
        if let Some(slot) = self.columns.iter_mut().find(|(column, _)| column == name) {
            slot.1 = value;
        } else {
            self.columns.push((name.to_string(), value));
        }
    }

    pub fn text(&self, name: &str) -> Option<&str> {
        self.get(name).and_then(sql::text)
    }
}

pub fn header_row(conn: &Connection, id: &str) -> Result<Option<HeaderRow>> {
    let mut stmt = conn.prepare_cached("SELECT * FROM composerHeaders WHERE composerId = ?1")?;
    let names: Vec<String> = stmt
        .column_names()
        .iter()
        .map(|name| name.to_string())
        .collect();
    let mut rows = stmt.query([id])?;
    let Some(row) = rows.next()? else {
        return Ok(None);
    };
    let mut columns = Vec::with_capacity(names.len());
    for (index, name) in names.into_iter().enumerate() {
        columns.push((name, row.get::<_, Value>(index)?));
    }
    Ok(Some(HeaderRow { columns }))
}

pub struct Target<'t> {
    pub id: &'t str,
    pub identity: &'t Json,
}

pub struct Writer<'a> {
    conn: &'a Connection,
    journal: &'a mut Journal,
}

impl<'a> Writer<'a> {
    pub fn new(conn: &'a Connection, journal: &'a mut Journal) -> Self {
        Self { conn, journal }
    }

    pub fn conn(&self) -> &'a Connection {
        self.conn
    }

    pub fn put(&mut self, table: Table, key: &str, value: &Value) -> Result<()> {
        self.journal.record_row(self.conn, table, key)?;
        self.conn
            .prepare_cached(&format!(
                "INSERT OR REPLACE INTO {} (key, value) VALUES (?1, ?2)",
                table.name()
            ))?
            .execute(params![key, value])
            .with_context(|| format!("failed to write {key}"))?;
        Ok(())
    }

    pub fn delete(&mut self, table: Table, key: &str) -> Result<()> {
        self.journal.record_row(self.conn, table, key)?;
        self.conn
            .prepare_cached(&format!(
                "DELETE FROM {} WHERE {} = ?1",
                table.name(),
                table.key_column()
            ))?
            .execute([key])?;
        Ok(())
    }

    pub fn put_header(&mut self, row: &HeaderRow) -> Result<()> {
        let id = row
            .text("composerId")
            .context("composerHeaders row without composerId")?
            .to_string();
        self.journal.record_row(self.conn, Table::Headers, &id)?;
        let known = crate::cursor::registry::table_columns(self.conn, "composerHeaders")?;
        let kept: Vec<&(String, Value)> = row
            .columns
            .iter()
            .filter(|(name, _)| known.contains(name))
            .collect();
        let columns: Vec<&str> = kept.iter().map(|(name, _)| name.as_str()).collect();
        let marks: Vec<String> = (1..=columns.len())
            .map(|index| format!("?{index}"))
            .collect();
        self.conn
            .prepare_cached(&format!(
                "INSERT OR REPLACE INTO composerHeaders ({}) VALUES ({})",
                columns.join(", "),
                marks.join(", ")
            ))?
            .execute(params_from_iter(kept.iter().map(|(_, value)| value)))?;
        Ok(())
    }

    /// Rewrite one row in place when `rewriter` changes it.
    fn rewrite_row(&mut self, table: Table, key: &str, rewriter: &Rewriter) -> Result<bool> {
        let Some(value) = read_value(self.conn, table, key)? else {
            return Ok(false);
        };
        let Some(text) = sql::text(&value) else {
            return Ok(false);
        };
        let updated = rewriter.rewrite(text);
        if updated == text {
            return Ok(false);
        }
        let updated = like(&value, updated.into_owned());
        self.put(table, key, &updated)?;
        Ok(true)
    }

    fn legacy(&mut self, edit: impl FnOnce(&mut Vec<Json>) -> Result<bool>) -> Result<()> {
        let Some(raw) = read_item(self.conn, LEGACY_HEADERS_KEY)? else {
            return Ok(());
        };
        let mut json: Json = serde_json::from_str(&raw).context("legacy headers are not json")?;
        let Some(composers) = json.get_mut("allComposers").and_then(|v| v.as_array_mut()) else {
            return Ok(());
        };
        if edit(composers)? {
            self.put(
                Table::Item,
                LEGACY_HEADERS_KEY,
                &Value::Text(json.to_string()),
            )?;
        }
        Ok(())
    }

    /// Insert or replace entries of the legacy `allComposers` registry by `composerId`.
    pub fn upsert_legacy_headers(&mut self, entries: Vec<Json>) -> Result<()> {
        if entries.is_empty() {
            return Ok(());
        }
        if read_item(self.conn, LEGACY_HEADERS_KEY)?.is_none() {
            self.put(
                Table::Item,
                LEGACY_HEADERS_KEY,
                &Value::Text(serde_json::json!({"allComposers": []}).to_string()),
            )?;
        }
        self.legacy(|composers| {
            for entry in entries {
                let id = entry.get("composerId").cloned();
                composers.retain(|composer| composer.get("composerId") != id.as_ref());
                composers.push(entry);
            }
            Ok(true)
        })
    }

    /// Move chats to `target`: ownership and identity change, content stays.
    pub fn reassign_chats(&mut self, ids: &[String], target: &Target<'_>) -> Result<usize> {
        let mut moved = 0usize;
        if composer_headers_table(self.conn)? {
            for id in ids {
                let Some(mut row) = header_row(self.conn, id)? else {
                    continue;
                };
                row.set("workspaceId", Value::Text(target.id.to_string()));
                if let Some(value) = row.text("value").map(str::to_string)
                    && let Some(updated) = with_identity(&value, target.identity, true)
                {
                    row.set("value", Value::Text(updated));
                }
                self.put_header(&row)?;
                moved += 1;
            }
        } else {
            let wanted: HashSet<&String> = ids.iter().collect();
            self.legacy(|composers| {
                for composer in composers.iter_mut() {
                    if composer
                        .get("composerId")
                        .and_then(|v| v.as_str())
                        .is_some_and(|id| wanted.contains(&id.to_string()))
                        && let Some(object) = composer.as_object_mut()
                    {
                        object.insert("workspaceIdentifier".to_string(), target.identity.clone());
                        moved += 1;
                    }
                }
                Ok(moved > 0)
            })?;
        }
        for id in ids {
            self.retarget_composer_data(id, target.identity)?;
        }
        Ok(moved)
    }

    fn retarget_composer_data(&mut self, id: &str, identity: &Json) -> Result<()> {
        let key = format!("composerData:{id}");
        let Some(value) = read_value(self.conn, Table::Disk, &key)? else {
            return Ok(());
        };
        if let Some(text) = sql::text(&value)
            && let Some(updated) = with_identity(text, identity, false)
        {
            self.put(Table::Disk, &key, &like(&value, updated))?;
        }
        Ok(())
    }

    /// Copy chats to `target` under fresh ids. Old ids inside values are remapped in one pass
    /// per value, together with `extra` replacements.
    pub fn clone_chats(
        &mut self,
        ids: &[String],
        target: &Target<'_>,
        extra: &[(Replacement, Boundary)],
    ) -> Result<HashMap<String, String>> {
        let map: HashMap<String, String> = ids
            .iter()
            .map(|id| (id.clone(), uuid::Uuid::new_v4().to_string()))
            .collect();
        let mut entries: Vec<(Replacement, Boundary)> = ids
            .iter()
            .map(|id| (Replacement::new(id, &map[id]), Boundary::Token))
            .collect();
        entries.extend(extra.iter().cloned());
        let rewriter = Rewriter::build(entries);
        for id in ids {
            let new = &map[id];
            for key in composer_keys(self.conn, id)? {
                let Some(value) = read_value(self.conn, Table::Disk, &key)? else {
                    continue;
                };
                let new_key = retarget_composer_key(&key, id, new);
                let new_value = match sql::text(&value) {
                    Some(text) => {
                        let rewritten = rewriter.rewrite(text).into_owned();
                        let rewritten = if key.starts_with("composerData:") {
                            with_identity(&rewritten, target.identity, false).unwrap_or(rewritten)
                        } else {
                            rewritten
                        };
                        like(&value, rewritten)
                    }
                    None => value.clone(),
                };
                self.put(Table::Disk, &new_key, &new_value)?;
            }
        }
        if composer_headers_table(self.conn)? {
            for id in ids {
                let Some(mut row) = header_row(self.conn, id)? else {
                    continue;
                };
                row.set("composerId", Value::Text(map[id].clone()));
                row.set("workspaceId", Value::Text(target.id.to_string()));
                if let Some(value) = row.text("value").map(str::to_string) {
                    let rewritten = rewriter.rewrite(&value).into_owned();
                    let rewritten =
                        with_identity(&rewritten, target.identity, true).unwrap_or(rewritten);
                    row.set("value", Value::Text(rewritten));
                }
                self.put_header(&row)?;
            }
        } else {
            self.legacy(|composers| {
                let mut added = Vec::new();
                for composer in composers.iter() {
                    let Some(id) = composer.get("composerId").and_then(|v| v.as_str()) else {
                        continue;
                    };
                    if !map.contains_key(id) {
                        continue;
                    }
                    let text = rewriter.rewrite(&composer.to_string()).into_owned();
                    let mut copy: Json = serde_json::from_str(&text)?;
                    if let Some(object) = copy.as_object_mut() {
                        object.insert("composerId".to_string(), Json::String(map[id].clone()));
                        object.insert("workspaceIdentifier".to_string(), target.identity.clone());
                    }
                    added.push(copy);
                }
                let changed = !added.is_empty();
                composers.extend(added);
                Ok(changed)
            })?;
        }
        Ok(map)
    }

    /// Delete chats with every composer-keyed row. Shared blobs stay.
    pub fn delete_chats(&mut self, ids: &[String]) -> Result<usize> {
        let mut removed = 0usize;
        for id in ids {
            for key in composer_keys(self.conn, id)? {
                self.delete(Table::Disk, &key)?;
                removed += 1;
            }
        }
        if composer_headers_table(self.conn)? {
            for id in ids {
                if header_row(self.conn, id)?.is_some() {
                    self.delete(Table::Headers, id)?;
                    removed += 1;
                }
            }
        } else {
            let wanted: HashSet<&String> = ids.iter().collect();
            self.legacy(|composers| {
                let before = composers.len();
                composers.retain(|composer| {
                    !composer
                        .get("composerId")
                        .and_then(|v| v.as_str())
                        .is_some_and(|id| wanted.contains(&id.to_string()))
                });
                removed += before - composers.len();
                Ok(before != composers.len())
            })?;
        }
        Ok(removed)
    }

    pub fn delete_workspace_rows(&mut self, workspace_id: &str) -> Result<usize> {
        let keys = workspace_keys(self.conn, workspace_id)?;
        for key in &keys {
            self.delete(Table::Disk, key)?;
        }
        Ok(keys.len())
    }

    /// Re-key `inlineDiff:<old>:*` and `patch-graph:<old>:*` rows. `keep` copies instead.
    pub fn rekey_workspace_rows(
        &mut self,
        old: &str,
        new: &str,
        rewriter: &Rewriter,
        keep: bool,
    ) -> Result<usize> {
        let keys = workspace_keys(self.conn, old)?;
        for key in &keys {
            let Some(value) = read_value(self.conn, Table::Disk, key)? else {
                continue;
            };
            let new_key = retarget_composer_key(key, old, new);
            let new_value = match sql::text(&value) {
                Some(text) => like(&value, rewriter.rewrite(text).into_owned()),
                None => value.clone(),
            };
            if new_key != *key && !keep {
                self.delete(Table::Disk, key)?;
            }
            self.put(Table::Disk, &new_key, &new_value)?;
        }
        Ok(keys.len())
    }

    /// Rewrite every composer-keyed row of `ids` in place.
    pub fn rewrite_chats(&mut self, ids: &[String], rewriter: &Rewriter) -> Result<usize> {
        if rewriter.is_empty() {
            return Ok(0);
        }
        let mut touched = 0usize;
        for id in ids {
            for key in composer_keys(self.conn, id)? {
                if self.rewrite_row(Table::Disk, &key, rewriter)? {
                    touched += 1;
                }
            }
        }
        Ok(touched)
    }

    /// Repath one workspace in place: chat rows, headers, workspace rows, and plan keys.
    pub fn rewrite_workspace(
        &mut self,
        source_id: &str,
        target: &Target<'_>,
        rewriter: &Rewriter,
    ) -> Result<usize> {
        let ids = composer_ids_for_workspace(self.conn, source_id)?;
        let ids = expand_chat_ids(self.conn, &ids)?;
        let mut touched = self.rewrite_chats(&ids, rewriter)?;
        for id in &ids {
            self.retarget_composer_data(id, target.identity)?;
        }
        if composer_headers_table(self.conn)? {
            for id in &ids {
                let Some(mut row) = header_row(self.conn, id)? else {
                    continue;
                };
                if row.text("workspaceId") == Some(source_id) {
                    row.set("workspaceId", Value::Text(target.id.to_string()));
                }
                if let Some(value) = row.text("value").map(str::to_string) {
                    let rewritten = rewriter.rewrite(&value).into_owned();
                    let rewritten =
                        with_identity(&rewritten, target.identity, true).unwrap_or(rewritten);
                    row.set("value", Value::Text(rewritten));
                }
                self.put_header(&row)?;
                touched += 1;
            }
        } else {
            let wanted: HashSet<&String> = ids.iter().collect();
            self.legacy(|composers| {
                let mut changed = false;
                for composer in composers.iter_mut() {
                    if !composer
                        .get("composerId")
                        .and_then(|v| v.as_str())
                        .is_some_and(|id| wanted.contains(&id.to_string()))
                    {
                        continue;
                    }
                    let text = rewriter.rewrite(&composer.to_string()).into_owned();
                    let mut updated: Json = serde_json::from_str(&text)?;
                    if let Some(object) = updated.as_object_mut() {
                        object.insert("workspaceIdentifier".to_string(), target.identity.clone());
                    }
                    *composer = updated;
                    changed = true;
                    touched += 1;
                }
                Ok(changed)
            })?;
        }
        touched += self.rekey_workspace_rows(source_id, target.id, rewriter, false)?;
        for key in PLAN_KEYS {
            if self.rewrite_row(Table::Item, key, rewriter)? {
                touched += 1;
            }
        }
        Ok(touched)
    }
}

pub fn touched_paths(conn: &Connection, composer_id: &str) -> Result<Vec<String>> {
    let mut paths = Vec::new();
    let start = format!("ofsContent:{composer_id}:");
    for key in keys_in(conn, &start, &key_range_end(&start))? {
        if let Some(rest) = key.strip_prefix(&start) {
            paths.push(rest.to_string());
        }
    }
    if let Some(raw) = read_text(conn, &format!("composerData:{composer_id}"))?
        && let Ok(json) = serde_json::from_str::<Json>(&raw)
    {
        collect_file_paths(&json, &mut paths, true);
    }
    let bubble_start = format!("bubbleId:{composer_id}:");
    scan(
        conn,
        &bubble_start,
        &key_range_end(&bubble_start),
        |_, value| {
            if let Some(text) = sql::text(value)
                && let Ok(json) = serde_json::from_str::<Json>(text)
            {
                collect_file_paths(&json, &mut paths, false);
            }
            Ok(())
        },
    )?;
    Ok(paths)
}

fn collect_file_paths(value: &Json, out: &mut Vec<String>, honor_ignore: bool) {
    match value {
        Json::Object(map) => {
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
        Json::Array(items) => {
            for item in items {
                if let Some(text) = item.as_str() {
                    out.push(text.to_string());
                } else {
                    collect_file_paths(item, out, honor_ignore);
                }
            }
        }
        Json::String(text)
            if text.contains("://") || text.starts_with('/') || text.contains(":\\") =>
        {
            out.push(text.clone());
        }
        _ => {}
    }
}

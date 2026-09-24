//! Aggregate Cursor usage from the local databases.

use anyhow::Result;
use chrono::{TimeZone, Utc};
use rusqlite::Connection;
use serde_json::Value;
use std::collections::BTreeMap;
use std::fs;

use super::fsops::dir_size;
use super::{Runtime, discover, open_global_ro};
use crate::cursor::registry::load_headers;
use crate::engine::db;
use crate::ui;

pub fn render(rt: &Runtime) -> Result<String> {
    let workspaces = discover(rt)?;
    let mut by_kind: BTreeMap<String, usize> = BTreeMap::new();
    for workspace in &workspaces {
        *by_kind
            .entry(workspace.kind.label().to_string())
            .or_default() += 1;
    }
    let conn = open_global_ro(rt)?;
    let headers = match &conn {
        Some(conn) => load_headers(conn)?,
        None => Vec::new(),
    };
    let chats = headers.iter().filter(|header| !header.is_subagent).count();
    let subs = headers.iter().filter(|header| header.is_subagent).count();
    let archived = headers.iter().filter(|header| header.is_archived).count();
    let mut per_month: BTreeMap<String, usize> = BTreeMap::new();
    let mut per_workspace: BTreeMap<String, usize> = BTreeMap::new();
    for header in &headers {
        if header.is_subagent {
            continue;
        }
        *per_workspace
            .entry(header.workspace_id.clone())
            .or_default() += 1;
        if let Some(created) = header.created_at
            && let Some(stamp) = Utc.timestamp_millis_opt(created).single()
        {
            *per_month
                .entry(stamp.format("%Y-%m").to_string())
                .or_default() += 1;
        }
    }
    let (tokens, models) = match &conn {
        Some(conn) => usage(conn, &headers)?,
        None => (0, BTreeMap::new()),
    };
    let mut lines = String::new();
    lines.push_str(&format!(
        "profiles: {}\nworkspaces: {}\nchats: {chats}\nsubagents: {subs}\narchived: {archived}\n",
        profile_count(rt),
        workspaces.len()
    ));
    for (kind, count) in &by_kind {
        lines.push_str(&format!("kind {kind}: {count}\n"));
    }
    if let Some(size) = file_len(&rt.layout.global_db()) {
        lines.push_str(&format!("global db: {}\n", ui::format_size(size)));
    }
    for workspace in &workspaces {
        if let Some(size) = file_len(&workspace.dir.join("state.vscdb")) {
            lines.push_str(&format!("db {}: {}\n", workspace.id, ui::format_size(size)));
        }
    }
    lines.push_str(&format!("tokens: {tokens}\n"));
    if let Some((model, count)) = models.iter().max_by_key(|(_, count)| *count) {
        lines.push_str(&format!("most used model: {model} ({count})\n"));
    }
    let mut busiest: Vec<_> = per_workspace.into_iter().collect();
    busiest.sort_by_key(|item| std::cmp::Reverse(item.1));
    for (id, count) in busiest.into_iter().take(5) {
        lines.push_str(&format!("busy {id}: {count}\n"));
    }
    for (month, count) in per_month {
        lines.push_str(&format!("month {month}: {count}\n"));
    }
    Ok(lines)
}

fn usage(
    conn: &Connection,
    headers: &[crate::cursor::registry::ComposerHeader],
) -> Result<(u64, BTreeMap<String, usize>)> {
    let mut tokens = 0u64;
    let mut models = BTreeMap::new();
    for header in headers {
        let Some(raw) = db::read_text(conn, &format!("composerData:{}", header.composer_id))?
        else {
            continue;
        };
        let Ok(json) = serde_json::from_str::<Value>(&raw) else {
            continue;
        };
        if let Some(usage) = json.get("usageData") {
            tokens += sum_tokens(usage);
        }
        if let Some(name) = json
            .pointer("/modelConfig/modelName")
            .and_then(|v| v.as_str())
            .or_else(|| json.get("modelConfig").and_then(|v| v.as_str()))
        {
            *models.entry(name.to_string()).or_default() += 1;
        }
    }
    Ok((tokens, models))
}

fn sum_tokens(value: &Value) -> u64 {
    match value {
        Value::Object(map) => map
            .iter()
            .map(|(key, child)| {
                if key.to_ascii_lowercase().contains("token") {
                    child.as_u64().unwrap_or_else(|| sum_tokens(child))
                } else {
                    sum_tokens(child)
                }
            })
            .sum(),
        Value::Array(items) => items.iter().map(sum_tokens).sum(),
        Value::Number(number) => number.as_u64().unwrap_or(0),
        _ => 0,
    }
}

fn profile_count(rt: &Runtime) -> usize {
    let path = rt.layout.storage_json();
    if !path.exists() {
        return 1;
    }
    let Ok(raw) = fs::read_to_string(path) else {
        return 1;
    };
    let Ok(json) = serde_json::from_str::<Value>(&raw) else {
        return 1;
    };
    super::profiles_from_storage(&json).len()
}

fn file_len(path: &std::path::Path) -> Option<u64> {
    fs::metadata(path).ok().map(|meta| meta.len()).or_else(|| {
        if path.is_dir() {
            dir_size(path).ok()
        } else {
            None
        }
    })
}

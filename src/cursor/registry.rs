//! Chat registry: `composerHeaders` table, with the legacy ItemTable key as fallback.

use anyhow::{Context, Result};
use rusqlite::{Connection, OptionalExtension, params};
use serde_json::Value;

pub const LEGACY_HEADERS_KEY: &str = "composer.composerHeaders";
pub const LOCAL_COMPOSER_DATA_KEY: &str = "composer.composerData";
pub const LOCAL_PINNED_KEY: &str = "cursor/pinnedComposers";

/// Families keyed `<prefix><chatId>:<rest>`.
pub const COMPOSER_PREFIXES: &[&str] = &[
    "bubbleId:",
    "checkpointId:",
    "codeBlockDiff:",
    "codeBlockPartialInlineDiffFates:",
    "messageRequestContext:",
    "ofsContent:",
    "agentKv:bubbleCheckpoint:",
];

/// Families keyed exactly `<prefix><chatId>`.
pub const COMPOSER_EXACT_PREFIXES: &[&str] = &[
    "composerData:",
    "composerVirtualRowHeights:",
    "agentKv:checkpoint:",
];

/// Content-addressed blobs shared between chats.
pub const BLOB_PREFIX: &str = "agentKv:blob:";

/// Families keyed `<prefix><workspaceId>:<rest>`.
pub const WORKSPACE_PREFIXES: &[&str] = &["inlineDiff:", "patch-graph:"];

/// ItemTable keys in the global database that carry workspace paths.
pub const PLAN_KEYS: &[&str] = &["composer.planRegistry", "composer.planRedirects"];

/// Global `state.vscdb` schema as Cursor creates it.
pub const GLOBAL_SCHEMA: &str = "
CREATE TABLE IF NOT EXISTS ItemTable (key TEXT UNIQUE ON CONFLICT REPLACE, value BLOB);
CREATE TABLE IF NOT EXISTS cursorDiskKV (key TEXT UNIQUE ON CONFLICT REPLACE, value BLOB);
CREATE TABLE IF NOT EXISTS composerHeaders (composerId TEXT PRIMARY KEY, workspaceId TEXT, createdAt INTEGER, lastUpdatedAt INTEGER, isArchived INTEGER, isSubagent INTEGER, recency INTEGER, checkpointAt INTEGER, value TEXT, subagentTypeName TEXT);
CREATE INDEX IF NOT EXISTS idx_composerHeaders_0 ON composerHeaders (workspaceId, isSubagent, isArchived, recency);
CREATE INDEX IF NOT EXISTS idx_composerHeaders_1 ON composerHeaders (recency, composerId);
";

/// Per-workspace `state.vscdb` table Cursor reads chat selection from.
pub const LOCAL_SCHEMA: &str =
    "CREATE TABLE IF NOT EXISTS ItemTable (key TEXT UNIQUE ON CONFLICT REPLACE, value BLOB);";

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ComposerHeader {
    pub composer_id: String,
    pub workspace_id: String,
    pub created_at: Option<i64>,
    pub last_updated_at: Option<i64>,
    pub is_archived: bool,
    pub is_subagent: bool,
    pub recency: Option<i64>,
    pub checkpoint_at: Option<i64>,
    pub value: String,
    pub subagent_type_name: Option<String>,
    pub title: Option<String>,
}

pub fn table_exists(conn: &Connection, table: &str) -> Result<bool> {
    let count: i64 = conn
        .query_row(
            "SELECT COUNT(*) FROM sqlite_master WHERE type = 'table' AND name = ?1",
            params![table],
            |row| row.get(0),
        )
        .context("failed to query sqlite_master")?;
    Ok(count > 0)
}

pub fn composer_headers_table(conn: &Connection) -> Result<bool> {
    table_exists(conn, "composerHeaders")
}

pub fn table_columns(conn: &Connection, table: &str) -> Result<Vec<String>> {
    let mut stmt = conn.prepare_cached("SELECT name FROM pragma_table_info(?1)")?;
    let names = stmt
        .query_map([table], |row| row.get(0))?
        .collect::<Result<Vec<String>, _>>()
        .with_context(|| format!("failed to read the columns of {table}"))?;
    Ok(names)
}

/// `composerHeaders` columns in `ComposerHeader` order; older Cursor builds have no
/// `subagentTypeName` column.
fn header_fields(conn: &Connection) -> Result<&'static str> {
    let has_type = table_columns(conn, "composerHeaders")?
        .iter()
        .any(|name| name == "subagentTypeName");
    Ok(if has_type {
        "composerId, workspaceId, createdAt, lastUpdatedAt, isArchived, isSubagent, \
         recency, checkpointAt, value, subagentTypeName"
    } else {
        "composerId, workspaceId, createdAt, lastUpdatedAt, isArchived, isSubagent, \
         recency, checkpointAt, value, NULL"
    })
}

/// Read registry rows. Uses `composerHeaders` when that table exists, otherwise the legacy key.
pub fn load_headers(conn: &Connection) -> Result<Vec<ComposerHeader>> {
    if composer_headers_table(conn)? {
        return load_headers_table(conn, None);
    }
    load_legacy_headers(conn)
}

pub fn load_headers_for_workspace(
    conn: &Connection,
    workspace_id: &str,
) -> Result<Vec<ComposerHeader>> {
    if composer_headers_table(conn)? {
        return load_headers_table(conn, Some(workspace_id));
    }
    Ok(load_legacy_headers(conn)?
        .into_iter()
        .filter(|header| header.workspace_id == workspace_id)
        .collect())
}

pub fn load_header(conn: &Connection, composer_id: &str) -> Result<Option<ComposerHeader>> {
    if composer_headers_table(conn)? {
        let mut stmt = conn.prepare_cached(&format!(
            "SELECT {} FROM composerHeaders WHERE composerId = ?1",
            header_fields(conn)?
        ))?;
        return stmt
            .query_row(params![composer_id], map_row)
            .optional()
            .context("failed to read composerHeaders");
    }
    Ok(load_legacy_headers(conn)?
        .into_iter()
        .find(|header| header.composer_id == composer_id))
}

fn map_row(row: &rusqlite::Row<'_>) -> rusqlite::Result<ComposerHeader> {
    let value: Option<String> = row.get(8)?;
    let value = value.unwrap_or_default();
    let title = title_from_value(&value);
    Ok(ComposerHeader {
        composer_id: row.get(0)?,
        workspace_id: row.get::<_, Option<String>>(1)?.unwrap_or_default(),
        created_at: row.get(2)?,
        last_updated_at: row.get(3)?,
        is_archived: flag(row.get(4)?),
        is_subagent: flag(row.get(5)?),
        recency: row.get(6)?,
        checkpoint_at: row.get(7)?,
        title,
        value,
        subagent_type_name: row.get(9)?,
    })
}

fn load_headers_table(
    conn: &Connection,
    workspace_id: Option<&str>,
) -> Result<Vec<ComposerHeader>> {
    let fields = header_fields(conn)?;
    let sql = match workspace_id {
        Some(_) => format!("SELECT {fields} FROM composerHeaders WHERE workspaceId = ?1"),
        None => format!("SELECT {fields} FROM composerHeaders"),
    };
    let mut stmt = conn
        .prepare(&sql)
        .context("failed to prepare composerHeaders query")?;
    let rows = if let Some(workspace_id) = workspace_id {
        stmt.query_map(params![workspace_id], map_row)?
    } else {
        stmt.query_map([], map_row)?
    };
    rows.collect::<Result<Vec<_>, _>>()
        .context("failed to read composerHeaders")
}

fn load_legacy_headers(conn: &Connection) -> Result<Vec<ComposerHeader>> {
    if !table_exists(conn, "ItemTable")? {
        return Ok(Vec::new());
    }
    let raw: Option<String> = conn
        .query_row(
            "SELECT value FROM ItemTable WHERE key = ?1",
            params![LEGACY_HEADERS_KEY],
            |row| row.get(0),
        )
        .optional()
        .context("failed to read legacy composer.composerHeaders")?;
    let Some(raw) = raw else {
        return Ok(Vec::new());
    };
    parse_legacy(&raw)
}

pub fn parse_legacy(raw: &str) -> Result<Vec<ComposerHeader>> {
    let json: Value = serde_json::from_str(raw).context("legacy composer headers are not json")?;
    let Some(composers) = json.get("allComposers").and_then(|value| value.as_array()) else {
        return Ok(Vec::new());
    };
    let mut headers = Vec::new();
    for composer in composers {
        let Some(composer_id) = composer.get("composerId").and_then(|v| v.as_str()) else {
            continue;
        };
        let workspace_id = composer
            .pointer("/workspaceIdentifier/id")
            .and_then(|v| v.as_str())
            .unwrap_or("")
            .to_string();
        headers.push(ComposerHeader {
            composer_id: composer_id.to_string(),
            workspace_id,
            created_at: composer.get("createdAt").and_then(|v| v.as_i64()),
            last_updated_at: composer.get("lastUpdatedAt").and_then(|v| v.as_i64()),
            is_archived: composer
                .get("isArchived")
                .and_then(|v| v.as_bool())
                .unwrap_or(false),
            is_subagent: composer
                .get("isSubagent")
                .and_then(|v| v.as_bool())
                .unwrap_or(false),
            recency: composer.get("recency").and_then(|v| v.as_i64()),
            checkpoint_at: composer.get("checkpointAt").and_then(|v| v.as_i64()),
            value: composer.to_string(),
            subagent_type_name: composer
                .get("subagentTypeName")
                .and_then(|v| v.as_str())
                .map(str::to_string),
            title: title_from_value(&composer.to_string()),
        });
    }
    Ok(headers)
}

fn title_from_value(value: &str) -> Option<String> {
    let Ok(json) = serde_json::from_str::<Value>(value) else {
        return None;
    };
    json.get("name")
        .or_else(|| json.get("title"))
        .and_then(|v| v.as_str())
        .map(str::trim)
        .filter(|name| !name.is_empty())
        .map(str::to_string)
}

fn flag(value: Option<i64>) -> bool {
    value.unwrap_or(0) != 0
}

pub fn ensure_global_schema(conn: &Connection) -> Result<()> {
    conn.execute_batch(GLOBAL_SCHEMA)
        .context("failed to ensure global schema")
}

pub fn ensure_local_schema(conn: &Connection) -> Result<()> {
    conn.execute_batch(LOCAL_SCHEMA)
        .context("failed to ensure local schema")
}

//! `index.db`: one SQLite file with a row per installation, workspace, chat, and source.

use anyhow::{Context, Result};
use rusqlite::{Connection, OptionalExtension, Transaction, TransactionBehavior, params};
use std::collections::HashMap;
use std::fs;
use std::path::{Path, PathBuf};
use std::time::Duration;

use super::super::facts::{ChatFacts, ChatUsage, InstallFacts, WorkspaceFacts};
use super::super::{Kind, Layout, Workspace};
use crate::cursor::install::UserProfile;

pub const SCHEMA: i64 = 1;

const TABLES: &str = "
CREATE TABLE meta (key TEXT PRIMARY KEY, value TEXT NOT NULL);
CREATE TABLE installations (
    name TEXT PRIMARY KEY,
    root TEXT NOT NULL,
    scanned_at INTEGER NOT NULL,
    duration_ms INTEGER NOT NULL,
    dirty_at INTEGER,
    global_db INTEGER
);
CREATE TABLE profiles (
    install TEXT NOT NULL,
    position INTEGER NOT NULL,
    id TEXT NOT NULL,
    name TEXT NOT NULL,
    PRIMARY KEY (install, position)
);
CREATE TABLE workspaces (
    install TEXT NOT NULL,
    id TEXT NOT NULL,
    kind TEXT NOT NULL,
    uri TEXT,
    path TEXT,
    profile TEXT NOT NULL,
    dest_missing INTEGER NOT NULL,
    size INTEGER NOT NULL,
    local_db INTEGER,
    PRIMARY KEY (install, id)
);
CREATE TABLE chats (
    install TEXT NOT NULL,
    id TEXT NOT NULL,
    workspace_id TEXT NOT NULL,
    is_subagent INTEGER NOT NULL,
    is_archived INTEGER NOT NULL,
    created_at INTEGER,
    last_updated_at INTEGER,
    title TEXT,
    origin_kind TEXT,
    origin_path TEXT,
    fingerprint TEXT NOT NULL,
    has_data INTEGER NOT NULL,
    model TEXT,
    cost_cents INTEGER NOT NULL,
    requests INTEGER NOT NULL,
    context_tokens INTEGER NOT NULL,
    input_tokens INTEGER NOT NULL,
    output_tokens INTEGER NOT NULL,
    PRIMARY KEY (install, id)
);
CREATE INDEX chats_by_workspace ON chats (install, workspace_id);
CREATE TABLE evidence (
    install TEXT NOT NULL,
    id TEXT NOT NULL,
    workspace_id TEXT NOT NULL,
    last_updated_at INTEGER,
    paths TEXT NOT NULL,
    PRIMARY KEY (install, id)
);
CREATE TABLE sources (
    install TEXT NOT NULL,
    key TEXT NOT NULL,
    workspace TEXT,
    size INTEGER NOT NULL,
    mtime INTEGER NOT NULL,
    PRIMARY KEY (install, key)
);
";

const INSTALL_TABLES: &[&str] = &[
    "installations",
    "profiles",
    "workspaces",
    "chats",
    "evidence",
    "sources",
];

pub fn db_path(home: &Path) -> PathBuf {
    home.join("index.db")
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Stamp {
    pub size: i64,
    pub mtime: i64,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Source {
    pub key: String,
    pub workspace: Option<String>,
    pub stamp: Stamp,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Scan {
    pub name: String,
    pub root: String,
    pub scanned_at: i64,
    pub duration_ms: i64,
    pub dirty_at: Option<i64>,
}

impl Scan {
    pub fn current(&self, layout: &Layout) -> bool {
        self.dirty_at.is_none() && self.root == root_text(layout)
    }
}

#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct Counts {
    pub workspaces: usize,
    pub chats: usize,
    pub subagents: usize,
}

/// What the last refresh stored for one installation, reused by the next one.
#[derive(Debug, Default)]
pub struct Prior {
    pub known: bool,
    pub profiles: Vec<UserProfile>,
    pub workspaces: HashMap<String, WorkspaceFacts>,
    pub chats: HashMap<String, (ChatFacts, String)>,
    pub sources: HashMap<String, Stamp>,
}

pub struct Update {
    pub scanned_at: i64,
    pub duration_ms: i64,
    pub global_db: Option<u64>,
    pub profiles: Vec<UserProfile>,
    pub workspaces: Vec<WorkspaceFacts>,
    pub sources: Vec<Source>,
    pub replace_chats: bool,
    pub upserts: Vec<(ChatFacts, String)>,
    pub removed: Vec<String>,
}

pub type Evidence = HashMap<String, (Option<i64>, Vec<PathBuf>)>;

pub fn root_text(layout: &Layout) -> String {
    layout.cursor_root.to_string_lossy().to_string()
}

fn size(value: Option<u64>) -> Option<i64> {
    value.map(|value| i64::try_from(value).unwrap_or(i64::MAX))
}

fn unsigned(value: i64) -> u64 {
    u64::try_from(value).unwrap_or(0)
}

pub struct Index {
    conn: Connection,
    path: PathBuf,
}

impl Index {
    /// Open or create the index, rebuilding it when the schema version differs.
    pub fn open(home: &Path) -> Result<Self> {
        fs::create_dir_all(home).with_context(|| format!("failed to create {}", home.display()))?;
        let path = db_path(home);
        let conn = Connection::open(&path)
            .with_context(|| format!("failed to open {}", path.display()))?;
        conn.busy_timeout(Duration::from_secs(30))?;
        conn.pragma_update(None, "journal_mode", "WAL")?;
        conn.pragma_update(None, "synchronous", "NORMAL")?;
        let index = Self { conn, path };
        index.ensure_schema()?;
        Ok(index)
    }

    pub fn open_existing(home: &Path) -> Result<Option<Self>> {
        if !db_path(home).exists() {
            return Ok(None);
        }
        Self::open(home).map(Some)
    }

    pub fn path(&self) -> &Path {
        &self.path
    }

    fn write(&self) -> Result<Transaction<'_>> {
        Ok(Transaction::new_unchecked(
            &self.conn,
            TransactionBehavior::Immediate,
        )?)
    }

    pub fn version(&self) -> Result<i64> {
        Ok(self
            .conn
            .pragma_query_value(None, "user_version", |row| row.get(0))?)
    }

    fn ensure_schema(&self) -> Result<()> {
        if self.version()? == SCHEMA {
            return Ok(());
        }
        let tx = self.write()?;
        if tx.pragma_query_value(None, "user_version", |row| row.get::<_, i64>(0))? != SCHEMA {
            let tables: Vec<String> = tx
                .prepare(
                    "SELECT name FROM sqlite_master WHERE type = 'table' AND name NOT LIKE 'sqlite_%'",
                )?
                .query_map([], |row| row.get(0))?
                .collect::<Result<_, _>>()?;
            for table in tables {
                tx.execute_batch(&format!("DROP TABLE IF EXISTS \"{table}\""))?;
            }
            tx.execute_batch(TABLES)?;
            tx.pragma_update(None, "user_version", SCHEMA)?;
        }
        tx.commit()?;
        Ok(())
    }

    pub fn meta(&self, key: &str) -> Result<Option<String>> {
        Ok(self
            .conn
            .query_row("SELECT value FROM meta WHERE key = ?1", [key], |row| {
                row.get(0)
            })
            .optional()?)
    }

    pub fn meta_millis(&self, key: &str) -> Result<Option<i64>> {
        Ok(self.meta(key)?.and_then(|value| value.parse().ok()))
    }

    pub fn set_meta(&self, key: &str, value: &str) -> Result<()> {
        self.conn.execute(
            "INSERT INTO meta (key, value) VALUES (?1, ?2) ON CONFLICT(key) DO UPDATE SET value = excluded.value",
            params![key, value],
        )?;
        Ok(())
    }

    pub fn scans(&self) -> Result<HashMap<String, Scan>> {
        let mut stmt = self
            .conn
            .prepare("SELECT name, root, scanned_at, duration_ms, dirty_at FROM installations")?;
        let rows = stmt.query_map([], |row| {
            Ok(Scan {
                name: row.get(0)?,
                root: row.get(1)?,
                scanned_at: row.get(2)?,
                duration_ms: row.get(3)?,
                dirty_at: row.get(4)?,
            })
        })?;
        let mut out = HashMap::new();
        for scan in rows {
            let scan = scan?;
            out.insert(scan.name.clone(), scan);
        }
        Ok(out)
    }

    pub fn counts(&self, install: &str) -> Result<Counts> {
        let workspaces: i64 = self.conn.query_row(
            "SELECT COUNT(*) FROM workspaces WHERE install = ?1",
            [install],
            |row| row.get(0),
        )?;
        let (chats, subagents): (i64, i64) = self.conn.query_row(
            "SELECT COALESCE(SUM(is_subagent = 0), 0), COALESCE(SUM(is_subagent != 0), 0) FROM chats WHERE install = ?1",
            [install],
            |row| Ok((row.get(0)?, row.get(1)?)),
        )?;
        Ok(Counts {
            workspaces: workspaces as usize,
            chats: chats as usize,
            subagents: subagents as usize,
        })
    }

    pub fn sources(&self, install: &str) -> Result<Vec<Source>> {
        let mut stmt = self.conn.prepare_cached(
            "SELECT key, workspace, size, mtime FROM sources WHERE install = ?1 ORDER BY key",
        )?;
        let rows = stmt.query_map([install], |row| {
            Ok(Source {
                key: row.get(0)?,
                workspace: row.get(1)?,
                stamp: Stamp {
                    size: row.get(2)?,
                    mtime: row.get(3)?,
                },
            })
        })?;
        Ok(rows.collect::<Result<_, _>>()?)
    }

    fn profiles(&self, install: &str) -> Result<Vec<UserProfile>> {
        let mut stmt = self
            .conn
            .prepare_cached("SELECT id, name FROM profiles WHERE install = ?1 ORDER BY position")?;
        let rows = stmt.query_map([install], |row| {
            Ok(UserProfile {
                id: row.get(0)?,
                name: row.get(1)?,
            })
        })?;
        Ok(rows.collect::<Result<_, _>>()?)
    }

    pub fn workspaces(&self, layout: &Layout) -> Result<Vec<WorkspaceFacts>> {
        let mut stmt = self.conn.prepare_cached(
            "SELECT id, kind, uri, path, profile, dest_missing, size, local_db FROM workspaces WHERE install = ?1 ORDER BY id",
        )?;
        let rows = stmt.query_map([&layout.name], |row| {
            let id: String = row.get(0)?;
            let kind: String = row.get(1)?;
            let path: Option<String> = row.get(3)?;
            Ok(WorkspaceFacts {
                workspace: Workspace {
                    dir: layout.workspace_storage().join(&id),
                    id,
                    kind: Kind::from_label(&kind).unwrap_or(Kind::EmptyWindow),
                    uri: row.get(2)?,
                    path: path.map(PathBuf::from),
                    install: layout.name.clone(),
                    profile: row.get(4)?,
                    destination_missing: row.get(5)?,
                },
                size: unsigned(row.get(6)?),
                local_db: row.get::<_, Option<i64>>(7)?.map(unsigned),
            })
        })?;
        Ok(rows.collect::<Result<_, _>>()?)
    }

    fn chats(&self, install: &str) -> Result<Vec<(ChatFacts, String)>> {
        let mut stmt = self.conn.prepare_cached(
            "SELECT id, workspace_id, is_subagent, is_archived, created_at, last_updated_at, title, \
             origin_kind, origin_path, fingerprint, has_data, model, cost_cents, requests, \
             context_tokens, input_tokens, output_tokens FROM chats WHERE install = ?1 ORDER BY id",
        )?;
        let rows = stmt.query_map([install], |row| {
            let origin_kind: Option<String> = row.get(7)?;
            let origin_path: Option<String> = row.get(8)?;
            let has_data: bool = row.get(10)?;
            let usage = if has_data {
                Some(ChatUsage {
                    model: row.get(11)?,
                    cost_cents: unsigned(row.get(12)?),
                    requests: unsigned(row.get(13)?),
                    context_tokens: unsigned(row.get(14)?),
                    input_tokens: unsigned(row.get(15)?),
                    output_tokens: unsigned(row.get(16)?),
                })
            } else {
                None
            };
            Ok((
                ChatFacts {
                    id: row.get(0)?,
                    workspace_id: row.get(1)?,
                    is_subagent: row.get(2)?,
                    is_archived: row.get(3)?,
                    created_at: row.get(4)?,
                    last_updated_at: row.get(5)?,
                    title: row.get(6)?,
                    origin_kind: origin_kind.as_deref().and_then(Kind::from_label),
                    origin_path: origin_path.map(PathBuf::from),
                    usage,
                },
                row.get::<_, String>(9)?,
            ))
        })?;
        Ok(rows.collect::<Result<_, _>>()?)
    }

    /// Everything stored for one installation, read in one snapshot.
    pub fn load(&self, layout: &Layout) -> Result<Option<InstallFacts>> {
        let tx = self.conn.unchecked_transaction()?;
        let global_db: Option<Option<i64>> = tx
            .query_row(
                "SELECT global_db FROM installations WHERE name = ?1",
                [&layout.name],
                |row| row.get(0),
            )
            .optional()?;
        let Some(global_db) = global_db else {
            return Ok(None);
        };
        let facts = InstallFacts {
            name: layout.name.clone(),
            root: layout.cursor_root.clone(),
            global_db: global_db.map(unsigned),
            profiles: self.profiles(&layout.name)?,
            workspaces: self.workspaces(layout)?,
            chats: self
                .chats(&layout.name)?
                .into_iter()
                .map(|(chat, _)| chat)
                .collect(),
        };
        tx.commit()?;
        Ok(Some(facts))
    }

    /// Stored rows of an installation, or nothing when it was scanned under another root.
    pub fn prior(&self, layout: &Layout) -> Result<Prior> {
        let tx = self.conn.unchecked_transaction()?;
        let root: Option<String> = tx
            .query_row(
                "SELECT root FROM installations WHERE name = ?1",
                [&layout.name],
                |row| row.get(0),
            )
            .optional()?;
        if root.as_deref() != Some(root_text(layout).as_str()) {
            return Ok(Prior::default());
        }
        let prior = Prior {
            known: true,
            profiles: self.profiles(&layout.name)?,
            workspaces: self
                .workspaces(layout)?
                .into_iter()
                .map(|facts| (facts.workspace.id.clone(), facts))
                .collect(),
            chats: self
                .chats(&layout.name)?
                .into_iter()
                .map(|(chat, fingerprint)| (chat.id.clone(), (chat, fingerprint)))
                .collect(),
            sources: self
                .sources(&layout.name)?
                .into_iter()
                .map(|source| (source.key, source.stamp))
                .collect(),
        };
        tx.commit()?;
        Ok(prior)
    }

    /// Store one refresh in a single transaction. A crepath write that landed after the
    /// refresh started keeps the installation marked dirty.
    pub fn apply(&self, layout: &Layout, update: &Update) -> Result<()> {
        let tx = self.write()?;
        let install = layout.name.as_str();
        tx.execute(
            "INSERT INTO installations (name, root, scanned_at, duration_ms, dirty_at, global_db) \
             VALUES (?1, ?2, ?3, ?4, NULL, ?5) ON CONFLICT(name) DO UPDATE SET \
             root = excluded.root, scanned_at = excluded.scanned_at, duration_ms = excluded.duration_ms, \
             global_db = excluded.global_db, \
             dirty_at = CASE WHEN installations.dirty_at >= excluded.scanned_at THEN installations.dirty_at ELSE NULL END",
            params![
                install,
                root_text(layout),
                update.scanned_at,
                update.duration_ms,
                size(update.global_db)
            ],
        )?;
        for table in ["profiles", "workspaces", "sources"] {
            tx.execute(
                &format!("DELETE FROM {table} WHERE install = ?1"),
                [install],
            )?;
        }
        if update.replace_chats {
            tx.execute("DELETE FROM chats WHERE install = ?1", [install])?;
        }
        {
            let mut stmt = tx.prepare_cached(
                "INSERT INTO profiles (install, position, id, name) VALUES (?1, ?2, ?3, ?4)",
            )?;
            for (position, profile) in update.profiles.iter().enumerate() {
                stmt.execute(params![install, position as i64, profile.id, profile.name])?;
            }
        }
        {
            let mut stmt = tx.prepare_cached(
                "INSERT INTO workspaces (install, id, kind, uri, path, profile, dest_missing, size, local_db) \
                 VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9)",
            )?;
            for facts in &update.workspaces {
                let workspace = &facts.workspace;
                stmt.execute(params![
                    install,
                    workspace.id,
                    workspace.kind.label(),
                    workspace.uri,
                    workspace
                        .path
                        .as_ref()
                        .map(|path| path.to_string_lossy().to_string()),
                    workspace.profile,
                    workspace.destination_missing,
                    size(Some(facts.size)),
                    size(facts.local_db),
                ])?;
            }
        }
        {
            let mut stmt = tx.prepare_cached(
                "INSERT INTO sources (install, key, workspace, size, mtime) VALUES (?1, ?2, ?3, ?4, ?5)",
            )?;
            for source in &update.sources {
                stmt.execute(params![
                    install,
                    source.key,
                    source.workspace,
                    source.stamp.size,
                    source.stamp.mtime
                ])?;
            }
        }
        upsert_chats(&tx, install, &update.upserts)?;
        {
            let mut chats =
                tx.prepare_cached("DELETE FROM chats WHERE install = ?1 AND id = ?2")?;
            let mut evidence =
                tx.prepare_cached("DELETE FROM evidence WHERE install = ?1 AND id = ?2")?;
            for id in &update.removed {
                chats.execute(params![install, id])?;
                evidence.execute(params![install, id])?;
            }
        }
        tx.execute(
            "INSERT INTO meta (key, value) VALUES ('refreshed_at', ?1) ON CONFLICT(key) DO UPDATE SET value = excluded.value",
            [crate::engine::index::now_ms().to_string()],
        )?;
        tx.commit()?;
        Ok(())
    }

    /// Drop every row of `install`.
    pub fn forget(&self, install: &str) -> Result<()> {
        let tx = self.write()?;
        for table in INSTALL_TABLES {
            let column = if *table == "installations" {
                "name"
            } else {
                "install"
            };
            tx.execute(
                &format!("DELETE FROM {table} WHERE {column} = ?1"),
                [install],
            )?;
        }
        tx.commit()?;
        Ok(())
    }

    /// Installation names with rows in any table.
    pub fn installs(&self) -> Result<Vec<String>> {
        let mut stmt = self.conn.prepare(
            "SELECT name FROM installations UNION SELECT install FROM workspaces \
             UNION SELECT install FROM chats UNION SELECT install FROM evidence ORDER BY 1",
        )?;
        let rows = stmt.query_map([], |row| row.get(0))?;
        Ok(rows.collect::<Result<_, _>>()?)
    }

    /// Drop installations that are no longer on disk.
    pub fn retain(&self, keep: &[String]) -> Result<()> {
        for name in self.installs()? {
            if !keep.contains(&name) {
                self.forget(&name)?;
            }
        }
        Ok(())
    }

    /// Mark an installation as changed by crepath and drop split evidence of `chats`.
    pub fn invalidate(&self, install: &str, chats: &[String], at: i64) -> Result<()> {
        let tx = self.write()?;
        tx.execute(
            "UPDATE installations SET dirty_at = ?2 WHERE name = ?1",
            params![install, at],
        )?;
        {
            let mut stmt =
                tx.prepare_cached("DELETE FROM evidence WHERE install = ?1 AND id = ?2")?;
            for id in chats {
                stmt.execute(params![install, id])?;
            }
        }
        tx.commit()?;
        Ok(())
    }

    pub fn evidence(&self, install: &str, ids: &[String]) -> Result<Evidence> {
        let mut stmt = self.conn.prepare_cached(
            "SELECT last_updated_at, paths FROM evidence WHERE install = ?1 AND id = ?2",
        )?;
        let mut out = HashMap::new();
        for id in ids {
            let row: Option<(Option<i64>, String)> = stmt
                .query_row(params![install, id], |row| Ok((row.get(0)?, row.get(1)?)))
                .optional()?;
            if let Some((stamp, raw)) = row
                && let Ok(paths) = serde_json::from_str::<Vec<String>>(&raw)
            {
                out.insert(
                    id.clone(),
                    (stamp, paths.into_iter().map(PathBuf::from).collect()),
                );
            }
        }
        Ok(out)
    }

    pub fn store_evidence(
        &self,
        install: &str,
        workspace_id: &str,
        rows: &[(String, Option<i64>, Vec<PathBuf>)],
    ) -> Result<()> {
        if rows.is_empty() {
            return Ok(());
        }
        let tx = self.write()?;
        {
            let mut stmt = tx.prepare_cached(
                "INSERT INTO evidence (install, id, workspace_id, last_updated_at, paths) VALUES (?1, ?2, ?3, ?4, ?5) \
                 ON CONFLICT(install, id) DO UPDATE SET workspace_id = excluded.workspace_id, \
                 last_updated_at = excluded.last_updated_at, paths = excluded.paths",
            )?;
            for (id, stamp, paths) in rows {
                let paths: Vec<String> = paths
                    .iter()
                    .map(|path| path.to_string_lossy().to_string())
                    .collect();
                stmt.execute(params![
                    install,
                    id,
                    workspace_id,
                    stamp,
                    serde_json::to_string(&paths)?
                ])?;
            }
        }
        tx.commit()?;
        Ok(())
    }
}

fn upsert_chats(tx: &Transaction<'_>, install: &str, rows: &[(ChatFacts, String)]) -> Result<()> {
    let mut stmt = tx.prepare_cached(
        "INSERT OR REPLACE INTO chats (install, id, workspace_id, is_subagent, is_archived, created_at, \
         last_updated_at, title, origin_kind, origin_path, fingerprint, has_data, model, cost_cents, \
         requests, context_tokens, input_tokens, output_tokens) \
         VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10, ?11, ?12, ?13, ?14, ?15, ?16, ?17, ?18)",
    )?;
    let count = |value: u64| i64::try_from(value).unwrap_or(i64::MAX);
    for (chat, fingerprint) in rows {
        let usage = chat.usage.clone().unwrap_or_default();
        stmt.execute(params![
            install,
            chat.id,
            chat.workspace_id,
            chat.is_subagent,
            chat.is_archived,
            chat.created_at,
            chat.last_updated_at,
            chat.title,
            chat.origin_kind.as_ref().map(Kind::label),
            chat.origin_path
                .as_ref()
                .map(|path| path.to_string_lossy().to_string()),
            fingerprint,
            chat.usage.is_some(),
            usage.model,
            count(usage.cost_cents),
            count(usage.requests),
            count(usage.context_tokens),
            count(usage.input_tokens),
            count(usage.output_tokens),
        ])?;
    }
    Ok(())
}

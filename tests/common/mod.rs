#![allow(dead_code)]

use std::collections::BTreeMap;
use std::fs;
use std::path::{Path, PathBuf};
use std::sync::Arc;

use crepath::cursor::registry::{GLOBAL_SCHEMA, LOCAL_SCHEMA};
use crepath::cursor::uri::{Platform, fs_path, normalize_path, path_uri, uri_parts};
use crepath::engine::{FixedProbe, Layout, Probe, Runtime};
use rusqlite::types::Value;
use rusqlite::{Connection, params};
use serde_json::{Value as Json, json};
use tempfile::TempDir;

pub struct Home {
    pub _tmp: TempDir,
    pub root: PathBuf,
    pub layout: Layout,
}

pub fn disable_update_check() {
    static ONCE: std::sync::Once = std::sync::Once::new();
    ONCE.call_once(|| unsafe {
        std::env::set_var("CREPATH_NO_UPDATE_CHECK", "1");
    });
}

pub fn cursor_home() -> Home {
    disable_update_check();
    let tmp = TempDir::new().unwrap();
    let root = tmp.path().to_path_buf();
    let layout = install_layout(&root, "default", root.join("Cursor"));
    Home {
        _tmp: tmp,
        root,
        layout,
    }
}

/// A second (or further) installation that shares the projects and crepath dirs.
pub fn install_layout(root: &Path, name: &str, cursor_root: PathBuf) -> Layout {
    let layout = Layout {
        name: name.to_string(),
        cursor_root,
        projects_dir: root.join("dot-cursor/projects"),
        crepath_home: root.join("dot-crepath"),
    };
    fs::create_dir_all(layout.workspace_storage()).unwrap();
    fs::create_dir_all(layout.global_storage()).unwrap();
    fs::create_dir_all(&layout.projects_dir).unwrap();
    let conn = Connection::open(layout.global_db()).unwrap();
    conn.pragma_update(None, "journal_mode", "WAL").unwrap();
    conn.execute_batch(GLOBAL_SCHEMA).unwrap();
    drop(conn);
    layout
}

pub fn runtime(layout: &Layout, running: bool, dry_run: bool) -> Runtime {
    runtime_with(layout, Arc::new(FixedProbe(running)), dry_run)
}

pub fn runtime_with(layout: &Layout, probe: Arc<dyn Probe>, dry_run: bool) -> Runtime {
    Runtime {
        layout: layout.clone(),
        installs: vec![layout.clone()],
        pinned: false,
        dry_run,
        yes: true,
        profile: None,
        probe,
        quiet: true,
    }
}

pub fn uri(path: &Path) -> String {
    path_uri(path)
}

pub fn components(path: &Path) -> Json {
    let path = normalize_path(path);
    let text = path.to_string_lossy();
    json!({
        "$mid": 1,
        "fsPath": fs_path(Platform::current(), &text),
        "external": uri(&path),
        "path": uri_parts(Platform::current(), &text).1,
        "scheme": "file",
    })
}

pub fn folder_identity(id: &str, path: &Path) -> Json {
    json!({"id": id, "uri": components(path)})
}

pub const A: &str = "aaaaaaaa-0000-4000-8000-000000000001";
pub const B: &str = "bbbbbbbb-0000-4000-8000-000000000002";
pub const C: &str = "cccccccc-0000-4000-8000-000000000003";
pub const SUB: &str = "dddddddd-0000-4000-8000-000000000004";

pub fn write_workspace(layout: &Layout, id: &str, key: &str, target: &Path) -> PathBuf {
    let dir = layout.workspace_storage().join(id);
    fs::create_dir_all(&dir).unwrap();
    fs::write(
        dir.join("workspace.json"),
        serde_json::to_string_pretty(&json!({ key: uri(target) })).unwrap(),
    )
    .unwrap();
    let conn = Connection::open(dir.join("state.vscdb")).unwrap();
    conn.pragma_update(None, "journal_mode", "WAL").unwrap();
    conn.execute_batch(LOCAL_SCHEMA).unwrap();
    dir
}

pub fn write_folder_workspace(layout: &Layout, id: &str, folder: &Path) -> PathBuf {
    write_workspace(layout, id, "folder", folder)
}

pub fn set_local(layout: &Layout, id: &str, selected: &[&str], pinned: &[&str]) {
    let conn = Connection::open(layout.workspace_storage().join(id).join("state.vscdb")).unwrap();
    conn.execute(
        "INSERT INTO ItemTable (key, value) VALUES ('composer.composerData', ?1)",
        params![
            json!({
                "selectedComposerIds": selected,
                "lastFocusedComposerIds": selected,
                "hasMigratedComposerData": true
            })
            .to_string()
        ],
    )
    .unwrap();
    conn.execute(
        "INSERT INTO ItemTable (key, value) VALUES ('cursor/pinnedComposers', ?1)",
        params![json!(pinned).to_string()],
    )
    .unwrap();
}

pub fn local_ids(layout: &Layout, id: &str) -> (Vec<String>, Vec<String>) {
    let conn = Connection::open(layout.workspace_storage().join(id).join("state.vscdb")).unwrap();
    let read = |key: &str| -> Option<String> {
        conn.query_row("SELECT value FROM ItemTable WHERE key = ?1", [key], |row| {
            row.get(0)
        })
        .ok()
    };
    let data: Json = read("composer.composerData")
        .map(|raw| serde_json::from_str(&raw).unwrap())
        .unwrap_or(Json::Null);
    let pinned: Json = read("cursor/pinnedComposers")
        .map(|raw| serde_json::from_str(&raw).unwrap())
        .unwrap_or(Json::Null);
    let list = |value: &Json| -> Vec<String> {
        value
            .as_array()
            .map(|items| {
                items
                    .iter()
                    .map(|v| v.as_str().unwrap().to_string())
                    .collect()
            })
            .unwrap_or_default()
    };
    (list(&data["selectedComposerIds"]), list(&pinned))
}

pub fn global(layout: &Layout) -> Connection {
    Connection::open(layout.global_db()).unwrap()
}

pub fn insert_header(layout: &Layout, id: &str, workspace: &str, sub: bool, value: &Json) {
    global(layout)
        .execute(
            "INSERT INTO composerHeaders (composerId, workspaceId, createdAt, lastUpdatedAt, isArchived, isSubagent, recency, checkpointAt, value, subagentTypeName) \
             VALUES (?1, ?2, 1700000000000, 1700000100000, 0, ?3, 1, NULL, ?4, NULL)",
            params![id, workspace, if sub { 1 } else { 0 }, value.to_string()],
        )
        .unwrap();
}

pub fn chat(layout: &Layout, id: &str, workspace: &str, identity: &Json) {
    insert_header(
        layout,
        id,
        workspace,
        false,
        &json!({"name": id, "workspaceIdentifier": identity}),
    );
    kv(
        layout,
        &format!("composerData:{id}"),
        Value::Text(json!({"composerId": id, "workspaceIdentifier": identity}).to_string()),
    );
}

pub fn kv(layout: &Layout, key: &str, value: Value) {
    global(layout)
        .execute(
            "INSERT INTO cursorDiskKV (key, value) VALUES (?1, ?2)",
            params![key, value],
        )
        .unwrap();
}

pub fn kv_text(layout: &Layout, key: &str, value: &str) {
    kv(layout, key, Value::Text(value.to_string()));
}

pub fn kv_get(layout: &Layout, key: &str) -> Option<Value> {
    global(layout)
        .query_row(
            "SELECT value FROM cursorDiskKV WHERE key = ?1",
            params![key],
            |row| row.get(0),
        )
        .ok()
}

pub fn kv_str(layout: &Layout, key: &str) -> Option<String> {
    match kv_get(layout, key)? {
        Value::Text(text) => Some(text),
        Value::Blob(bytes) => Some(String::from_utf8(bytes).unwrap()),
        other => panic!("{key} is {other:?}"),
    }
}

pub fn header_ids(layout: &Layout, workspace: &str) -> Vec<String> {
    let conn = global(layout);
    let mut stmt = conn
        .prepare(
            "SELECT composerId FROM composerHeaders WHERE workspaceId = ?1 ORDER BY composerId",
        )
        .unwrap();
    stmt.query_map(params![workspace], |row| row.get(0))
        .unwrap()
        .map(|row| row.unwrap())
        .collect()
}

pub fn header_value(layout: &Layout, id: &str) -> Json {
    let raw: String = global(layout)
        .query_row(
            "SELECT value FROM composerHeaders WHERE composerId = ?1",
            [id],
            |row| row.get(0),
        )
        .unwrap();
    serde_json::from_str(&raw).unwrap()
}

pub fn dump_db(path: &Path) -> Vec<String> {
    let conn = Connection::open(path).unwrap();
    let mut out = Vec::new();
    let tables: Vec<String> = conn
        .prepare("SELECT name FROM sqlite_master WHERE type = 'table' ORDER BY name")
        .unwrap()
        .query_map([], |row| row.get(0))
        .unwrap()
        .map(|row| row.unwrap())
        .collect();
    for table in tables {
        let mut stmt = conn
            .prepare(&format!("SELECT * FROM {table} ORDER BY 1"))
            .unwrap();
        let columns = stmt.column_count();
        let mut rows = stmt.query([]).unwrap();
        while let Some(row) = rows.next().unwrap() {
            let values: Vec<String> = (0..columns)
                .map(|index| format!("{:?}", row.get::<_, Value>(index).unwrap()))
                .collect();
            out.push(format!("{table}|{}", values.join("|")));
        }
    }
    out
}

/// Every directory and file (with bytes) under the home.
pub fn snapshot(home: &Home) -> BTreeMap<String, Option<Vec<u8>>> {
    let mut out = BTreeMap::new();
    for entry in walkdir::WalkDir::new(&home.root).sort_by_file_name() {
        let entry = entry.unwrap();
        let rel = entry
            .path()
            .strip_prefix(&home.root)
            .unwrap()
            .to_string_lossy()
            .to_string();
        if entry.file_type().is_dir() {
            out.insert(rel, None);
        } else {
            out.insert(rel, Some(fs::read(entry.path()).unwrap()));
        }
    }
    out
}

pub fn assert_same(
    before: &BTreeMap<String, Option<Vec<u8>>>,
    after: &BTreeMap<String, Option<Vec<u8>>>,
    what: &str,
) {
    let added: Vec<&String> = after
        .keys()
        .filter(|key| !before.contains_key(*key))
        .collect();
    let removed: Vec<&String> = before
        .keys()
        .filter(|key| !after.contains_key(*key))
        .collect();
    let changed: Vec<&String> = before
        .iter()
        .filter(|(key, value)| after.get(*key).is_some_and(|other| other != *value))
        .map(|(key, _)| key)
        .collect();
    assert!(
        added.is_empty() && removed.is_empty() && changed.is_empty(),
        "{what} changed the layout: added {added:?}, removed {removed:?}, changed {changed:?}"
    );
}

pub fn backups_empty(layout: &Layout) -> bool {
    let root = layout.backup_root();
    !root.exists() || fs::read_dir(root).unwrap().next().is_none()
}

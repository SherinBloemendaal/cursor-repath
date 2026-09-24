use std::collections::BTreeMap;
use std::fs;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};

use crepath::cursor::registry::{self, ComposerHeader};
use crepath::cursor::rewrite::Replacement;
use crepath::cursor::storage::rewrite_storage_json;
use crepath::cursor::workspace::compute_workspace_hash;
use crepath::engine::{
    self, FixedProbe, Layout, Probe, Runtime, combine_workspaces, load_registry, move_paths,
    rewrite_workspace, save_unsaved, split_workspace, suggest_split,
};
use rusqlite::params;
use serde_json::json;
use tempfile::TempDir;
use url::Url;

struct Home {
    _tmp: TempDir,
    root: PathBuf,
    layout: Layout,
}

fn disable_update_check() {
    static ONCE: std::sync::Once = std::sync::Once::new();
    ONCE.call_once(|| unsafe {
        std::env::set_var("CREPATH_NO_UPDATE_CHECK", "1");
    });
}

fn cursor_home() -> Home {
    disable_update_check();
    let tmp = TempDir::new().unwrap();
    let root = tmp.path().to_path_buf();
    let layout = Layout {
        cursor_root: root.join("Cursor"),
        projects_dir: root.join("projects"),
        crepath_home: root.join("crepath"),
    };
    fs::create_dir_all(layout.workspace_storage()).unwrap();
    fs::create_dir_all(layout.global_storage()).unwrap();
    fs::create_dir_all(&layout.projects_dir).unwrap();
    let conn =
        registry::ensure_global_schema(&rusqlite::Connection::open(layout.global_db()).unwrap())
            .ok();
    let _ = conn;
    registry::ensure_global_schema(&rusqlite::Connection::open(layout.global_db()).unwrap())
        .unwrap();
    Home {
        _tmp: tmp,
        root,
        layout,
    }
}

fn runtime(layout: &Layout, running: bool, dry_run: bool) -> Runtime {
    Runtime {
        layout: layout.clone(),
        dry_run,
        yes: true,
        profile: None,
        probe: Arc::new(FixedProbe(running)),
        quiet: true,
    }
}

fn file_uri(path: &Path) -> String {
    Url::from_file_path(path).unwrap().to_string()
}

fn write_folder_workspace(layout: &Layout, id: &str, folder: &Path) {
    let dir = layout.workspace_storage().join(id);
    fs::create_dir_all(&dir).unwrap();
    fs::write(
        dir.join("workspace.json"),
        json!({ "folder": file_uri(folder) }).to_string(),
    )
    .unwrap();
    let conn = rusqlite::Connection::open(dir.join("state.vscdb")).unwrap();
    registry::ensure_local_schema(&conn).unwrap();
}

fn insert_header(layout: &Layout, header: &ComposerHeader) {
    let conn = rusqlite::Connection::open(layout.global_db()).unwrap();
    conn.execute(
        "INSERT INTO composerHeaders (composerId, workspaceId, createdAt, lastUpdatedAt, isArchived, isSubagent, recency, checkpointAt, value, subagentTypeName) \
         VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10)",
        params![
            header.composer_id,
            header.workspace_id,
            header.created_at,
            header.last_updated_at,
            if header.is_archived { 1 } else { 0 },
            if header.is_subagent { 1 } else { 0 },
            header.recency,
            header.checkpoint_at,
            header.value,
            header.subagent_type_name,
        ],
    )
    .unwrap();
}

fn header(id: &str, workspace: &str, sub: bool, value: &str) -> ComposerHeader {
    ComposerHeader {
        composer_id: id.to_string(),
        workspace_id: workspace.to_string(),
        created_at: Some(1_700_000_000_000),
        last_updated_at: Some(1_700_000_100_000),
        is_archived: false,
        is_subagent: sub,
        recency: Some(1),
        checkpoint_at: None,
        value: value.to_string(),
        subagent_type_name: None,
        title: Some(id.to_string()),
    }
}

fn kv(layout: &Layout, key: &str, value: &str) {
    let conn = rusqlite::Connection::open(layout.global_db()).unwrap();
    conn.execute(
        "INSERT INTO cursorDiskKV (key, value) VALUES (?1, ?2)",
        params![key, value],
    )
    .unwrap();
}

fn kv_get(layout: &Layout, key: &str) -> Option<String> {
    let conn = rusqlite::Connection::open(layout.global_db()).unwrap();
    conn.query_row(
        "SELECT value FROM cursorDiskKV WHERE key = ?1",
        params![key],
        |row| row.get(0),
    )
    .ok()
}

fn header_ids(layout: &Layout, workspace: &str) -> Vec<String> {
    let conn = rusqlite::Connection::open(layout.global_db()).unwrap();
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

#[test]
fn mv_replace_skips_missing_destination_with_warning() {
    let home = cursor_home();
    let one = home.root.join("noble/one");
    let two = home.root.join("noble/two");
    let dest_one = home.root.join("resolute/one");
    fs::create_dir_all(&one).unwrap();
    fs::create_dir_all(&two).unwrap();
    fs::create_dir_all(&dest_one).unwrap();
    write_folder_workspace(&home.layout, "hash-one", &one);
    write_folder_workspace(&home.layout, "hash-two", &two);
    let value = json!({
        "name": "one",
        "workspaceIdentifier": {"id": "hash-one", "uri": file_uri(&one)}
    })
    .to_string();
    insert_header(&home.layout, &header("chat-one", "hash-one", false, &value));
    kv(
        &home.layout,
        "bubbleId:chat-one:b1",
        &format!("path {}", one.display()),
    );
    kv(
        &home.layout,
        "bubbleId:chat-two:b1",
        &format!("path {}", two.display()),
    );
    insert_header(
        &home.layout,
        &header(
            "chat-two",
            "hash-two",
            false,
            &json!({"name":"two","workspaceIdentifier":{"id":"hash-two","uri":file_uri(&two)}})
                .to_string(),
        ),
    );

    let rt = runtime(&home.layout, false, false);
    let report = move_paths(&rt, &[], Some(("/noble/", "/resolute/")), false, false).unwrap();

    assert!(
        report
            .warnings
            .iter()
            .any(|warning| warning.contains("hash-two"))
    );
    assert!(report.skipped.iter().any(|id| id == "hash-two"));
    assert!(report.applied.iter().any(|item| item.contains("hash-one")));
    let new_hash = compute_workspace_hash(&dest_one).unwrap();
    let raw = fs::read_to_string(
        home.layout
            .workspace_storage()
            .join(&new_hash)
            .join("workspace.json"),
    )
    .unwrap();
    assert!(raw.contains(&file_uri(&dest_one)));
    assert!(
        home.layout
            .workspace_storage()
            .join("hash-two")
            .join("workspace.json")
            .exists()
    );
    let skipped = fs::read_to_string(
        home.layout
            .workspace_storage()
            .join("hash-two")
            .join("workspace.json"),
    )
    .unwrap();
    assert!(skipped.contains(&file_uri(&two)));
    let moved = kv_get(&home.layout, "bubbleId:chat-one:b1").unwrap();
    assert!(moved.contains(&dest_one.display().to_string()));
    assert!(!moved.contains(&one.display().to_string()));
    assert!(
        kv_get(&home.layout, "bubbleId:chat-two:b1")
            .unwrap()
            .contains(&two.display().to_string())
    );
}

#[test]
fn save_unsaved_workspace_writes_folder_uri() {
    let home = cursor_home();
    let id = "1765558213752";
    let unsaved = home.layout.cursor_root.join("Workspaces").join(id);
    fs::create_dir_all(&unsaved).unwrap();
    fs::write(unsaved.join("workspace.json"), "{}\n").unwrap();
    let storage_id = "unsaved-hash";
    let dir = home.layout.workspace_storage().join(storage_id);
    fs::create_dir_all(&dir).unwrap();
    let unsaved_file = unsaved.join("workspace.json");
    fs::write(
        dir.join("workspace.json"),
        json!({ "workspace": file_uri(&unsaved_file) }).to_string(),
    )
    .unwrap();
    let dest = home.root.join("projects-foo");
    fs::create_dir_all(&dest).unwrap();

    let rt = runtime(&home.layout, false, false);
    save_unsaved(&rt, id, &dest).unwrap();

    let hash = compute_workspace_hash(&dest).unwrap();
    let raw = fs::read_to_string(
        home.layout
            .workspace_storage()
            .join(hash)
            .join("workspace.json"),
    )
    .unwrap();
    let json: serde_json::Value = serde_json::from_str(&raw).unwrap();
    assert_eq!(json["folder"], file_uri(&dest));
    assert!(json.get("workspace").is_none());
}

#[test]
fn split_autosuggest_blocks_unassigned_copy_keeps_source_and_move_empties() {
    let home = cursor_home();
    let source_path = home.root.join("source");
    let api = home.root.join("api");
    let web = home.root.join("web");
    fs::create_dir_all(source_path.join("keep")).unwrap();
    fs::create_dir_all(api.join("src")).unwrap();
    fs::create_dir_all(web.join("src")).unwrap();
    write_folder_workspace(&home.layout, "source-hash", &source_path);
    let api_file = api.join("src/main.rs");
    fs::write(&api_file, "fn main() {}\n").unwrap();
    let web_file = web.join("src/new.ts");
    let ident = json!({"id":"source-hash","uri":file_uri(&source_path)}).to_string();
    insert_header(
        &home.layout,
        &header("chat-api", "source-hash", false, &json!({"name":"api","workspaceIdentifier":{"id":"source-hash","uri":file_uri(&source_path)}}).to_string()),
    );
    insert_header(
        &home.layout,
        &header("chat-web", "source-hash", false, &json!({"name":"web","workspaceIdentifier":{"id":"source-hash","uri":file_uri(&source_path)}}).to_string()),
    );
    insert_header(
        &home.layout,
        &header("chat-free", "source-hash", false, &json!({"name":"free","workspaceIdentifier":{"id":"source-hash","uri":file_uri(&source_path)}}).to_string()),
    );
    kv(
        &home.layout,
        &format!("ofsContent:chat-api:{}", file_uri(&api_file)),
        "body",
    );
    kv(
        &home.layout,
        "composerData:chat-web",
        &json!({
            "workspaceIdentifier": {"id":"source-hash","uri":file_uri(&source_path)},
            "newlyCreatedFiles": [web_file.display().to_string()],
            "trackedGitRepos": [home.root.display().to_string()]
        })
        .to_string(),
    );
    kv(
        &home.layout,
        "composerData:chat-api",
        &format!(r#"{{"workspaceIdentifier":{ident}}}"#),
    );
    kv(
        &home.layout,
        "composerData:chat-free",
        &format!(r#"{{"workspaceIdentifier":{ident}}}"#),
    );
    let _ = ident;

    let rt = runtime(&home.layout, false, false);
    let source = engine::find_workspace(&rt, "source-hash").unwrap().unwrap();
    let suggestion = suggest_split(&rt, &source, &[api.clone(), web.clone()]).unwrap();
    assert_eq!(
        suggestion.assigned.get("chat-api").map(|paths| paths.len()),
        Some(1)
    );
    assert!(suggestion.assigned["chat-api"][0].ends_with("api"));
    assert!(suggestion.assigned["chat-web"][0].ends_with("web"));
    assert!(suggestion.unassigned.iter().any(|id| id == "chat-free"));

    let err = split_workspace(
        &rt,
        "source-hash",
        &[api.clone(), web.clone()],
        &suggestion.assigned,
        false,
    )
    .unwrap_err();
    assert!(err.to_string().contains("unassigned"));
    assert_eq!(header_ids(&home.layout, "source-hash").len(), 3);

    let mut assigned = suggestion.assigned.clone();
    assigned.insert("chat-free".to_string(), vec![api.clone()]);
    split_workspace(
        &rt,
        "source-hash",
        &[api.clone(), web.clone()],
        &assigned,
        false,
    )
    .unwrap();
    assert_eq!(header_ids(&home.layout, "source-hash").len(), 3);
    let api_hash = compute_workspace_hash(&api).unwrap();
    let web_hash = compute_workspace_hash(&web).unwrap();
    assert!(!header_ids(&home.layout, &api_hash).is_empty());
    assert!(!header_ids(&home.layout, &web_hash).is_empty());

    let home = cursor_home();
    let source_path = home.root.join("source");
    let api = home.root.join("api");
    fs::create_dir_all(&source_path).unwrap();
    fs::create_dir_all(api.join("src")).unwrap();
    write_folder_workspace(&home.layout, "source-hash", &source_path);
    let parent = json!({
        "name":"parent",
        "subagentComposerIds":["chat-sub"],
        "workspaceIdentifier":{"id":"source-hash","uri":file_uri(&source_path)}
    });
    insert_header(
        &home.layout,
        &header("chat-parent", "source-hash", false, &parent.to_string()),
    );
    insert_header(
        &home.layout,
        &header(
            "chat-sub",
            "source-hash",
            true,
            &json!({"name":"sub","isSubagent":true,"workspaceIdentifier":{"id":"source-hash"}})
                .to_string(),
        ),
    );
    kv(
        &home.layout,
        "composerData:chat-parent",
        &parent.to_string(),
    );
    kv(
        &home.layout,
        "composerData:chat-sub",
        r#"{"isSubagent":true}"#,
    );
    let mut assigned = BTreeMap::new();
    assigned.insert("chat-parent".to_string(), vec![api.clone()]);
    let rt = runtime(&home.layout, false, false);
    split_workspace(
        &rt,
        "source-hash",
        std::slice::from_ref(&api),
        &assigned,
        true,
    )
    .unwrap();
    assert!(header_ids(&home.layout, "source-hash").is_empty());
    let api_hash = compute_workspace_hash(&api).unwrap();
    let moved = header_ids(&home.layout, &api_hash);
    assert!(moved.iter().any(|id| id == "chat-parent"));
    assert!(moved.iter().any(|id| id == "chat-sub"));
}

#[test]
fn combine_copy_clones_rows_move_reassigns_and_dedup_skips() {
    let home = cursor_home();
    let source_path = home.root.join("source");
    let target_path = home.root.join("target");
    fs::create_dir_all(&source_path).unwrap();
    fs::create_dir_all(&target_path).unwrap();
    write_folder_workspace(&home.layout, "source-hash", &source_path);
    write_folder_workspace(&home.layout, "target-hash", &target_path);
    let parent = json!({
        "name":"parent",
        "subagentComposerIds":["sub-1"],
        "workspaceIdentifier":{"id":"source-hash","uri":file_uri(&source_path)}
    });
    insert_header(
        &home.layout,
        &header("parent-1", "source-hash", false, &parent.to_string()),
    );
    insert_header(
        &home.layout,
        &header(
            "sub-1",
            "source-hash",
            true,
            &json!({"name":"sub","workspaceIdentifier":{"id":"source-hash"}}).to_string(),
        ),
    );
    for (prefix, key) in [
        ("composerData:", "parent-1"),
        ("composerVirtualRowHeights:", "parent-1"),
        ("bubbleId:", "parent-1:b"),
        ("checkpointId:", "parent-1:c"),
        ("codeBlockDiff:", "parent-1:d"),
        ("codeBlockPartialInlineDiffFates:", "parent-1:e"),
        ("messageRequestContext:", "parent-1:m"),
        ("ofsContent:", "parent-1:file:///tmp/a"),
    ] {
        kv(
            &home.layout,
            &format!("{prefix}{key}"),
            &format!(r#"{{"id":"parent-1","sub":"sub-1","prefix":"{prefix}"}}"#),
        );
    }
    kv(
        &home.layout,
        "composerData:sub-1",
        r#"{"composerId":"sub-1","parent":"parent-1"}"#,
    );
    kv(&home.layout, "agentKv:blob:abc", "shared-blob");

    let rt = runtime(&home.layout, false, false);
    let report =
        combine_workspaces(&rt, &target_path, &["source-hash".to_string()], false).unwrap();
    assert!(
        report
            .warnings
            .iter()
            .any(|warning| warning.contains("local state"))
    );
    assert_eq!(
        header_ids(&home.layout, "source-hash"),
        vec!["parent-1".to_string(), "sub-1".to_string()]
    );
    let target_ids = header_ids(&home.layout, "target-hash");
    assert_eq!(target_ids.len(), 2);
    assert!(
        !target_ids
            .iter()
            .any(|id| id == "parent-1" || id == "sub-1")
    );
    let conn = rusqlite::Connection::open(home.layout.global_db()).unwrap();
    for id in &target_ids {
        let sub: i64 = conn
            .query_row(
                "SELECT isSubagent FROM composerHeaders WHERE composerId = ?1",
                params![id],
                |row| row.get(0),
            )
            .unwrap();
        let value: String = conn
            .query_row(
                "SELECT value FROM composerHeaders WHERE composerId = ?1",
                params![id],
                |row| row.get(0),
            )
            .unwrap();
        if value.contains("parent") || sub == 0 {
            for prefix in [
                "composerData:",
                "composerVirtualRowHeights:",
                "bubbleId:",
                "checkpointId:",
                "codeBlockDiff:",
                "codeBlockPartialInlineDiffFates:",
                "messageRequestContext:",
                "ofsContent:",
            ] {
                let count: i64 = conn
                    .query_row(
                        "SELECT COUNT(*) FROM cursorDiskKV WHERE key = ?1 OR key LIKE ?2",
                        params![format!("{prefix}{id}"), format!("{prefix}{id}:%")],
                        |row| row.get(0),
                    )
                    .unwrap();
                if prefix == "composerData:" || prefix == "bubbleId:" || prefix == "ofsContent:" {
                    assert!(count >= 1, "missing {prefix} for {id}");
                }
            }
        }
        let _ = sub;
    }
    assert_eq!(
        kv_get(&home.layout, "agentKv:blob:abc").as_deref(),
        Some("shared-blob")
    );
    let blob_count: i64 = conn
        .query_row(
            "SELECT COUNT(*) FROM cursorDiskKV WHERE key = 'agentKv:blob:abc'",
            [],
            |row| row.get(0),
        )
        .unwrap();
    assert_eq!(blob_count, 1);

    let home = cursor_home();
    let source_path = home.root.join("source");
    let target_path = home.root.join("target");
    fs::create_dir_all(&source_path).unwrap();
    fs::create_dir_all(&target_path).unwrap();
    write_folder_workspace(&home.layout, "source-hash", &source_path);
    write_folder_workspace(&home.layout, "target-hash", &target_path);
    insert_header(
        &home.layout,
        &header(
            "keep-me",
            "source-hash",
            false,
            &json!({"workspaceIdentifier":{"id":"source-hash"}}).to_string(),
        ),
    );
    kv(
        &home.layout,
        "composerData:keep-me",
        r#"{"workspaceIdentifier":{"id":"source-hash"}}"#,
    );
    kv(&home.layout, "bubbleId:keep-me:b", "from-source");
    let rt = runtime(&home.layout, false, false);
    combine_workspaces(&rt, &target_path, &["source-hash".to_string()], true).unwrap();
    assert!(header_ids(&home.layout, "source-hash").is_empty());
    assert_eq!(
        header_ids(&home.layout, "target-hash"),
        vec!["keep-me".to_string()]
    );
    assert!(
        kv_get(&home.layout, "composerData:keep-me")
            .unwrap()
            .contains("target-hash")
    );
    assert_eq!(
        kv_get(&home.layout, "bubbleId:keep-me:b").as_deref(),
        Some("from-source")
    );

    let before = header_ids(&home.layout, "target-hash");
    let report = combine_workspaces(&rt, &target_path, &["keep-me".to_string()], false).unwrap();
    assert!(report.skipped.iter().any(|id| id == "keep-me"));
    assert_eq!(header_ids(&home.layout, "target-hash"), before);
}

#[test]
fn registry_reads_composer_headers_and_falls_back() {
    disable_update_check();
    let conn = rusqlite::Connection::open_in_memory().unwrap();
    registry::ensure_global_schema(&conn).unwrap();
    conn.execute(
        "INSERT INTO composerHeaders (composerId, workspaceId, createdAt, lastUpdatedAt, isArchived, isSubagent, recency, checkpointAt, value, subagentTypeName) VALUES ('from-table', 'ws', 1, 2, 0, 0, 3, NULL, '{\"name\":\"Table\"}', NULL)",
        [],
    )
    .unwrap();
    conn.execute(
        "INSERT INTO ItemTable (key, value) VALUES ('composer.composerHeaders', ?1)",
        params![
            r#"{"allComposers":[{"composerId":"from-legacy","workspaceIdentifier":{"id":"ws"}}]}"#
        ],
    )
    .unwrap();
    let headers = load_registry(&conn).unwrap();
    assert_eq!(headers.len(), 1);
    assert_eq!(headers[0].composer_id, "from-table");

    let legacy = rusqlite::Connection::open_in_memory().unwrap();
    legacy
        .execute_batch(
            "CREATE TABLE ItemTable (key TEXT PRIMARY KEY, value TEXT);
             CREATE TABLE cursorDiskKV (key TEXT PRIMARY KEY, value TEXT);",
        )
        .unwrap();
    legacy
        .execute(
            "INSERT INTO ItemTable (key, value) VALUES ('composer.composerHeaders', ?1)",
            params![r#"{"allComposers":[{"composerId":"from-legacy","name":"Old","createdAt":4,"workspaceIdentifier":{"id":"ws-legacy"},"isArchived":false}]}"#],
        )
        .unwrap();
    let headers = load_registry(&legacy).unwrap();
    assert_eq!(headers.len(), 1);
    assert_eq!(headers[0].composer_id, "from-legacy");
    assert_eq!(headers[0].workspace_id, "ws-legacy");
    assert_eq!(headers[0].title.as_deref(), Some("Old"));
}

#[test]
fn indexed_rewrite_touches_only_workspace_and_skips_sibling_prefix() {
    let home = cursor_home();
    insert_header(
        &home.layout,
        &header(
            "chat-a",
            "ws-a",
            false,
            r#"{"workspaceIdentifier":{"id":"ws-a"}}"#,
        ),
    );
    insert_header(
        &home.layout,
        &header(
            "chat-b",
            "ws-b",
            false,
            r#"{"workspaceIdentifier":{"id":"ws-b"}}"#,
        ),
    );
    kv(
        &home.layout,
        "bubbleId:chat-a:1",
        r#"{"active":"/home/user/project","other":"/home/user/projects/foo"}"#,
    );
    kv(
        &home.layout,
        "bubbleId:chat-b:1",
        r#"{"active":"/home/user/project"}"#,
    );
    kv(&home.layout, "agentKv:blob:deadbeef", "/home/user/project");
    kv(&home.layout, "inlineDiff:oldhash:1", "/home/user/project");
    kv(&home.layout, "inlineDiff:other:1", "/home/user/project");
    kv(
        &home.layout,
        "patch-graph:oldhash:node",
        "/home/user/project",
    );
    let conn = rusqlite::Connection::open(home.layout.global_db()).unwrap();
    conn.execute(
        "INSERT INTO ItemTable (key, value) VALUES ('composer.planRegistry', '/home/user/project')",
        [],
    )
    .unwrap();
    conn.execute(
        "INSERT INTO ItemTable (key, value) VALUES ('unrelated.key', '/home/user/project')",
        [],
    )
    .unwrap();
    rewrite_workspace(
        &conn,
        "ws-a",
        &[Replacement::new(
            "/home/user/project",
            "/home/user/project-copy",
        )],
        "oldhash",
        "newhash",
        false,
    )
    .unwrap();
    let bubble = kv_get(&home.layout, "bubbleId:chat-a:1").unwrap();
    assert!(bubble.contains("/home/user/project-copy"));
    assert!(bubble.contains("/home/user/projects/foo"));
    assert!(!bubble.contains("project-copy-copy"));
    assert_eq!(
        kv_get(&home.layout, "bubbleId:chat-b:1").as_deref(),
        Some(r#"{"active":"/home/user/project"}"#)
    );
    assert_eq!(
        kv_get(&home.layout, "agentKv:blob:deadbeef").as_deref(),
        Some("/home/user/project")
    );
    assert!(
        kv_get(&home.layout, "inlineDiff:newhash:1")
            .unwrap()
            .contains("project-copy")
    );
    assert!(kv_get(&home.layout, "inlineDiff:oldhash:1").is_none());
    assert_eq!(
        kv_get(&home.layout, "inlineDiff:other:1").as_deref(),
        Some("/home/user/project")
    );
    assert!(
        kv_get(&home.layout, "patch-graph:newhash:node")
            .unwrap()
            .contains("project-copy")
    );
    let plan: String = conn
        .query_row(
            "SELECT value FROM ItemTable WHERE key = 'composer.planRegistry'",
            [],
            |row| row.get(0),
        )
        .unwrap();
    let other: String = conn
        .query_row(
            "SELECT value FROM ItemTable WHERE key = 'unrelated.key'",
            [],
            |row| row.get(0),
        )
        .unwrap();
    assert_eq!(plan, "/home/user/project-copy");
    assert_eq!(other, "/home/user/project");
    let workspace: String = conn
        .query_row(
            "SELECT workspaceId FROM composerHeaders WHERE composerId = 'chat-a'",
            [],
            |row| row.get(0),
        )
        .unwrap();
    assert_eq!(workspace, "newhash");
    let sibling: String = conn
        .query_row(
            "SELECT workspaceId FROM composerHeaders WHERE composerId = 'chat-b'",
            [],
            |row| row.get(0),
        )
        .unwrap();
    assert_eq!(sibling, "ws-b");
}

#[test]
fn storage_json_full_walk_rewrites_known_fields() {
    disable_update_check();
    let tmp = TempDir::new().unwrap();
    let path = tmp.path().join("storage.json");
    let body = json!({
        "backupWorkspaces": {
            "folders": [{"folderUri": "file:///old/path"}],
            "workspaces": [{"configURIPath": "file:///old/path/app.code-workspace"}]
        },
        "profileAssociations": {
            "workspaces": {"file:///old/path": "__default__profile__"}
        },
        "windowsState": {
            "lastActiveWindow": {"folder": "file:///old/path"},
            "openedWindows": [{"folderUri": "file:///old/paths/foo"}]
        },
        "windowSplashWorkspaceOverride": {"workspace": {"configPath": "file:///old/path/app.code-workspace"}},
        "workspace": {"configPath": "file:///old/path/app.code-workspace"},
        "hashes": {"id": "hash-old", "neighbor": "hash-old-extra"}
    });
    fs::write(&path, serde_json::to_string_pretty(&body).unwrap()).unwrap();
    let keys = rewrite_storage_json(
        &path,
        &[
            Replacement::new("file:///old/path", "file:///new/path"),
            Replacement::new("hash-old", "hash-new"),
        ],
        false,
    )
    .unwrap();
    assert!(!keys.is_empty());
    let updated: serde_json::Value =
        serde_json::from_str(&fs::read_to_string(&path).unwrap()).unwrap();
    assert_eq!(
        updated["backupWorkspaces"]["folders"][0]["folderUri"],
        "file:///new/path"
    );
    assert_eq!(
        updated["backupWorkspaces"]["workspaces"][0]["configURIPath"],
        "file:///new/path/app.code-workspace"
    );
    assert_eq!(
        updated["profileAssociations"]["workspaces"]["file:///new/path"],
        "__default__profile__"
    );
    assert!(
        updated["profileAssociations"]["workspaces"]
            .get("file:///old/path")
            .is_none()
    );
    assert_eq!(
        updated["windowsState"]["lastActiveWindow"]["folder"],
        "file:///new/path"
    );
    assert_eq!(
        updated["windowsState"]["openedWindows"][0]["folderUri"],
        "file:///old/paths/foo"
    );
    assert_eq!(
        updated["windowSplashWorkspaceOverride"]["workspace"]["configPath"],
        "file:///new/path/app.code-workspace"
    );
    assert_eq!(
        updated["workspace"]["configPath"],
        "file:///new/path/app.code-workspace"
    );
    assert_eq!(updated["hashes"]["id"], "hash-new");
    assert_eq!(updated["hashes"]["neighbor"], "hash-old-extra");
}

#[test]
fn cursor_running_aborts() {
    let home = cursor_home();
    let from = home.root.join("from");
    let to = home.root.join("to");
    fs::create_dir_all(&from).unwrap();
    fs::create_dir_all(&to).unwrap();
    write_folder_workspace(&home.layout, "hash-from", &from);
    let before = fs::read(
        home.layout
            .workspace_storage()
            .join("hash-from")
            .join("workspace.json"),
    )
    .unwrap();
    let rt = runtime(&home.layout, true, false);
    let err = move_paths(
        &rt,
        &[("hash-from".into(), to.display().to_string())],
        None,
        false,
        false,
    )
    .unwrap_err();
    assert!(err.to_string().contains("Cursor is running"));
    let after = fs::read(
        home.layout
            .workspace_storage()
            .join("hash-from")
            .join("workspace.json"),
    )
    .unwrap();
    assert_eq!(before, after);
}

#[test]
fn dry_run_mutates_nothing() {
    let home = cursor_home();
    let from = home.root.join("from");
    let to = home.root.join("to");
    fs::create_dir_all(&from).unwrap();
    fs::create_dir_all(&to).unwrap();
    write_folder_workspace(&home.layout, "hash-from", &from);
    insert_header(
        &home.layout,
        &header(
            "chat",
            "hash-from",
            false,
            r#"{"workspaceIdentifier":{"id":"hash-from"}}"#,
        ),
    );
    kv(&home.layout, "bubbleId:chat:1", &from.display().to_string());
    let before_ws = fs::read(
        home.layout
            .workspace_storage()
            .join("hash-from")
            .join("workspace.json"),
    )
    .unwrap();
    let before_bubble = kv_get(&home.layout, "bubbleId:chat:1");
    let rt = runtime(&home.layout, false, true);
    let report = move_paths(
        &rt,
        &[("hash-from".into(), to.display().to_string())],
        None,
        false,
        false,
    )
    .unwrap();
    assert!(report.applied.iter().any(|item| item.contains("dry-run")));
    assert_eq!(
        fs::read(
            home.layout
                .workspace_storage()
                .join("hash-from")
                .join("workspace.json")
        )
        .unwrap(),
        before_ws
    );
    assert_eq!(kv_get(&home.layout, "bubbleId:chat:1"), before_bubble);
    assert!(compute_workspace_hash(&to).is_ok());
    assert!(
        !home
            .layout
            .workspace_storage()
            .join(compute_workspace_hash(&to).unwrap())
            .exists()
    );
}

#[test]
fn collision_refuses_overwrite() {
    let home = cursor_home();
    let from = home.root.join("from");
    let to = home.root.join("to");
    fs::create_dir_all(&from).unwrap();
    fs::create_dir_all(&to).unwrap();
    write_folder_workspace(&home.layout, "hash-from", &from);
    let occupied = compute_workspace_hash(&to).unwrap();
    write_folder_workspace(&home.layout, &occupied, &home.root.join("other"));
    let before = fs::read_to_string(
        home.layout
            .workspace_storage()
            .join("hash-from")
            .join("workspace.json"),
    )
    .unwrap();
    let rt = runtime(&home.layout, false, false);
    let err = move_paths(
        &rt,
        &[("hash-from".into(), to.display().to_string())],
        None,
        false,
        false,
    )
    .unwrap_err();
    assert!(err.to_string().contains("collision"));
    assert_eq!(
        fs::read_to_string(
            home.layout
                .workspace_storage()
                .join("hash-from")
                .join("workspace.json")
        )
        .unwrap(),
        before
    );
    let occupied_raw = fs::read_to_string(
        home.layout
            .workspace_storage()
            .join(&occupied)
            .join("workspace.json"),
    )
    .unwrap();
    assert!(!occupied_raw.contains(&file_uri(&to)));
}

struct AbortAfterWrite {
    db: PathBuf,
    db_bytes: Vec<u8>,
    storage: PathBuf,
    storage_bytes: Vec<u8>,
    tripped: AtomicBool,
}

impl Probe for AbortAfterWrite {
    fn running(&self) -> bool {
        if self.tripped.load(Ordering::SeqCst) {
            return true;
        }
        let db_changed = fs::read(&self.db).ok().as_deref() != Some(self.db_bytes.as_slice());
        let storage_changed =
            fs::read(&self.storage).ok().as_deref() != Some(self.storage_bytes.as_slice());
        if db_changed && storage_changed {
            self.tripped.store(true, Ordering::SeqCst);
            return true;
        }
        false
    }
}

#[test]
fn abort_after_commit_restores_global_db_bytes() {
    let home = cursor_home();
    let from = home.root.join("from");
    let to = home.root.join("to");
    fs::create_dir_all(&from).unwrap();
    fs::create_dir_all(&to).unwrap();
    write_folder_workspace(&home.layout, "hash-from", &from);
    insert_header(
        &home.layout,
        &header(
            "chat",
            "hash-from",
            false,
            &json!({
                "name": "chat",
                "workspaceIdentifier": {"id": "hash-from", "uri": file_uri(&from)}
            })
            .to_string(),
        ),
    );
    kv(&home.layout, "bubbleId:chat:1", &from.display().to_string());
    let storage = json!({
        "backupWorkspaces": {
            "folders": [{ "folderUri": file_uri(&from) }]
        }
    });
    fs::write(
        home.layout.storage_json(),
        serde_json::to_string_pretty(&storage).unwrap(),
    )
    .unwrap();
    let db_bytes = fs::read(home.layout.global_db()).unwrap();
    let storage_bytes = fs::read(home.layout.storage_json()).unwrap();
    let workspace_before = fs::read(
        home.layout
            .workspace_storage()
            .join("hash-from")
            .join("workspace.json"),
    )
    .unwrap();
    let rt = Runtime {
        layout: home.layout.clone(),
        dry_run: false,
        yes: true,
        profile: None,
        probe: Arc::new(AbortAfterWrite {
            db: home.layout.global_db(),
            db_bytes: db_bytes.clone(),
            storage: home.layout.storage_json(),
            storage_bytes: storage_bytes.clone(),
            tripped: AtomicBool::new(false),
        }),
        quiet: true,
    };
    let err = move_paths(
        &rt,
        &[("hash-from".into(), to.display().to_string())],
        None,
        false,
        false,
    )
    .unwrap_err();
    assert!(err.to_string().contains("Cursor is running"));
    assert_eq!(fs::read(home.layout.global_db()).unwrap(), db_bytes);
    assert_eq!(fs::read(home.layout.storage_json()).unwrap(), storage_bytes);
    assert_eq!(
        fs::read(
            home.layout
                .workspace_storage()
                .join("hash-from")
                .join("workspace.json")
        )
        .unwrap(),
        workspace_before
    );
    let relocated = home
        .layout
        .workspace_storage()
        .join(compute_workspace_hash(&to).unwrap());
    assert!(!relocated.exists());
    assert_eq!(
        header_ids(&home.layout, "hash-from"),
        vec!["chat".to_string()]
    );
}

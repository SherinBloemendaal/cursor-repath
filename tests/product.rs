mod common;

use std::collections::BTreeMap;
use std::fs;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};

use common::*;
use crepath::cursor::registry::{self, GLOBAL_SCHEMA};
use crepath::cursor::rewrite::Replacement;
use crepath::cursor::storage::rewrite_storage_json;
use crepath::cursor::workspace::compute_workspace_hash;
use crepath::engine::{
    self, Layout, Probe, combine_workspaces, load_registry, move_paths, save_unsaved,
    split_workspace, suggest_split,
};
use rusqlite::params;
use rusqlite::types::Value;
use serde_json::json;
use tempfile::TempDir;

#[test]
fn mv_replace_skips_missing_destination_with_warning() {
    let home = cursor_home();
    let one = native(&home.root, "noble/one");
    let two = native(&home.root, "noble/two");
    let dest_one = native(&home.root, "resolute/one");
    fs::create_dir_all(&one).unwrap();
    fs::create_dir_all(&two).unwrap();
    fs::create_dir_all(&dest_one).unwrap();
    write_folder_workspace(&home.layout, "hash-one", &one);
    write_folder_workspace(&home.layout, "hash-two", &two);
    chat(
        &home.layout,
        A,
        "hash-one",
        &folder_identity("hash-one", &one),
    );
    chat(
        &home.layout,
        B,
        "hash-two",
        &folder_identity("hash-two", &two),
    );
    kv_text(
        &home.layout,
        &format!("bubbleId:{A}:b1"),
        &format!("path {}", one.display()),
    );
    kv_text(
        &home.layout,
        &format!("bubbleId:{B}:b1"),
        &format!("path {}", two.display()),
    );

    let rt = runtime(&home.layout, false, false);
    let sep = std::path::MAIN_SEPARATOR;
    let report = move_paths(
        &rt,
        &[],
        Some((&format!("{sep}noble{sep}"), &format!("{sep}resolute{sep}"))),
        false,
        false,
    )
    .unwrap();

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
    assert!(raw.contains(&uri(&dest_one)));
    let skipped = fs::read_to_string(
        home.layout
            .workspace_storage()
            .join("hash-two")
            .join("workspace.json"),
    )
    .unwrap();
    assert!(skipped.contains(&uri(&two)));
    let moved = kv_str(&home.layout, &format!("bubbleId:{A}:b1")).unwrap();
    assert!(moved.contains(&dest_one.display().to_string()));
    assert!(!moved.contains(&one.display().to_string()));
    assert!(
        kv_str(&home.layout, &format!("bubbleId:{B}:b1"))
            .unwrap()
            .contains(&two.display().to_string())
    );
    assert_eq!(header_ids(&home.layout, &new_hash), vec![A.to_string()]);
    assert_eq!(
        header_value(&home.layout, A)["workspaceIdentifier"],
        folder_identity(&new_hash, &dest_one)
    );
    assert!(backups_empty(&home.layout));
}

#[test]
fn save_unsaved_workspace_writes_folder_uri_and_identity() {
    let home = cursor_home();
    let id = "1765558213752";
    let unsaved = home.layout.cursor_root.join("Workspaces").join(id);
    fs::create_dir_all(&unsaved).unwrap();
    let unsaved_file = unsaved.join("workspace.json");
    fs::write(&unsaved_file, "{\"folders\":[]}\n").unwrap();
    let storage_id = compute_workspace_hash(&unsaved_file).unwrap();
    write_workspace(&home.layout, &storage_id, "workspace", &unsaved_file);
    let identity = json!({"id": storage_id, "configPath": components(&unsaved_file)});
    chat(&home.layout, A, &storage_id, &identity);
    let dest = home.root.join("projects-foo");
    fs::create_dir_all(&dest).unwrap();

    let rt = runtime(&home.layout, false, false);
    save_unsaved(&rt, id, &dest).unwrap();

    let hash = compute_workspace_hash(&dest).unwrap();
    let raw = fs::read_to_string(
        home.layout
            .workspace_storage()
            .join(&hash)
            .join("workspace.json"),
    )
    .unwrap();
    let json: serde_json::Value = serde_json::from_str(&raw).unwrap();
    assert_eq!(json["folder"], uri(&dest));
    assert!(json.get("workspace").is_none());
    assert_eq!(header_ids(&home.layout, &hash), vec![A.to_string()]);
    assert_eq!(
        header_value(&home.layout, A)["workspaceIdentifier"],
        folder_identity(&hash, &dest)
    );
    assert!(unsaved_file.exists());
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
    let identity = folder_identity("source-hash", &source_path);
    let api_file = native(&api, "src/main.rs");
    fs::write(&api_file, "fn main() {}\n").unwrap();
    let web_file = native(&web, "src/new.ts");
    for id in [A, B, C] {
        insert_header(
            &home.layout,
            id,
            "source-hash",
            false,
            &json!({"name": id, "workspaceIdentifier": identity}),
        );
    }
    kv_text(
        &home.layout,
        &format!("ofsContent:{A}:{}", uri(&api_file)),
        "body",
    );
    kv_text(
        &home.layout,
        &format!("composerData:{B}"),
        &json!({
            "workspaceIdentifier": identity,
            "newlyCreatedFiles": [web_file.display().to_string()],
            "trackedGitRepos": [home.root.display().to_string()]
        })
        .to_string(),
    );
    kv_text(
        &home.layout,
        &format!("composerData:{A}"),
        &json!({"workspaceIdentifier": identity}).to_string(),
    );
    kv_text(
        &home.layout,
        &format!("composerData:{C}"),
        &json!({"workspaceIdentifier": identity}).to_string(),
    );

    let rt = runtime(&home.layout, false, false);
    let source = engine::find_workspace(&rt, "source-hash").unwrap().unwrap();
    let suggestion = suggest_split(&rt, &source, &[api.clone(), web.clone()]).unwrap();
    assert_eq!(suggestion.assigned.get(A).map(|paths| paths.len()), Some(1));
    assert!(suggestion.assigned[A][0].ends_with("api"));
    assert!(suggestion.assigned[B][0].ends_with("web"));
    assert!(suggestion.unassigned.iter().any(|id| id == C));

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
    assigned.insert(C.to_string(), vec![api.clone()]);
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
    assert_eq!(header_ids(&home.layout, &api_hash).len(), 2);
    assert_eq!(header_ids(&home.layout, &web_hash).len(), 1);
    let copy = &header_ids(&home.layout, &web_hash)[0];
    assert_eq!(
        header_value(&home.layout, copy)["workspaceIdentifier"],
        folder_identity(&web_hash, &web)
    );

    let home = cursor_home();
    let source_path = home.root.join("source");
    let api = home.root.join("api");
    fs::create_dir_all(&source_path).unwrap();
    fs::create_dir_all(api.join("src")).unwrap();
    write_folder_workspace(&home.layout, "source-hash", &source_path);
    let identity = folder_identity("source-hash", &source_path);
    let parent =
        json!({"name": "parent", "subagentComposerIds": [SUB], "workspaceIdentifier": identity});
    insert_header(&home.layout, A, "source-hash", false, &parent);
    insert_header(
        &home.layout,
        SUB,
        "source-hash",
        true,
        &json!({"name": "sub", "isSubagent": true, "workspaceIdentifier": identity}),
    );
    kv_text(
        &home.layout,
        &format!("composerData:{A}"),
        &parent.to_string(),
    );
    kv_text(
        &home.layout,
        &format!("composerData:{SUB}"),
        r#"{"isSubagent":true}"#,
    );
    let mut assigned = BTreeMap::new();
    assigned.insert(A.to_string(), vec![api.clone()]);
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
    assert!(moved.iter().any(|id| id == A));
    assert!(moved.iter().any(|id| id == SUB));
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
    let identity = folder_identity("source-hash", &source_path);
    let parent =
        json!({"name": "parent", "subagentComposerIds": [SUB], "workspaceIdentifier": identity});
    insert_header(&home.layout, A, "source-hash", false, &parent);
    insert_header(
        &home.layout,
        SUB,
        "source-hash",
        true,
        &json!({"name": "sub", "workspaceIdentifier": {"id": "source-hash"}}),
    );
    let families = [
        format!("composerVirtualRowHeights:{A}"),
        format!("agentKv:checkpoint:{A}"),
        format!("bubbleId:{A}:b"),
        format!("checkpointId:{A}:c"),
        format!("codeBlockDiff:{A}:d"),
        format!("codeBlockPartialInlineDiffFates:{A}:e"),
        format!("messageRequestContext:{A}:m"),
        format!("ofsContent:{A}:file:///tmp/a"),
        format!("agentKv:bubbleCheckpoint:{A}:b"),
    ];
    kv_text(
        &home.layout,
        &format!("composerData:{A}"),
        &parent.to_string(),
    );
    for key in &families {
        kv_text(
            &home.layout,
            key,
            &format!(r#"{{"id":"{A}","sub":"{SUB}"}}"#),
        );
    }
    kv_text(
        &home.layout,
        &format!("composerData:{SUB}"),
        &format!(r#"{{"composerId":"{SUB}","parent":"{A}"}}"#),
    );
    kv_text(&home.layout, "agentKv:blob:abc", "shared-blob");

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
        vec![A.to_string(), SUB.to_string()]
    );
    let target_ids = header_ids(&home.layout, "target-hash");
    assert_eq!(target_ids.len(), 2);
    assert!(!target_ids.iter().any(|id| id == A || id == SUB));
    let conn = global(&home.layout);
    let new_parent: String = conn
        .query_row(
            "SELECT composerId FROM composerHeaders WHERE workspaceId = 'target-hash' AND isSubagent = 0",
            [],
            |row| row.get(0),
        )
        .unwrap();
    let new_sub: String = conn
        .query_row(
            "SELECT composerId FROM composerHeaders WHERE workspaceId = 'target-hash' AND isSubagent = 1",
            [],
            |row| row.get(0),
        )
        .unwrap();
    for key in &families {
        let cloned = key.replace(A, &new_parent);
        let value = kv_str(&home.layout, &cloned).unwrap_or_else(|| panic!("missing {cloned}"));
        assert_eq!(
            value,
            format!(r#"{{"id":"{new_parent}","sub":"{new_sub}"}}"#)
        );
    }
    let data: serde_json::Value =
        serde_json::from_str(&kv_str(&home.layout, &format!("composerData:{new_parent}")).unwrap())
            .unwrap();
    assert_eq!(data["subagentComposerIds"], json!([new_sub]));
    assert_eq!(
        data["workspaceIdentifier"],
        folder_identity("target-hash", &target_path)
    );
    assert_eq!(
        kv_str(&home.layout, &format!("composerData:{new_sub}")).unwrap(),
        format!(r#"{{"composerId":"{new_sub}","parent":"{new_parent}"}}"#)
    );
    let blob_count: i64 = conn
        .query_row(
            "SELECT COUNT(*) FROM cursorDiskKV WHERE key LIKE 'agentKv:blob:%'",
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
    chat(
        &home.layout,
        A,
        "source-hash",
        &folder_identity("source-hash", &source_path),
    );
    kv_text(&home.layout, &format!("bubbleId:{A}:b"), "from-source");
    let rt = runtime(&home.layout, false, false);
    combine_workspaces(&rt, &target_path, &["source-hash".to_string()], true).unwrap();
    assert!(header_ids(&home.layout, "source-hash").is_empty());
    assert_eq!(header_ids(&home.layout, "target-hash"), vec![A.to_string()]);
    let data: serde_json::Value =
        serde_json::from_str(&kv_str(&home.layout, &format!("composerData:{A}")).unwrap()).unwrap();
    assert_eq!(
        data["workspaceIdentifier"],
        folder_identity("target-hash", &target_path)
    );
    assert_eq!(
        kv_str(&home.layout, &format!("bubbleId:{A}:b")).as_deref(),
        Some("from-source")
    );

    let before = header_ids(&home.layout, "target-hash");
    let report = combine_workspaces(&rt, &target_path, &[A.to_string()], false).unwrap();
    assert!(report.skipped.iter().any(|id| id == A));
    assert_eq!(header_ids(&home.layout, "target-hash"), before);
}

#[test]
fn combine_moves_individual_chat_ids_from_their_owner() {
    let home = cursor_home();
    let one = home.root.join("one");
    let two = home.root.join("two");
    let target = home.root.join("target");
    for dir in [&one, &two, &target] {
        fs::create_dir_all(dir).unwrap();
    }
    write_folder_workspace(&home.layout, "hash-one", &one);
    write_folder_workspace(&home.layout, "hash-two", &two);
    chat(
        &home.layout,
        A,
        "hash-one",
        &folder_identity("hash-one", &one),
    );
    chat(
        &home.layout,
        B,
        "hash-two",
        &folder_identity("hash-two", &two),
    );
    chat(
        &home.layout,
        C,
        "hash-two",
        &folder_identity("hash-two", &two),
    );
    set_local(&home.layout, "hash-two", &[B, C], &[B]);

    let rt = runtime(&home.layout, false, false);
    let report = combine_workspaces(&rt, &target, &[A.to_string(), B.to_string()], true).unwrap();
    let target_hash = compute_workspace_hash(&target).unwrap();
    assert_eq!(
        header_ids(&home.layout, &target_hash),
        vec![A.to_string(), B.to_string()]
    );
    assert!(header_ids(&home.layout, "hash-one").is_empty());
    assert_eq!(header_ids(&home.layout, "hash-two"), vec![C.to_string()]);
    assert!(
        report
            .applied
            .iter()
            .any(|line| line.contains("moved 1 chats hash-one"))
    );
    assert_eq!(
        local_ids(&home.layout, "hash-two"),
        (vec![C.to_string()], vec![])
    );
    assert_eq!(
        local_ids(&home.layout, &target_hash),
        (vec![B.to_string()], vec![B.to_string()])
    );
}

#[test]
fn registry_reads_composer_headers_and_falls_back() {
    disable_update_check();
    let conn = rusqlite::Connection::open_in_memory().unwrap();
    conn.execute_batch(GLOBAL_SCHEMA).unwrap();
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
            "CREATE TABLE ItemTable (key TEXT UNIQUE ON CONFLICT REPLACE, value BLOB);
             CREATE TABLE cursorDiskKV (key TEXT UNIQUE ON CONFLICT REPLACE, value BLOB);",
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
fn mv_touches_only_the_workspace_and_skips_sibling_prefix() {
    let home = cursor_home();
    let project = native(&home.root, "user/project");
    let copy = native(&home.root, "user/project-copy");
    let sibling = native(&home.root, "user/projects/foo");
    fs::create_dir_all(&project).unwrap();
    fs::create_dir_all(&copy).unwrap();
    write_folder_workspace(&home.layout, "oldhash", &project);
    write_folder_workspace(&home.layout, "ws-b", &sibling);
    chat(
        &home.layout,
        A,
        "oldhash",
        &folder_identity("oldhash", &project),
    );
    chat(&home.layout, B, "ws-b", &folder_identity("ws-b", &sibling));
    let p = project.display().to_string();
    let s = sibling.display().to_string();
    kv_text(
        &home.layout,
        &format!("bubbleId:{A}:1"),
        &json!({"active": p, "other": s}).to_string(),
    );
    kv_text(
        &home.layout,
        &format!("bubbleId:{B}:1"),
        &json!({"active": p}).to_string(),
    );
    kv_text(&home.layout, "agentKv:blob:deadbeef", &p);
    kv_text(&home.layout, "inlineDiff:oldhash:1", &p);
    kv_text(&home.layout, "inlineDiff:other:1", &p);
    kv_text(&home.layout, "patch-graph:oldhash:node", &p);
    let conn = global(&home.layout);
    conn.execute(
        "INSERT INTO ItemTable (key, value) VALUES ('composer.planRegistry', ?1)",
        [&p],
    )
    .unwrap();
    conn.execute(
        "INSERT INTO ItemTable (key, value) VALUES ('unrelated.key', ?1)",
        [&p],
    )
    .unwrap();
    drop(conn);

    let rt = runtime(&home.layout, false, false);
    move_paths(
        &rt,
        &[("oldhash".into(), copy.display().to_string())],
        None,
        false,
        false,
    )
    .unwrap();
    let new_hash = compute_workspace_hash(&copy).unwrap();
    let c = copy.display().to_string();
    let bubble = kv_str(&home.layout, &format!("bubbleId:{A}:1")).unwrap();
    assert_eq!(bubble, json!({"active": c, "other": s}).to_string());
    assert_eq!(
        kv_str(&home.layout, &format!("bubbleId:{B}:1")).unwrap(),
        json!({"active": p}).to_string()
    );
    assert_eq!(kv_str(&home.layout, "agentKv:blob:deadbeef").unwrap(), p);
    assert_eq!(
        kv_str(&home.layout, &format!("inlineDiff:{new_hash}:1")).unwrap(),
        c
    );
    assert!(kv_get(&home.layout, "inlineDiff:oldhash:1").is_none());
    assert_eq!(kv_str(&home.layout, "inlineDiff:other:1").unwrap(), p);
    assert_eq!(
        kv_str(&home.layout, &format!("patch-graph:{new_hash}:node")).unwrap(),
        c
    );
    let conn = global(&home.layout);
    let item = |key: &str| -> String {
        conn.query_row("SELECT value FROM ItemTable WHERE key = ?1", [key], |row| {
            row.get(0)
        })
        .unwrap()
    };
    assert_eq!(item("composer.planRegistry"), c);
    assert_eq!(item("unrelated.key"), p);
    assert_eq!(header_ids(&home.layout, &new_hash), vec![A.to_string()]);
    assert_eq!(header_ids(&home.layout, "ws-b"), vec![B.to_string()]);
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
    let before = snapshot(&home);
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
    assert_same(&before, &snapshot(&home), "aborted mv");
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
    let before = snapshot(&home);
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
    assert_same(&before, &snapshot(&home), "refused mv");
}

struct AbortAfterCommit {
    layout: Layout,
    tripped: AtomicBool,
}

impl Probe for AbortAfterCommit {
    fn running(&self) -> bool {
        if self.tripped.load(Ordering::SeqCst) {
            return true;
        }
        let owner: Option<String> = rusqlite::Connection::open(self.layout.global_db())
            .ok()
            .and_then(|conn| {
                conn.query_row(
                    "SELECT workspaceId FROM composerHeaders WHERE composerId = ?1",
                    [A],
                    |row| row.get(0),
                )
                .ok()
            });
        if owner.as_deref() != Some("hash-from") {
            self.tripped.store(true, Ordering::SeqCst);
            return true;
        }
        false
    }
}

#[test]
fn cursor_starting_after_commit_restores_everything_from_the_undo_log() {
    let home = cursor_home();
    let from = home.root.join("from");
    let to = home.root.join("to");
    fs::create_dir_all(&from).unwrap();
    fs::create_dir_all(&to).unwrap();
    write_folder_workspace(&home.layout, "hash-from", &from);
    chat(
        &home.layout,
        A,
        "hash-from",
        &folder_identity("hash-from", &from),
    );
    kv_text(
        &home.layout,
        &format!("bubbleId:{A}:1"),
        &from.display().to_string(),
    );
    kv(&home.layout, &format!("bubbleId:{A}:2"), Value::Null);
    kv(
        &home.layout,
        &format!("bubbleId:{A}:3"),
        Value::Blob(from.display().to_string().into_bytes()),
    );
    fs::write(
        home.layout.storage_json(),
        serde_json::to_string_pretty(&json!({
            "backupWorkspaces": {"folders": [{ "folderUri": uri(&from) }]}
        }))
        .unwrap(),
    )
    .unwrap();
    let db_before = dump_db(&home.layout.global_db());
    let before = snapshot(&home);
    let rt = runtime_with(
        &home.layout,
        Arc::new(AbortAfterCommit {
            layout: home.layout.clone(),
            tripped: AtomicBool::new(false),
        }),
        false,
    );
    let err = move_paths(
        &rt,
        &[("hash-from".into(), to.display().to_string())],
        None,
        false,
        false,
    )
    .unwrap_err();
    let message = format!("{err:#}");
    assert!(message.contains("Cursor is running"), "{message}");
    assert!(message.contains("rolled back"), "{message}");
    assert_eq!(dump_db(&home.layout.global_db()), db_before);
    let mut after = snapshot(&home);
    let mut before = before;
    for map in [&mut before, &mut after] {
        map.retain(|key, _| {
            !key.ends_with("state.vscdb")
                && !key.contains("state.vscdb-")
                && !key.starts_with("dot-crepath")
        });
    }
    assert_same(&before, &after, "rolled back mv");
    assert!(
        !home
            .layout
            .workspace_storage()
            .join(compute_workspace_hash(&to).unwrap())
            .exists()
    );
    assert!(backups_empty(&home.layout));
    assert!(
        registry::load_header(&global(&home.layout), A)
            .unwrap()
            .is_some()
    );
}

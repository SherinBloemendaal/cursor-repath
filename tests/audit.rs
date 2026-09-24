mod common;

use std::collections::BTreeMap;
use std::fs;
use std::io::Write;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};

use common::*;
use crepath::cursor::folder_id::path_to_folder_id;
use crepath::cursor::uri::{Platform, normalize_path};
use crepath::cursor::workspace::{compute_renamed_hash, compute_workspace_hash, workspace_file_id};
use crepath::engine::{
    self, FixedProbe, Instance, Probe, combine_workspaces, copy_paths, export_workspace,
    import_archive, list_workspaces, move_paths, reindex, remove_targets, save_unsaved,
    show_history, show_stats, split_workspace, suggest_split,
};
use rusqlite::types::Value;
use serde_json::json;
use tempfile::TempDir;

fn transcripts(home: &Home, path: &Path, id: &str) -> PathBuf {
    home.layout
        .projects_dir
        .join(path_to_folder_id(normalize_path(path)))
        .join("agent-transcripts")
        .join(id)
}

fn without_own_state(
    mut map: BTreeMap<String, Option<Vec<u8>>>,
) -> BTreeMap<String, Option<Vec<u8>>> {
    map.retain(|key, _| {
        !key.ends_with("state.vscdb")
            && !key.contains("state.vscdb-")
            && !key.starts_with("dot-crepath")
    });
    map
}

#[test]
fn code_workspace_mv_hashes_the_lowercased_config_path() {
    let home = cursor_home();
    let source = home.root.join("Proj/Src.code-workspace");
    let dest = home.root.join("Other/App.code-workspace");
    fs::create_dir_all(source.parent().unwrap()).unwrap();
    fs::create_dir_all(dest.parent().unwrap()).unwrap();
    fs::write(&source, "{\"folders\":[]}").unwrap();
    fs::write(&dest, "{\"folders\":[]}").unwrap();
    let source_id = compute_workspace_hash(&source).unwrap();
    assert_eq!(
        source_id,
        workspace_file_id(
            Platform::current(),
            &normalize_path(&source).to_string_lossy()
        )
    );
    write_workspace(&home.layout, &source_id, "workspace", &source);
    let identity = json!({"id": source_id, "configPath": components(&source)});
    chat(&home.layout, A, &source_id, &identity);

    let rt = runtime(&home.layout, false, false);
    move_paths(
        &rt,
        &[(source_id.clone(), dest.display().to_string())],
        None,
        false,
        false,
    )
    .unwrap();

    let dest_text = normalize_path(&dest).to_string_lossy().to_string();
    let new_id = workspace_file_id(Platform::current(), &dest_text);
    if Platform::current() != Platform::Linux {
        assert_eq!(
            new_id,
            workspace_file_id(Platform::Linux, &dest_text.to_lowercase())
        );
    }
    let raw = fs::read_to_string(
        home.layout
            .workspace_storage()
            .join(&new_id)
            .join("workspace.json"),
    )
    .unwrap();
    assert_eq!(
        serde_json::from_str::<serde_json::Value>(&raw).unwrap(),
        json!({"workspace": uri(&dest)})
    );
    assert_eq!(header_ids(&home.layout, &new_id), vec![A.to_string()]);
    assert_eq!(
        header_value(&home.layout, A)["workspaceIdentifier"],
        json!({"id": new_id, "configPath": components(&dest)})
    );
}

#[test]
fn mv_normalizes_trailing_separator_and_keeps_null_and_blob_types() {
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
    kv(&home.layout, &format!("bubbleId:{A}:null"), Value::Null);
    kv(
        &home.layout,
        &format!("bubbleId:{A}:blob"),
        Value::Blob(format!("see {}/x.rs", from.display()).into_bytes()),
    );
    kv(
        &home.layout,
        &format!("bubbleId:{A}:binary"),
        Value::Blob(vec![0xff, 0x00, 0x10]),
    );
    let rt = runtime(&home.layout, false, false);
    let trailing = format!("{}{}", to.display(), std::path::MAIN_SEPARATOR);
    move_paths(&rt, &[("hash-from".into(), trailing)], None, false, false).unwrap();

    let hash = compute_workspace_hash(&to).unwrap();
    let raw = fs::read_to_string(
        home.layout
            .workspace_storage()
            .join(&hash)
            .join("workspace.json"),
    )
    .unwrap();
    assert_eq!(
        serde_json::from_str::<serde_json::Value>(&raw).unwrap(),
        json!({"folder": uri(&to)})
    );
    assert!(!uri(&to).ends_with('/'));
    assert_eq!(
        kv_get(&home.layout, &format!("bubbleId:{A}:null")),
        Some(Value::Null)
    );
    assert_eq!(
        kv_get(&home.layout, &format!("bubbleId:{A}:blob")),
        Some(Value::Blob(
            format!("see {}/x.rs", to.display()).into_bytes()
        ))
    );
    assert_eq!(
        kv_get(&home.layout, &format!("bubbleId:{A}:binary")),
        Some(Value::Blob(vec![0xff, 0x00, 0x10]))
    );
}

#[test]
fn cp_keeps_source_storage_maps_local_ids_and_never_nests_projects() {
    let home = cursor_home();
    let src = home.root.join("src");
    let dst = home.root.join("dst");
    fs::create_dir_all(&src).unwrap();
    fs::create_dir_all(&dst).unwrap();
    write_folder_workspace(&home.layout, "hash-src", &src);
    let identity = folder_identity("hash-src", &src);
    let parent =
        json!({"name": "p", "subagentComposerIds": [SUB], "workspaceIdentifier": identity});
    insert_header(&home.layout, A, "hash-src", false, &parent);
    kv_text(
        &home.layout,
        &format!("composerData:{A}"),
        &parent.to_string(),
    );
    insert_header(
        &home.layout,
        SUB,
        "hash-src",
        true,
        &json!({"workspaceIdentifier": identity}),
    );
    kv_text(&home.layout, &format!("composerData:{SUB}"), "{}");
    kv_text(
        &home.layout,
        &format!("bubbleId:{A}:1"),
        &format!("{}/main.rs", src.display()),
    );
    kv(&home.layout, &format!("bubbleId:{A}:2"), Value::Null);
    kv(
        &home.layout,
        &format!("bubbleId:{A}:3"),
        Value::Blob(src.display().to_string().into_bytes()),
    );
    set_local(&home.layout, "hash-src", &[A], &[A]);
    let storage = json!({
        "backupWorkspaces": {"folders": [{"folderUri": uri(&src)}]},
        "profileAssociations": {"workspaces": {uri(&src): "work"}}
    });
    fs::write(
        home.layout.storage_json(),
        serde_json::to_string_pretty(&storage).unwrap(),
    )
    .unwrap();
    let old_transcript = transcripts(&home, &src, A);
    fs::create_dir_all(&old_transcript).unwrap();
    fs::write(old_transcript.join("t.jsonl"), "line").unwrap();
    let old_slug = home
        .layout
        .projects_dir
        .join(path_to_folder_id(normalize_path(&src)));
    fs::create_dir_all(old_slug.join("terminals")).unwrap();
    fs::write(old_slug.join("terminals/1.txt"), "term").unwrap();
    let other = transcripts(&home, &dst, C);
    fs::create_dir_all(&other).unwrap();
    fs::write(other.join("x.jsonl"), "other").unwrap();

    let rt = runtime(&home.layout, false, false);
    copy_paths(
        &rt,
        &[("hash-src".into(), dst.display().to_string())],
        None,
        false,
        false,
    )
    .unwrap();

    let hash = compute_workspace_hash(&dst).unwrap();
    assert_eq!(
        header_ids(&home.layout, "hash-src"),
        vec![A.to_string(), SUB.to_string()]
    );
    let copies = header_ids(&home.layout, &hash);
    assert_eq!(copies.len(), 2);
    let conn = global(&home.layout);
    let new_a: String = conn
        .query_row(
            "SELECT composerId FROM composerHeaders WHERE workspaceId = ?1 AND isSubagent = 0",
            [&hash],
            |row| row.get(0),
        )
        .unwrap();
    drop(conn);
    let storage: serde_json::Value =
        serde_json::from_str(&fs::read_to_string(home.layout.storage_json()).unwrap()).unwrap();
    assert_eq!(
        storage["backupWorkspaces"]["folders"][0]["folderUri"],
        uri(&src)
    );
    assert_eq!(
        storage["profileAssociations"]["workspaces"][uri(&src)],
        "work"
    );
    assert_eq!(
        storage["profileAssociations"]["workspaces"][uri(&dst)],
        "work"
    );
    assert_eq!(
        local_ids(&home.layout, &hash),
        (vec![new_a.clone()], vec![new_a.clone()])
    );
    assert_eq!(
        local_ids(&home.layout, "hash-src"),
        (vec![A.to_string()], vec![A.to_string()])
    );
    let new_slug = home
        .layout
        .projects_dir
        .join(path_to_folder_id(normalize_path(&dst)));
    assert_eq!(
        fs::read_to_string(transcripts(&home, &dst, &new_a).join("t.jsonl")).unwrap(),
        "line"
    );
    assert!(!transcripts(&home, &dst, A).exists());
    assert!(!new_slug.join(old_slug.file_name().unwrap()).exists());
    assert_eq!(
        fs::read_to_string(new_slug.join("terminals/1.txt")).unwrap(),
        "term"
    );
    assert_eq!(fs::read_to_string(other.join("x.jsonl")).unwrap(), "other");
    assert!(old_transcript.join("t.jsonl").exists());
    assert_eq!(
        kv_str(&home.layout, &format!("bubbleId:{new_a}:1")).unwrap(),
        format!("{}/main.rs", dst.display())
    );
    assert_eq!(
        kv_get(&home.layout, &format!("bubbleId:{new_a}:2")),
        Some(Value::Null)
    );
    assert_eq!(
        kv_get(&home.layout, &format!("bubbleId:{new_a}:3")),
        Some(Value::Blob(dst.display().to_string().into_bytes()))
    );
    assert_eq!(
        kv_str(&home.layout, &format!("bubbleId:{A}:1")).unwrap(),
        format!("{}/main.rs", src.display())
    );
    assert!(backups_empty(&home.layout));
}

#[test]
fn rm_removes_a_workspace_and_a_single_chat() {
    let home = cursor_home();
    let gone = home.root.join("gone");
    let keep = home.root.join("keep");
    fs::create_dir_all(&gone).unwrap();
    fs::create_dir_all(&keep).unwrap();
    write_folder_workspace(&home.layout, "hash-gone", &gone);
    write_folder_workspace(&home.layout, "hash-keep", &keep);
    let parent = json!({"subagentComposerIds": [SUB], "workspaceIdentifier": folder_identity("hash-gone", &gone)});
    insert_header(&home.layout, A, "hash-gone", false, &parent);
    kv_text(
        &home.layout,
        &format!("composerData:{A}"),
        &parent.to_string(),
    );
    insert_header(&home.layout, SUB, "hash-gone", true, &json!({}));
    kv_text(&home.layout, &format!("bubbleId:{SUB}:1"), "sub");
    kv(&home.layout, &format!("bubbleId:{A}:1"), Value::Null);
    kv_text(
        &home.layout,
        &format!("agentKv:checkpoint:{A}"),
        &"a".repeat(64),
    );
    kv_text(
        &home.layout,
        &format!("agentKv:blob:{}", "a".repeat(64)),
        "shared",
    );
    kv_text(&home.layout, "inlineDiff:hash-gone:1", "x");
    let transcript = transcripts(&home, &gone, A);
    fs::create_dir_all(&transcript).unwrap();
    chat(
        &home.layout,
        B,
        "hash-keep",
        &folder_identity("hash-keep", &keep),
    );
    chat(
        &home.layout,
        C,
        "hash-keep",
        &folder_identity("hash-keep", &keep),
    );
    set_local(&home.layout, "hash-keep", &[B, C], &[B]);

    let rt = runtime(&home.layout, false, false);
    remove_targets(&rt, &["hash-gone".to_string(), B.to_string()]).unwrap();

    assert!(!home.layout.workspace_storage().join("hash-gone").exists());
    assert!(header_ids(&home.layout, "hash-gone").is_empty());
    for key in [
        format!("composerData:{A}"),
        format!("bubbleId:{A}:1"),
        format!("bubbleId:{SUB}:1"),
        format!("agentKv:checkpoint:{A}"),
        "inlineDiff:hash-gone:1".to_string(),
        format!("composerData:{B}"),
    ] {
        assert!(kv_get(&home.layout, &key).is_none(), "{key} survived");
    }
    assert_eq!(
        kv_str(&home.layout, &format!("agentKv:blob:{}", "a".repeat(64))).unwrap(),
        "shared"
    );
    assert!(!transcript.exists());
    assert_eq!(header_ids(&home.layout, "hash-keep"), vec![C.to_string()]);
    assert_eq!(
        local_ids(&home.layout, "hash-keep"),
        (vec![C.to_string()], vec![])
    );
    assert!(backups_empty(&home.layout));
}

#[test]
fn rx_rebuilds_identity_reassigns_orphans_and_rewrites_stale_paths() {
    let home = cursor_home();
    let proj = home.root.join("proj");
    let old = home.root.join("old-proj");
    let elsewhere = home.root.join("elsewhere");
    fs::create_dir_all(&proj).unwrap();
    fs::create_dir_all(&elsewhere).unwrap();
    let id = compute_workspace_hash(&proj).unwrap();
    write_folder_workspace(&home.layout, &id, &proj);
    write_folder_workspace(&home.layout, "hash-else", &elsewhere);
    let orphan = "0123456789abcdef0123456789abcdef";
    chat(&home.layout, A, &id, &folder_identity("stale", &old));
    kv_text(
        &home.layout,
        &format!("bubbleId:{A}:1"),
        &format!("{}/lib.rs", old.display()),
    );
    chat(&home.layout, B, orphan, &folder_identity(orphan, &proj));
    kv_text(&home.layout, &format!("inlineDiff:{orphan}:1"), "diff");
    chat(&home.layout, C, &id, &folder_identity(&id, &proj));
    let c_before = header_value(&home.layout, C);
    let d = "eeeeeeee-0000-4000-8000-000000000005";
    chat(
        &home.layout,
        d,
        "hash-else",
        &folder_identity("hash-else", &proj),
    );
    let d_before = header_value(&home.layout, d);
    let stale_transcript = transcripts(&home, &old, A);
    fs::create_dir_all(&stale_transcript).unwrap();
    fs::write(stale_transcript.join("t.jsonl"), "t").unwrap();

    let rt = runtime(&home.layout, false, false);
    reindex(&rt, &id).unwrap();

    let identity = engine::find_workspace(&rt, &id)
        .unwrap()
        .unwrap()
        .identity();
    for chat_id in [A, B, C] {
        assert_eq!(
            header_value(&home.layout, chat_id)["workspaceIdentifier"],
            identity
        );
    }
    assert_eq!(
        header_ids(&home.layout, &id),
        vec![A.to_string(), B.to_string(), C.to_string()]
    );
    let data: serde_json::Value =
        serde_json::from_str(&kv_str(&home.layout, &format!("composerData:{A}")).unwrap()).unwrap();
    assert_eq!(data["workspaceIdentifier"], identity);
    assert_eq!(
        kv_str(&home.layout, &format!("bubbleId:{A}:1")).unwrap(),
        format!("{}/lib.rs", proj.display())
    );
    assert!(kv_get(&home.layout, &format!("inlineDiff:{orphan}:1")).is_none());
    assert_eq!(
        kv_str(&home.layout, &format!("inlineDiff:{id}:1")).unwrap(),
        "diff"
    );
    assert_eq!(header_value(&home.layout, C), c_before);
    assert_eq!(header_value(&home.layout, d), d_before);
    assert_eq!(header_ids(&home.layout, "hash-else"), vec![d.to_string()]);
    assert!(transcripts(&home, &proj, A).join("t.jsonl").exists());
    assert!(!stale_transcript.exists());
}

fn blob_hash(seed: u8) -> String {
    format!("{seed:02x}").repeat(32)
}

fn archive_source() -> (Home, PathBuf, String) {
    let home = cursor_home();
    let src = home.root.join("src");
    fs::create_dir_all(&src).unwrap();
    let id = compute_workspace_hash(&src).unwrap();
    write_folder_workspace(&home.layout, &id, &src);
    set_local(&home.layout, &id, &[A], &[]);
    let identity = folder_identity(&id, &src);
    let mut proto = vec![0x0a, 0x20];
    proto.extend(std::iter::repeat_n(0x11u8, 32));
    let state = format!(
        "~{}",
        base64::Engine::encode(&base64::engine::general_purpose::STANDARD, &proto)
    );
    let parent = json!({
        "name": "archived",
        "subagentComposerIds": [SUB],
        "workspaceIdentifier": identity,
        "conversationState": state,
    });
    insert_header(&home.layout, A, &id, false, &parent);
    kv_text(
        &home.layout,
        &format!("composerData:{A}"),
        &parent.to_string(),
    );
    insert_header(
        &home.layout,
        SUB,
        &id,
        true,
        &json!({"workspaceIdentifier": identity}),
    );
    kv(&home.layout, &format!("composerData:{SUB}"), Value::Null);
    kv(&home.layout, &format!("bubbleId:{A}:null"), Value::Null);
    kv(
        &home.layout,
        &format!("bubbleId:{A}:bin"),
        Value::Blob(vec![0xff, 0x00, 0x10]),
    );
    kv_text(
        &home.layout,
        &format!("bubbleId:{A}:path"),
        &format!("{}/a.rs", src.display()),
    );
    kv_text(
        &home.layout,
        &format!("agentKv:checkpoint:{A}"),
        &blob_hash(0x22),
    );
    kv(
        &home.layout,
        &format!("agentKv:blob:{}", blob_hash(0x11)),
        Value::Blob(json!({"next": blob_hash(0x33)}).to_string().into_bytes()),
    );
    kv(
        &home.layout,
        &format!("agentKv:blob:{}", blob_hash(0x22)),
        Value::Blob(vec![1, 2, 3]),
    );
    kv(
        &home.layout,
        &format!("agentKv:blob:{}", blob_hash(0x33)),
        Value::Blob(vec![4]),
    );
    kv(
        &home.layout,
        &format!("agentKv:blob:{}", blob_hash(0x44)),
        Value::Blob(vec![5]),
    );
    let transcript = transcripts(&home, &src, A);
    fs::create_dir_all(&transcript).unwrap();
    fs::write(transcript.join("t.jsonl"), "t").unwrap();
    (home, src, id)
}

fn export_to(home: &Home, id: &str, dir: &Path) -> PathBuf {
    let file = dir.join("ws.crepath");
    let rt = runtime(&home.layout, false, false);
    export_workspace(&rt, id, &file).unwrap();
    file
}

#[test]
fn export_import_round_trip_keeps_nulls_blobs_and_referenced_agent_blobs() {
    let (source, _src, id) = archive_source();
    let out = TempDir::new().unwrap();
    let before = snapshot(&source);
    let file = export_to(&source, &id, out.path());
    assert_same(&before, &snapshot(&source), "export");

    let dest = cursor_home();
    let rt = runtime(&dest.layout, false, false);
    import_archive(&rt, &file, None, false).unwrap();

    assert_eq!(
        header_ids(&dest.layout, &id),
        vec![A.to_string(), SUB.to_string()]
    );
    for key in [
        format!("composerData:{A}"),
        format!("composerData:{SUB}"),
        format!("bubbleId:{A}:null"),
        format!("bubbleId:{A}:bin"),
        format!("bubbleId:{A}:path"),
        format!("agentKv:checkpoint:{A}"),
        format!("agentKv:blob:{}", blob_hash(0x11)),
        format!("agentKv:blob:{}", blob_hash(0x22)),
        format!("agentKv:blob:{}", blob_hash(0x33)),
    ] {
        assert!(kv_get(&source.layout, &key).is_some(), "{key}");
        assert_eq!(
            kv_get(&dest.layout, &key),
            kv_get(&source.layout, &key),
            "{key}"
        );
    }
    assert!(kv_get(&dest.layout, &format!("agentKv:blob:{}", blob_hash(0x44))).is_none());
    assert_eq!(
        kv_get(&dest.layout, &format!("composerData:{SUB}")),
        Some(Value::Null)
    );
    assert_eq!(
        kv_get(&dest.layout, &format!("bubbleId:{A}:bin")),
        Some(Value::Blob(vec![0xff, 0x00, 0x10]))
    );
    let conn = global(&dest.layout);
    let checkpoint: Value = conn
        .query_row(
            "SELECT checkpointAt FROM composerHeaders WHERE composerId = ?1",
            [A],
            |row| row.get(0),
        )
        .unwrap();
    assert_eq!(checkpoint, Value::Null);
    drop(conn);
    assert!(
        dest.layout
            .workspace_storage()
            .join(&id)
            .join("workspace.json")
            .exists()
    );
    assert_eq!(local_ids(&dest.layout, &id).0, vec![A.to_string()]);
    assert!(backups_empty(&dest.layout));
}

#[test]
fn import_to_recomputes_hash_and_skips_or_overwrites_existing_chats() {
    let (source, src, _id) = archive_source();
    let out = TempDir::new().unwrap();
    let file = export_to(&source, &_id, out.path());
    let dest = cursor_home();
    let target = dest.root.join("new-home");
    fs::create_dir_all(&target).unwrap();
    let rt = runtime(&dest.layout, false, false);
    import_archive(&rt, &file, Some(&target), false).unwrap();
    let hash = compute_workspace_hash(&target).unwrap();
    assert_eq!(
        header_ids(&dest.layout, &hash),
        vec![A.to_string(), SUB.to_string()]
    );
    assert_eq!(
        header_value(&dest.layout, A)["workspaceIdentifier"],
        folder_identity(&hash, &target)
    );
    let raw = fs::read_to_string(
        dest.layout
            .workspace_storage()
            .join(&hash)
            .join("workspace.json"),
    )
    .unwrap();
    assert_eq!(
        serde_json::from_str::<serde_json::Value>(&raw).unwrap(),
        json!({"folder": uri(&target)})
    );
    assert_eq!(
        kv_str(&dest.layout, &format!("bubbleId:{A}:path")).unwrap(),
        format!("{}/a.rs", target.display())
    );
    assert!(
        !kv_str(&dest.layout, &format!("bubbleId:{A}:path"))
            .unwrap()
            .contains(&src.display().to_string())
    );
    assert!(transcripts(&dest, &target, A).join("t.jsonl").exists());

    kv_text(&dest.layout, &format!("bubbleId:{A}:local"), "local");
    global(&dest.layout)
        .execute(
            "UPDATE cursorDiskKV SET value = 'edited' WHERE key = ?1",
            [format!("bubbleId:{A}:path")],
        )
        .unwrap();
    let report = import_archive(&rt, &file, Some(&target), false).unwrap();
    assert!(report.skipped.iter().any(|id| id == A));
    assert!(report.skipped.iter().any(|id| id == SUB));
    assert_eq!(
        kv_str(&dest.layout, &format!("bubbleId:{A}:path")).unwrap(),
        "edited"
    );

    import_archive(&rt, &file, Some(&target), true).unwrap();
    assert_eq!(
        kv_str(&dest.layout, &format!("bubbleId:{A}:path")).unwrap(),
        format!("{}/a.rs", target.display())
    );
    assert!(kv_get(&dest.layout, &format!("bubbleId:{A}:local")).is_none());
    assert_eq!(
        header_ids(&dest.layout, &hash),
        vec![A.to_string(), SUB.to_string()]
    );
}

fn repack(file: &Path, dir: &Path, edit: impl FnOnce(&Path)) -> PathBuf {
    let staging = dir.join("staging");
    fs::create_dir_all(&staging).unwrap();
    tar::Archive::new(flate2::read::GzDecoder::new(fs::File::open(file).unwrap()))
        .unpack(&staging)
        .unwrap();
    edit(&staging);
    let out = dir.join("tampered.crepath");
    let gz = flate2::write::GzEncoder::new(
        fs::File::create(&out).unwrap(),
        flate2::Compression::default(),
    );
    let mut builder = tar::Builder::new(gz);
    builder.append_dir_all(".", &staging).unwrap();
    builder.into_inner().unwrap().finish().unwrap();
    out
}

#[test]
fn import_rejects_traversal_ids_and_entries() {
    let (source, _src, id) = archive_source();
    let out = TempDir::new().unwrap();
    let file = export_to(&source, &id, out.path());
    let dest = cursor_home();
    let rt = runtime(&dest.layout, false, false);
    let before = snapshot(&dest);

    let tampered = repack(&file, &out.path().join("a"), |staging| {
        let path = staging.join("manifest.json");
        let mut manifest: serde_json::Value =
            serde_json::from_str(&fs::read_to_string(&path).unwrap()).unwrap();
        manifest["workspace"]["id"] = json!("../../../evil");
        fs::write(&path, manifest.to_string()).unwrap();
    });
    let err = import_archive(&rt, &tampered, None, false).unwrap_err();
    assert!(
        format!("{err:#}").contains("invalid workspace id"),
        "{err:#}"
    );
    assert!(!dest.root.join("evil").exists());
    assert!(!dest.layout.user_dir().join("evil").exists());
    assert_same(&before, &snapshot(&dest), "rejected import");

    let tampered = repack(&file, &out.path().join("b"), |staging| {
        let path = staging.join("headers.jsonl");
        let text = fs::read_to_string(&path)
            .unwrap()
            .replace(A, "../../escape-0000-4000-8000-00000000");
        fs::write(&path, text).unwrap();
        let manifest_path = staging.join("manifest.json");
        let mut manifest: serde_json::Value =
            serde_json::from_str(&fs::read_to_string(&manifest_path).unwrap()).unwrap();
        for entry in manifest["files"].as_array_mut().unwrap() {
            if entry["path"] == "headers.jsonl" {
                use sha2::Digest;
                let digest = sha2::Sha256::digest(fs::read(&path).unwrap());
                entry["sha256"] = json!(
                    digest
                        .iter()
                        .map(|b| format!("{b:02x}"))
                        .collect::<String>()
                );
            }
        }
        fs::write(&manifest_path, manifest.to_string()).unwrap();
    });
    let err = import_archive(&rt, &tampered, None, false).unwrap_err();
    assert!(format!("{err:#}").contains("invalid chat id"), "{err:#}");
    assert_same(&before, &snapshot(&dest), "rejected import");

    let evil = out.path().join("evil.crepath");
    {
        let gz = flate2::write::GzEncoder::new(
            fs::File::create(&evil).unwrap(),
            flate2::Compression::default(),
        );
        let mut builder = tar::Builder::new(gz);
        let mut header = tar::Header::new_gnu();
        let name = b"../escape.txt";
        header.as_gnu_mut().unwrap().name[..name.len()].copy_from_slice(name);
        header.set_size(1);
        header.set_mode(0o644);
        header.set_entry_type(tar::EntryType::Regular);
        header.set_cksum();
        builder.append(&header, &b"x"[..]).unwrap();
        builder
            .into_inner()
            .unwrap()
            .finish()
            .unwrap()
            .flush()
            .unwrap();
    }
    let err = import_archive(&rt, &evil, None, false).unwrap_err();
    assert!(format!("{err:#}").contains("escapes"), "{err:#}");
    assert!(!out.path().join("escape.txt").exists());
    assert_same(&before, &snapshot(&dest), "rejected import");
}

#[test]
fn mv_project_moves_the_real_folder_and_keeps_its_identity() {
    let home = cursor_home();
    let from = home.root.join("code/app");
    let to = home.root.join("work/app");
    fs::create_dir_all(&from).unwrap();
    fs::create_dir_all(to.parent().unwrap()).unwrap();
    fs::write(from.join("main.rs"), "fn main() {}").unwrap();
    write_folder_workspace(&home.layout, "hash-app", &from);
    chat(
        &home.layout,
        A,
        "hash-app",
        &folder_identity("hash-app", &from),
    );
    kv_text(
        &home.layout,
        &format!("bubbleId:{A}:1"),
        &from.join("main.rs").display().to_string(),
    );
    let transcript = transcripts(&home, &from, A);
    fs::create_dir_all(&transcript).unwrap();

    let rt = runtime(&home.layout, false, false);
    move_paths(
        &rt,
        &[("hash-app".into(), to.display().to_string())],
        None,
        false,
        true,
    )
    .unwrap();

    assert!(!from.exists());
    assert_eq!(
        fs::read_to_string(to.join("main.rs")).unwrap(),
        "fn main() {}"
    );
    let hash = compute_workspace_hash(&to).unwrap();
    assert_eq!(header_ids(&home.layout, &hash), vec![A.to_string()]);
    assert_eq!(
        kv_str(&home.layout, &format!("bubbleId:{A}:1")).unwrap(),
        to.join("main.rs").display().to_string()
    );
    assert!(transcripts(&home, &to, A).exists());
    assert!(!transcript.exists());
}

struct CollideAfterRename {
    dest: PathBuf,
    storage: PathBuf,
    done: AtomicBool,
}

impl Probe for CollideAfterRename {
    fn instances(&self) -> anyhow::Result<Vec<Instance>> {
        FixedProbe(self.running()).instances()
    }
}

impl CollideAfterRename {
    fn running(&self) -> bool {
        if !self.done.load(Ordering::SeqCst) && self.dest.exists() {
            let dir = self
                .storage
                .join(compute_workspace_hash(&self.dest).unwrap());
            fs::create_dir_all(&dir).unwrap();
            fs::write(
                dir.join("workspace.json"),
                r#"{"folder":"file:///elsewhere"}"#,
            )
            .unwrap();
            self.done.store(true, Ordering::SeqCst);
        }
        false
    }
}

#[test]
fn collision_after_project_rename_rolls_back_every_step() {
    let home = cursor_home();
    let from = home.root.join("code/app");
    let to = home.root.join("work/app");
    fs::create_dir_all(&from).unwrap();
    fs::create_dir_all(to.parent().unwrap()).unwrap();
    fs::write(from.join("main.rs"), "fn main() {}").unwrap();
    write_folder_workspace(&home.layout, "hash-app", &from);
    chat(
        &home.layout,
        A,
        "hash-app",
        &folder_identity("hash-app", &from),
    );
    let transcript = transcripts(&home, &from, A);
    fs::create_dir_all(&transcript).unwrap();
    fs::write(
        home.layout.storage_json(),
        json!({"backupWorkspaces": {"folders": [{"folderUri": uri(&from)}]}}).to_string(),
    )
    .unwrap();
    let db_before = dump_db(&home.layout.global_db());
    let before = snapshot(&home);
    let rt = runtime_with(
        &home.layout,
        Arc::new(CollideAfterRename {
            dest: to.clone(),
            storage: home.layout.workspace_storage(),
            done: AtomicBool::new(false),
        }),
        false,
    );
    let err = move_paths(
        &rt,
        &[("hash-app".into(), to.display().to_string())],
        None,
        false,
        true,
    )
    .unwrap_err();
    let message = format!("{err:#}");
    assert!(message.contains("collision"), "{message}");
    assert!(message.contains("rolled back"), "{message}");
    assert!(from.join("main.rs").exists());
    assert!(!to.exists());
    assert_eq!(dump_db(&home.layout.global_db()), db_before);
    let mut after = without_own_state(snapshot(&home));
    let collision = home
        .layout
        .workspace_storage()
        .join(compute_renamed_hash(&from, &to).unwrap());
    let collision_rel = collision
        .strip_prefix(&home.root)
        .unwrap()
        .to_string_lossy()
        .to_string();
    assert!(
        after.contains_key(&collision_rel),
        "collision dir must survive"
    );
    after.retain(|key, _| !key.starts_with(&collision_rel));
    assert_same(
        &without_own_state(before),
        &after,
        "rolled back --project mv",
    );
    assert!(backups_empty(&home.layout));
}

struct CollideSecond {
    first: PathBuf,
    second: PathBuf,
    done: AtomicBool,
}

impl Probe for CollideSecond {
    fn instances(&self) -> anyhow::Result<Vec<Instance>> {
        FixedProbe(self.running()).instances()
    }
}

impl CollideSecond {
    fn running(&self) -> bool {
        if !self.done.load(Ordering::SeqCst) && self.first.exists() {
            fs::create_dir_all(&self.second).unwrap();
            self.done.store(true, Ordering::SeqCst);
        }
        false
    }
}

#[test]
fn failure_on_the_second_item_rolls_back_the_first() {
    let home = cursor_home();
    let sep = std::path::MAIN_SEPARATOR;
    let one = home.root.join("noble/one");
    let two = home.root.join("noble/two");
    let dest_one = home.root.join("resolute/one");
    let dest_two = home.root.join("resolute/two");
    for dir in [&one, &two, &dest_one, &dest_two] {
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
    fs::write(
        home.layout.storage_json(),
        json!({"profileAssociations": {"workspaces": {uri(&one): "a", uri(&two): "b"}}})
            .to_string(),
    )
    .unwrap();
    let db_before = dump_db(&home.layout.global_db());
    let before = snapshot(&home);
    let storage = home.layout.workspace_storage();
    let rt = runtime_with(
        &home.layout,
        Arc::new(CollideSecond {
            first: storage.join(compute_workspace_hash(&dest_one).unwrap()),
            second: storage.join(compute_workspace_hash(&dest_two).unwrap()),
            done: AtomicBool::new(false),
        }),
        false,
    );
    let err = move_paths(
        &rt,
        &[],
        Some((&format!("{sep}noble{sep}"), &format!("{sep}resolute{sep}"))),
        false,
        false,
    )
    .unwrap_err();
    assert!(format!("{err:#}").contains("rolled back"), "{err:#}");
    assert_eq!(dump_db(&home.layout.global_db()), db_before);
    let mut after = without_own_state(snapshot(&home));
    let second_rel = storage
        .join(compute_workspace_hash(&dest_two).unwrap())
        .strip_prefix(&home.root)
        .unwrap()
        .to_string_lossy()
        .to_string();
    after.remove(&second_rel);
    assert_same(&without_own_state(before), &after, "rolled back batch");
}

#[test]
fn project_slug_conflicts_are_refused_and_free_slugs_merge() {
    let home = cursor_home();
    let from = home.root.join("from");
    let to = home.root.join("to");
    fs::create_dir_all(&from).unwrap();
    fs::create_dir_all(&to).unwrap();
    write_folder_workspace(&home.layout, "hash-from", &from);
    let old_slug = home
        .layout
        .projects_dir
        .join(path_to_folder_id(normalize_path(&from)));
    let new_slug = home
        .layout
        .projects_dir
        .join(path_to_folder_id(normalize_path(&to)));
    fs::create_dir_all(old_slug.join("terminals")).unwrap();
    fs::write(old_slug.join("terminals/1.txt"), "old").unwrap();
    fs::create_dir_all(new_slug.join("terminals")).unwrap();
    fs::write(new_slug.join("terminals/1.txt"), "new").unwrap();
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
    assert!(err.to_string().contains("collision"), "{err:#}");
    assert_same(&before, &snapshot(&home), "refused slug merge");

    fs::rename(
        new_slug.join("terminals/1.txt"),
        new_slug.join("terminals/2.txt"),
    )
    .unwrap();
    move_paths(
        &rt,
        &[("hash-from".into(), to.display().to_string())],
        None,
        false,
        false,
    )
    .unwrap();
    assert!(!old_slug.exists());
    assert_eq!(
        fs::read_to_string(new_slug.join("terminals/1.txt")).unwrap(),
        "old"
    );
    assert_eq!(
        fs::read_to_string(new_slug.join("terminals/2.txt")).unwrap(),
        "new"
    );
}

struct Rich {
    home: Home,
    dest: PathBuf,
    fresh: PathBuf,
    api: PathBuf,
    unsaved_id: String,
    archive: PathBuf,
    _archive_dir: TempDir,
}

fn rich_home() -> Rich {
    let (source, _, source_id) = archive_source();
    let archive_dir = TempDir::new().unwrap();
    let archive = export_to(&source, &source_id, archive_dir.path());
    let home = cursor_home();
    let src = home.root.join("src");
    let dest = home.root.join("dest");
    let fresh = home.root.join("fresh/app");
    let api = home.root.join("api");
    for dir in [&src, &dest, &api, &fresh.parent().unwrap().to_path_buf()] {
        fs::create_dir_all(dir).unwrap();
    }
    write_folder_workspace(&home.layout, "hash-src", &src);
    let identity = folder_identity("hash-src", &src);
    let parent = json!({"subagentComposerIds": [SUB], "workspaceIdentifier": identity});
    insert_header(&home.layout, A, "hash-src", false, &parent);
    kv_text(
        &home.layout,
        &format!("composerData:{A}"),
        &parent.to_string(),
    );
    insert_header(
        &home.layout,
        SUB,
        "hash-src",
        true,
        &json!({"workspaceIdentifier": identity}),
    );
    chat(
        &home.layout,
        B,
        "hash-src",
        &folder_identity("stale", &home.root.join("gone")),
    );
    kv(&home.layout, &format!("bubbleId:{A}:null"), Value::Null);
    kv_text(
        &home.layout,
        &format!("bubbleId:{A}:path"),
        &src.display().to_string(),
    );
    kv_text(
        &home.layout,
        "inlineDiff:hash-src:1",
        &src.display().to_string(),
    );
    set_local(&home.layout, "hash-src", &[A, B], &[A]);
    fs::create_dir_all(transcripts(&home, &src, A)).unwrap();
    fs::write(
        home.layout.storage_json(),
        serde_json::to_string_pretty(&json!({
            "backupWorkspaces": {"folders": [{"folderUri": uri(&src)}]},
            "profileAssociations": {"workspaces": {uri(&src): "__default__profile__"}}
        }))
        .unwrap(),
    )
    .unwrap();
    let unsaved_id = "1765558213752".to_string();
    let unsaved_file = home
        .layout
        .cursor_root
        .join("Workspaces")
        .join(&unsaved_id)
        .join("workspace.json");
    fs::create_dir_all(unsaved_file.parent().unwrap()).unwrap();
    fs::write(&unsaved_file, "{\"folders\":[]}").unwrap();
    let unsaved_hash = compute_workspace_hash(&unsaved_file).unwrap();
    write_workspace(&home.layout, &unsaved_hash, "workspace", &unsaved_file);
    chat(
        &home.layout,
        C,
        &unsaved_hash,
        &json!({"id": unsaved_hash, "configPath": components(&unsaved_file)}),
    );
    Rich {
        home,
        dest,
        fresh,
        api,
        unsaved_id,
        archive,
        _archive_dir: archive_dir,
    }
}

type DryRun<'a> = Box<dyn Fn() -> anyhow::Result<engine::Report> + 'a>;

#[test]
fn dry_run_is_byte_identical_for_every_write_command() {
    let rich = rich_home();
    let home = &rich.home;
    let rt = runtime(&home.layout, false, true);
    let before = snapshot(home);
    let sep = std::path::MAIN_SEPARATOR;
    let text = |path: &Path| path.display().to_string();
    let mut assigned = BTreeMap::new();
    assigned.insert(A.to_string(), vec![rich.api.clone()]);
    assigned.insert(B.to_string(), vec![rich.api.clone(), rich.dest.clone()]);
    let archive_target = home.root.join("dest");
    let runs: Vec<(&str, DryRun<'_>)> = vec![
        (
            "mv",
            Box::new(|| {
                move_paths(
                    &rt,
                    &[("hash-src".into(), text(&rich.dest))],
                    None,
                    false,
                    false,
                )
            }),
        ),
        (
            "mv --replace",
            Box::new(|| {
                move_paths(
                    &rt,
                    &[],
                    Some((&format!("{sep}src"), &format!("{sep}dest"))),
                    false,
                    false,
                )
            }),
        ),
        (
            "mv --project",
            Box::new(|| {
                move_paths(
                    &rt,
                    &[("hash-src".into(), text(&rich.fresh))],
                    None,
                    false,
                    true,
                )
            }),
        ),
        (
            "cp",
            Box::new(|| {
                copy_paths(
                    &rt,
                    &[("hash-src".into(), text(&rich.dest))],
                    None,
                    false,
                    false,
                )
            }),
        ),
        (
            "cp --project",
            Box::new(|| {
                copy_paths(
                    &rt,
                    &[("hash-src".into(), text(&rich.fresh))],
                    None,
                    false,
                    true,
                )
            }),
        ),
        (
            "save",
            Box::new(|| save_unsaved(&rt, &rich.unsaved_id, &rich.dest)),
        ),
        (
            "split",
            Box::new(|| {
                split_workspace(
                    &rt,
                    "hash-src",
                    &[rich.api.clone(), rich.dest.clone()],
                    &assigned,
                    false,
                )
            }),
        ),
        (
            "split --move",
            Box::new(|| {
                split_workspace(
                    &rt,
                    "hash-src",
                    &[rich.api.clone(), rich.dest.clone()],
                    &assigned,
                    true,
                )
            }),
        ),
        (
            "combine",
            Box::new(|| combine_workspaces(&rt, &rich.api, &["hash-src".to_string()], false)),
        ),
        (
            "combine --move",
            Box::new(|| {
                combine_workspaces(
                    &rt,
                    &rich.api,
                    &["hash-src".to_string(), C.to_string()],
                    true,
                )
            }),
        ),
        ("rx", Box::new(|| reindex(&rt, "hash-src"))),
        (
            "rm workspace",
            Box::new(|| remove_targets(&rt, &["hash-src".to_string()])),
        ),
        (
            "rm chat",
            Box::new(|| remove_targets(&rt, &[A.to_string()])),
        ),
        (
            "import",
            Box::new(|| import_archive(&rt, &rich.archive, None, false)),
        ),
        (
            "import TO",
            Box::new(|| import_archive(&rt, &rich.archive, Some(&archive_target), true)),
        ),
        (
            "export",
            Box::new(|| export_workspace(&rt, "hash-src", &home.root.join("out.crepath"))),
        ),
    ];
    for (name, run) in runs {
        let report = run().unwrap_or_else(|err| panic!("{name}: {err:#}"));
        assert!(
            report.applied.iter().any(|line| line.contains("dry-run")),
            "{name}: {:?}",
            report.applied
        );
        assert_same(&before, &snapshot(home), name);
    }
}

#[test]
fn read_only_commands_leave_the_layout_untouched() {
    let rich = rich_home();
    let home = &rich.home;
    let rt = runtime(&home.layout, false, false);
    let before = snapshot(home);
    list_workspaces(&rt, false, None).unwrap();
    list_workspaces(&rt, false, Some("hash-src")).unwrap();
    list_workspaces(&rt, true, None).unwrap();
    show_stats(&rt).unwrap();
    show_history(&rt).unwrap();
    let source = engine::find_workspace(&rt, "hash-src").unwrap().unwrap();
    suggest_split(&rt, &source, std::slice::from_ref(&rich.api)).unwrap();
    let out = TempDir::new().unwrap();
    export_workspace(&rt, "hash-src", &out.path().join("a.crepath")).unwrap();
    assert_same(&before, &snapshot(home), "read-only commands");
}

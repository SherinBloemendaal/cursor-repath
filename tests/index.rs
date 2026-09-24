mod common;

use std::fs;
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};

use clap::{CommandFactory, Parser};
use common::*;
use crepath::cli::{self, Cli};
use crepath::cursor::workspace::compute_workspace_hash;
use crepath::engine::index::{self, Config, Index, Lock, Note, Spawner};
use crepath::engine::{self, FixedProbe, Layout, Runtime, cache, suggest_split};
use crepath::ui::Theme;
use rusqlite::params;
use serde_json::{Value as Json, json};

const ORPHAN: &str = "ffffffff-0000-4000-8000-000000000006";
const LATE: &str = "99999999-0000-4000-8000-000000000007";

#[derive(Default)]
struct Recorder(Mutex<Vec<PathBuf>>);

impl Spawner for Recorder {
    fn spawn(&self, home: &Path) -> anyhow::Result<()> {
        self.0.lock().unwrap().push(home.to_path_buf());
        Ok(())
    }
}

impl Recorder {
    fn count(&self) -> usize {
        self.0.lock().unwrap().len()
    }
}

struct Machine {
    home: Home,
    other: Layout,
    app: PathBuf,
    api: PathBuf,
    spawns: Arc<Recorder>,
}

fn usage(cents: u64, model: &str) -> Json {
    json!({
        "usageData": {"m": {"costInCents": cents, "amount": 3}},
        "contextTokensUsed": 4_000,
        "conversation": [{"tokenCount": {"inputTokens": 100, "outputTokens": 20}}],
        "modelConfig": {"modelName": model}
    })
}

fn chat_with(layout: &Layout, id: &str, workspace: &str, sub: bool, path: &Path, data: Json) {
    let identity = folder_identity(workspace, path);
    insert_header(
        layout,
        id,
        workspace,
        sub,
        &json!({"name": format!("title {id}"), "workspaceIdentifier": identity}),
    );
    let mut data = data;
    data["composerId"] = json!(id);
    data["workspaceIdentifier"] = identity;
    kv_text(layout, &format!("composerData:{id}"), &data.to_string());
}

fn set_updated(layout: &Layout, id: &str, at: i64) {
    global(layout)
        .execute(
            "UPDATE composerHeaders SET lastUpdatedAt = ?2 WHERE composerId = ?1",
            params![id, at],
        )
        .unwrap();
}

fn machine() -> Machine {
    let home = cursor_home();
    let other = install_layout(
        &home.root,
        "resolved",
        home.root.join("dot-cursor-resolved"),
    );
    let app = native(&home.root, "code/app");
    let api = native(&home.root, "code/api");
    let gone = native(&home.root, "code/gone");
    for dir in [&app, &api] {
        fs::create_dir_all(dir).unwrap();
    }
    write_folder_workspace(&home.layout, "hash-app", &app);
    write_folder_workspace(&home.layout, "hash-gone", &gone);
    write_folder_workspace(&other, "hash-api", &api);
    fs::write(
        home.layout.storage_json(),
        json!({
            "userDataProfiles": [{"location": "-1a2b", "name": "Work"}],
            "profileAssociations": {"workspaces": {uri(&app): "-1a2b"}}
        })
        .to_string(),
    )
    .unwrap();
    chat_with(
        &home.layout,
        A,
        "hash-app",
        false,
        &app,
        usage(1_234, "opus"),
    );
    chat_with(
        &home.layout,
        SUB,
        "hash-app",
        true,
        &app,
        usage(66, "haiku"),
    );
    chat_with(&home.layout, B, "hash-gone", false, &gone, json!({}));
    global(&home.layout)
        .execute(
            "UPDATE composerHeaders SET isArchived = 1, createdAt = 1760000000000 WHERE composerId = ?1",
            [B],
        )
        .unwrap();
    chat_with(
        &home.layout,
        ORPHAN,
        "orphan-hash",
        false,
        &native(&home.root, "code/old"),
        usage(10, "opus"),
    );
    chat_with(&other, C, "hash-api", false, &api, usage(500, "gpt"));
    Machine {
        home,
        other,
        app,
        api,
        spawns: Arc::new(Recorder::default()),
    }
}

fn indexed(machine: &Machine) -> Runtime {
    Runtime {
        layout: machine.home.layout.clone(),
        installs: vec![machine.home.layout.clone(), machine.other.clone()],
        pinned: false,
        dry_run: false,
        yes: true,
        profile: None,
        probe: Arc::new(FixedProbe(false)),
        quiet: true,
        index: Some(Config::new(machine.spawns.clone())),
    }
}

fn live(machine: &Machine) -> Runtime {
    Runtime {
        index: None,
        ..indexed(machine)
    }
}

fn state(machine: &Machine) -> &Path {
    &machine.home.layout.crepath_home
}

fn ls(rt: &Runtime) -> (String, Option<Note>) {
    engine::render_workspaces_noted(rt, false, None, Theme::plain()).unwrap()
}

fn detail(rt: &Runtime, id: &str) -> String {
    engine::render_workspaces(rt, false, Some(id), Theme::plain()).unwrap()
}

fn stats(rt: &Runtime) -> (String, Option<Note>) {
    engine::render_stats_noted(rt, Theme::plain()).unwrap()
}

fn run(rt: &Runtime, args: &[&str]) -> anyhow::Result<()> {
    let argv = std::iter::once("crepath").chain(args.iter().copied());
    cli::dispatch(Cli::try_parse_from(argv).unwrap(), Some(rt.clone()))
}

fn age(machine: &Machine) {
    let index = Index::open(state(machine)).unwrap();
    index.set_meta("refreshed_at", "0").unwrap();
    index.set_meta("spawned_at", "0").unwrap();
}

fn scan_of(machine: &Machine, install: &str) -> index::Scan {
    Index::open(state(machine))
        .unwrap()
        .scans()
        .unwrap()
        .remove(install)
        .unwrap()
}

fn outcome<'a>(outcomes: &'a [index::Outcome], install: &str) -> &'a index::Outcome {
    outcomes
        .iter()
        .find(|outcome| outcome.install == install)
        .unwrap()
}

fn dead_pid() -> u32 {
    let mut child = std::process::Command::new(std::env::current_exe().unwrap())
        .arg("--list")
        .stdout(std::process::Stdio::null())
        .spawn()
        .unwrap();
    let pid = child.id();
    child.wait().unwrap();
    pid
}

#[test]
fn the_first_run_builds_the_index_and_matches_live_output() {
    let machine = machine();
    let rt = indexed(&machine);
    assert!(!index::db_path(state(&machine)).exists());
    let (text, note) = ls(&rt);
    assert_eq!(text, ls(&live(&machine)).0);
    assert!(note.is_none());
    assert!(index::db_path(state(&machine)).exists());
    for install in ["default", "resolved"] {
        assert!(scan_of(&machine, install).dirty_at.is_none());
    }
    assert!(text.contains("default/Work"), "{text}");
    assert!(text.contains("1 missing destination"), "{text}");
    let (with_index, _) = stats(&rt);
    assert_eq!(with_index, stats(&live(&machine)).0);
    for needle in [
        "$18.10",
        "4 chats",
        "1 subagent chat",
        "1 archived chat",
        "opus",
    ] {
        assert!(
            with_index.contains(needle),
            "missing {needle} in\n{with_index}"
        );
    }
    assert_eq!(detail(&rt, "hash-app"), detail(&live(&machine), "hash-app"));
    let work = cli::select_profile(rt.clone(), "default/Work").unwrap();
    let live_work = cli::select_profile(live(&machine), "default/Work").unwrap();
    assert_eq!(stats(&work).0, stats(&live_work).0);
    assert_eq!(
        engine::render_workspaces(&work, false, None, Theme::plain()).unwrap(),
        engine::render_workspaces(&live_work, false, None, Theme::plain()).unwrap()
    );
    assert_eq!(machine.spawns.count(), 0);
}

#[test]
fn later_runs_render_from_the_index_without_reading_cursor() {
    let machine = machine();
    let rt = indexed(&machine);
    let listed = ls(&rt).0;
    let counted = stats(&rt).0;
    let shown = detail(&rt, "hash-app");
    let pickable = engine::picker_workspaces(&rt).unwrap();
    for layout in [&machine.home.layout, &machine.other] {
        fs::rename(
            &layout.cursor_root,
            layout.cursor_root.with_extension("gone"),
        )
        .unwrap();
    }
    assert!(!machine.home.layout.global_db().exists());
    let (text, note) = ls(&rt);
    assert_eq!(text, listed);
    let note = note.expect("an as-of note");
    assert!(!note.background, "a refresh just ran, so none starts");
    assert!(note.text(Theme::plain()).starts_with("as of "));
    assert_eq!(stats(&rt).0, counted);
    assert_eq!(detail(&rt, "hash-app"), shown);
    let from_index: Vec<String> = engine::picker_workspaces(&rt)
        .unwrap()
        .into_iter()
        .map(|workspace| workspace.id)
        .collect();
    let before: Vec<String> = pickable.into_iter().map(|workspace| workspace.id).collect();
    assert_eq!(from_index, before);
    assert_eq!(machine.spawns.count(), 0);

    age(&machine);
    let (_, note) = ls(&rt);
    let note = note.unwrap();
    assert!(note.background);
    assert!(
        note.text(Theme::plain())
            .ends_with(" · refreshing in background")
    );
    assert_eq!(
        *machine.spawns.0.lock().unwrap(),
        vec![state(&machine).to_path_buf()]
    );
    let (_, note) = stats(&rt);
    assert!(!note.unwrap().background, "spawned under a minute ago");
    assert_eq!(machine.spawns.count(), 1);
}

#[test]
fn a_refresh_rereads_only_what_changed() {
    let machine = machine();
    let rt = indexed(&machine);
    let first = cache::scan(&rt, false).unwrap().outcomes;
    assert_eq!(outcome(&first, "default").chats_read, 4);
    assert_eq!(outcome(&first, "resolved").chats_read, 1);
    assert!(outcome(&first, "default").changed > 0);
    let again = cache::scan(&rt, false).unwrap().outcomes;
    for install in ["default", "resolved"] {
        assert_eq!(outcome(&again, install).changed, 0, "{again:?}");
        assert_eq!(outcome(&again, install).chats_read, 0, "{again:?}");
    }

    let moved = native(&machine.home.root, "code/app-elsewhere");
    fs::create_dir_all(&moved).unwrap();
    fs::write(
        machine
            .home
            .layout
            .workspace_storage()
            .join("hash-app/workspace.json"),
        json!({"folder": uri(&moved)}).to_string(),
    )
    .unwrap();
    let after = cache::scan(&rt, false).unwrap().outcomes;
    assert_eq!(outcome(&after, "default").changed, 1, "{after:?}");
    assert_eq!(outcome(&after, "default").chats_read, 0);
    assert_eq!(outcome(&after, "resolved").changed, 0);
    let (text, _) = ls(&rt);
    assert!(text.contains("app-elsewhere"), "{text}");
    assert_eq!(text, ls(&live(&machine)).0);

    set_updated(&machine.home.layout, A, 1_800_000_000_000);
    global(&machine.home.layout)
        .execute(
            "UPDATE cursorDiskKV SET value = ?2 WHERE key = ?1",
            params![
                format!("composerData:{A}"),
                usage(9_900, "opus").to_string()
            ],
        )
        .unwrap();
    let after = cache::scan(&rt, false).unwrap().outcomes;
    assert_eq!(outcome(&after, "default").chats_read, 1, "{after:?}");
    assert!(outcome(&after, "default").changed >= 1);
    assert_eq!(outcome(&after, "resolved").chats_read, 0);
    let (text, _) = stats(&rt);
    assert!(text.contains("$104.76"), "{text}");
    assert_eq!(text, stats(&live(&machine)).0);
}

#[test]
fn vanished_chats_and_workspaces_leave_the_index() {
    let machine = machine();
    let rt = indexed(&machine);
    ls(&rt);
    global(&machine.home.layout)
        .execute(
            "DELETE FROM composerHeaders WHERE composerId = ?1",
            [ORPHAN],
        )
        .unwrap();
    fs::remove_dir_all(machine.home.layout.workspace_storage().join("hash-gone")).unwrap();
    let after = cache::scan(&rt, false).unwrap().outcomes;
    assert_eq!(outcome(&after, "default").workspaces, 1);
    assert_eq!(outcome(&after, "default").chats, 2);
    let counts = Index::open(state(&machine))
        .unwrap()
        .counts("default")
        .unwrap();
    assert_eq!(
        (counts.workspaces, counts.chats, counts.subagents),
        (1, 2, 1)
    );
    assert_eq!(ls(&rt).0, ls(&live(&machine)).0);
    assert_eq!(stats(&rt).0, stats(&live(&machine)).0);
    assert!(!stats(&rt).0.contains("code/old"));
}

#[test]
fn fresh_reads_live_data_and_refreshes_the_index() {
    let machine = machine();
    let rt = indexed(&machine);
    ls(&rt);
    chat_with(
        &machine.home.layout,
        LATE,
        "hash-app",
        false,
        &machine.app,
        json!({}),
    );
    let stale = ls(&rt).0;
    let current = ls(&live(&machine)).0;
    assert_ne!(stale, current, "the index should lag behind");
    let (text, note) = ls(&rt.fresh());
    assert_eq!(text, current);
    assert!(note.is_none());
    let (text, note) = ls(&rt);
    assert_eq!(text, current);
    assert!(note.is_some());
    run(&rt, &["stats", "--fresh"]).unwrap();
    assert!(
        run(&rt, &["ls", "--fresh", "hash-app"]).is_ok(),
        "ls --fresh with an id"
    );
}

#[test]
fn crepath_no_index_turns_the_index_off() {
    let machine = machine();
    assert!(Config::from_env(Some("1")).is_none());
    assert!(Config::from_env(Some("yes")).is_none());
    assert!(Config::from_env(Some("0")).is_some());
    assert!(Config::from_env(Some("")).is_some());
    assert!(Config::from_env(None).is_some());
    assert_eq!(std::env::var("CREPATH_NO_INDEX").as_deref(), Ok("1"));
    assert!(cli::index_config().is_none());
    let rt = live(&machine);
    ls(&rt);
    stats(&rt);
    run(&rt, &["ls", "--fresh"]).unwrap();
    assert!(!index::db_path(state(&machine)).exists());
    assert_eq!(machine.spawns.count(), 0);
}

#[test]
fn a_stale_lock_is_recovered() {
    let machine = machine();
    let rt = indexed(&machine);
    fs::create_dir_all(state(&machine)).unwrap();
    let lock = index::lock_path(state(&machine));
    fs::write(
        &lock,
        json!({"pid": dead_pid(), "started": index::now_ms()}).to_string(),
    )
    .unwrap();
    assert!(!index::holder(state(&machine)).unwrap().alive);
    let (text, _) = ls(&rt);
    assert_eq!(text, ls(&live(&machine)).0);
    assert!(!lock.exists());
    fs::write(
        &lock,
        json!({"pid": dead_pid(), "started": index::now_ms()}).to_string(),
    )
    .unwrap();
    cache::scan(&rt, false).unwrap();
    assert!(!lock.exists());
}

#[test]
fn a_live_lock_blocks_a_second_refresh() {
    let machine = machine();
    let rt = indexed(&machine);
    let held = Lock::acquire(state(&machine)).unwrap().unwrap();
    let err = cache::scan(&rt, false).err().unwrap().to_string();
    assert!(err.contains("already running"), "{err}");
    assert!(
        err.contains(&format!("pid {}", std::process::id())),
        "{err}"
    );
    index::background(&rt).unwrap();
    assert!(!index::db_path(state(&machine)).exists());
    drop(held);

    ls(&rt);
    age(&machine);
    let held = Lock::acquire(state(&machine)).unwrap().unwrap();
    let (_, note) = ls(&rt);
    assert!(note.unwrap().background, "the running refresh counts");
    assert_eq!(machine.spawns.count(), 0);
    let err = run(&rt, &["cache", "clear"]).unwrap_err().to_string();
    assert!(err.contains("an index refresh is running"), "{err}");
    assert!(index::db_path(state(&machine)).exists());
    drop(held);
}

#[test]
fn readers_see_the_last_committed_refresh() {
    let machine = machine();
    let rt = indexed(&machine);
    let before = ls(&rt).0;
    let writer = rusqlite::Connection::open(index::db_path(state(&machine))).unwrap();
    writer
        .execute_batch("BEGIN IMMEDIATE; DELETE FROM chats; DELETE FROM workspaces;")
        .unwrap();
    assert_eq!(ls(&rt).0, before);
    writer.execute_batch("ROLLBACK").unwrap();
}

#[test]
fn cache_clear_drops_everything_or_one_installation() {
    let machine = machine();
    let rt = indexed(&machine);
    ls(&rt);
    run(&rt, &["cache", "clear", "--profile", "resolved"]).unwrap();
    let index = Index::open(state(&machine)).unwrap();
    assert_eq!(index.installs().unwrap(), vec!["default".to_string()]);
    assert_eq!(index.counts("resolved").unwrap().workspaces, 0);
    assert_eq!(index.counts("default").unwrap().workspaces, 2);
    drop(index);
    let err = run(&rt, &["cache", "clear", "--profile", "default/Work"])
        .unwrap_err()
        .to_string();
    assert!(err.contains("per Cursor installation"), "{err}");

    fs::write(
        index::lock_path(state(&machine)),
        json!({"pid": dead_pid(), "started": index::now_ms()}).to_string(),
    )
    .unwrap();
    run(&rt, &["cache", "clear", "-n"]).unwrap();
    assert!(index::db_path(state(&machine)).exists());
    assert!(index::lock_path(state(&machine)).exists());
    run(&rt, &["cache", "clear"]).unwrap();
    for suffix in ["", "-wal", "-shm"] {
        let path = format!("{}{suffix}", index::db_path(state(&machine)).display());
        assert!(!Path::new(&path).exists(), "{path}");
    }
    assert!(!index::lock_path(state(&machine)).exists());
    run(&rt, &["cache", "clear"]).unwrap();
    let history = fs::read_to_string(machine.home.layout.history_file()).unwrap();
    assert_eq!(
        history.matches(r#""command":"cache""#).count(),
        4,
        "{history}"
    );
    assert!(history.contains(r#""args":["clear"]"#), "{history}");
}

#[test]
fn cache_scan_full_rebuilds_and_can_target_one_installation() {
    let machine = machine();
    let rt = indexed(&machine);
    cache::scan(&rt, false).unwrap();
    let full = cache::scan(&rt, true).unwrap();
    assert!(full.full);
    assert_eq!(outcome(&full.outcomes, "default").chats_read, 4);
    assert_eq!(outcome(&full.outcomes, "resolved").chats_read, 1);
    assert_eq!(outcome(&full.outcomes, "default").changed, 3 + 2 * 4);
    let pinned = cli::select_profile(rt.clone(), "resolved").unwrap();
    let one = cache::scan(&pinned, true).unwrap();
    assert_eq!(one.outcomes.len(), 1);
    assert_eq!(one.outcomes[0].install, "resolved");
    let text = cache::render_scan(Theme::plain(), &full);
    assert!(text.contains("==> Index rebuild"), "{text}");
    assert!(text.contains("2 installations scanned"), "{text}");
    run(&rt, &["cache", "scan", "--full"]).unwrap();
    run(&rt, &["cache", "scan", "-n"]).unwrap();
    assert_eq!(ls(&rt).0, ls(&live(&machine)).0);
}

#[test]
fn cache_stats_reports_freshness_without_opening_cursor() {
    let machine = machine();
    let rt = indexed(&machine);
    let empty = cache::collect(&rt).unwrap();
    assert!(!empty.exists);
    assert!(empty.installs.iter().all(|row| row.scan.is_none()));
    ls(&rt);
    let fresh = cache::collect(&rt).unwrap();
    for row in &fresh.installs {
        let stale = row.stale.as_ref().unwrap();
        assert!(stale.reasons.is_empty(), "{}: {stale:?}", row.name);
    }
    chat_with(
        &machine.home.layout,
        LATE,
        "hash-app",
        false,
        &machine.app,
        json!({}),
    );
    let extra = native(&machine.home.root, "code/extra");
    fs::create_dir_all(&extra).unwrap();
    write_folder_workspace(&machine.home.layout, "hash-extra", &extra);
    let stats = cache::collect(&rt).unwrap();
    let default = stats
        .installs
        .iter()
        .find(|row| row.name == "default")
        .unwrap();
    let stale = default.stale.as_ref().unwrap();
    assert!(
        stale.reasons.contains(&"global db changed".to_string()),
        "{stale:?}"
    );
    assert!(
        stale.reasons.contains(&"1 new workspace".to_string()),
        "{stale:?}"
    );
    assert!(stale.sources >= 4, "{stale:?}");
    assert_eq!(default.counts.workspaces, 2);
    assert_eq!(default.counts.chats, 3);
    assert_eq!(default.counts.subagents, 1);
    let resolved = stats
        .installs
        .iter()
        .find(|row| row.name == "resolved")
        .unwrap();
    assert!(resolved.stale.as_ref().unwrap().reasons.is_empty());
    let out = cache::render_stats(Theme::plain(), &stats, index::now_ms());
    for text in [
        "==> Index",
        "version 1",
        "│ installation │ last scan",
        "stale sources",
        "global db changed, 1 new workspace",
        "up to date",
        "idle",
    ] {
        assert!(out.contains(text), "missing {text:?} in\n{out}");
    }
    let held = Lock::acquire(state(&machine)).unwrap().unwrap();
    let out = cache::render_stats(
        Theme::plain(),
        &cache::collect(&rt).unwrap(),
        index::now_ms(),
    );
    assert!(
        out.contains(&format!("running (pid {}", std::process::id())),
        "{out}"
    );
    drop(held);
    run(&rt, &["cache", "stats"]).unwrap();
}

#[test]
fn write_commands_use_live_data_even_when_the_index_is_stale() {
    let machine = machine();
    let rt = indexed(&machine);
    ls(&rt);
    chat_with(
        &machine.home.layout,
        LATE,
        "hash-app",
        false,
        &machine.app,
        json!({}),
    );
    let stale = Index::open(state(&machine)).unwrap();
    assert_eq!(stale.counts("default").unwrap().chats, 3);
    drop(stale);
    let dest = native(&machine.home.root, "code/app-moved");
    fs::create_dir_all(&dest).unwrap();
    run(
        &rt,
        &[
            "mv",
            &machine.app.display().to_string(),
            &dest.display().to_string(),
        ],
    )
    .unwrap();
    let hash = compute_workspace_hash(&dest).unwrap();
    assert_eq!(header_ids(&machine.home.layout, &hash).len(), 3);
    assert!(header_ids(&machine.home.layout, "hash-app").is_empty());
    assert!(scan_of(&machine, "default").dirty_at.is_some());
    assert_eq!(machine.spawns.count(), 1, "a write starts a refresh");
    std::thread::sleep(std::time::Duration::from_millis(5));
    let (text, _) = ls(&rt);
    assert_eq!(text, ls(&live(&machine)).0);
    assert!(text.contains("app-moved"), "{text}");
    assert!(scan_of(&machine, "default").dirty_at.is_none());

    chat_with(
        &machine.other,
        ORPHAN,
        "hash-api",
        false,
        &machine.api,
        json!({}),
    );
    run(&rt, &["rm", ORPHAN, "--profile", "resolved"]).unwrap();
    assert_eq!(header_ids(&machine.other, "hash-api"), vec![C.to_string()]);
    assert_eq!(stats(&rt).0, stats(&live(&machine)).0);
}

#[test]
fn split_suggestions_reuse_evidence_until_the_chat_changes() {
    let machine = machine();
    let rt = indexed(&machine);
    let api = native(&machine.home.root, "code/app/api");
    fs::create_dir_all(api.join("src")).unwrap();
    let file = native(&api, "src/main.rs");
    kv_text(
        &machine.home.layout,
        &format!("ofsContent:{A}:{}", uri(&file)),
        "body",
    );
    let source = engine::find_workspace(&rt, "hash-app").unwrap().unwrap();
    let targets = [api.clone()];
    let first = suggest_split(&rt, &source, &targets).unwrap();
    assert!(first.assigned[A][0].ends_with("api"));
    global(&machine.home.layout)
        .execute("DELETE FROM cursorDiskKV WHERE key LIKE 'ofsContent:%'", [])
        .unwrap();
    let live_now = suggest_split(&live(&machine), &source, &targets).unwrap();
    assert!(live_now.unassigned.contains(&A.to_string()));
    let cached = suggest_split(&rt, &source, &targets).unwrap();
    assert_eq!(cached.assigned, first.assigned);
    set_updated(&machine.home.layout, A, 1_900_000_000_000);
    let recomputed = suggest_split(&rt, &source, &targets).unwrap();
    assert!(recomputed.unassigned.contains(&A.to_string()));
    assert!(recomputed.assigned.is_empty());
}

#[test]
fn writes_drop_the_evidence_of_every_chat_they_touch() {
    let machine = machine();
    let rt = indexed(&machine);
    ls(&rt);
    let api = native(&machine.home.root, "code/app/api");
    fs::create_dir_all(&api).unwrap();
    kv_text(
        &machine.home.layout,
        &format!("ofsContent:{A}:{}", uri(&native(&api, "x.rs"))),
        "body",
    );
    let source = engine::find_workspace(&rt, "hash-app").unwrap().unwrap();
    suggest_split(&rt, &source, std::slice::from_ref(&api)).unwrap();
    let ids = [A.to_string()];
    assert_eq!(
        Index::open(state(&machine))
            .unwrap()
            .evidence("default", &ids)
            .unwrap()
            .len(),
        1
    );
    let moved = native(&machine.home.root, "code/app-two");
    fs::create_dir_all(&moved).unwrap();
    run(&rt, &["mv", "hash-app", &moved.display().to_string()]).unwrap();
    assert!(
        Index::open(state(&machine))
            .unwrap()
            .evidence("default", &ids)
            .unwrap()
            .is_empty()
    );
}

#[test]
fn a_schema_change_rebuilds_the_index() {
    let machine = machine();
    let rt = indexed(&machine);
    ls(&rt);
    rusqlite::Connection::open(index::db_path(state(&machine)))
        .unwrap()
        .pragma_update(None, "user_version", 0)
        .unwrap();
    let stats = cache::collect(&rt).unwrap();
    assert_eq!(stats.version, Some(0));
    let out = cache::render_stats(Theme::plain(), &stats, index::now_ms());
    assert!(out.contains("rebuilt as version 1 on next use"), "{out}");
    let (text, note) = ls(&rt);
    assert!(note.is_none(), "rebuilt in the foreground");
    assert_eq!(text, ls(&live(&machine)).0);
    assert_eq!(
        Index::open(state(&machine)).unwrap().version().unwrap(),
        index::SCHEMA
    );
}

#[test]
fn the_background_refresh_is_hidden_and_not_logged() {
    let machine = machine();
    let rt = indexed(&machine);
    run(&rt, &[index::REFRESH_COMMAND]).unwrap();
    for install in ["default", "resolved"] {
        assert!(scan_of(&machine, install).scanned_at > 0);
    }
    assert!(!machine.home.layout.history_file().exists());
    let mut help = Vec::new();
    Cli::command().write_help(&mut help).unwrap();
    assert!(
        !String::from_utf8(help)
            .unwrap()
            .contains(index::REFRESH_COMMAND)
    );
    assert!(!crepath::ui::help_text(Theme::plain(), 100).contains(index::REFRESH_COMMAND));

    let mut three = rt.clone();
    three.installs.push(install_layout(
        &machine.home.root,
        "legacy",
        machine.home.root.join("dot-cursor-legacy"),
    ));
    run(&three, &[index::REFRESH_COMMAND]).unwrap();
    assert!(scan_of(&machine, "legacy").scanned_at > 0);
    run(&rt, &[index::REFRESH_COMMAND]).unwrap();
    assert_eq!(
        Index::open(state(&machine)).unwrap().installs().unwrap(),
        vec!["default".to_string(), "resolved".to_string()]
    );
}

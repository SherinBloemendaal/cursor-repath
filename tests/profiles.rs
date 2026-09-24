mod common;

use std::collections::BTreeMap;
use std::fs::{self, File};
use std::path::{Path, PathBuf};
use std::sync::Arc;

use clap::Parser;
use common::*;
use crepath::cli::{self, Cli, CommonArgs};
use crepath::cursor::folder_id::path_to_folder_id;
use crepath::cursor::process::{ProcessInfo, ProcessSource};
use crepath::cursor::uri::normalize_path;
use crepath::cursor::workspace::compute_workspace_hash;
use crepath::engine::{
    self, FixedProbe, Layout, Probe, ProcessProbe, Runtime, list_rows, move_paths,
};
use crepath::ui::Theme;
use serde_json::json;

const E: &str = "eeeeeeee-0000-4000-8000-000000000005";

struct Machine {
    home: Home,
    other: Layout,
    app: PathBuf,
    api: PathBuf,
    shared: PathBuf,
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
    let shared = native(&home.root, "code/shared");
    for dir in [&app, &api, &shared] {
        fs::create_dir_all(dir).unwrap();
    }
    write_folder_workspace(&home.layout, "hash-app", &app);
    chat(
        &home.layout,
        A,
        "hash-app",
        &folder_identity("hash-app", &app),
    );
    write_folder_workspace(&other, "hash-api", &api);
    chat(&other, B, "hash-api", &folder_identity("hash-api", &api));
    write_folder_workspace(&home.layout, "hash-shared", &shared);
    chat(
        &home.layout,
        C,
        "hash-shared",
        &folder_identity("hash-shared", &shared),
    );
    write_folder_workspace(&other, "hash-shared", &shared);
    chat(
        &other,
        E,
        "hash-shared",
        &folder_identity("hash-shared", &shared),
    );
    let slug = home.layout.projects_dir.join(slug_of(&shared));
    for id in [C, E] {
        let dir = slug.join("agent-transcripts").join(id);
        fs::create_dir_all(&dir).unwrap();
        fs::write(dir.join(format!("{id}.jsonl")), id).unwrap();
    }
    fs::create_dir_all(slug.join("terminals")).unwrap();
    fs::write(native(&slug, "terminals/1.txt"), "term").unwrap();
    Machine {
        home,
        other,
        app,
        api,
        shared,
    }
}

fn slug_of(path: &Path) -> String {
    path_to_folder_id(normalize_path(path))
}

fn runtime_of(machine: &Machine, probe: Arc<dyn Probe>) -> Runtime {
    Runtime {
        layout: machine.home.layout.clone(),
        installs: vec![machine.home.layout.clone(), machine.other.clone()],
        pinned: false,
        dry_run: false,
        yes: true,
        profile: None,
        probe,
        quiet: true,
        index: None,
    }
}

fn idle(machine: &Machine) -> Runtime {
    runtime_of(machine, Arc::new(FixedProbe(false)))
}

fn run(rt: &Runtime, args: &[&str]) -> anyhow::Result<()> {
    let argv = std::iter::once("crepath").chain(args.iter().copied());
    cli::dispatch(Cli::try_parse_from(argv).unwrap(), Some(rt.clone()))
}

fn with_profile(rt: &Runtime, selector: &str) -> anyhow::Result<Runtime> {
    cli::configure(
        rt.clone(),
        &CommonArgs {
            dry_run: false,
            yes: false,
            profile: Some(selector.to_string()),
            replace: Vec::new(),
            regex: false,
            unsaved: false,
        },
    )
}

fn tree(root: &Path) -> BTreeMap<String, Option<Vec<u8>>> {
    let mut out = BTreeMap::new();
    for entry in walkdir::WalkDir::new(root).sort_by_file_name() {
        let entry = entry.unwrap();
        let rel = entry
            .path()
            .strip_prefix(root)
            .unwrap()
            .to_string_lossy()
            .to_string();
        let bytes = entry
            .file_type()
            .is_file()
            .then(|| fs::read(entry.path()).unwrap());
        out.insert(rel, bytes);
    }
    out
}

fn labels(rt: &Runtime) -> Vec<(String, String)> {
    let mut found: Vec<(String, String)> = list_rows(rt, false)
        .unwrap()
        .into_iter()
        .map(|row| (row.workspace.profile_label(), row.workspace.id))
        .collect();
    found.sort();
    found
}

fn add_work_profile(machine: &Machine) {
    fs::write(
        machine.other.storage_json(),
        serde_json::to_string_pretty(&json!({
            "userDataProfiles": [{"location": "-1a2b", "name": "Work"}],
            "profileAssociations": {"workspaces": {uri(&machine.api): "-1a2b"}}
        }))
        .unwrap(),
    )
    .unwrap();
}

#[test]
fn ls_lists_every_installation_under_its_own_name() {
    let machine = machine();
    let rt = idle(&machine);
    assert_eq!(
        labels(&rt),
        [
            ("default".to_string(), "hash-app".to_string()),
            ("default".to_string(), "hash-shared".to_string()),
            ("resolved".to_string(), "hash-api".to_string()),
            ("resolved".to_string(), "hash-shared".to_string()),
        ]
    );
    let out = engine::render_workspaces(&rt, false, None, Theme::plain()).unwrap();
    assert!(out.contains("│ resolved "), "{out}");
    assert!(out.contains("4 workspaces in 2 profiles"), "{out}");
    let detail =
        engine::render_workspaces(&rt, false, Some("hash-shared"), Theme::plain()).unwrap();
    assert_eq!(detail.matches("==> Workspace\n").count(), 2, "{detail}");
    assert!(detail.contains(&machine.other.cursor_root.display().to_string()));
}

#[test]
fn profile_flag_pins_an_installation_and_a_vs_code_profile() {
    let machine = machine();
    add_work_profile(&machine);
    let rt = idle(&machine);
    let resolved = with_profile(&rt, "resolved").unwrap();
    assert!(resolved.pinned);
    assert_eq!(resolved.layout.name, "resolved");
    assert_eq!(
        labels(&resolved),
        [
            ("resolved".to_string(), "hash-shared".to_string()),
            ("resolved/Work".to_string(), "hash-api".to_string()),
        ]
    );
    for selector in ["resolved/work", "Work", "RESOLVED/-1a2b"] {
        let work = with_profile(&rt, selector).unwrap();
        assert_eq!(
            labels(&work),
            [("resolved/Work".to_string(), "hash-api".to_string())],
            "{selector}"
        );
    }
    let default = with_profile(&rt, "default").unwrap();
    assert_eq!(
        labels(&default),
        [
            ("default".to_string(), "hash-app".to_string()),
            ("default".to_string(), "hash-shared".to_string()),
        ]
    );
    let refused = |selector: &str| match with_profile(&rt, selector) {
        Ok(_) => panic!("{selector} was accepted"),
        Err(err) => err.to_string(),
    };
    let err = refused("nope");
    assert!(err.contains("no Cursor profile named nope"), "{err}");
    assert!(err.contains("default, resolved, resolved/Work"), "{err}");
    let err = refused("default/Work");
    assert!(
        err.contains("no Cursor profile named default/Work"),
        "{err}"
    );
}

#[test]
fn stats_count_installations_and_break_them_down() {
    let machine = machine();
    let rt = idle(&machine);
    let out = engine::render_stats(&rt, Theme::plain()).unwrap();
    for text in [
        "2 profiles",
        "2 Cursor installations with no extra VS Code profiles",
        "globalStorage/state.vscdb in 2 installations",
        "==> Profiles",
        "│ profile      │ user data",
        "│ workspaces │ chats │ subagents │ global db │",
        "all profiles",
        "4 workspaces",
        "4 chats",
    ] {
        assert!(out.contains(text), "missing {text:?} in\n{out}");
    }
    let row = |name: &str| {
        out.lines()
            .find(|line| line.starts_with(&format!("│ {name} ")))
            .unwrap_or_else(|| panic!("no {name} row in\n{out}"))
            .to_string()
    };
    assert!(row("resolved").contains("dot-cursor-resolved"));
    assert!(row("default").split('│').nth(3).unwrap().trim() == "2");

    add_work_profile(&machine);
    let out = engine::render_stats(&rt, Theme::plain()).unwrap();
    assert!(out.contains("3 profiles"), "{out}");
    assert!(
        out.contains("2 Cursor installations with 1 VS Code profile"),
        "{out}"
    );
    assert!(out.contains("│ resolved/Work "), "{out}");
    assert!(out.contains("shared"), "{out}");

    let pinned = with_profile(&rt, "resolved/Work").unwrap();
    let out = engine::render_stats(&pinned, Theme::plain()).unwrap();
    assert!(out.contains("1 profile "), "{out}");
    assert!(out.contains("1 workspace "), "{out}");
    assert!(out.contains("1 chat "), "{out}");
}

#[test]
fn write_commands_infer_the_installation_and_leave_the_other_alone() {
    let machine = machine();
    let rt = idle(&machine);
    let default_before = tree(&machine.home.layout.cursor_root);
    let moved = native(&machine.home.root, "code/api-moved");
    fs::create_dir_all(&moved).unwrap();
    run(
        &rt,
        &[
            "mv",
            &machine.api.display().to_string(),
            &moved.display().to_string(),
        ],
    )
    .unwrap();
    let hash = compute_workspace_hash(&moved).unwrap();
    assert_eq!(header_ids(&machine.other, &hash), vec![B.to_string()]);
    assert!(header_ids(&machine.other, "hash-api").is_empty());
    assert!(machine.other.workspace_storage().join(&hash).is_dir());
    assert!(!machine.home.layout.workspace_storage().join(&hash).exists());
    assert_eq!(default_before, tree(&machine.home.layout.cursor_root));

    let resolved_before = tree(&machine.other.cursor_root);
    run(&rt, &["rm", "hash-app"]).unwrap();
    assert!(header_ids(&machine.home.layout, "hash-app").is_empty());
    assert_eq!(resolved_before, tree(&machine.other.cursor_root));
}

#[test]
fn ambiguous_targets_need_a_profile() {
    let machine = machine();
    let rt = idle(&machine);
    let default_before = tree(&machine.home.layout.cursor_root);
    let resolved_before = tree(&machine.other.cursor_root);
    let err = run(&rt, &["rm", "hash-shared"]).unwrap_err().to_string();
    assert!(
        err.contains("hash-shared exists in profiles default, resolved"),
        "{err}"
    );
    assert!(
        err.contains("Pass --profile with one of: default, resolved"),
        "{err}"
    );
    assert_eq!(default_before, tree(&machine.home.layout.cursor_root));
    assert_eq!(resolved_before, tree(&machine.other.cursor_root));

    run(&rt, &["rm", "--profile", "resolved", "hash-shared"]).unwrap();
    assert!(header_ids(&machine.other, "hash-shared").is_empty());
    assert!(
        !machine
            .other
            .workspace_storage()
            .join("hash-shared")
            .exists()
    );
    assert_eq!(default_before, tree(&machine.home.layout.cursor_root));
    assert_eq!(
        header_ids(&machine.home.layout, "hash-shared"),
        vec![C.to_string()]
    );
    let slug = machine
        .home
        .layout
        .projects_dir
        .join(slug_of(&machine.shared));
    assert!(slug.join("agent-transcripts").join(C).is_dir());
    assert!(!slug.join("agent-transcripts").join(E).exists());
}

#[test]
fn mixing_installations_is_refused() {
    let machine = machine();
    let rt = idle(&machine);
    let target = native(&machine.home.root, "code/together");
    fs::create_dir_all(&target).unwrap();
    let before = snapshot(&machine.home);
    let err = run(
        &rt,
        &[
            "combine",
            &target.display().to_string(),
            "hash-app",
            "hash-api",
        ],
    )
    .unwrap_err()
    .to_string();
    assert!(
        err.contains("hash-app is in profile default, but hash-api is in profile resolved"),
        "{err}"
    );
    assert!(err.contains("never mixes Cursor installations"), "{err}");
    let err = run(
        &rt,
        &[
            "combine",
            "--profile",
            "default",
            &target.display().to_string(),
            B,
        ],
    )
    .unwrap_err()
    .to_string();
    assert!(
        err.contains(&format!("{B} belongs to profile resolved, not default")),
        "{err}"
    );
    let err = run(&rt, &["rm", "--profile", "default", "hash-api"])
        .unwrap_err()
        .to_string();
    assert!(
        err.contains("hash-api belongs to profile resolved, not default"),
        "{err}"
    );
    let mut after = snapshot(&machine.home);
    after.retain(|key, _| !key.starts_with("dot-crepath"));
    let mut before = before;
    before.retain(|key, _| !key.starts_with("dot-crepath"));
    assert_same(&before, &after, "refused cross-installation writes");
}

#[test]
fn mv_of_a_shared_folder_moves_only_this_installations_transcripts() {
    let machine = machine();
    let rt = idle(&machine);
    let resolved_before = tree(&machine.other.cursor_root);
    let dest = native(&machine.home.root, "code/shared-moved");
    fs::create_dir_all(&dest).unwrap();
    run(
        &rt,
        &[
            "mv",
            "--profile",
            "default",
            "hash-shared",
            &dest.display().to_string(),
        ],
    )
    .unwrap();
    let projects = &machine.home.layout.projects_dir;
    let old = projects.join(slug_of(&machine.shared));
    let new = projects.join(slug_of(&dest));
    assert!(new.join("agent-transcripts").join(C).is_dir());
    assert!(!old.join("agent-transcripts").join(C).exists());
    assert!(old.join("agent-transcripts").join(E).is_dir());
    assert!(!new.join("agent-transcripts").join(E).exists());
    assert_eq!(
        fs::read_to_string(native(&old, "terminals/1.txt")).unwrap(),
        "term"
    );
    assert_eq!(
        fs::read_to_string(native(&new, "terminals/1.txt")).unwrap(),
        "term"
    );
    assert_eq!(resolved_before, tree(&machine.other.cursor_root));
    assert_eq!(
        header_ids(&machine.other, "hash-shared"),
        vec![E.to_string()]
    );
}

#[test]
fn mv_project_of_a_shared_folder_warns_about_the_other_installation() {
    let machine = machine();
    let rt = with_profile(&idle(&machine), "default").unwrap();
    let resolved_before = tree(&machine.other.cursor_root);
    let dest = native(&machine.home.root, "code/shared-renamed");
    let report = move_paths(
        &rt,
        &[("hash-shared".into(), dest.display().to_string())],
        None,
        false,
        true,
    )
    .unwrap();
    assert!(
        report
            .warnings
            .iter()
            .any(|warning| warning.contains(&format!(
                "profile resolved also has a workspace on {}",
                normalize_path(&machine.shared).display()
            ))),
        "{:?}",
        report.warnings
    );
    assert!(dest.is_dir());
    assert!(!machine.shared.exists());
    assert_eq!(resolved_before, tree(&machine.other.cursor_root));
    let old = machine
        .home
        .layout
        .projects_dir
        .join(slug_of(&machine.shared));
    assert!(old.join("agent-transcripts").join(E).is_dir());
}

#[test]
fn mv_of_a_folder_only_this_installation_uses_moves_the_whole_slug() {
    let machine = machine();
    let rt = idle(&machine);
    let projects = &machine.home.layout.projects_dir;
    let old = projects.join(slug_of(&machine.app));
    fs::create_dir_all(old.join("agent-transcripts").join(A)).unwrap();
    fs::create_dir_all(native(&old, "agent-transcripts/orphan")).unwrap();
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
    let new = projects.join(slug_of(&dest));
    assert!(!old.exists());
    assert!(new.join("agent-transcripts").join(A).is_dir());
    assert!(native(&new, "agent-transcripts/orphan").is_dir());
}

fn archive_names(file: &Path) -> Vec<String> {
    let mut archive = tar::Archive::new(flate2::read::GzDecoder::new(File::open(file).unwrap()));
    archive
        .entries()
        .unwrap()
        .map(|entry| entry.unwrap().path().unwrap().to_string_lossy().to_string())
        .collect()
}

#[test]
fn export_of_a_shared_folder_leaves_out_other_installations_transcripts() {
    let machine = machine();
    let rt = idle(&machine);
    let out = machine.home.root.join("shared.crepath");
    run(
        &rt,
        &[
            "export",
            "--profile",
            "default",
            "hash-shared",
            &out.display().to_string(),
        ],
    )
    .unwrap();
    let names = archive_names(&out);
    let slug = slug_of(&machine.shared);
    let has = |suffix: &str| {
        names
            .iter()
            .any(|name| name == &format!("projects/{slug}/{suffix}"))
    };
    assert!(
        has(&format!("agent-transcripts/{C}/{C}.jsonl")),
        "{names:?}"
    );
    assert!(has("terminals/1.txt"), "{names:?}");
    assert!(!names.iter().any(|name| name.contains(E)), "{names:?}");
    let solo = machine.home.root.join("app.crepath");
    let projects = &machine.home.layout.projects_dir;
    let orphan = native(
        &projects.join(slug_of(&machine.app)),
        "agent-transcripts/orphan",
    );
    fs::create_dir_all(&orphan).unwrap();
    fs::write(orphan.join("o.jsonl"), "o").unwrap();
    run(&rt, &["export", "hash-app", &solo.display().to_string()]).unwrap();
    assert!(
        archive_names(&solo)
            .iter()
            .any(|name| name.ends_with("agent-transcripts/orphan/o.jsonl"))
    );
}

#[test]
fn import_writes_only_into_the_chosen_installation() {
    let machine = machine();
    let rt = idle(&machine);
    let folder = native(&machine.home.root, "code/lib");
    fs::create_dir_all(&folder).unwrap();
    let hash = compute_workspace_hash(&folder).unwrap();
    write_folder_workspace(&machine.home.layout, &hash, &folder);
    chat(
        &machine.home.layout,
        SUB,
        &hash,
        &folder_identity(&hash, &folder),
    );
    let file = machine.home.root.join("lib.crepath");
    run(&rt, &["export", &hash, &file.display().to_string()]).unwrap();
    let default_before = tree(&machine.home.layout.cursor_root);
    let resolved_before = tree(&machine.other.cursor_root);
    let err = run(&rt, &["import", &file.display().to_string()])
        .unwrap_err()
        .to_string();
    assert!(
        err.contains("import needs one Cursor installation to write into"),
        "{err}"
    );
    assert_eq!(resolved_before, tree(&machine.other.cursor_root));
    run(
        &rt,
        &[
            "import",
            "--profile",
            "resolved",
            &file.display().to_string(),
        ],
    )
    .unwrap();
    assert_eq!(header_ids(&machine.other, &hash), vec![SUB.to_string()]);
    assert!(machine.other.workspace_storage().join(&hash).is_dir());
    assert_eq!(default_before, tree(&machine.home.layout.cursor_root));
}

#[test]
fn installations_from_older_cursor_builds_are_read_and_written() {
    let machine = machine();
    let legacy_root = machine.home.root.join("dot-cursor-legacy");
    fs::create_dir_all(native(&legacy_root, "User/globalStorage")).unwrap();
    rusqlite::Connection::open(native(&legacy_root, "User/globalStorage/state.vscdb"))
        .unwrap()
        .execute_batch(
            "CREATE TABLE composerHeaders (composerId TEXT PRIMARY KEY, workspaceId TEXT, \
             createdAt INTEGER, lastUpdatedAt INTEGER, isArchived INTEGER, isSubagent INTEGER, \
             recency INTEGER, checkpointAt INTEGER, value TEXT);",
        )
        .unwrap();
    let legacy = install_layout(&machine.home.root, "legacy", legacy_root);
    let old = native(&machine.home.root, "code/old");
    fs::create_dir_all(&old).unwrap();
    write_folder_workspace(&legacy, "hash-old", &old);
    global(&legacy)
        .execute(
            "INSERT INTO composerHeaders VALUES (?1, 'hash-old', 1700000000000, 1700000100000, 0, 0, 1, NULL, '{}')",
            [E],
        )
        .unwrap();
    let mut rt = idle(&machine);
    rt.installs.push(legacy.clone());
    let rows = list_rows(&rt, false).unwrap();
    let old_row = rows
        .iter()
        .find(|row| row.workspace.install == "legacy")
        .unwrap();
    assert_eq!(old_row.chats, 1);
    assert!(
        engine::render_stats(&rt, Theme::plain())
            .unwrap()
            .contains("│ legacy ")
    );

    let folder = native(&machine.home.root, "code/lib");
    fs::create_dir_all(&folder).unwrap();
    let hash = compute_workspace_hash(&folder).unwrap();
    write_folder_workspace(&machine.home.layout, &hash, &folder);
    chat(
        &machine.home.layout,
        SUB,
        &hash,
        &folder_identity(&hash, &folder),
    );
    let file = machine.home.root.join("lib.crepath");
    run(&rt, &["export", &hash, &file.display().to_string()]).unwrap();
    run(
        &rt,
        &["import", "--profile", "legacy", &file.display().to_string()],
    )
    .unwrap();
    assert_eq!(header_ids(&legacy, &hash), vec![SUB.to_string()]);
}

struct Listing(Result<Vec<ProcessInfo>, String>);

impl ProcessSource for Listing {
    fn processes(&self) -> anyhow::Result<Vec<ProcessInfo>> {
        self.0.clone().map_err(anyhow::Error::msg)
    }
}

fn process(pid: u32, name: &str, exe: &str, args: &[&str]) -> ProcessInfo {
    ProcessInfo {
        pid,
        name: name.to_string(),
        exe: Some(PathBuf::from(exe)),
        args: args.iter().map(|arg| arg.to_string()).collect(),
    }
}

fn probed(machine: &Machine, listing: Result<Vec<ProcessInfo>, String>) -> Runtime {
    let roots = vec![
        machine.home.layout.cursor_root.clone(),
        machine.other.cursor_root.clone(),
    ];
    runtime_of(
        machine,
        Arc::new(ProcessProbe::new(Listing(listing), roots)),
    )
}

fn next_key() -> String {
    native(Path::new("code"), "app-next")
        .to_string_lossy()
        .to_string()
}

fn try_mv(machine: &Machine, rt: &Runtime) -> anyhow::Result<engine::Report> {
    let dest = native(&machine.home.root, "code/app-next");
    fs::create_dir_all(&dest).unwrap();
    move_paths(
        rt,
        &[("hash-app".into(), dest.display().to_string())],
        None,
        false,
        false,
    )
}

#[test]
fn the_running_guard_names_every_instance() {
    let machine = machine();
    let resolved = format!("--user-data-dir={}", machine.other.cursor_root.display());
    let rt = probed(
        &machine,
        Ok(vec![
            process(1, "launchd", "/sbin/launchd", &[]),
            process(
                10,
                "Cursor",
                "/Applications/Cursor.app/Contents/MacOS/Cursor",
                &[],
            ),
            process(
                11,
                "Cursor Helper (Renderer)",
                "/Applications/Cursor.app/Contents/Frameworks/Cursor Helper (Renderer).app/Contents/MacOS/Cursor Helper (Renderer)",
                &["x", "--type=renderer", &resolved],
            ),
            process(
                20,
                "Cursor",
                "/Applications/Cursor.app/Contents/MacOS/Cursor",
                &["/Applications/Cursor.app/Contents/MacOS/Cursor", &resolved],
            ),
        ]),
    );
    let before = snapshot(&machine.home);
    let err = try_mv(&machine, &rt).unwrap_err().to_string();
    assert!(
        err.contains("Cursor is running: default (pid 10), resolved (pid 20)."),
        "{err}"
    );
    assert!(
        err.contains("Quit every Cursor window of every profile"),
        "{err}"
    );
    let mut after = snapshot(&machine.home);
    after.remove(&next_key());
    assert_same(&before, &after, "guarded mv");
}

#[test]
fn the_running_guard_passes_when_no_cursor_process_exists() {
    let machine = machine();
    let rt = probed(
        &machine,
        Ok(vec![
            process(1, "launchd", "/sbin/launchd", &[]),
            process(
                2,
                "CursorUIViewService",
                "/System/Library/PrivateFrameworks/TextInputUIMacHelper.framework/Versions/A/XPCServices/CursorUIViewService.xpc/Contents/MacOS/CursorUIViewService",
                &[],
            ),
        ]),
    );
    try_mv(&machine, &rt).unwrap();
    assert!(header_ids(&machine.home.layout, "hash-app").is_empty());
}

#[test]
fn an_unreadable_process_table_blocks_writes_but_not_reads() {
    let machine = machine();
    let rt = probed(&machine, Err("permission denied".to_string()));
    let before = snapshot(&machine.home);
    let err = try_mv(&machine, &rt).unwrap_err().to_string();
    assert!(
        err.contains("Cannot tell whether Cursor is running: permission denied"),
        "{err}"
    );
    assert!(
        err.contains("confirmed that every Cursor instance is closed"),
        "{err}"
    );
    let mut after = snapshot(&machine.home);
    after.remove(&next_key());
    assert_same(&before, &after, "mv with an unreadable process table");
    assert_eq!(list_rows(&rt, false).unwrap().len(), 4);
    engine::render_stats(&rt, Theme::plain()).unwrap();
    engine::render_history(&rt, Theme::plain()).unwrap();
}

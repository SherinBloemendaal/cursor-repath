//! Shared Cursor repath engine.

mod db;
mod view;

use anyhow::{Context, Result, bail};
use chrono::Utc;
use fs_extra::dir::{self, CopyOptions};
use regex::Regex;
use rusqlite::Connection;
use serde_json::{Value, json};
use std::collections::{BTreeMap, HashMap, HashSet};
use std::fs;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::Instant;
use url::Url;

use crate::cursor::folder_id::path_to_folder_id;
use crate::cursor::registry::{self, ComposerHeader};
use crate::cursor::rewrite::Replacement;
use crate::cursor::storage::rewrite_storage_json;
use crate::cursor::workspace::{self, compute_workspace_hash};
use crate::ui::{self, DualProgress, Theme};

pub use db::{rewrite_workspace, touched_paths};
pub use view::{ListRow, sort_rows};

#[derive(Debug, Clone)]
pub struct Layout {
    pub cursor_root: PathBuf,
    pub projects_dir: PathBuf,
    pub crepath_home: PathBuf,
}

impl Layout {
    pub fn user_dir(&self) -> PathBuf {
        self.cursor_root.join("User")
    }

    pub fn global_storage(&self) -> PathBuf {
        self.user_dir().join("globalStorage")
    }

    pub fn global_db(&self) -> PathBuf {
        self.global_storage().join("state.vscdb")
    }

    pub fn storage_json(&self) -> PathBuf {
        self.global_storage().join("storage.json")
    }

    pub fn workspace_storage(&self) -> PathBuf {
        self.user_dir().join("workspaceStorage")
    }

    pub fn unsaved_root(&self) -> PathBuf {
        self.cursor_root.join("Workspaces")
    }

    pub fn history_file(&self) -> PathBuf {
        self.crepath_home.join("history.jsonl")
    }

    pub fn backup_root(&self) -> PathBuf {
        self.crepath_home.join("backups")
    }
}

pub trait Probe: Send + Sync {
    fn running(&self) -> bool;
}

pub struct SystemProbe;

impl Probe for SystemProbe {
    fn running(&self) -> bool {
        cursor_process_running()
    }
}

pub struct FixedProbe(pub bool);

impl Probe for FixedProbe {
    fn running(&self) -> bool {
        self.0
    }
}

#[derive(Clone)]
pub struct Runtime {
    pub layout: Layout,
    pub dry_run: bool,
    pub yes: bool,
    pub profile: Option<String>,
    pub probe: Arc<dyn Probe>,
    pub quiet: bool,
}

impl Runtime {
    pub fn check(&self) -> Result<()> {
        if self.probe.running() {
            return Err(ui::hinted(
                "Cursor is running.",
                "Close Cursor completely and retry.",
            ));
        }
        Ok(())
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Kind {
    Folder,
    CodeWorkspace,
    Unsaved,
    EmptyWindow,
}

impl Kind {
    pub fn label(&self) -> &'static str {
        match self {
            Self::Folder => "folder",
            Self::CodeWorkspace => "code-workspace",
            Self::Unsaved => "unsaved",
            Self::EmptyWindow => "empty-window",
        }
    }
}

#[derive(Debug, Clone)]
pub struct Workspace {
    pub id: String,
    pub dir: PathBuf,
    pub kind: Kind,
    pub uri: Option<String>,
    pub path: Option<PathBuf>,
    pub profile: String,
    pub destination_missing: bool,
}

#[derive(Debug, Clone, Default)]
pub struct Report {
    pub warnings: Vec<String>,
    pub skipped: Vec<String>,
    pub applied: Vec<String>,
    pub rewritten_keys: Vec<String>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Transfer {
    Move,
    Copy,
}

#[derive(Debug, Clone)]
struct Planned {
    source: Workspace,
    dest: PathBuf,
    dest_uri: String,
    new_hash: String,
    project: bool,
    transfer: Transfer,
}

pub fn move_paths(
    rt: &Runtime,
    pairs: &[(String, String)],
    replace: Option<(&str, &str)>,
    regex: bool,
    project: bool,
) -> Result<Report> {
    repath(rt, pairs, replace, regex, project, Transfer::Move)
}

pub fn copy_paths(
    rt: &Runtime,
    pairs: &[(String, String)],
    replace: Option<(&str, &str)>,
    regex: bool,
    project: bool,
) -> Result<Report> {
    repath(rt, pairs, replace, regex, project, Transfer::Copy)
}

pub fn save_unsaved(rt: &Runtime, id: &str, dest: &Path) -> Result<Report> {
    rt.check()?;
    let workspaces = discover(rt)?;
    let source = workspaces
        .into_iter()
        .find(|workspace| is_unsaved_id(workspace, id))
        .with_context(|| format!("no unsaved workspace matches {id}"))?;
    if !dest.exists() {
        bail!("destination does not exist: {}", dest.display());
    }
    let dest = abs_path(dest);
    let planned = plan_one(&source, &dest, false, Transfer::Move)?;
    let mut report = Report::default();
    apply_plans(rt, &[planned], &mut report)?;
    Ok(report)
}

pub fn suggest_split(
    rt: &Runtime,
    source: &Workspace,
    targets: &[PathBuf],
) -> Result<SplitSuggestion> {
    let conn = open_global(rt)?;
    let headers = db::headers(&conn, &source.id)?;
    let mut assigned: BTreeMap<String, Vec<PathBuf>> = BTreeMap::new();
    let mut unassigned = Vec::new();
    let roots: Vec<PathBuf> = targets.iter().map(|path| abs_path(path)).collect();
    for header in headers.iter().filter(|header| !header.is_subagent) {
        let paths = db::touched_paths(&conn, &header.composer_id)?;
        let mut hits = Vec::new();
        for raw in paths {
            if let Some(path) = materialize_path(&raw)
                && let Some(target) = longest_target(&path, &roots)
                && !hits.iter().any(|existing: &PathBuf| existing == &target)
            {
                hits.push(target);
            }
        }
        if hits.is_empty() {
            unassigned.push(header.composer_id.clone());
        } else {
            assigned.insert(header.composer_id.clone(), hits);
        }
    }
    Ok(SplitSuggestion {
        assigned,
        unassigned,
        titles: headers
            .iter()
            .map(|header| (header.composer_id.clone(), header.title.clone()))
            .collect(),
    })
}

#[derive(Debug, Clone)]
pub struct SplitSuggestion {
    pub assigned: BTreeMap<String, Vec<PathBuf>>,
    pub unassigned: Vec<String>,
    pub titles: HashMap<String, Option<String>>,
}

pub fn split_workspace(
    rt: &Runtime,
    source_id: &str,
    targets: &[PathBuf],
    assignments: &BTreeMap<String, Vec<PathBuf>>,
    move_chats: bool,
) -> Result<Report> {
    rt.check()?;
    let source = require_workspace(rt, source_id)?;
    let suggestion = suggest_split(rt, &source, targets)?;
    for id in &suggestion.unassigned {
        if !assignments.contains_key(id) {
            bail!("unassigned chats need a target before split can continue: {id}");
        }
    }
    let mut report = Report::default();
    if rt.dry_run {
        report.applied.push(format!("dry-run split {}", source.id));
        return Ok(report);
    }
    rt.check()?;
    let conn = open_global(rt)?;
    let tx = conn.unchecked_transaction()?;
    let mut moved_ids = HashSet::new();
    for target in targets {
        let target = abs_path(target);
        let workspace = ensure_folder_workspace(rt, &target)?;
        let chats: Vec<String> = assignments
            .iter()
            .filter(|(_, dests)| dests.iter().any(|dest| abs_path(dest) == target))
            .map(|(id, _)| id.clone())
            .collect();
        transfer_chats(
            rt,
            &tx,
            &source,
            &workspace,
            &chats,
            if move_chats {
                Transfer::Move
            } else {
                Transfer::Copy
            },
            &mut TransferBook {
                already: &mut moved_ids,
                report: &mut report,
            },
        )?;
    }
    tx.commit()?;
    report.applied.push(source.id.clone());
    Ok(report)
}

pub fn combine_workspaces(
    rt: &Runtime,
    target: &Path,
    sources: &[String],
    move_chats: bool,
) -> Result<Report> {
    rt.check()?;
    let target_path = abs_path(target);
    if !target_path.exists()
        && discover(rt)?
            .iter()
            .all(|ws| ws.id != target.to_string_lossy())
    {
        bail!("combine target does not exist: {}", target_path.display());
    }
    let target_ws = if let Some(existing) = find_workspace(rt, &target.to_string_lossy())? {
        existing
    } else {
        ensure_folder_workspace(rt, &target_path)?
    };
    let mut report = Report::default();
    if rt.dry_run {
        report
            .applied
            .push(format!("dry-run combine {}", target_ws.id));
        return Ok(report);
    }
    rt.check()?;
    let conn = open_global(rt)?;
    let tx = conn.unchecked_transaction()?;
    let mut moved_ids = HashSet::new();
    for source_id in sources {
        if let Some(source) = find_workspace(rt, source_id)? {
            let ids = db::composer_ids_for_workspace(&tx, &source.id)?;
            let tops = top_level_ids(&tx, &ids)?;
            transfer_chats(
                rt,
                &tx,
                &source,
                &target_ws,
                &tops,
                if move_chats {
                    Transfer::Move
                } else {
                    Transfer::Copy
                },
                &mut TransferBook {
                    already: &mut moved_ids,
                    report: &mut report,
                },
            )?;
        } else {
            transfer_chats(
                rt,
                &tx,
                &target_ws,
                &target_ws,
                std::slice::from_ref(source_id),
                if move_chats {
                    Transfer::Move
                } else {
                    Transfer::Copy
                },
                &mut TransferBook {
                    already: &mut moved_ids,
                    report: &mut report,
                },
            )?;
        }
    }
    tx.commit()?;
    report.applied.push(target_ws.id);
    Ok(report)
}

fn top_level_ids(conn: &Connection, ids: &[String]) -> Result<Vec<String>> {
    let expanded = db::expand_chat_ids(conn, ids)?;
    let set: HashSet<String> = expanded.iter().cloned().collect();
    let mut children = HashSet::new();
    for id in &expanded {
        if let Some(raw) = db::read_text(conn, &format!("composerData:{id}"))?
            && let Ok(json) = serde_json::from_str::<Value>(&raw)
        {
            for field in ["subComposerIds", "subagentComposerIds"] {
                if let Some(items) = json.get(field).and_then(|v| v.as_array()) {
                    for item in items {
                        if let Some(child) = item.as_str()
                            && set.contains(child)
                        {
                            children.insert(child.to_string());
                        }
                    }
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

struct TransferBook<'a> {
    already: &'a mut HashSet<String>,
    report: &'a mut Report,
}

fn transfer_chats(
    rt: &Runtime,
    conn: &Connection,
    source: &Workspace,
    target: &Workspace,
    chat_ids: &[String],
    mode: Transfer,
    book: &mut TransferBook<'_>,
) -> Result<()> {
    rt.check()?;
    let owned = db::owned_ids(conn, &target.id)?;
    let mut seeds = Vec::new();
    for id in chat_ids {
        if book.already.contains(id) && mode == Transfer::Move {
            let replacements = identifier_replacements(source, target)?;
            let expanded = db::expand_chat_ids(conn, std::slice::from_ref(id))?;
            db::clone_chats(conn, &expanded, &target.id, &replacements)?;
            book.report
                .applied
                .push(format!("copied extra {id} -> {}", target.id));
            continue;
        }
        if owned.contains(id) {
            book.report
                .warnings
                .push(format!("skipped {id}: already owned by {}", target.id));
            book.report.skipped.push(id.clone());
            continue;
        }
        seeds.push(id.clone());
    }
    if seeds.is_empty() {
        return Ok(());
    }
    let expanded = db::expand_chat_ids(conn, &seeds)?;
    let replacements = identifier_replacements(source, target)?;
    match mode {
        Transfer::Move => {
            db::reassign_chats(conn, &expanded, &source.id, &target.id, &replacements)?;
            shift_local_ids(rt, source, target, &expanded, &HashMap::new(), true)?;
            move_transcripts(rt, source, target, &expanded, &HashMap::new(), true)?;
            for id in &expanded {
                book.already.insert(id.clone());
            }
        }
        Transfer::Copy => {
            let map = db::clone_chats(conn, &expanded, &target.id, &replacements)?;
            shift_local_ids(rt, source, target, &expanded, &map, false)?;
            move_transcripts(rt, source, target, &expanded, &map, false)?;
            book.report
                .warnings
                .push("copied chats continue from local state only".to_string());
        }
    }
    Ok(())
}

fn identifier_replacements(source: &Workspace, target: &Workspace) -> Result<Vec<Replacement>> {
    let mut reps = vec![Replacement::new(&source.id, &target.id)];
    if let (Some(old), Some(new)) = (&source.uri, &target.uri) {
        reps.push(Replacement::new(old, new));
    }
    if let (Some(old), Some(new)) = (&source.path, &target.path) {
        reps.push(Replacement::new(
            old.to_string_lossy(),
            new.to_string_lossy(),
        ));
    }
    Ok(reps)
}

pub fn reindex(rt: &Runtime, target: &str) -> Result<Report> {
    rt.check()?;
    let workspace = require_workspace(rt, target)?;
    let mut report = Report::default();
    if rt.dry_run {
        report
            .applied
            .push(format!("dry-run reindex {}", workspace.id));
        return Ok(report);
    }
    rt.check()?;
    let conn = open_global(rt)?;
    let replacements = Vec::new();
    db::rewrite_workspace(
        &conn,
        &workspace.id,
        &replacements,
        &workspace.id,
        &workspace.id,
        false,
    )?;
    clear_workspace_caches(&workspace.dir)?;
    report.applied.push(workspace.id);
    Ok(report)
}

pub fn remove_targets(rt: &Runtime, targets: &[String]) -> Result<Report> {
    rt.check()?;
    let mut report = Report::default();
    if targets.is_empty() {
        bail!("nothing selected to remove");
    }
    if rt.dry_run {
        report.applied.push("dry-run remove".to_string());
        return Ok(report);
    }
    rt.check()?;
    let conn = open_global(rt)?;
    for target in targets {
        if let Some(workspace) = find_workspace(rt, target)? {
            let ids = db::composer_ids_for_workspace(&conn, &workspace.id)?;
            let expanded = db::expand_chat_ids(&conn, &ids)?;
            db::delete_chats(&conn, &expanded)?;
            if workspace.dir.exists() {
                fs::remove_dir_all(&workspace.dir)?;
            }
            report.applied.push(workspace.id);
        } else {
            let expanded = db::expand_chat_ids(&conn, std::slice::from_ref(target))?;
            db::delete_chats(&conn, &expanded)?;
            report.applied.push(target.clone());
        }
    }
    Ok(report)
}

pub fn list_workspaces(rt: &Runtime, unsaved_only: bool, detail: Option<&str>) -> Result<String> {
    render_workspaces(rt, unsaved_only, detail, Theme::stdout())
}

pub fn render_workspaces(
    rt: &Runtime,
    unsaved_only: bool,
    detail: Option<&str>,
    theme: Theme,
) -> Result<String> {
    let _spinner = ui::spinner("Scanning workspaces", rt.quiet);
    let mut workspaces = filtered_workspaces(rt, unsaved_only)?;
    let conn = open_global_ro(rt)?;
    if let Some(id) = detail {
        let index = workspaces
            .iter()
            .position(|workspace| {
                workspace.id == id
                    || workspace
                        .path
                        .as_ref()
                        .is_some_and(|path| path.to_string_lossy() == id)
            })
            .with_context(|| format!("no workspace matches {id}"))?;
        let workspace = workspaces.swap_remove(index);
        let headers = match &conn {
            Some(conn) => db::headers(conn, &workspace.id)?,
            None => Vec::new(),
        };
        let row = list_row(conn.as_ref(), workspace)?;
        return Ok(view::render_detail(theme, &row, &headers));
    }
    let mut rows = workspaces
        .into_iter()
        .map(|workspace| list_row(conn.as_ref(), workspace))
        .collect::<Result<Vec<_>>>()?;
    sort_rows(&mut rows);
    Ok(view::render_list(theme, &rows))
}

pub fn list_rows(rt: &Runtime, unsaved_only: bool) -> Result<Vec<ListRow>> {
    let conn = open_global_ro(rt)?;
    let mut rows = filtered_workspaces(rt, unsaved_only)?
        .into_iter()
        .map(|workspace| list_row(conn.as_ref(), workspace))
        .collect::<Result<Vec<_>>>()?;
    sort_rows(&mut rows);
    Ok(rows)
}

fn filtered_workspaces(rt: &Runtime, unsaved_only: bool) -> Result<Vec<Workspace>> {
    let mut workspaces = discover(rt)?;
    if let Some(profile) = &rt.profile {
        workspaces.retain(|workspace| &workspace.profile == profile);
    }
    if unsaved_only {
        workspaces.retain(|workspace| workspace.kind == Kind::Unsaved);
    }
    Ok(workspaces)
}

fn open_global_ro(rt: &Runtime) -> Result<Option<Connection>> {
    let path = rt.layout.global_db();
    if path.exists() {
        Ok(Some(db::open_ro(&path)?))
    } else {
        Ok(None)
    }
}

fn list_row(conn: Option<&Connection>, workspace: Workspace) -> Result<ListRow> {
    let headers = match conn {
        Some(conn) => db::headers(conn, &workspace.id)?,
        None => Vec::new(),
    };
    let subagents = headers.iter().filter(|header| header.is_subagent).count();
    let archived = headers.iter().filter(|header| header.is_archived).count();
    let size = dir_size(&workspace.dir).unwrap_or(0);
    Ok(ListRow {
        chats: headers.len().saturating_sub(subagents),
        subagents,
        archived,
        size,
        workspace,
    })
}

pub fn export_workspace(rt: &Runtime, target: &str, file: &Path) -> Result<Report> {
    rt.check()?;
    let workspace = require_workspace(rt, target)?;
    let mut report = Report::default();
    if rt.dry_run {
        report
            .applied
            .push(format!("dry-run export {}", file.display()));
        return Ok(report);
    }
    rt.check()?;
    archive::write_export(rt, &workspace, file)?;
    report.applied.push(file.display().to_string());
    Ok(report)
}

pub fn import_archive(rt: &Runtime, file: &Path, dest: Option<&Path>) -> Result<Report> {
    rt.check()?;
    let mut report = Report::default();
    if rt.dry_run {
        report
            .applied
            .push(format!("dry-run import {}", file.display()));
        return Ok(report);
    }
    rt.check()?;
    archive::read_import(rt, file, dest)?;
    report.applied.push(file.display().to_string());
    Ok(report)
}

pub fn show_history(rt: &Runtime) -> Result<String> {
    render_history(rt, Theme::stdout())
}

pub fn render_history(rt: &Runtime, theme: Theme) -> Result<String> {
    let path = rt.layout.history_file();
    let raw = if path.exists() {
        Some(fs::read_to_string(path)?)
    } else {
        None
    };
    Ok(view::render_history(theme, raw.as_deref()))
}

pub fn show_stats(rt: &Runtime) -> Result<String> {
    render_stats(rt, Theme::stdout())
}

pub fn render_stats(rt: &Runtime, theme: Theme) -> Result<String> {
    let _spinner = ui::spinner("Reading chat usage", rt.quiet);
    stats::render(rt, theme)
}

pub fn record_history(
    rt: &Runtime,
    command: &str,
    args: &[String],
    outcome: &str,
    started: Instant,
) {
    if let Err(err) = write_history(rt, command, args, outcome, started) {
        ui::warn(&format!("history was not written: {err}"));
    }
}

fn repath(
    rt: &Runtime,
    pairs: &[(String, String)],
    replace: Option<(&str, &str)>,
    regex: bool,
    project: bool,
    transfer: Transfer,
) -> Result<Report> {
    rt.check()?;
    let mut report = Report::default();
    let planned = preflight(rt, pairs, replace, regex, project, transfer, &mut report)?;
    if planned.is_empty() {
        return Ok(report);
    }
    if !report.warnings.is_empty() && !rt.yes && !rt.quiet {
        let rows: Vec<Vec<String>> = planned
            .iter()
            .map(|item| vec![item.source.id.clone(), item.dest.display().to_string()])
            .collect();
        print!(
            "{}",
            ui::validation(
                Theme::stdout(),
                match transfer {
                    Transfer::Move => "Move plan",
                    Transfer::Copy => "Copy plan",
                },
                &["workspace", "destination"],
                &rows,
                &report.warnings,
            )
        );
        return Err(ui::hinted(
            "warnings need confirmation",
            "Pass -y to continue.",
        ));
    }
    if rt.dry_run {
        for item in &planned {
            report.applied.push(format!(
                "dry-run {} -> {}",
                item.source.id,
                item.dest.display()
            ));
        }
        return Ok(report);
    }
    apply_plans(rt, &planned, &mut report)?;
    Ok(report)
}

fn preflight(
    rt: &Runtime,
    pairs: &[(String, String)],
    replace: Option<(&str, &str)>,
    regex: bool,
    project: bool,
    transfer: Transfer,
    report: &mut Report,
) -> Result<Vec<Planned>> {
    let workspaces = discover(rt)?;
    let mut specs: Vec<(Workspace, PathBuf)> = Vec::new();
    if let Some((from, to)) = replace {
        let pattern = if regex {
            Some(Regex::new(from).with_context(|| format!("invalid regex: {from}"))?)
        } else {
            None
        };
        for workspace in workspaces {
            if rt
                .profile
                .as_ref()
                .is_some_and(|name| &workspace.profile != name)
            {
                continue;
            }
            let Some(path) = workspace.path.clone() else {
                continue;
            };
            let path_str = path.to_string_lossy().to_string();
            let updated = if let Some(pattern) = &pattern {
                if !pattern.is_match(&path_str) {
                    continue;
                }
                pattern.replace(&path_str, to).into_owned()
            } else if path_str.contains(from) {
                path_str.replacen(from, to, 1)
            } else {
                continue;
            };
            let dest = PathBuf::from(updated);
            if project
                && let Some(parent) = dest.parent()
                && !parent.exists()
            {
                bail!(
                    "--project refused: destination parent is missing: {}",
                    parent.display()
                );
            }
            if !project && !dest.exists() {
                report.warnings.push(format!(
                    "skipped {}: destination missing {}",
                    workspace.id,
                    dest.display()
                ));
                report.skipped.push(workspace.id.clone());
                continue;
            }
            specs.push((workspace, dest));
        }
    } else {
        for (from, to) in pairs {
            let source = require_workspace(rt, from)?;
            let dest = PathBuf::from(to);
            if project {
                if let Some(parent) = dest.parent()
                    && !parent.as_os_str().is_empty()
                    && !parent.exists()
                {
                    bail!(
                        "--project refused: destination parent is missing: {}",
                        parent.display()
                    );
                }
            } else if !dest.exists() {
                report.warnings.push(format!(
                    "skipped {}: destination missing {}",
                    source.id,
                    dest.display()
                ));
                report.skipped.push(source.id);
                continue;
            }
            specs.push((source, abs_path(&dest)));
        }
    }
    let mut planned = Vec::new();
    for (source, dest) in specs {
        let item = plan_one(&source, &dest, project, transfer)?;
        let dest_dir = rt.layout.workspace_storage().join(&item.new_hash);
        if dest_dir.exists() && dest_dir != source.dir {
            bail!(
                "collision: {} already holds another workspace; refusing overwrite",
                dest_dir.display()
            );
        }
        planned.push(item);
    }
    if transfer == Transfer::Copy {
        for item in &planned {
            let needed = dir_size(&item.source.dir).unwrap_or(0);
            if let Some(free) = free_bytes(&item.dest)
                && free < needed
            {
                bail!(
                    "not enough disk space to copy {}: need {needed} bytes, {free} free",
                    item.source.id
                );
            }
        }
    }
    Ok(planned)
}

#[cfg(unix)]
#[allow(
    clippy::useless_conversion,
    reason = "fsblkcnt_t is u32 on macOS and u64 on Linux"
)]
fn free_bytes(path: &Path) -> Option<u64> {
    let text = path.to_string_lossy();
    let c_path = std::ffi::CString::new(text.as_bytes()).ok()?;
    unsafe {
        let mut stat: libc::statvfs = std::mem::zeroed();
        if libc::statvfs(c_path.as_ptr(), &mut stat) != 0 {
            return None;
        }
        Some(u64::from(stat.f_bavail).saturating_mul(stat.f_frsize))
    }
}

#[cfg(windows)]
fn free_bytes(path: &Path) -> Option<u64> {
    use std::os::windows::ffi::OsStrExt;
    use windows_sys::Win32::Storage::FileSystem::GetDiskFreeSpaceExW;

    // GetDiskFreeSpaceExW only accepts directories; `.code-workspace` destinations are files.
    let dir = if path.is_file() { path.parent()? } else { path };
    let wide: Vec<u16> = dir
        .as_os_str()
        .encode_wide()
        .chain(std::iter::once(0))
        .collect();
    let mut available = 0u64;
    let ok = unsafe {
        GetDiskFreeSpaceExW(
            wide.as_ptr(),
            &mut available,
            std::ptr::null_mut(),
            std::ptr::null_mut(),
        )
    };
    (ok != 0).then_some(available)
}

fn plan_one(source: &Workspace, dest: &Path, project: bool, transfer: Transfer) -> Result<Planned> {
    let dest = if project {
        dest.to_path_buf()
    } else {
        abs_path(dest)
    };
    if !project && !dest.exists() {
        bail!("destination does not exist: {}", dest.display());
    }
    let hash_path = if project && !dest.exists() {
        source
            .path
            .clone()
            .context("project move needs a source path")?
    } else {
        dest.clone()
    };
    let new_hash = if project && !dest.exists() {
        "pending".to_string()
    } else {
        compute_workspace_hash(&hash_path)?
    };
    let dest_uri = file_uri(&dest)?;
    Ok(Planned {
        source: source.clone(),
        dest,
        dest_uri,
        new_hash,
        project,
        transfer,
    })
}

fn apply_plans(rt: &Runtime, planned: &[Planned], report: &mut Report) -> Result<()> {
    let backup = backup_plans(rt, planned)?;
    let label = match planned.first().map(|item| item.transfer) {
        Some(Transfer::Copy) => "copy",
        _ => "move",
    };
    let progress = DualProgress::new(label, planned.len() as u64, rt.quiet);
    for (index, item) in planned.iter().enumerate() {
        rt.check().inspect_err(|_| {
            restore_backup(&backup).ok();
        })?;
        progress.step(
            index as u64,
            &format!("{} {}", item.source.id, item.dest.display()),
        );
        let applied = apply_one(rt, item, report, &progress).inspect_err(|_| {
            restore_backup(&backup).ok();
        })?;
        verify_plan(rt, &applied)?;
        report
            .applied
            .push(format!("{} -> {}", applied.source.id, applied.new_hash));
    }
    progress.finish();
    Ok(())
}

fn apply_one(
    rt: &Runtime,
    item: &Planned,
    report: &mut Report,
    progress: &DualProgress,
) -> Result<Planned> {
    let mut item = item.clone();
    rt.check()?;
    if item.project {
        shift_project_dir(&item)?;
        item.new_hash = compute_workspace_hash(&item.dest)?;
        item.dest_uri = file_uri(&item.dest)?;
    }
    rt.check()?;
    relocate_storage(rt, &item)?;
    rt.check()?;
    rewrite_global(rt, &item, progress)?;
    rt.check()?;
    let keys = rewrite_storage_json(rt.layout.storage_json(), &plan_replacements(&item)?, false)?;
    report.rewritten_keys.extend(keys);
    rt.check()?;
    relocate_projects(rt, &item)?;
    Ok(item)
}

fn plan_replacements(item: &Planned) -> Result<Vec<Replacement>> {
    let mut reps = vec![
        Replacement::new(&item.source.id, &item.new_hash),
        Replacement::new(&item.dest_uri, &item.dest_uri),
    ];
    if let Some(old_uri) = &item.source.uri {
        reps.push(Replacement::new(old_uri, &item.dest_uri));
    }
    if let Some(old_path) = &item.source.path {
        reps.push(Replacement::new(
            old_path.to_string_lossy().as_ref(),
            item.dest.to_string_lossy().as_ref(),
        ));
    }
    Ok(reps)
}

fn shift_project_dir(item: &Planned) -> Result<()> {
    let Some(source) = &item.source.path else {
        bail!("--project needs a real source folder");
    };
    if item.dest.exists() {
        bail!(
            "collision: destination already exists: {}",
            item.dest.display()
        );
    }
    match item.transfer {
        Transfer::Move => {
            fs::rename(source, &item.dest).with_context(|| {
                format!(
                    "failed to move {} to {}",
                    source.display(),
                    item.dest.display()
                )
            })?;
        }
        Transfer::Copy => {
            let options = CopyOptions::new().copy_inside(true);
            dir::copy(source, &item.dest, &options)?;
        }
    }
    Ok(())
}

fn relocate_storage(rt: &Runtime, item: &Planned) -> Result<()> {
    let dest_dir = rt.layout.workspace_storage().join(&item.new_hash);
    if item.source.dir == dest_dir {
        write_workspace_json(&dest_dir, &item.dest_uri, &item.dest)?;
        return Ok(());
    }
    if dest_dir.exists() {
        bail!(
            "collision: {} already holds another workspace; refusing overwrite",
            dest_dir.display()
        );
    }
    fs::create_dir_all(rt.layout.workspace_storage())?;
    match item.transfer {
        Transfer::Move => {
            fs::rename(&item.source.dir, &dest_dir)?;
        }
        Transfer::Copy => {
            let options = CopyOptions::new().copy_inside(true);
            dir::copy(&item.source.dir, &dest_dir, &options)?;
        }
    }
    write_workspace_json(&dest_dir, &item.dest_uri, &item.dest)?;
    Ok(())
}

fn write_workspace_json(dir: &Path, uri: &str, path: &Path) -> Result<()> {
    fs::create_dir_all(dir)?;
    let body = if path.extension().and_then(|ext| ext.to_str()) == Some("code-workspace") {
        json!({ "workspace": uri })
    } else {
        json!({ "folder": uri })
    };
    fs::write(
        dir.join("workspace.json"),
        serde_json::to_string_pretty(&body)?,
    )?;
    Ok(())
}

fn rewrite_global(rt: &Runtime, item: &Planned, progress: &DualProgress) -> Result<()> {
    let path = rt.layout.global_db();
    if !path.exists() {
        return Ok(());
    }
    let conn = db::open_rw(&path)?;
    let replacements = plan_replacements(item)?;
    let count = if item.transfer == Transfer::Copy {
        let ids = db::composer_ids_for_workspace(&conn, &item.source.id)?;
        db::clone_chats(&conn, &ids, &item.new_hash, &replacements)?.len()
    } else {
        db::rewrite_workspace(
            &conn,
            &item.source.id,
            &replacements,
            &item.source.id,
            &item.new_hash,
            false,
        )?
    };
    progress.rows(count as u64, count as u64, "rows");
    Ok(())
}

fn relocate_projects(rt: &Runtime, item: &Planned) -> Result<()> {
    let Some(old_path) = &item.source.path else {
        return Ok(());
    };
    let old_slug = path_to_folder_id(old_path);
    let new_slug = path_to_folder_id(&item.dest);
    let source = rt.layout.projects_dir.join(&old_slug);
    let dest = rt.layout.projects_dir.join(&new_slug);
    if !source.exists() || old_slug == new_slug {
        return Ok(());
    }
    fs::create_dir_all(&rt.layout.projects_dir)?;
    match item.transfer {
        Transfer::Move => {
            if dest.exists() {
                bail!("collision: project slug already exists: {}", dest.display());
            }
            fs::rename(&source, &dest)?;
        }
        Transfer::Copy => {
            let options = CopyOptions::new().copy_inside(true);
            dir::copy(&source, &dest, &options)?;
        }
    }
    Ok(())
}

fn verify_plan(rt: &Runtime, item: &Planned) -> Result<()> {
    let dir = rt.layout.workspace_storage().join(&item.new_hash);
    let raw = fs::read_to_string(dir.join("workspace.json"))
        .with_context(|| format!("missing workspace.json in {}", dir.display()))?;
    let json: Value = serde_json::from_str(&raw)?;
    let uri = json
        .get("folder")
        .or_else(|| json.get("workspace"))
        .and_then(|v| v.as_str())
        .context("workspace.json has no folder or workspace uri")?;
    if uri != item.dest_uri {
        bail!("verify failed: workspace.json uri is {uri}");
    }
    if item.dest.exists() {
        let hashed = compute_workspace_hash(&item.dest)?;
        if hashed != item.new_hash && item.new_hash != "pending" {
            bail!("verify failed: hash dir {} != {hashed}", item.new_hash);
        }
    }
    if rt.layout.global_db().exists() {
        let conn = db::open_rw(&rt.layout.global_db())?;
        let count = db::count_headers(&conn, &item.new_hash)?;
        if count == 0
            && db::count_headers(&conn, &item.source.id)? > 0
            && item.transfer == Transfer::Move
        {
            bail!(
                "verify failed: composerHeaders were not moved to {}",
                item.new_hash
            );
        }
        if let Some(old) = &item.source.path {
            let old = old.to_string_lossy().to_string();
            if old != item.dest.to_string_lossy()
                && db::row_contains_old(&conn, &item.new_hash, &old)?
            {
                bail!("verify failed: old path still present in touched rows");
            }
        }
    }
    Ok(())
}

#[derive(Clone)]
struct SavedFile {
    live: PathBuf,
    copy: PathBuf,
}

#[derive(Clone)]
struct WorkspaceBackup {
    original: PathBuf,
    copy: PathBuf,
    relocated: Option<PathBuf>,
}

#[derive(Clone)]
struct Backup {
    storage_json: Option<SavedFile>,
    global_db: Option<SavedFile>,
    global_sidecars: Vec<SavedFile>,
    workspaces: Vec<WorkspaceBackup>,
}

fn backup_plans(rt: &Runtime, planned: &[Planned]) -> Result<Backup> {
    let dir = rt
        .layout
        .backup_root()
        .join(Utc::now().timestamp_millis().to_string());
    fs::create_dir_all(&dir)?;
    let storage_json = copy_file(&rt.layout.storage_json(), &dir.join("storage.json"))?;
    let global_db = copy_file(&rt.layout.global_db(), &dir.join("state.vscdb"))?;
    let mut global_sidecars = Vec::new();
    for sidecar in sqlite_sidecars(&rt.layout.global_db()) {
        if let Some(saved) =
            copy_file(&sidecar, &dir.join(sidecar.file_name().unwrap_or_default()))?
        {
            global_sidecars.push(saved);
        }
    }
    let mut workspaces = Vec::new();
    for item in planned {
        if item.source.dir.exists() {
            let dest = dir.join("workspace").join(&item.source.id);
            let options = CopyOptions::new().copy_inside(true);
            if let Some(parent) = dest.parent() {
                fs::create_dir_all(parent)?;
            }
            dir::copy(&item.source.dir, &dest, &options)?;
            let relocated = rt.layout.workspace_storage().join(&item.new_hash);
            let relocated = if item.new_hash != item.source.id && item.new_hash != "pending" {
                Some(relocated)
            } else {
                None
            };
            workspaces.push(WorkspaceBackup {
                original: item.source.dir.clone(),
                copy: dest,
                relocated,
            });
        }
    }
    Ok(Backup {
        storage_json,
        global_db,
        global_sidecars,
        workspaces,
    })
}

fn copy_file(live: &Path, copy: &Path) -> Result<Option<SavedFile>> {
    if !live.exists() {
        return Ok(None);
    }
    if let Some(parent) = copy.parent() {
        fs::create_dir_all(parent)?;
    }
    fs::copy(live, copy)?;
    Ok(Some(SavedFile {
        live: live.to_path_buf(),
        copy: copy.to_path_buf(),
    }))
}

fn sqlite_sidecars(db: &Path) -> Vec<PathBuf> {
    ["-wal", "-shm", "-journal"]
        .into_iter()
        .map(|suffix| {
            let mut name = db.file_name().unwrap_or_default().to_os_string();
            name.push(suffix);
            db.with_file_name(name)
        })
        .collect()
}

fn restore_saved(file: &SavedFile) -> Result<()> {
    if let Some(parent) = file.live.parent() {
        fs::create_dir_all(parent)?;
    }
    fs::copy(&file.copy, &file.live)?;
    Ok(())
}

fn restore_dir(copy: &Path, original: &Path) -> Result<()> {
    if original.exists() {
        fs::remove_dir_all(original)?;
    }
    if let Some(parent) = original.parent() {
        fs::create_dir_all(parent)?;
    }
    let options = CopyOptions::new().copy_inside(true);
    dir::copy(copy, original, &options)?;
    Ok(())
}

fn restore_backup(backup: &Backup) -> Result<()> {
    if let Some(file) = &backup.storage_json {
        restore_saved(file)?;
    }
    if let Some(file) = &backup.global_db {
        restore_saved(file)?;
        for sidecar in sqlite_sidecars(&file.live) {
            if sidecar.exists() {
                fs::remove_file(&sidecar)?;
            }
        }
        for sidecar in &backup.global_sidecars {
            restore_saved(sidecar)?;
        }
    }
    for workspace in &backup.workspaces {
        if let Some(relocated) = &workspace.relocated
            && relocated != &workspace.original
            && relocated.exists()
        {
            fs::remove_dir_all(relocated)?;
        }
        if workspace.copy.exists() {
            restore_dir(&workspace.copy, &workspace.original)?;
        }
    }
    Ok(())
}

fn shift_local_ids(
    rt: &Runtime,
    source: &Workspace,
    target: &Workspace,
    ids: &[String],
    map: &HashMap<String, String>,
    remove_source: bool,
) -> Result<()> {
    let source_db = source.dir.join("state.vscdb");
    let target_dir = rt.layout.workspace_storage().join(&target.id);
    let target_db = target_dir.join("state.vscdb");
    if source_db.exists() && remove_source {
        let conn = db::open_rw(&source_db)?;
        registry::ensure_local_schema(&conn)?;
        let (mut selected, mut focused) = db::local_selected(&conn)?;
        selected.retain(|id| !ids.contains(id));
        focused.retain(|id| !ids.contains(id));
        db::write_local_selected(&conn, &selected, &focused)?;
    }
    fs::create_dir_all(&target_dir)?;
    let conn = if target_db.exists() {
        db::open_rw(&target_db)?
    } else {
        let conn = Connection::open(&target_db)?;
        registry::ensure_local_schema(&conn)?;
        conn
    };
    let (mut selected, mut focused) = db::local_selected(&conn)?;
    for id in ids {
        let stored = map.get(id).cloned().unwrap_or_else(|| id.clone());
        if !selected.contains(&stored) {
            selected.push(stored.clone());
        }
        if !focused.contains(&stored) {
            focused.push(stored);
        }
    }
    db::write_local_selected(&conn, &selected, &focused)?;
    Ok(())
}

fn move_transcripts(
    rt: &Runtime,
    source: &Workspace,
    target: &Workspace,
    ids: &[String],
    map: &HashMap<String, String>,
    remove_source: bool,
) -> Result<()> {
    let Some(source_path) = &source.path else {
        return Ok(());
    };
    let Some(target_path) = &target.path else {
        return Ok(());
    };
    let source_root = rt
        .layout
        .projects_dir
        .join(path_to_folder_id(source_path))
        .join("agent-transcripts");
    let target_root = rt
        .layout
        .projects_dir
        .join(path_to_folder_id(target_path))
        .join("agent-transcripts");
    if !source_root.exists() {
        return Ok(());
    }
    fs::create_dir_all(&target_root)?;
    for id in ids {
        let from = source_root.join(id);
        if !from.exists() {
            continue;
        }
        let dest_id = map.get(id).cloned().unwrap_or_else(|| id.clone());
        let dest = target_root.join(dest_id);
        match (remove_source, map.is_empty()) {
            (true, true) => {
                fs::rename(&from, &dest)?;
            }
            _ => {
                let options = CopyOptions::new().copy_inside(true);
                dir::copy(&from, &dest, &options)?;
                if remove_source {
                    fs::remove_dir_all(&from).ok();
                }
            }
        }
    }
    Ok(())
}

fn ensure_folder_workspace(rt: &Runtime, path: &Path) -> Result<Workspace> {
    if let Some(existing) = discover(rt)?
        .into_iter()
        .find(|workspace| workspace.path.as_ref().is_some_and(|found| found == path))
    {
        return Ok(existing);
    }
    let hash = compute_workspace_hash(path)?;
    let dir = rt.layout.workspace_storage().join(&hash);
    if dir.exists() {
        bail!(
            "collision: {} already holds another workspace; refusing overwrite",
            dir.display()
        );
    }
    let uri = file_uri(path)?;
    write_workspace_json(&dir, &uri, path)?;
    let db_path = dir.join("state.vscdb");
    let conn = Connection::open(&db_path)?;
    registry::ensure_local_schema(&conn)?;
    Ok(Workspace {
        id: hash,
        dir,
        kind: Kind::Folder,
        uri: Some(uri),
        path: Some(path.to_path_buf()),
        profile: "default".to_string(),
        destination_missing: false,
    })
}

pub fn discover(rt: &Runtime) -> Result<Vec<Workspace>> {
    let mut found = Vec::new();
    let root = rt.layout.workspace_storage();
    if root.exists() {
        let profiles = profile_map(rt)?;
        for entry in fs::read_dir(&root)?.flatten() {
            if !entry.file_type()?.is_dir() {
                continue;
            }
            let dir = entry.path();
            let id = entry.file_name().to_string_lossy().to_string();
            let uri = workspace::read_workspace_target_uri(&dir)?;
            let path = uri.as_deref().and_then(uri_to_path);
            let kind = classify(&id, uri.as_deref(), path.as_deref());
            let profile = uri
                .as_ref()
                .and_then(|value| profiles.get(value).cloned())
                .unwrap_or_else(|| "default".to_string());
            let destination_missing = match kind {
                Kind::Folder | Kind::CodeWorkspace => {
                    path.as_ref().is_none_or(|path| !path.exists())
                }
                Kind::Unsaved | Kind::EmptyWindow => false,
            };
            found.push(Workspace {
                id,
                dir,
                kind,
                uri,
                path,
                profile,
                destination_missing,
            });
        }
    }
    found.sort_by(|a, b| a.id.cmp(&b.id));
    Ok(found)
}

fn profile_map(rt: &Runtime) -> Result<HashMap<String, String>> {
    let path = rt.layout.storage_json();
    if !path.exists() {
        return Ok(HashMap::new());
    }
    let raw = fs::read_to_string(path)?;
    let json: Value = serde_json::from_str(&raw).unwrap_or(Value::Null);
    let mut map = HashMap::new();
    if let Some(workspaces) = json
        .pointer("/profileAssociations/workspaces")
        .and_then(|v| v.as_object())
    {
        for (uri, name) in workspaces {
            let label = name.as_str().unwrap_or("default");
            let label = if label == "__default__profile__" {
                "default"
            } else {
                label
            };
            map.insert(uri.clone(), label.to_string());
        }
    }
    Ok(map)
}

fn classify(id: &str, uri: Option<&str>, path: Option<&Path>) -> Kind {
    if id == "empty-window" {
        return Kind::EmptyWindow;
    }
    if let Some(uri) = uri
        && (uri.contains("/Workspaces/") || id.chars().all(|ch| ch.is_ascii_digit()))
    {
        return Kind::Unsaved;
    }
    if path
        .is_some_and(|path| path.extension().and_then(|ext| ext.to_str()) == Some("code-workspace"))
    {
        return Kind::CodeWorkspace;
    }
    if uri.is_none() && path.is_none() {
        return Kind::EmptyWindow;
    }
    Kind::Folder
}

fn is_unsaved_id(workspace: &Workspace, id: &str) -> bool {
    if workspace.id == id {
        return workspace.kind == Kind::Unsaved
            || workspace.id.chars().all(|ch| ch.is_ascii_digit());
    }
    workspace.uri.as_ref().is_some_and(|uri| {
        uri.contains(&format!("/Workspaces/{id}/")) || uri.contains(&format!("/Workspaces/{id}"))
    })
}

pub fn find_workspace(rt: &Runtime, id: &str) -> Result<Option<Workspace>> {
    let path = PathBuf::from(id);
    Ok(discover(rt)?.into_iter().find(|workspace| {
        workspace.id == id
            || workspace
                .path
                .as_ref()
                .is_some_and(|found| found == &path || found.to_string_lossy() == id)
            || is_unsaved_id(workspace, id)
    }))
}

fn require_workspace(rt: &Runtime, id: &str) -> Result<Workspace> {
    find_workspace(rt, id)
        .and_then(|found| found.with_context(|| format!("no workspace matches {id}")))
}

fn open_global(rt: &Runtime) -> Result<Connection> {
    db::ensure_db(&rt.layout.global_db())
}

fn dir_size(path: &Path) -> Result<u64> {
    let mut total = 0u64;
    if !path.exists() {
        return Ok(0);
    }
    for entry in walkdir::WalkDir::new(path) {
        let entry = entry?;
        if entry.file_type().is_file() {
            total += entry.metadata()?.len();
        }
    }
    Ok(total)
}

fn clear_workspace_caches(dir: &Path) -> Result<()> {
    for name in ["Cache", "CachedData", "GPUCache", "Code Cache"] {
        let path = dir.join(name);
        if path.exists() {
            fs::remove_dir_all(path)?;
        }
    }
    Ok(())
}

fn write_history(
    rt: &Runtime,
    command: &str,
    args: &[String],
    outcome: &str,
    started: Instant,
) -> Result<()> {
    fs::create_dir_all(&rt.layout.crepath_home)?;
    let path = rt.layout.history_file();
    let mut kept = String::new();
    if path.exists() {
        let cutoff = Utc::now() - chrono::Duration::days(30);
        for line in fs::read_to_string(&path)?.lines() {
            let Ok(json) = serde_json::from_str::<Value>(line) else {
                continue;
            };
            let Some(at) = json.get("at").and_then(|v| v.as_str()) else {
                continue;
            };
            let Ok(stamp) = chrono::DateTime::parse_from_rfc3339(at) else {
                continue;
            };
            if stamp > cutoff {
                kept.push_str(line);
                kept.push('\n');
            }
        }
    }
    let entry = json!({
        "at": Utc::now().to_rfc3339(),
        "command": command,
        "args": args,
        "outcome": outcome,
        "duration_ms": started.elapsed().as_millis() as u64,
    });
    kept.push_str(&entry.to_string());
    kept.push('\n');
    fs::write(path, kept)?;
    Ok(())
}

fn file_uri(path: &Path) -> Result<String> {
    Url::from_file_path(path)
        .map(|url| url.to_string())
        .map_err(|_| anyhow::anyhow!("failed to convert path to uri: {}", path.display()))
}

fn uri_to_path(uri: &str) -> Option<PathBuf> {
    let url = Url::parse(uri).ok()?;
    match url.scheme() {
        "file" => url.to_file_path().ok(),
        _ => Some(PathBuf::from(url.path())),
    }
}

fn materialize_path(raw: &str) -> Option<PathBuf> {
    if let Some(path) = uri_to_path(raw) {
        return Some(path);
    }
    let decoded = percent_encoding::percent_decode_str(raw)
        .decode_utf8()
        .ok()?;
    if decoded.starts_with('/') || decoded.contains(":\\") {
        Some(PathBuf::from(decoded.as_ref()))
    } else {
        None
    }
}

fn longest_target(path: &Path, targets: &[PathBuf]) -> Option<PathBuf> {
    targets
        .iter()
        .filter(|target| path.starts_with(target))
        .max_by_key(|target| target.as_os_str().len())
        .cloned()
}

fn abs_path(path: &Path) -> PathBuf {
    if path.is_absolute() {
        path.to_path_buf()
    } else {
        std::env::current_dir()
            .unwrap_or_else(|_| PathBuf::from("."))
            .join(path)
    }
}

fn cursor_process_running() -> bool {
    #[cfg(target_os = "macos")]
    {
        std::process::Command::new("pgrep")
            .args(["-x", "Cursor"])
            .output()
            .map(|out| out.status.success())
            .unwrap_or(false)
    }
    #[cfg(target_os = "linux")]
    {
        std::process::Command::new("pgrep")
            .args(["-x", "cursor"])
            .output()
            .map(|out| out.status.success())
            .unwrap_or(false)
    }
    #[cfg(target_os = "windows")]
    {
        std::process::Command::new("tasklist")
            .args(["/FI", "IMAGENAME eq Cursor.exe"])
            .output()
            .map(|out| String::from_utf8_lossy(&out.stdout).contains("Cursor.exe"))
            .unwrap_or(false)
    }
}

mod archive;
mod stats;

pub fn profiles_from_storage(json: &Value) -> Vec<String> {
    let mut names = vec!["default".to_string()];
    if let Some(items) = json.get("userDataProfiles").and_then(|v| v.as_array()) {
        for item in items {
            if let Some(name) = item.get("name").and_then(|v| v.as_str()) {
                names.push(name.to_string());
            }
        }
    }
    names
}

pub fn load_registry(conn: &Connection) -> Result<Vec<ComposerHeader>> {
    registry::load_headers(conn)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn free_bytes_reports_space_for_directory() {
        let dir = tempfile::tempdir().unwrap();

        assert!(free_bytes(dir.path()).is_some_and(|free| free > 0));
    }

    #[test]
    fn free_bytes_reports_space_for_file() {
        let dir = tempfile::tempdir().unwrap();
        let file = dir.path().join("project.code-workspace");
        fs::write(&file, "{}").unwrap();

        assert!(free_bytes(&file).is_some_and(|free| free > 0));
    }

    #[test]
    fn free_bytes_is_none_for_missing_path() {
        let dir = tempfile::tempdir().unwrap();

        assert_eq!(free_bytes(&dir.path().join("missing").join("deeper")), None);
    }
}

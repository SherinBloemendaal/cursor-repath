//! `mv`, `cp`, and `save`: repath whole workspaces.

use anyhow::{Context, Result, bail};
use regex::Regex;
use serde_json::Value;
use std::collections::{HashMap, HashSet};
use std::fs;
use std::path::{Path, PathBuf};

use super::db::{self, Target};
use super::fsops::{SpaceNeeds, copy_tree, dir_size, exists, write_atomic};
use super::journal::Table;
use super::local;
use super::session::{self, Session};
use super::{
    Kind, Report, Runtime, Workspace, discover, is_unsaved_id, open_global_ro, require_workspace,
};
use crate::cursor::rewrite::{Boundary, Leftovers, Replacement, Rewriter, path_replacements};
use crate::cursor::storage::rewrite_storage_file;
use crate::cursor::uri::{Platform, normalize_path, path_uri};
use crate::cursor::workspace::{compute_renamed_hash, compute_workspace_hash};
use crate::ui::{self, DualProgress, Theme};

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Transfer {
    Move,
    Copy,
}

#[derive(Debug, Clone)]
struct Plan {
    source: Workspace,
    dest: Workspace,
    project: bool,
    transfer: Transfer,
    chats: Vec<String>,
    owned: HashSet<String>,
    source_headers: usize,
    dest_headers: usize,
}

struct Applied {
    plan: Plan,
    entries: Vec<(Replacement, Boundary)>,
    map: HashMap<String, String>,
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
    let source = discover(rt)?
        .into_iter()
        .find(|workspace| is_unsaved_id(workspace, id))
        .with_context(|| format!("no unsaved workspace matches {id}"))?;
    let dest = normalize_path(dest);
    if !dest.exists() {
        bail!("destination does not exist: {}", dest.display());
    }
    let mut report = Report::default();
    let plans = plan_all(rt, vec![(source, dest)], false, Transfer::Move, &mut report)?;
    execute(rt, plans, report, "save")
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
    let specs = collect_specs(rt, pairs, replace, regex, project, &mut report)?;
    let plans = plan_all(rt, specs, project, transfer, &mut report)?;
    if plans.is_empty() {
        return Ok(report);
    }
    if !report.warnings.is_empty() && !rt.yes && !rt.quiet {
        let rows: Vec<Vec<String>> = plans
            .iter()
            .map(|plan| vec![plan.source.id.clone(), destination(plan)])
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
    let label = match transfer {
        Transfer::Move => "mv",
        Transfer::Copy => "cp",
    };
    execute(rt, plans, report, label)
}

fn check_project_parent(dest: &Path) -> Result<()> {
    if let Some(parent) = dest.parent()
        && !parent.as_os_str().is_empty()
        && !parent.exists()
    {
        bail!(
            "--project refused: destination parent is missing: {}",
            parent.display()
        );
    }
    Ok(())
}

fn collect_specs(
    rt: &Runtime,
    pairs: &[(String, String)],
    replace: Option<(&str, &str)>,
    regex: bool,
    project: bool,
    report: &mut Report,
) -> Result<Vec<(Workspace, PathBuf)>> {
    let mut specs = Vec::new();
    let mut accept = |source: Workspace, dest: PathBuf, report: &mut Report| -> Result<()> {
        if project {
            check_project_parent(&dest)?;
        } else if !dest.exists() {
            report.warnings.push(format!(
                "skipped {}: destination missing {}",
                source.id,
                dest.display()
            ));
            report.skipped.push(source.id);
            return Ok(());
        }
        specs.push((source, dest));
        Ok(())
    };
    if let Some((from, to)) = replace {
        let pattern = if regex {
            Some(Regex::new(from).with_context(|| format!("invalid regex: {from}"))?)
        } else {
            None
        };
        for workspace in discover(rt)? {
            if rt
                .profile
                .as_ref()
                .is_some_and(|name| &workspace.profile != name)
            {
                continue;
            }
            if workspace.kind == Kind::Remote {
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
            let dest = normalize_path(Path::new(&updated));
            if dest == path {
                continue;
            }
            accept(workspace, dest, report)?;
        }
    } else {
        for (from, to) in pairs {
            let source = require_workspace(rt, from)?;
            accept(source, normalize_path(Path::new(to)), report)?;
        }
    }
    Ok(specs)
}

const PENDING: &str = "pending";

fn plan_dest(
    rt: &Runtime,
    source: &Workspace,
    dest: &Path,
    project: bool,
    transfer: Transfer,
) -> Result<Workspace> {
    if !project {
        return Workspace::planned(&rt.layout, dest);
    }
    let Some(source_path) = source.path.as_ref().filter(|_| source.kind == Kind::Folder) else {
        bail!("--project needs a folder workspace: {}", source.id);
    };
    if !source_path.is_dir() {
        bail!(
            "--project needs the real folder to exist: {}",
            source_path.display()
        );
    }
    if exists(dest) {
        bail!("collision: destination already exists: {}", dest.display());
    }
    let id = match transfer {
        Transfer::Move => compute_renamed_hash(source_path, dest)?,
        Transfer::Copy => PENDING.to_string(),
    };
    Ok(Workspace {
        dir: rt.layout.workspace_storage().join(&id),
        id,
        kind: Kind::Folder,
        uri: Some(path_uri(dest)),
        path: Some(dest.to_path_buf()),
        profile: source.profile.clone(),
        destination_missing: false,
    })
}

fn plan_all(
    rt: &Runtime,
    specs: Vec<(Workspace, PathBuf)>,
    project: bool,
    transfer: Transfer,
    report: &mut Report,
) -> Result<Vec<Plan>> {
    let conn = open_global_ro(rt)?;
    let mut plans = Vec::new();
    let mut needs = SpaceNeeds::default();
    let mut targets = HashSet::new();
    for (source, dest_path) in specs {
        if source.kind == Kind::Remote {
            bail!("{}: remote workspaces are not supported", source.id);
        }
        let mut dest = plan_dest(rt, &source, &dest_path, project, transfer)?;
        dest.profile = source.profile.clone();
        if dest.id == source.id && dest.path == source.path && dest.uri == source.uri {
            report.warnings.push(format!(
                "skipped {}: already at {}",
                source.id,
                dest_path.display()
            ));
            report.skipped.push(source.id.clone());
            continue;
        }
        if transfer == Transfer::Copy && dest.id == source.id {
            bail!(
                "collision: {} is the source workspace itself",
                dest_path.display()
            );
        }
        if dest.id != PENDING {
            if !targets.insert(dest.id.clone()) {
                bail!(
                    "collision: two workspaces would land on {}",
                    dest.dir.display()
                );
            }
            if dest.dir.exists() && dest.dir != source.dir {
                bail!(
                    "collision: {} already holds another workspace; refusing overwrite",
                    dest.dir.display()
                );
            }
        }
        if let (Some(old), Some(new)) = (source.slug(), dest.slug())
            && old != new
        {
            let from = rt.layout.projects_dir.join(&old);
            let to = rt.layout.projects_dir.join(&new);
            let conflicts = local::merge_conflicts(&from, &to, transfer == Transfer::Copy)?;
            if let Some(first) = conflicts.first() {
                bail!(
                    "collision: {} already exists ({} conflicting entries under {})",
                    first.display(),
                    conflicts.len(),
                    to.display()
                );
            }
            if transfer == Transfer::Copy {
                needs.add(
                    &rt.layout.projects_dir,
                    dir_size(&from)?,
                    "project data copy",
                );
            }
        }
        let (chats, owned, source_headers, dest_headers) = match &conn {
            Some(conn) => {
                let ids = db::composer_ids_for_workspace(conn, &source.id)?;
                let count = ids.len();
                let owned: HashSet<String> = ids.iter().cloned().collect();
                let chats = db::expand_chat_ids(conn, &ids)?;
                let bytes = db::chat_bytes(conn, &chats)? + db::workspace_bytes(conn, &source.id)?;
                needs.add(
                    &rt.layout.global_storage(),
                    bytes.saturating_mul(2),
                    "database log",
                );
                if transfer == Transfer::Move {
                    needs.add(&rt.layout.backup_root(), bytes, "undo log");
                }
                let existing = if dest.id == PENDING {
                    0
                } else {
                    db::count_headers(conn, &dest.id)?
                };
                (chats, owned, count, existing)
            }
            None => (Vec::new(), HashSet::new(), 0, 0),
        };
        if transfer == Transfer::Copy {
            needs.add(
                &rt.layout.workspace_storage(),
                dir_size(&source.dir)?,
                "workspace storage copy",
            );
            if project && let Some(path) = &source.path {
                needs.add(&dest_path, dir_size(path)?, "project folder copy");
            }
        }
        needs.add(
            &rt.layout.backup_root(),
            fs::metadata(rt.layout.storage_json()).map_or(0, |meta| meta.len())
                + fs::metadata(source.dir.join("state.vscdb")).map_or(0, |meta| meta.len()),
            "backups",
        );
        plans.push(Plan {
            source,
            dest,
            project,
            transfer,
            chats,
            owned,
            source_headers,
            dest_headers,
        });
    }
    needs.check()?;
    Ok(plans)
}

fn destination(plan: &Plan) -> String {
    plan.dest
        .path
        .as_ref()
        .map(|path| path.display().to_string())
        .unwrap_or_default()
}

fn execute(rt: &Runtime, plans: Vec<Plan>, mut report: Report, label: &str) -> Result<Report> {
    if plans.is_empty() {
        return Ok(report);
    }
    if rt.dry_run {
        for plan in &plans {
            report.applied.push(format!(
                "dry-run {} -> {} ({} chats)",
                plan.source.id,
                destination(plan),
                plan.source_headers
            ));
        }
        return Ok(report);
    }
    let verb = match plans.first().map(|plan| plan.transfer) {
        Some(Transfer::Copy) => "copy",
        _ => "move",
    };
    let progress = DualProgress::new(verb, plans.len() as u64, rt.quiet);
    let mut keys = Vec::new();
    let applied = session::run(
        rt,
        label,
        |session| {
            let mut applied = Vec::new();
            for (index, plan) in plans.iter().enumerate() {
                progress.step(
                    index as u64,
                    &format!("{} {}", plan.source.id, destination(plan)),
                );
                applied.push(apply_one(session, plan.clone(), &progress, &mut keys)?);
            }
            Ok(applied)
        },
        |session, applied| {
            for item in applied {
                verify(session, item)?;
            }
            Ok(())
        },
    );
    progress.finish();
    for item in applied? {
        report
            .applied
            .push(format!("{} -> {}", item.plan.source.id, item.plan.dest.id));
    }
    report.rewritten_keys.extend(keys);
    Ok(report)
}

fn replacement_entries(plan: &Plan) -> Vec<(Replacement, Boundary)> {
    let mut entries = Vec::new();
    if let (Some(old), Some(new)) = (&plan.source.path, &plan.dest.path) {
        for item in path_replacements(
            Platform::current(),
            &old.to_string_lossy(),
            &new.to_string_lossy(),
        ) {
            entries.push((item, Boundary::Path));
        }
    }
    if let (Some(old), Some(new)) = (&plan.source.uri, &plan.dest.uri) {
        entries.push((Replacement::new(old, new), Boundary::Path));
    }
    entries.push((
        Replacement::new(&plan.source.id, &plan.dest.id),
        Boundary::Token,
    ));
    entries
}

fn write_workspace_json(session: &mut Session<'_>, workspace: &Workspace) -> Result<()> {
    let path = workspace.dir.join("workspace.json");
    session.journal.save(&path)?;
    write_atomic(
        &path,
        serde_json::to_string_pretty(&workspace.workspace_json()?)?.as_bytes(),
    )
}

fn apply_one(
    session: &mut Session<'_>,
    mut plan: Plan,
    progress: &DualProgress,
    keys: &mut Vec<String>,
) -> Result<Applied> {
    session.step()?;
    if plan.project {
        shift_project_folder(session, &mut plan)?;
    }
    session.step()?;
    relocate_storage(session, &plan)?;
    let entries = replacement_entries(&plan);
    let rewriter = Rewriter::build(entries.clone());
    session.step()?;
    let mut map = HashMap::new();
    if session.has_global() {
        let identity = plan.dest.identity();
        let target = Target {
            id: &plan.dest.id,
            identity: &identity,
        };
        let mut writer = session.db()?;
        let count = match plan.transfer {
            Transfer::Move => writer.rewrite_workspace(&plan.source.id, &target, &rewriter)?,
            Transfer::Copy => {
                let extra: Vec<(Replacement, Boundary)> = entries.clone();
                map = writer.clone_chats(&plan.chats, &target, &extra)?;
                let id_entries: Vec<(Replacement, Boundary)> = map
                    .iter()
                    .map(|(old, new)| (Replacement::new(old, new), Boundary::Token))
                    .chain(entries.iter().cloned())
                    .collect();
                writer.rekey_workspace_rows(
                    &plan.source.id,
                    &plan.dest.id,
                    &Rewriter::build(id_entries),
                    true,
                )? + map.len()
            }
        };
        progress.rows(count as u64, count as u64, "rows");
    }
    session.step()?;
    keys.extend(update_storage_json(session, &plan, &rewriter)?);
    session.step()?;
    relocate_projects(session, &plan, &map)?;
    if plan.transfer == Transfer::Copy {
        session.step()?;
        local::remap_selection(&mut session.journal, &plan.dest.dir, &map)?;
    }
    Ok(Applied { plan, entries, map })
}

fn shift_project_folder(session: &mut Session<'_>, plan: &mut Plan) -> Result<()> {
    let rt = session.rt;
    let source = plan
        .source
        .path
        .clone()
        .context("--project needs a real source folder")?;
    let dest = plan
        .dest
        .path
        .clone()
        .context("--project needs a destination")?;
    if exists(&dest) {
        bail!("collision: destination already exists: {}", dest.display());
    }
    match plan.transfer {
        Transfer::Move => session.journal.move_path(&source, &dest).with_context(|| {
            format!("failed to move {} to {}", source.display(), dest.display())
        })?,
        Transfer::Copy => {
            session.journal.created(&dest)?;
            copy_tree(&source, &dest)?;
        }
    }
    let id = compute_workspace_hash(&dest)?;
    if id != plan.dest.id {
        let dir = rt.layout.workspace_storage().join(&id);
        if dir.exists() && dir != plan.source.dir {
            bail!(
                "collision: {} already holds another workspace; refusing overwrite",
                dir.display()
            );
        }
        plan.dest.dir = dir;
        plan.dest.id = id;
    }
    Ok(())
}

fn relocate_storage(session: &mut Session<'_>, plan: &Plan) -> Result<()> {
    let dest_dir = plan.dest.dir.clone();
    if plan.source.dir == dest_dir {
        return write_workspace_json(session, &plan.dest);
    }
    if exists(&dest_dir) {
        bail!(
            "collision: {} already holds another workspace; refusing overwrite",
            dest_dir.display()
        );
    }
    match plan.transfer {
        Transfer::Move => session.journal.move_path(&plan.source.dir, &dest_dir)?,
        Transfer::Copy => {
            session.journal.created(&dest_dir)?;
            copy_tree(&plan.source.dir, &dest_dir)?;
        }
    }
    write_workspace_json(session, &plan.dest)
}

fn update_storage_json(
    session: &mut Session<'_>,
    plan: &Plan,
    rewriter: &Rewriter,
) -> Result<Vec<String>> {
    let path = session.rt.layout.storage_json();
    if !path.exists() {
        return Ok(Vec::new());
    }
    match plan.transfer {
        Transfer::Move => {
            session.journal.save(&path)?;
            rewrite_storage_file(&path, rewriter, false)
        }
        Transfer::Copy => {
            let (Some(old_uri), Some(new_uri)) = (&plan.source.uri, &plan.dest.uri) else {
                return Ok(Vec::new());
            };
            let mut json: Value = serde_json::from_str(&fs::read_to_string(&path)?)
                .context("Failed to parse storage.json")?;
            let Some(workspaces) = json
                .pointer_mut("/profileAssociations/workspaces")
                .and_then(|value| value.as_object_mut())
            else {
                return Ok(Vec::new());
            };
            let Some(profile) = workspaces.get(old_uri).cloned() else {
                return Ok(Vec::new());
            };
            if workspaces.contains_key(new_uri) {
                return Ok(Vec::new());
            }
            workspaces.insert(new_uri.clone(), profile);
            session.journal.save(&path)?;
            write_atomic(&path, serde_json::to_string_pretty(&json)?.as_bytes())?;
            Ok(vec![format!("profileAssociations.workspaces.{new_uri}")])
        }
    }
}

fn relocate_projects(
    session: &mut Session<'_>,
    plan: &Plan,
    map: &HashMap<String, String>,
) -> Result<()> {
    let (Some(old), Some(new)) = (plan.source.slug(), plan.dest.slug()) else {
        return Ok(());
    };
    if old == new {
        return Ok(());
    }
    let projects = session.rt.layout.projects_dir.clone();
    let from = projects.join(&old);
    let to = projects.join(&new);
    match plan.transfer {
        Transfer::Move => local::merge_move(&mut session.journal, &from, &to),
        Transfer::Copy => {
            local::copy_project(&mut session.journal, &from, &to, map, false).map(|_| ())
        }
    }
}

fn verify(session: &mut Session<'_>, applied: &Applied) -> Result<()> {
    let plan = &applied.plan;
    let path = plan.dest.dir.join("workspace.json");
    let raw = fs::read_to_string(&path)
        .with_context(|| format!("verify failed: missing {}", path.display()))?;
    let json: Value = serde_json::from_str(&raw)?;
    if json != plan.dest.workspace_json()? {
        bail!(
            "verify failed: {} does not point at the destination",
            path.display()
        );
    }
    if let Some(dest) = &plan.dest.path
        && dest.exists()
    {
        let hashed = compute_workspace_hash(dest)?;
        if hashed != plan.dest.id {
            bail!(
                "verify failed: {} hashes to {hashed}, not {}",
                dest.display(),
                plan.dest.id
            );
        }
    }
    if !session.has_global() {
        return Ok(());
    }
    let conn = session.conn()?;
    let expected = match plan.transfer {
        Transfer::Move if plan.source.id == plan.dest.id => plan.source_headers,
        _ => plan.dest_headers + plan.source_headers,
    };
    let found = db::count_headers(conn, &plan.dest.id)?;
    if found != expected {
        bail!(
            "verify failed: {} owns {found} chats, expected {expected}",
            plan.dest.id
        );
    }
    let source_now = db::count_headers(conn, &plan.source.id)?;
    match plan.transfer {
        Transfer::Move if plan.source.id != plan.dest.id && source_now != 0 => bail!(
            "verify failed: {} still owns {source_now} chats",
            plan.source.id
        ),
        Transfer::Copy if source_now != plan.source_headers => bail!(
            "verify failed: source {} now owns {source_now} chats, expected {}",
            plan.source.id,
            plan.source_headers
        ),
        _ => {}
    }
    let mut entries = applied.entries.clone();
    let ids: Vec<(String, bool)> = match plan.transfer {
        Transfer::Move => plan
            .chats
            .iter()
            .map(|id| (id.clone(), plan.owned.contains(id)))
            .collect(),
        Transfer::Copy => {
            entries.extend(
                applied
                    .map
                    .iter()
                    .map(|(old, new)| (Replacement::new(old, new), Boundary::Token)),
            );
            plan.chats
                .iter()
                .filter_map(|id| {
                    applied
                        .map
                        .get(id)
                        .map(|new| (new.clone(), plan.owned.contains(id)))
                })
                .collect()
        }
    };
    let leftovers = Leftovers::new(&entries);
    for (id, owned) in &ids {
        for key in db::composer_keys(conn, id)? {
            if let Some(value) = db::read_value(conn, Table::Disk, &key)?
                && let Some(text) = super::sql::text(&value)
                && leftovers.found(text)
            {
                bail!("verify failed: old path or id still present in {key}");
            }
        }
        if let Some(header) = crate::cursor::registry::load_header(conn, id)? {
            if *owned && header.workspace_id != plan.dest.id {
                bail!(
                    "verify failed: chat {id} is owned by {}",
                    header.workspace_id
                );
            }
            if leftovers.found(&header.value) {
                bail!("verify failed: old path or id still present in header {id}");
            }
        }
    }
    if plan.transfer == Transfer::Move {
        let storage = session.rt.layout.storage_json();
        if storage.exists() && leftovers.found(&fs::read_to_string(&storage)?) {
            bail!("verify failed: storage.json still references the old workspace");
        }
    }
    Ok(())
}

//! Chat-level commands: `split`, `combine`, `rm`, and `rx`.

use anyhow::{Context, Result, bail};
use rusqlite::Connection;
use serde_json::Value;
use std::collections::{BTreeMap, HashMap, HashSet};
use std::fs;
use std::path::{Path, PathBuf};

use super::db::{self, Target};
use super::fsops::{SpaceNeeds, write_atomic};
use super::index;
use super::local;
use super::session::{self, Session};
use super::{
    Kind, Report, Runtime, Workspace, discover, find_workspace, longest_target, materialize_path,
    open_global_ro, require_workspace,
};
use crate::cursor::registry::{self, load_header};
use crate::cursor::rewrite::{Boundary, Replacement, Rewriter, path_replacements};
use crate::cursor::uri::{Platform, normalize_path, uri_path};
use crate::ui;

#[derive(Debug, Clone)]
pub struct SplitSuggestion {
    pub assigned: BTreeMap<String, Vec<PathBuf>>,
    pub unassigned: Vec<String>,
    pub titles: HashMap<String, Option<String>>,
}

pub fn suggest_split(
    rt: &Runtime,
    source: &Workspace,
    targets: &[PathBuf],
) -> Result<SplitSuggestion> {
    let mut assigned: BTreeMap<String, Vec<PathBuf>> = BTreeMap::new();
    let mut unassigned = Vec::new();
    let mut titles = HashMap::new();
    let Some(conn) = open_global_ro(rt)? else {
        return Ok(SplitSuggestion {
            assigned,
            unassigned,
            titles,
        });
    };
    let headers = db::headers(&conn, &source.id)?;
    let roots: Vec<PathBuf> = targets.iter().map(|path| normalize_path(path)).collect();
    let chats: Vec<&registry::ComposerHeader> = headers
        .iter()
        .filter(|header| !header.is_subagent)
        .collect();
    let ids: Vec<String> = chats
        .iter()
        .map(|header| header.composer_id.clone())
        .collect();
    let cached = index::cached_evidence(rt, &ids);
    let mut read = Vec::new();
    let spinner = ui::spinner("Matching chats to targets", rt.quiet);
    for (done, header) in chats.iter().enumerate() {
        spinner.set_message(&format!(
            "Matching chats to targets ({}/{})",
            done + 1,
            chats.len()
        ));
        let paths = match cached.get(&header.composer_id) {
            Some((stamp, paths)) if *stamp == header.last_updated_at => paths.clone(),
            _ => {
                let paths = evidence(&conn, &header.composer_id)?;
                read.push((
                    header.composer_id.clone(),
                    header.last_updated_at,
                    paths.clone(),
                ));
                paths
            }
        };
        let mut hits = Vec::new();
        for path in &paths {
            if let Some(target) = longest_target(path, &roots)
                && !hits.contains(&target)
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
    drop(spinner);
    index::keep_evidence(rt, &source.id, &read);
    for header in &headers {
        titles.insert(header.composer_id.clone(), header.title.clone());
    }
    Ok(SplitSuggestion {
        assigned,
        unassigned,
        titles,
    })
}

/// Every local path a chat touched, deduplicated in first-seen order.
fn evidence(conn: &Connection, composer_id: &str) -> Result<Vec<PathBuf>> {
    let mut paths = Vec::new();
    let mut seen = HashSet::new();
    for raw in db::touched_paths(conn, composer_id)? {
        if let Some(path) = materialize_path(&raw)
            && seen.insert(path.clone())
        {
            paths.push(path);
        }
    }
    Ok(paths)
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Mode {
    Move,
    Copy,
}

#[derive(Debug, Clone)]
struct Resolved {
    workspace: Workspace,
    new: bool,
}

#[derive(Debug, Clone)]
struct Transfer {
    source: Workspace,
    target: Workspace,
    seeds: Vec<String>,
    mode: Mode,
}

struct Done {
    transfer: Transfer,
    ids: Vec<String>,
    map: HashMap<String, String>,
}

fn resolve_target(rt: &Runtime, spec: &str) -> Result<Resolved> {
    if let Some(existing) = find_workspace(rt, spec)? {
        if existing.kind == Kind::Remote {
            bail!("{}: remote workspaces are not supported", existing.id);
        }
        return Ok(Resolved {
            workspace: existing,
            new: false,
        });
    }
    let path = normalize_path(Path::new(spec));
    if !path.exists() {
        bail!("target does not exist: {}", path.display());
    }
    let workspace = Workspace::planned(&rt.layout, &path)?;
    if workspace.dir.exists() {
        bail!(
            "collision: {} already holds another workspace; refusing overwrite",
            workspace.dir.display()
        );
    }
    Ok(Resolved {
        workspace,
        new: true,
    })
}

fn owner(rt: &Runtime, id: &str) -> Result<Workspace> {
    if id.is_empty() || id.contains(['/', '\\']) || id == "." || id == ".." {
        bail!("invalid workspace id {id:?}");
    }
    if let Some(found) = discover(rt)?
        .into_iter()
        .find(|workspace| workspace.id == id)
    {
        return Ok(found);
    }
    Ok(Workspace {
        id: id.to_string(),
        dir: rt.layout.workspace_storage().join(id),
        kind: Kind::EmptyWindow,
        uri: None,
        path: None,
        install: rt.layout.name.clone(),
        profile: crate::cursor::install::DEFAULT.to_string(),
        destination_missing: false,
    })
}

fn global(rt: &Runtime) -> Result<Connection> {
    open_global_ro(rt)?.with_context(|| {
        format!(
            "Cursor global database not found: {}",
            rt.layout.global_db().display()
        )
    })
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
    let conn = global(rt)?;
    let owned = db::owned_ids(&conn, &source.id)?;
    let mut resolved: Vec<(PathBuf, Resolved)> = Vec::new();
    for target in targets {
        let normalized = normalize_path(target);
        let item = resolve_target(rt, &normalized.to_string_lossy())?;
        if item.workspace.id == source.id {
            bail!(
                "split target {} is the source workspace",
                normalized.display()
            );
        }
        if resolved
            .iter()
            .any(|(_, existing)| existing.workspace.id == item.workspace.id)
        {
            bail!("split target {} is listed twice", normalized.display());
        }
        resolved.push((normalized, item));
    }
    let mut per_chat: BTreeMap<&String, Vec<usize>> = BTreeMap::new();
    for (id, dests) in assignments {
        if !owned.contains(id) {
            bail!("chat {id} does not belong to {}", source.id);
        }
        for dest in dests {
            let dest = normalize_path(dest);
            let index = resolved
                .iter()
                .position(|(path, _)| path == &dest)
                .with_context(|| {
                    format!("chat {id} is assigned to unknown target {}", dest.display())
                })?;
            let slot = per_chat.entry(id).or_default();
            if !slot.contains(&index) {
                slot.push(index);
            }
        }
    }
    let mut copies: Vec<Vec<String>> = vec![Vec::new(); resolved.len()];
    let mut moves: Vec<Vec<String>> = vec![Vec::new(); resolved.len()];
    for (id, indexes) in per_chat {
        let Some((first, rest)) = indexes.split_first() else {
            continue;
        };
        if move_chats {
            moves[*first].push(id.clone());
            for index in rest {
                copies[*index].push(id.clone());
            }
        } else {
            for index in indexes {
                copies[index].push(id.clone());
            }
        }
    }
    let mut transfers = Vec::new();
    for (index, (_, item)) in resolved.iter().enumerate() {
        if !copies[index].is_empty() {
            transfers.push(Transfer {
                source: source.clone(),
                target: item.workspace.clone(),
                seeds: copies[index].clone(),
                mode: Mode::Copy,
            });
        }
    }
    for (index, (_, item)) in resolved.iter().enumerate() {
        if !moves[index].is_empty() {
            transfers.push(Transfer {
                source: source.clone(),
                target: item.workspace.clone(),
                seeds: moves[index].clone(),
                mode: Mode::Move,
            });
        }
    }
    let created: Vec<Workspace> = resolved
        .iter()
        .filter(|(_, item)| item.new)
        .map(|(_, item)| item.workspace.clone())
        .collect();
    let mut report = Report::default();
    run_transfers(rt, &conn, "split", &created, transfers, &mut report)?;
    report.applied.push(source.id);
    Ok(report)
}

pub fn combine_workspaces(
    rt: &Runtime,
    target: &Path,
    sources: &[String],
    move_chats: bool,
) -> Result<Report> {
    rt.check()?;
    let target = resolve_target(rt, &target.to_string_lossy())?;
    let conn = global(rt)?;
    let owned = db::owned_ids(&conn, &target.workspace.id)?;
    let mut report = Report::default();
    let mut seen = HashSet::new();
    let mut groups: Vec<(Workspace, Vec<String>)> = Vec::new();
    for spec in sources {
        let (source, seeds) = if let Some(workspace) = find_workspace(rt, spec)? {
            if workspace.id == target.workspace.id {
                report
                    .warnings
                    .push(format!("skipped {spec}: it is the combine target"));
                report.skipped.push(spec.clone());
                continue;
            }
            let ids = db::composer_ids_for_workspace(&conn, &workspace.id)?;
            let tops = db::top_level_ids(&conn, &ids)?;
            (workspace, tops)
        } else if let Some(header) = load_header(&conn, spec)? {
            (owner(rt, &header.workspace_id)?, vec![spec.clone()])
        } else {
            bail!("no workspace or chat matches {spec}");
        };
        let mut kept = Vec::new();
        for id in seeds {
            if owned.contains(&id) {
                report.warnings.push(format!(
                    "skipped {id}: already owned by {}",
                    target.workspace.id
                ));
                report.skipped.push(id);
                continue;
            }
            if seen.insert(id.clone()) {
                kept.push(id);
            }
        }
        if !kept.is_empty() {
            match groups
                .iter_mut()
                .find(|(existing, _)| existing.id == source.id)
            {
                Some((_, ids)) => ids.extend(kept),
                None => groups.push((source, kept)),
            }
        }
    }
    let mode = if move_chats { Mode::Move } else { Mode::Copy };
    let transfers: Vec<Transfer> = groups
        .into_iter()
        .map(|(source, seeds)| Transfer {
            source,
            target: target.workspace.clone(),
            seeds,
            mode,
        })
        .collect();
    let created = if target.new {
        vec![target.workspace.clone()]
    } else {
        Vec::new()
    };
    run_transfers(rt, &conn, "combine", &created, transfers, &mut report)?;
    report.applied.push(target.workspace.id);
    Ok(report)
}

fn run_transfers(
    rt: &Runtime,
    conn: &Connection,
    label: &str,
    created: &[Workspace],
    transfers: Vec<Transfer>,
    report: &mut Report,
) -> Result<()> {
    let mut needs = SpaceNeeds::default();
    let mut planned = Vec::new();
    for transfer in &transfers {
        let ids = db::expand_chat_ids(conn, &transfer.seeds)?;
        let bytes = db::chat_bytes(conn, &ids)?;
        needs.add(
            &rt.layout.global_storage(),
            bytes.saturating_mul(2),
            "database log",
        );
        if transfer.mode == Mode::Move {
            needs.add(&rt.layout.backup_root(), bytes, "undo log");
        }
        planned.push(ids.len());
    }
    needs.check()?;
    if rt.dry_run {
        for workspace in created.iter().filter(|_| !transfers.is_empty()) {
            report.applied.push(format!(
                "dry-run create workspace {} for {}",
                workspace.id,
                workspace
                    .path
                    .as_ref()
                    .map(|path| path.display().to_string())
                    .unwrap_or_default()
            ));
        }
        for (transfer, count) in transfers.iter().zip(planned) {
            report.applied.push(format!(
                "dry-run {} {count} chats {} -> {}",
                match transfer.mode {
                    Mode::Move => "move",
                    Mode::Copy => "copy",
                },
                transfer.source.id,
                transfer.target.id
            ));
        }
        return Ok(());
    }
    if transfers.is_empty() {
        return Ok(());
    }
    let done = session::run(
        rt,
        label,
        |session| {
            for workspace in created {
                session.step()?;
                create_workspace(session, workspace)?;
            }
            let mut done = Vec::new();
            for transfer in &transfers {
                session.step()?;
                done.push(apply_transfer(session, transfer)?);
            }
            Ok(done)
        },
        |session, done| {
            for workspace in created {
                verify_workspace_json(workspace)?;
            }
            for item in done {
                verify_transfer(session, item)?;
            }
            Ok(())
        },
    )?;
    let mut copied = false;
    for item in done {
        match item.transfer.mode {
            Mode::Move => report.applied.push(format!(
                "moved {} chats {} -> {}",
                item.ids.len(),
                item.transfer.source.id,
                item.transfer.target.id
            )),
            Mode::Copy => {
                copied = true;
                report.applied.push(format!(
                    "copied {} chats {} -> {}",
                    item.map.len(),
                    item.transfer.source.id,
                    item.transfer.target.id
                ));
            }
        }
    }
    if copied {
        report
            .warnings
            .push("copied chats continue from local state only".to_string());
    }
    Ok(())
}

fn create_workspace(session: &mut Session<'_>, workspace: &Workspace) -> Result<()> {
    session.journal.created(&workspace.dir)?;
    fs::create_dir_all(&workspace.dir)?;
    write_atomic(
        &workspace.dir.join("workspace.json"),
        serde_json::to_string_pretty(&workspace.workspace_json()?)?.as_bytes(),
    )?;
    local::create_local_db(&workspace.dir)
}

fn verify_workspace_json(workspace: &Workspace) -> Result<()> {
    let path = workspace.dir.join("workspace.json");
    let raw = fs::read_to_string(&path)
        .with_context(|| format!("verify failed: missing {}", path.display()))?;
    if serde_json::from_str::<Value>(&raw)? != workspace.workspace_json()? {
        bail!("verify failed: {} has the wrong target", path.display());
    }
    Ok(())
}

fn apply_transfer(session: &mut Session<'_>, transfer: &Transfer) -> Result<Done> {
    let identity = transfer.target.identity();
    let target = Target {
        id: &transfer.target.id,
        identity: &identity,
    };
    let ids = db::expand_chat_ids(session.conn()?, &transfer.seeds)?;
    let projects = session.rt.layout.projects_dir.clone();
    let slugs = transfer.source.slug().zip(transfer.target.slug());
    let map = match transfer.mode {
        Mode::Move => {
            session.db()?.reassign_chats(&ids, &target)?;
            local::transfer_selection(
                &mut session.journal,
                &transfer.source.dir,
                &transfer.target.dir,
                &ids,
                None,
                true,
            )?;
            if let Some((from, to)) = &slugs {
                local::transfer_transcripts(&mut session.journal, &projects, from, to, &ids, None)?;
            }
            HashMap::new()
        }
        Mode::Copy => {
            let map = session.db()?.clone_chats(&ids, &target, &[])?;
            local::transfer_selection(
                &mut session.journal,
                &transfer.source.dir,
                &transfer.target.dir,
                &ids,
                Some(&map),
                false,
            )?;
            if let Some((from, to)) = &slugs {
                local::transfer_transcripts(
                    &mut session.journal,
                    &projects,
                    from,
                    to,
                    &ids,
                    Some(&map),
                )?;
            }
            map
        }
    };
    Ok(Done {
        transfer: transfer.clone(),
        ids,
        map,
    })
}

fn verify_transfer(session: &mut Session<'_>, done: &Done) -> Result<()> {
    let conn = session.conn()?;
    let target = &done.transfer.target.id;
    let check_owner = |id: &str| -> Result<()> {
        if let Some(header) = load_header(conn, id)?
            && &header.workspace_id != target
        {
            bail!(
                "verify failed: chat {id} is owned by {}",
                header.workspace_id
            );
        }
        Ok(())
    };
    match done.transfer.mode {
        Mode::Move => {
            for id in &done.ids {
                check_owner(id)?;
            }
        }
        Mode::Copy => {
            for id in &done.ids {
                let new = done
                    .map
                    .get(id)
                    .with_context(|| format!("verify failed: chat {id} was not copied"))?;
                if load_header(conn, id)?.is_some() {
                    if load_header(conn, new)?.is_none() {
                        bail!("verify failed: copy of {id} has no header");
                    }
                    check_owner(new)?;
                }
            }
        }
    }
    Ok(())
}

enum Removal {
    Workspace(Workspace),
    Chat {
        id: String,
        owner: Option<Workspace>,
    },
}

pub fn remove_targets(rt: &Runtime, targets: &[String]) -> Result<Report> {
    rt.check()?;
    if targets.is_empty() {
        bail!("nothing selected to remove");
    }
    let conn = open_global_ro(rt)?;
    let mut removals = Vec::new();
    let mut needs = SpaceNeeds::default();
    for target in targets {
        if let Some(workspace) = find_workspace(rt, target)? {
            if let Some(conn) = &conn {
                let ids = db::expand_chat_ids(
                    conn,
                    &db::composer_ids_for_workspace(conn, &workspace.id)?,
                )?;
                let bytes = db::chat_bytes(conn, &ids)? + db::workspace_bytes(conn, &workspace.id)?;
                needs.add(&rt.layout.backup_root(), bytes, "undo log");
                needs.add(&rt.layout.global_storage(), bytes, "database log");
            }
            removals.push(Removal::Workspace(workspace));
            continue;
        }
        let Some(conn) = &conn else {
            bail!("no workspace matches {target}");
        };
        let header = load_header(conn, target)?;
        let exists = header.is_some()
            || db::read_value(
                conn,
                super::journal::Table::Disk,
                &format!("composerData:{target}"),
            )?
            .is_some();
        if !exists {
            bail!("no workspace or chat matches {target}");
        }
        let owner = match header.map(|header| header.workspace_id) {
            Some(id) if !id.is_empty() => Some(owner(rt, &id)?),
            _ => None,
        };
        let ids = db::expand_chat_ids(conn, std::slice::from_ref(target))?;
        let bytes = db::chat_bytes(conn, &ids)?;
        needs.add(&rt.layout.backup_root(), bytes, "undo log");
        needs.add(&rt.layout.global_storage(), bytes, "database log");
        removals.push(Removal::Chat {
            id: target.clone(),
            owner,
        });
    }
    needs.check()?;
    let mut report = Report::default();
    if rt.dry_run {
        for removal in &removals {
            report.applied.push(match removal {
                Removal::Workspace(workspace) => {
                    format!("dry-run remove workspace {}", workspace.id)
                }
                Removal::Chat { id, .. } => format!("dry-run remove chat {id}"),
            });
        }
        return Ok(report);
    }
    let removed = session::run(
        rt,
        "rm",
        |session| {
            let mut removed: Vec<(String, Vec<String>, Option<PathBuf>)> = Vec::new();
            for removal in &removals {
                session.step()?;
                match removal {
                    Removal::Workspace(workspace) => {
                        let mut ids = Vec::new();
                        if session.has_global() {
                            let conn = session.conn()?;
                            ids = db::expand_chat_ids(
                                conn,
                                &db::composer_ids_for_workspace(conn, &workspace.id)?,
                            )?;
                            let mut writer = session.db()?;
                            writer.delete_chats(&ids)?;
                            writer.delete_workspace_rows(&workspace.id)?;
                        }
                        if let Some(slug) = workspace.slug() {
                            let projects = session.rt.layout.projects_dir.clone();
                            local::remove_transcripts(
                                &mut session.journal,
                                &projects,
                                &slug,
                                &ids,
                            )?;
                        }
                        session.journal.stash(&workspace.dir)?;
                        removed.push((workspace.id.clone(), ids, Some(workspace.dir.clone())));
                    }
                    Removal::Chat { id, owner } => {
                        let ids = db::expand_chat_ids(session.conn()?, std::slice::from_ref(id))?;
                        session.db()?.delete_chats(&ids)?;
                        if let Some(owner) = owner {
                            local::forget_selection(&mut session.journal, &owner.dir, &ids)?;
                            if let Some(slug) = owner.slug() {
                                let projects = session.rt.layout.projects_dir.clone();
                                local::remove_transcripts(
                                    &mut session.journal,
                                    &projects,
                                    &slug,
                                    &ids,
                                )?;
                            }
                        }
                        removed.push((id.clone(), ids, None));
                    }
                }
            }
            Ok(removed)
        },
        |session, removed| {
            for (_, ids, dir) in removed {
                if let Some(dir) = dir
                    && dir.exists()
                {
                    bail!("verify failed: {} still exists", dir.display());
                }
                if session.has_global() {
                    let conn = session.conn()?;
                    for id in ids {
                        if load_header(conn, id)?.is_some()
                            || !db::composer_keys(conn, id)?.is_empty()
                        {
                            bail!("verify failed: chat {id} still has rows");
                        }
                    }
                }
            }
            Ok(())
        },
    )?;
    for (target, ids, _) in removed {
        report
            .applied
            .push(format!("removed {target} ({} chats)", ids.len()));
    }
    Ok(report)
}

fn identifier_path(identifier: &Value) -> Option<PathBuf> {
    let components = identifier
        .get("uri")
        .or_else(|| identifier.get("configPath"))?;
    if let Some(external) = components.get("external").and_then(|v| v.as_str()) {
        return uri_path(external).map(|path| normalize_path(&path));
    }
    if let Some(text) = components.as_str() {
        return uri_path(text).map(|path| normalize_path(&path));
    }
    components
        .get("fsPath")
        .and_then(|v| v.as_str())
        .map(|path| normalize_path(Path::new(path)))
}

struct Fix {
    id: String,
    from: String,
    stale: Option<PathBuf>,
}

pub fn reindex(rt: &Runtime, target: &str) -> Result<Report> {
    rt.check()?;
    let workspace = require_workspace(rt, target)?;
    if workspace.kind == Kind::Remote {
        bail!("{}: remote workspaces are not supported", workspace.id);
    }
    let identity = workspace.identity();
    let known: HashSet<String> = discover(rt)?.into_iter().map(|found| found.id).collect();
    let mut fixes = Vec::new();
    if let Some(conn) = open_global_ro(rt)? {
        for header in registry::load_headers(&conn)? {
            let value: Value = serde_json::from_str(&header.value).unwrap_or(Value::Null);
            let stored = value
                .get("workspaceIdentifier")
                .cloned()
                .unwrap_or(Value::Null);
            let stored_path = identifier_path(&stored);
            let owned = header.workspace_id == workspace.id;
            let orphan = !owned
                && !known.contains(&header.workspace_id)
                && workspace.path.is_some()
                && stored_path == workspace.path;
            if !owned && !orphan {
                continue;
            }
            let stale = stored_path.filter(|path| Some(path) != workspace.path.as_ref());
            let data_stale = db::read_text(&conn, &format!("composerData:{}", header.composer_id))?
                .and_then(|raw| serde_json::from_str::<Value>(&raw).ok())
                .and_then(|json| json.get("workspaceIdentifier").cloned())
                .is_some_and(|found| found != identity);
            if owned && stored == identity && stale.is_none() && !data_stale {
                continue;
            }
            fixes.push(Fix {
                id: header.composer_id,
                from: header.workspace_id,
                stale,
            });
        }
    }
    if let Some(conn) = open_global_ro(rt)? {
        let ids: Vec<String> = fixes.iter().map(|fix| fix.id.clone()).collect();
        let bytes = db::chat_bytes(&conn, &db::expand_chat_ids(&conn, &ids)?)?;
        let mut needs = SpaceNeeds::default();
        needs.add(
            &rt.layout.global_storage(),
            bytes.saturating_mul(2),
            "database log",
        );
        needs.add(&rt.layout.backup_root(), bytes, "undo log");
        needs.check()?;
    }
    let caches: Vec<PathBuf> = ["Cache", "CachedData", "GPUCache", "Code Cache"]
        .iter()
        .map(|name| workspace.dir.join(name))
        .filter(|path| path.exists())
        .collect();
    let orphans: Vec<String> = {
        let mut ids: Vec<String> = fixes
            .iter()
            .filter(|fix| fix.from != workspace.id)
            .map(|fix| fix.from.clone())
            .collect();
        ids.sort();
        ids.dedup();
        ids
    };
    let mut report = Report::default();
    if rt.dry_run {
        report.applied.push(format!(
            "dry-run reindex {}: {} chats, {} orphaned workspace ids, {} caches",
            workspace.id,
            fixes.len(),
            orphans.len(),
            caches.len()
        ));
        return Ok(report);
    }
    let fixed = session::run(
        rt,
        "rx",
        |session| {
            let mut moved_transcripts: Vec<(PathBuf, Vec<String>)> = Vec::new();
            if !fixes.is_empty() {
                session.step()?;
                let target = Target {
                    id: &workspace.id,
                    identity: &identity,
                };
                let ids: Vec<String> = fixes.iter().map(|fix| fix.id.clone()).collect();
                let mut writer = session.db()?;
                writer.reassign_chats(&ids, &target)?;
                let mut by_stale: BTreeMap<(String, Option<PathBuf>), Vec<String>> =
                    BTreeMap::new();
                for fix in &fixes {
                    by_stale
                        .entry((fix.from.clone(), fix.stale.clone()))
                        .or_default()
                        .push(fix.id.clone());
                }
                for ((from, stale), chat_ids) in by_stale {
                    let mut entries = Vec::new();
                    if let (Some(old), Some(new)) = (&stale, &workspace.path) {
                        for item in path_replacements(
                            Platform::current(),
                            &old.to_string_lossy(),
                            &new.to_string_lossy(),
                        ) {
                            entries.push((item, Boundary::Path));
                        }
                    }
                    if from != workspace.id {
                        entries.push((Replacement::new(&from, &workspace.id), Boundary::Token));
                    }
                    let rewriter = Rewriter::build(entries);
                    let expanded = db::expand_chat_ids(writer.conn(), &chat_ids)?;
                    writer.rewrite_chats(&expanded, &rewriter)?;
                    if let Some(old) = &stale {
                        moved_transcripts.push((old.clone(), expanded));
                    }
                }
                for orphan in &orphans {
                    let rewriter = Rewriter::build(vec![(
                        Replacement::new(orphan, &workspace.id),
                        Boundary::Token,
                    )]);
                    writer.rekey_workspace_rows(orphan, &workspace.id, &rewriter, false)?;
                }
                if let Some(slug) = workspace.slug() {
                    let projects = session.rt.layout.projects_dir.clone();
                    for (old, ids) in &moved_transcripts {
                        let old_slug = crate::cursor::folder_id::path_to_folder_id(old);
                        if old_slug != slug {
                            session.step()?;
                            local::transfer_transcripts(
                                &mut session.journal,
                                &projects,
                                &old_slug,
                                &slug,
                                ids,
                                None,
                            )?;
                        }
                    }
                }
            }
            for cache in &caches {
                session.step()?;
                session.journal.stash(cache)?;
            }
            Ok(fixes.iter().map(|fix| fix.id.clone()).collect::<Vec<_>>())
        },
        |session, ids| {
            if ids.is_empty() {
                return Ok(());
            }
            let conn = session.conn()?;
            for id in ids {
                let header = load_header(conn, id)?
                    .with_context(|| format!("verify failed: chat {id} disappeared"))?;
                if header.workspace_id != workspace.id {
                    bail!(
                        "verify failed: chat {id} is owned by {}",
                        header.workspace_id
                    );
                }
                let value: Value = serde_json::from_str(&header.value).unwrap_or(Value::Null);
                if value.get("workspaceIdentifier") != Some(&identity) {
                    bail!("verify failed: chat {id} has a stale workspaceIdentifier");
                }
            }
            Ok(())
        },
    )?;
    report.applied.push(format!(
        "reindexed {}: {} chats, {} orphaned workspace ids, {} caches",
        workspace.id,
        fixed.len(),
        orphans.len(),
        caches.len()
    ));
    Ok(report)
}

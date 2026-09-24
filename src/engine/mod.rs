//! Shared Cursor repath engine.

mod archive;
pub mod cache;
mod chats;
mod db;
pub mod facts;
mod fsops;
pub mod index;
mod journal;
mod local;
mod repath;
mod session;
mod sql;
mod stats;
mod view;

use anyhow::{Context, Result};
use chrono::Utc;
use rusqlite::Connection;
use serde_json::{Value, json};
use std::collections::HashMap;
use std::fs;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::Instant;

use crate::cursor::install::{self, DEFAULT};
use crate::cursor::process::{self, NativeProcesses, ProcessSource};
use crate::cursor::registry::{self, ComposerHeader};
use crate::cursor::uri::{self, Platform, normalize_path, path_uri, uri_path};
use crate::cursor::workspace::{self, compute_workspace_hash, is_workspace_file};
use crate::ui::{self, Theme};

pub use crate::cursor::process::Instance;
pub use archive::{export_workspace, import_archive};
pub use chats::{
    SplitSuggestion, combine_workspaces, reindex, remove_targets, split_workspace, suggest_split,
};
pub use db::touched_paths;
pub use repath::{copy_paths, move_paths, replaced, save_unsaved};
pub use view::{ListRow, sort_rows};

/// One Cursor installation: its user data directory plus the shared crepath and
/// `~/.cursor/projects` directories every installation writes transcripts into.
#[derive(Debug, Clone)]
pub struct Layout {
    pub name: String,
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
    fn instances(&self) -> Result<Vec<Instance>>;
}

pub struct ProcessProbe<S> {
    source: S,
    roots: Vec<PathBuf>,
}

impl<S: ProcessSource> ProcessProbe<S> {
    pub fn new(source: S, roots: Vec<PathBuf>) -> Self {
        Self { source, roots }
    }
}

impl<S: ProcessSource> Probe for ProcessProbe<S> {
    fn instances(&self) -> Result<Vec<Instance>> {
        Ok(process::instances(&self.source.processes()?, &self.roots))
    }
}

pub type SystemProbe = ProcessProbe<NativeProcesses>;

pub struct FixedProbe(pub bool);

impl Probe for FixedProbe {
    fn instances(&self) -> Result<Vec<Instance>> {
        Ok(if self.0 {
            vec![Instance {
                pid: 0,
                user_data_dir: None,
                main: true,
            }]
        } else {
            Vec::new()
        })
    }
}

#[derive(Clone)]
pub struct Runtime {
    pub layout: Layout,
    pub installs: Vec<Layout>,
    pub pinned: bool,
    pub dry_run: bool,
    pub yes: bool,
    pub profile: Option<String>,
    pub probe: Arc<dyn Probe>,
    pub quiet: bool,
    pub index: Option<index::Config>,
}

impl Runtime {
    /// The same runtime with the index bypassed for reads and refreshed afterwards.
    pub fn fresh(&self) -> Runtime {
        let mut rt = self.clone();
        if let Some(config) = &mut rt.index {
            config.fresh = true;
        }
        rt
    }

    pub fn check(&self) -> Result<()> {
        let instances = self.probe.instances().map_err(|err| {
            ui::hinted(
                format!("Cannot tell whether Cursor is running: {err:#}"),
                "crepath only writes after it has confirmed that every Cursor instance is closed.",
            )
        })?;
        if instances.is_empty() {
            return Ok(());
        }
        Err(ui::hinted(
            format!("Cursor is running: {}.", self.describe(&instances)),
            "Quit every Cursor window of every profile and retry.",
        ))
    }

    fn describe(&self, instances: &[Instance]) -> String {
        let mut seen: Vec<(String, u32, bool)> = Vec::new();
        for instance in instances {
            let label = self.instance_label(instance);
            match seen.iter_mut().find(|(name, _, _)| *name == label) {
                Some(entry) => {
                    if instance.main && !entry.2 {
                        entry.1 = instance.pid;
                        entry.2 = true;
                    }
                }
                None => seen.push((label, instance.pid, instance.main)),
            }
        }
        seen.sort_by(|a, b| (a.0 != DEFAULT, &a.0).cmp(&(b.0 != DEFAULT, &b.0)));
        seen.iter()
            .map(|(label, pid, _)| format!("{label} (pid {pid})"))
            .collect::<Vec<_>>()
            .join(", ")
    }

    fn instance_label(&self, instance: &Instance) -> String {
        let Some(dir) = &instance.user_data_dir else {
            return DEFAULT.to_string();
        };
        self.installs
            .iter()
            .chain(std::iter::once(&self.layout))
            .find(|layout| &normalize_path(&layout.cursor_root) == dir)
            .map(|layout| layout.name.clone())
            .unwrap_or_else(|| ui::home_relative(&dir.display().to_string()))
    }

    pub fn scoped(&self, layout: &Layout) -> Runtime {
        let mut rt = self.clone();
        rt.layout = layout.clone();
        rt
    }

    /// Installations a read command covers: the pinned one, or every one found.
    pub fn scope(&self) -> Vec<Runtime> {
        if self.pinned || self.installs.is_empty() {
            return vec![self.clone()];
        }
        self.installs
            .iter()
            .map(|layout| self.scoped(layout))
            .collect()
    }

    /// Every other installation, pinned and unfiltered.
    pub fn siblings(&self) -> Vec<Runtime> {
        self.installs
            .iter()
            .filter(|layout| layout.name != self.layout.name)
            .map(|layout| {
                let mut rt = self.scoped(layout);
                rt.pinned = true;
                rt.profile = None;
                rt
            })
            .collect()
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Kind {
    Folder,
    CodeWorkspace,
    Unsaved,
    EmptyWindow,
    Remote,
}

impl Kind {
    pub fn label(&self) -> &'static str {
        match self {
            Self::Folder => "folder",
            Self::CodeWorkspace => "code-workspace",
            Self::Unsaved => "unsaved",
            Self::EmptyWindow => "empty-window",
            Self::Remote => "remote",
        }
    }

    pub fn from_label(label: &str) -> Option<Self> {
        [
            Self::Folder,
            Self::CodeWorkspace,
            Self::Unsaved,
            Self::EmptyWindow,
            Self::Remote,
        ]
        .into_iter()
        .find(|kind| kind.label() == label)
    }
}

#[derive(Debug, Clone)]
pub struct Workspace {
    pub id: String,
    pub dir: PathBuf,
    pub kind: Kind,
    pub uri: Option<String>,
    pub path: Option<PathBuf>,
    pub install: String,
    pub profile: String,
    pub destination_missing: bool,
}

fn uri_components(path: &Path) -> Value {
    let platform = Platform::current();
    let text = path.to_string_lossy();
    let (authority, uri_path) = uri::uri_parts(platform, &text);
    let mut components = json!({
        "$mid": 1,
        "fsPath": uri::fs_path(platform, &text),
        "external": uri::file_uri(platform, &text),
        "path": uri_path,
        "scheme": "file",
    });
    if !authority.is_empty()
        && let Some(object) = components.as_object_mut()
    {
        object.insert("authority".to_string(), Value::String(authority));
    }
    components
}

impl Workspace {
    /// `workspaceIdentifier` as Cursor stores it in chat headers.
    pub fn identity(&self) -> Value {
        match (&self.kind, &self.path) {
            (Kind::Folder, Some(path)) => json!({"id": self.id, "uri": uri_components(path)}),
            (Kind::CodeWorkspace | Kind::Unsaved, Some(path)) => {
                json!({"id": self.id, "configPath": uri_components(path)})
            }
            _ => json!({"id": self.id}),
        }
    }

    /// Target that Cursor would create for `path` (a folder or a workspace file).
    pub fn planned(layout: &Layout, path: &Path) -> Result<Self> {
        let path = normalize_path(path);
        let id = compute_workspace_hash(&path)?;
        let kind = if is_workspace_file(&path) {
            if path.starts_with(normalize_path(&layout.unsaved_root())) {
                Kind::Unsaved
            } else {
                Kind::CodeWorkspace
            }
        } else {
            Kind::Folder
        };
        Ok(Self {
            dir: layout.workspace_storage().join(&id),
            id,
            kind,
            uri: Some(path_uri(&path)),
            path: Some(path),
            install: layout.name.clone(),
            profile: DEFAULT.to_string(),
            destination_missing: false,
        })
    }

    /// Installation name, plus the VS Code profile when it is not the default one.
    pub fn profile_label(&self) -> String {
        if self.profile == DEFAULT {
            self.install.clone()
        } else {
            format!("{}/{}", self.install, self.profile)
        }
    }

    /// Directory name under `~/.cursor/projects`.
    pub fn slug(&self) -> Option<String> {
        match self.kind {
            Kind::Folder | Kind::CodeWorkspace => self
                .path
                .as_ref()
                .map(crate::cursor::folder_id::path_to_folder_id),
            Kind::Unsaved => self
                .path
                .as_ref()
                .and_then(|path| path.parent())
                .and_then(Path::file_name)
                .map(|name| name.to_string_lossy().to_string()),
            Kind::EmptyWindow => self
                .id
                .chars()
                .all(|ch| ch.is_ascii_digit())
                .then(|| self.id.clone()),
            Kind::Remote => None,
        }
    }

    pub fn workspace_json(&self) -> Result<Value> {
        let uri = self.uri.clone().context("workspace has no uri")?;
        Ok(match self.kind {
            Kind::CodeWorkspace | Kind::Unsaved => json!({ "workspace": uri }),
            _ => json!({ "folder": uri }),
        })
    }
}

#[derive(Debug, Clone, Default)]
pub struct Report {
    pub warnings: Vec<String>,
    pub skipped: Vec<String>,
    pub applied: Vec<String>,
    pub rewritten_keys: Vec<String>,
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
    Ok(render_workspaces_noted(rt, unsaved_only, detail, theme)?.0)
}

/// `ls` output, plus the "as of" note when it came from the index.
pub fn render_workspaces_noted(
    rt: &Runtime,
    unsaved_only: bool,
    detail: Option<&str>,
    theme: Theme,
) -> Result<(String, Option<index::Note>)> {
    if let Some(loaded) = index::load(rt)? {
        let text = render_loaded(&loaded, unsaved_only, detail, theme)?;
        return Ok((text, loaded.note));
    }
    let text = render_live(rt, unsaved_only, detail, theme)?;
    if rt.index.as_ref().is_some_and(|config| config.fresh) {
        index::sync(rt)?;
    }
    Ok((text, None))
}

fn matches_spec(workspace: &Workspace, id: &str, wanted: &Path) -> bool {
    workspace.id == id || workspace.path.as_deref() == Some(wanted)
}

fn render_loaded(
    loaded: &index::Loaded,
    unsaved_only: bool,
    detail: Option<&str>,
    theme: Theme,
) -> Result<String> {
    let Some(id) = detail else {
        let mut rows: Vec<ListRow> = loaded
            .installs
            .iter()
            .flat_map(|(scoped, facts)| facts.list_rows(scoped, unsaved_only))
            .collect();
        sort_rows(&mut rows);
        return Ok(view::render_list(theme, &rows));
    };
    let wanted = normalize_path(Path::new(id));
    let mut out = Vec::new();
    for (scoped, facts) in &loaded.installs {
        let Some(row) = facts
            .list_rows(scoped, unsaved_only)
            .into_iter()
            .find(|row| matches_spec(&row.workspace, id, &wanted))
        else {
            continue;
        };
        let headers = facts.headers_of(&row.workspace.id);
        out.push(view::render_detail(theme, &row, &headers));
    }
    if out.is_empty() {
        anyhow::bail!("no workspace matches {id}");
    }
    Ok(out.join("\n"))
}

fn render_live(
    rt: &Runtime,
    unsaved_only: bool,
    detail: Option<&str>,
    theme: Theme,
) -> Result<String> {
    let _spinner = ui::spinner("Scanning workspaces", rt.quiet);
    if let Some(id) = detail {
        let wanted = normalize_path(Path::new(id));
        let mut out = Vec::new();
        for scoped in rt.scope() {
            let mut workspaces = filtered_workspaces(&scoped, unsaved_only)?;
            let Some(position) = workspaces
                .iter()
                .position(|workspace| matches_spec(workspace, id, &wanted))
            else {
                continue;
            };
            let workspace = workspaces.swap_remove(position);
            let conn = open_global_ro(&scoped)?;
            let headers = match &conn {
                Some(conn) => db::headers(conn, &workspace.id)?,
                None => Vec::new(),
            };
            let row = list_row(conn.as_ref(), workspace)?;
            out.push(view::render_detail(theme, &row, &headers));
        }
        if out.is_empty() {
            anyhow::bail!("no workspace matches {id}");
        }
        return Ok(out.join("\n"));
    }
    Ok(view::render_list(theme, &list_rows(rt, unsaved_only)?))
}

pub fn list_rows(rt: &Runtime, unsaved_only: bool) -> Result<Vec<ListRow>> {
    let mut rows = Vec::new();
    for scoped in rt.scope() {
        let conn = open_global_ro(&scoped)?;
        for workspace in filtered_workspaces(&scoped, unsaved_only)? {
            rows.push(list_row(conn.as_ref(), workspace)?);
        }
    }
    sort_rows(&mut rows);
    Ok(rows)
}

/// Installations in scope that hold `spec` as a workspace id, path, unsaved id, or chat id.
pub fn locate(rt: &Runtime, spec: &str) -> Result<Vec<Layout>> {
    let mut found = Vec::new();
    for scoped in rt.scope() {
        let hit = find_workspace(&scoped, spec)?.is_some()
            || match open_global_ro(&scoped)? {
                Some(conn) => {
                    registry::load_header(&conn, spec)?.is_some()
                        || db::read_value(
                            &conn,
                            journal::Table::Disk,
                            &format!("composerData:{spec}"),
                        )?
                        .is_some()
                }
                None => false,
            };
        if hit {
            found.push(scoped.layout);
        }
    }
    Ok(found)
}

/// Other installations with a workspace that writes into `~/.cursor/projects/<slug>`.
pub fn slug_users(rt: &Runtime, slug: &str) -> Result<Vec<String>> {
    let mut names = Vec::new();
    for sibling in rt.siblings() {
        if discover(&sibling)?
            .iter()
            .any(|workspace| workspace.slug().as_deref() == Some(slug))
        {
            names.push(sibling.layout.name);
        }
    }
    Ok(names)
}

/// Other installations with a workspace on `path`.
pub fn path_users(rt: &Runtime, path: &Path) -> Result<Vec<String>> {
    let mut names = Vec::new();
    for sibling in rt.siblings() {
        if discover(&sibling)?
            .iter()
            .any(|workspace| workspace.path.as_deref() == Some(path))
        {
            names.push(sibling.layout.name);
        }
    }
    Ok(names)
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

fn list_row(conn: Option<&Connection>, workspace: Workspace) -> Result<ListRow> {
    let headers = match conn {
        Some(conn) => db::headers(conn, &workspace.id)?,
        None => Vec::new(),
    };
    let subagents = headers.iter().filter(|header| header.is_subagent).count();
    let archived = headers.iter().filter(|header| header.is_archived).count();
    let size = fsops::dir_size(&workspace.dir).unwrap_or(0);
    Ok(ListRow {
        chats: headers.len().saturating_sub(subagents),
        subagents,
        archived,
        size,
        workspace,
    })
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
    Ok(render_stats_noted(rt, theme)?.0)
}

/// `stats` output, plus the "as of" note when it came from the index.
pub fn render_stats_noted(rt: &Runtime, theme: Theme) -> Result<(String, Option<index::Note>)> {
    if let Some(loaded) = index::load(rt)? {
        let text = stats::render_stats(theme, &stats::gather(&loaded.installs));
        return Ok((text, loaded.note));
    }
    let text = {
        let _spinner = ui::spinner("Reading chat usage", rt.quiet);
        stats::render_stats(theme, &stats::collect(rt)?)
    };
    if rt.index.as_ref().is_some_and(|config| config.fresh) {
        index::sync(rt)?;
    }
    Ok((text, None))
}

pub fn record_history(
    rt: &Runtime,
    command: &str,
    args: &[String],
    outcome: &str,
    started: Instant,
) {
    if rt.dry_run {
        return;
    }
    if let Err(err) = write_history(rt, command, args, outcome, started) {
        ui::warn(&format!("history was not written: {err}"));
    }
}

pub fn discover(rt: &Runtime) -> Result<Vec<Workspace>> {
    let mut found = Vec::new();
    let root = rt.layout.workspace_storage();
    if root.exists() {
        let profiles = profile_map_of(&install::read_storage(&rt.layout.storage_json())?);
        for entry in fs::read_dir(&root)?.flatten() {
            if !entry.file_type()?.is_dir() {
                continue;
            }
            found.push(read_workspace(
                &rt.layout,
                entry.path(),
                entry.file_name().to_string_lossy().to_string(),
                &profiles,
            )?);
        }
    }
    found.sort_by(|a, b| a.id.cmp(&b.id));
    Ok(found)
}

/// Workspaces for a picker: the index when it is current, otherwise a live scan.
pub fn picker_workspaces(rt: &Runtime) -> Result<Vec<Workspace>> {
    match index::workspaces(rt) {
        Some(found) => Ok(found),
        None => discover(rt),
    }
}

/// One `workspaceStorage/<id>` directory as `discover` reports it.
pub fn read_workspace(
    layout: &Layout,
    dir: PathBuf,
    id: String,
    profiles: &HashMap<String, String>,
) -> Result<Workspace> {
    let unsaved_root = normalize_path(&layout.unsaved_root());
    let uri = workspace::read_workspace_target_uri(&dir)?;
    let path = uri.as_deref().and_then(|uri| {
        uri::parse_file_uri(Platform::current(), uri)
            .map(|path| normalize_path(Path::new(&path)))
            .or_else(|| uri_path(uri))
    });
    let kind = classify(&dir, uri.as_deref(), path.as_deref(), &unsaved_root);
    let profile = uri
        .as_ref()
        .and_then(|value| profiles.get(value).cloned())
        .unwrap_or_else(|| DEFAULT.to_string());
    let destination_missing = destination_missing(&kind, path.as_deref());
    Ok(Workspace {
        id,
        dir,
        kind,
        uri,
        path,
        install: layout.name.clone(),
        profile,
        destination_missing,
    })
}

pub fn destination_missing(kind: &Kind, path: Option<&Path>) -> bool {
    match kind {
        Kind::Folder | Kind::CodeWorkspace => path.is_none_or(|path| !path.exists()),
        Kind::Unsaved | Kind::EmptyWindow | Kind::Remote => false,
    }
}

/// VS Code user data profiles of the runtime's installation.
pub fn user_profiles(rt: &Runtime) -> Result<Vec<install::UserProfile>> {
    Ok(install::user_profiles(&install::read_storage(
        &rt.layout.storage_json(),
    )?))
}

/// Workspace URI to VS Code profile name, from `storage.json`.
pub fn profile_map_of(json: &Value) -> HashMap<String, String> {
    let profiles = install::user_profiles(json);
    let mut map = HashMap::new();
    if let Some(workspaces) = json
        .pointer("/profileAssociations/workspaces")
        .and_then(|v| v.as_object())
    {
        for (uri, id) in workspaces {
            let id = id.as_str().unwrap_or(install::DEFAULT_PROFILE);
            map.insert(uri.clone(), install::profile_name(&profiles, id));
        }
    }
    map
}

fn classify(dir: &Path, uri: Option<&str>, path: Option<&Path>, unsaved_root: &Path) -> Kind {
    let Some(uri) = uri else {
        return Kind::EmptyWindow;
    };
    if !uri
        .get(..7)
        .is_some_and(|scheme| scheme.eq_ignore_ascii_case("file://"))
    {
        return Kind::Remote;
    }
    let Some(path) = path else {
        return Kind::EmptyWindow;
    };
    let is_workspace_key = fs::read_to_string(dir.join("workspace.json"))
        .ok()
        .and_then(|raw| serde_json::from_str::<Value>(&raw).ok())
        .is_some_and(|json| json.get("folder").is_none() && json.get("workspace").is_some());
    if is_workspace_key || is_workspace_file(path) {
        if path.starts_with(unsaved_root) {
            return Kind::Unsaved;
        }
        return Kind::CodeWorkspace;
    }
    Kind::Folder
}

fn is_unsaved_id(workspace: &Workspace, id: &str) -> bool {
    if workspace.kind != Kind::Unsaved {
        return false;
    }
    if workspace.id == id {
        return true;
    }
    workspace.path.as_ref().is_some_and(|path| {
        path.parent()
            .and_then(Path::file_name)
            .is_some_and(|name| name.to_string_lossy() == id)
    })
}

pub fn find_workspace(rt: &Runtime, id: &str) -> Result<Option<Workspace>> {
    let wanted = normalize_path(Path::new(id));
    Ok(discover(rt)?.into_iter().find(|workspace| {
        workspace.id == id
            || workspace
                .path
                .as_ref()
                .is_some_and(|found| found == &wanted)
            || is_unsaved_id(workspace, id)
    }))
}

fn require_workspace(rt: &Runtime, id: &str) -> Result<Workspace> {
    find_workspace(rt, id)
        .and_then(|found| found.with_context(|| format!("no workspace matches {id}")))
}

/// Read-only global database, or `None` when Cursor has not created one.
fn open_global_ro(rt: &Runtime) -> Result<Option<Connection>> {
    let path = rt.layout.global_db();
    if !path.exists() {
        return Ok(None);
    }
    Ok(Some(sql::open_ro(&path)?))
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

fn materialize_path(raw: &str) -> Option<PathBuf> {
    if raw.contains("://") {
        return uri_path(raw);
    }
    let decoded = percent_encoding::percent_decode_str(raw)
        .decode_utf8()
        .ok()?;
    if decoded.starts_with('/') || decoded.contains(":\\") {
        Some(normalize_path(Path::new(decoded.as_ref())))
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

pub fn load_registry(conn: &Connection) -> Result<Vec<ComposerHeader>> {
    registry::load_headers(conn)
}

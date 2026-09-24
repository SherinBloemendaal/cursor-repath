//! Shared Cursor repath engine.

mod archive;
mod chats;
mod db;
mod fsops;
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

use crate::cursor::registry::{self, ComposerHeader};
use crate::cursor::uri::{self, Platform, normalize_path, path_uri, uri_path};
use crate::cursor::workspace::{self, compute_workspace_hash, is_workspace_file};
use crate::ui::{self, Theme};

pub use archive::{export_workspace, import_archive};
pub use chats::{
    SplitSuggestion, combine_workspaces, reindex, remove_targets, split_workspace, suggest_split,
};
pub use db::touched_paths;
pub use repath::{copy_paths, move_paths, save_unsaved};
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
            profile: "default".to_string(),
            destination_missing: false,
        })
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
    let _spinner = ui::spinner("Scanning workspaces", rt.quiet);
    let mut workspaces = filtered_workspaces(rt, unsaved_only)?;
    let conn = open_global_ro(rt)?;
    if let Some(id) = detail {
        let wanted = normalize_path(Path::new(id));
        let index = workspaces
            .iter()
            .position(|workspace| {
                workspace.id == id || workspace.path.as_ref().is_some_and(|path| path == &wanted)
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
        let profiles = profile_map(rt)?;
        let unsaved_root = normalize_path(&rt.layout.unsaved_root());
        for entry in fs::read_dir(&root)?.flatten() {
            if !entry.file_type()?.is_dir() {
                continue;
            }
            let dir = entry.path();
            let id = entry.file_name().to_string_lossy().to_string();
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
                .unwrap_or_else(|| "default".to_string());
            let destination_missing = match kind {
                Kind::Folder | Kind::CodeWorkspace => {
                    path.as_ref().is_none_or(|path| !path.exists())
                }
                Kind::Unsaved | Kind::EmptyWindow | Kind::Remote => false,
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

//! Incremental refresh: re-read only the Cursor files whose size or mtime moved.

use anyhow::Result;
use std::collections::{HashMap, HashSet};
use std::fs;
use std::path::{Path, PathBuf};
use std::time::{Instant, UNIX_EPOCH};

use super::now_ms;
use super::store::{Index, Prior, Scan, Source, Stamp, Update};
use crate::cursor::install::{self, DEFAULT, UserProfile};
use crate::cursor::registry::{ComposerHeader, load_headers};
use crate::engine::facts::{ChatFacts, WorkspaceFacts, file_len, read_usage};
use crate::engine::{
    Layout, Runtime, destination_missing, open_global_ro, profile_map_of, read_workspace,
};
use crate::ui::{self, DualProgress};

const GLOBAL: &str = "globalStorage/state.vscdb";
const GLOBAL_WAL: &str = "globalStorage/state.vscdb-wal";
const STORAGE: &str = "globalStorage/storage.json";
const WORKSPACE_FILES: &[&str] = &["", "workspace.json", "state.vscdb", "state.vscdb-wal"];

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Outcome {
    pub install: String,
    pub workspaces: usize,
    pub chats: usize,
    pub subagents: usize,
    pub changed: usize,
    pub chats_read: usize,
    pub duration_ms: u64,
}

#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct Staleness {
    pub sources: usize,
    pub reasons: Vec<String>,
}

pub fn stamp(path: &Path) -> Stamp {
    match fs::metadata(path) {
        Ok(meta) => Stamp {
            size: i64::try_from(meta.len()).unwrap_or(i64::MAX),
            mtime: meta
                .modified()
                .ok()
                .and_then(|time| time.duration_since(UNIX_EPOCH).ok())
                .map_or(0, |since| {
                    i64::try_from(since.as_nanos()).unwrap_or(i64::MAX)
                }),
        },
        Err(_) => Stamp { size: -1, mtime: 0 },
    }
}

fn wal(path: &Path) -> PathBuf {
    let mut name = path.file_name().unwrap_or_default().to_os_string();
    name.push("-wal");
    path.with_file_name(name)
}

fn globals(layout: &Layout) -> [(&'static str, PathBuf); 2] {
    [
        (GLOBAL, layout.global_db()),
        (GLOBAL_WAL, wal(&layout.global_db())),
    ]
}

fn workspace_key(id: &str, file: &str) -> String {
    if file.is_empty() {
        format!("workspaceStorage/{id}")
    } else {
        format!("workspaceStorage/{id}/{file}")
    }
}

fn workspace_file(dir: &Path, file: &str) -> PathBuf {
    if file.is_empty() {
        dir.to_path_buf()
    } else {
        dir.join(file)
    }
}

fn workspace_dirs(layout: &Layout) -> Result<Vec<(String, PathBuf)>> {
    let root = layout.workspace_storage();
    if !root.exists() {
        return Ok(Vec::new());
    }
    let mut dirs = Vec::new();
    for entry in fs::read_dir(&root)?.flatten() {
        if entry.file_type()?.is_dir() {
            dirs.push((
                entry.file_name().to_string_lossy().to_string(),
                entry.path(),
            ));
        }
    }
    dirs.sort();
    Ok(dirs)
}

fn fingerprint(header: &ComposerHeader) -> String {
    let text = format!(
        "{}\u{1f}{:?}\u{1f}{:?}\u{1f}{}\u{1f}{}\u{1f}{}",
        header.workspace_id,
        header.created_at,
        header.last_updated_at,
        header.is_archived,
        header.is_subagent,
        header.value
    );
    format!("{:x}", md5::compute(text))
}

fn profile_for(map: &HashMap<String, String>, uri: Option<&str>) -> String {
    uri.and_then(|uri| map.get(uri).cloned())
        .unwrap_or_else(|| DEFAULT.to_string())
}

struct Tracker<'a> {
    old: &'a HashMap<String, Stamp>,
    sources: Vec<Source>,
    changed: usize,
}

impl<'a> Tracker<'a> {
    fn new(old: &'a HashMap<String, Stamp>) -> Self {
        Self {
            old,
            sources: Vec::new(),
            changed: 0,
        }
    }

    fn check(&mut self, key: String, workspace: Option<&str>, path: &Path) -> bool {
        let now = stamp(path);
        let differs = self.old.get(&key) != Some(&now);
        if differs {
            self.changed += 1;
        }
        self.sources.push(Source {
            key,
            workspace: workspace.map(str::to_string),
            stamp: now,
        });
        differs
    }

    fn finish(&mut self) {
        let seen: HashSet<&str> = self
            .sources
            .iter()
            .map(|source| source.key.as_str())
            .collect();
        self.changed += self
            .old
            .keys()
            .filter(|key| !seen.contains(key.as_str()))
            .count();
    }
}

/// Refresh every installation in `scopes`, one step each. `full` ignores what is stored.
pub fn refresh(
    index: &Index,
    scopes: &[Runtime],
    full: bool,
    progress: &DualProgress,
) -> Result<Vec<Outcome>> {
    let mut outcomes = Vec::new();
    for (step, rt) in scopes.iter().enumerate() {
        progress.step(
            step as u64,
            &format!(
                "{} {}",
                rt.layout.name,
                ui::home_relative(&rt.layout.cursor_root.display().to_string())
            ),
        );
        outcomes.push(refresh_one(index, rt, full, progress)?);
    }
    progress.step(scopes.len() as u64, "done");
    Ok(outcomes)
}

type Storage = (Vec<UserProfile>, HashMap<String, String>);

fn read_storage(layout: &Layout) -> Result<Storage> {
    let json = install::read_storage(&layout.storage_json())?;
    Ok((install::user_profiles(&json), profile_map_of(&json)))
}

fn refresh_one(
    index: &Index,
    rt: &Runtime,
    full: bool,
    progress: &DualProgress,
) -> Result<Outcome> {
    let started = Instant::now();
    let scanned_at = now_ms();
    let layout = &rt.layout;
    let prior = if full {
        Prior::default()
    } else {
        index.prior(layout)?
    };
    let mut tracker = Tracker::new(&prior.sources);
    let storage_changed = tracker.check(STORAGE.to_string(), None, &layout.storage_json());
    let mut storage: Option<Storage> = if storage_changed || !prior.known {
        Some(read_storage(layout)?)
    } else {
        None
    };
    let dirs = workspace_dirs(layout)?;
    let total = dirs.len() as u64;
    progress.rows(0, total, "workspaces");
    let mut workspaces = Vec::with_capacity(dirs.len());
    for (done, (id, dir)) in dirs.iter().enumerate() {
        let mut differs = false;
        for file in WORKSPACE_FILES {
            differs |= tracker.check(
                workspace_key(id, file),
                Some(id),
                &workspace_file(dir, file),
            );
        }
        let facts = match prior.workspaces.get(id) {
            Some(previous) if !differs => {
                let mut facts = previous.clone();
                if let Some((_, map)) = &storage {
                    facts.workspace.profile = profile_for(map, facts.workspace.uri.as_deref());
                }
                facts.workspace.destination_missing =
                    destination_missing(&facts.workspace.kind, facts.workspace.path.as_deref());
                facts
            }
            _ => {
                if storage.is_none() {
                    storage = Some(read_storage(layout)?);
                }
                let map = storage.as_ref().map(|(_, map)| map);
                let workspace = read_workspace(
                    layout,
                    dir.clone(),
                    id.clone(),
                    map.unwrap_or(&HashMap::new()),
                )?;
                WorkspaceFacts::read(workspace, true)
            }
        };
        workspaces.push(facts);
        progress.rows(done as u64 + 1, total, "workspaces");
    }
    let profiles = match &storage {
        Some((profiles, _)) => profiles.clone(),
        None => prior.profiles.clone(),
    };
    let [(global_key, global_path), (wal_key, wal_path)] = globals(layout);
    let db_changed = tracker.check(global_key.to_string(), None, &global_path);
    let wal_changed = tracker.check(wal_key.to_string(), None, &wal_path);
    let mut upserts = Vec::new();
    let mut removed = Vec::new();
    let mut chats_read = 0usize;
    let (chats, subagents) = if db_changed || wal_changed {
        let conn = open_global_ro(rt)?;
        let headers = match &conn {
            Some(conn) => load_headers(conn)?,
            None => Vec::new(),
        };
        let mut live = HashSet::new();
        let mut pending: Vec<&ComposerHeader> = Vec::new();
        for header in &headers {
            live.insert(header.composer_id.as_str());
            let print = fingerprint(header);
            match prior.chats.get(&header.composer_id) {
                Some((chat, old)) if *old == print && chat.usage.is_some() => {}
                Some((chat, _))
                    if chat.last_updated_at == header.last_updated_at && chat.usage.is_some() =>
                {
                    upserts.push((ChatFacts::new(header, chat.usage.clone()), print));
                }
                _ => pending.push(header),
            }
        }
        let wanted = pending.len() as u64;
        progress.rows(0, wanted, "chats");
        if let Some(conn) = &conn {
            for (done, header) in pending.into_iter().enumerate() {
                let usage = read_usage(conn, &header.composer_id)?;
                upserts.push((ChatFacts::new(header, usage), fingerprint(header)));
                chats_read += 1;
                progress.rows(done as u64 + 1, wanted, "chats");
            }
        }
        removed = prior
            .chats
            .keys()
            .filter(|id| !live.contains(id.as_str()))
            .cloned()
            .collect();
        let subagents = headers.iter().filter(|header| header.is_subagent).count();
        (headers.len() - subagents, subagents)
    } else {
        let subagents = prior
            .chats
            .values()
            .filter(|(chat, _)| chat.is_subagent)
            .count();
        (prior.chats.len() - subagents, subagents)
    };
    tracker.finish();
    let changed = tracker.changed;
    let workspace_count = workspaces.len();
    let update = Update {
        scanned_at,
        duration_ms: i64::try_from(started.elapsed().as_millis()).unwrap_or(i64::MAX),
        global_db: file_len(&layout.global_db()),
        profiles,
        workspaces,
        sources: tracker.sources,
        replace_chats: !prior.known,
        upserts,
        removed,
    };
    index.apply(layout, &update)?;
    Ok(Outcome {
        install: layout.name.clone(),
        workspaces: workspace_count,
        chats,
        subagents,
        changed,
        chats_read,
        duration_ms: u64::try_from(started.elapsed().as_millis()).unwrap_or(u64::MAX),
    })
}

/// Sources that moved since the last scan, found by stat alone.
pub fn staleness(index: &Index, layout: &Layout, scan: &Scan) -> Result<Staleness> {
    let old: HashMap<String, Source> = index
        .sources(&layout.name)?
        .into_iter()
        .map(|source| (source.key.clone(), source))
        .collect();
    let known: HashSet<&str> = old
        .values()
        .filter_map(|source| source.workspace.as_deref())
        .collect();
    let mut seen: HashSet<String> = HashSet::new();
    let mut sources = 0usize;
    let differs =
        |key: &str, path: &Path| old.get(key).map(|source| source.stamp) != Some(stamp(path));
    let mut global = false;
    for (key, path) in globals(layout) {
        seen.insert(key.to_string());
        if differs(key, &path) {
            sources += 1;
            global = true;
        }
    }
    seen.insert(STORAGE.to_string());
    let storage = differs(STORAGE, &layout.storage_json());
    sources += usize::from(storage);
    let mut changed: HashSet<String> = HashSet::new();
    let mut added = 0usize;
    for (id, dir) in workspace_dirs(layout)? {
        let present = known.contains(id.as_str());
        added += usize::from(!present);
        for file in WORKSPACE_FILES {
            let key = workspace_key(&id, file);
            if differs(&key, &workspace_file(&dir, file)) {
                sources += 1;
                if present {
                    changed.insert(id.clone());
                }
            }
            seen.insert(key);
        }
    }
    let mut gone: HashSet<&str> = HashSet::new();
    for (key, source) in &old {
        if !seen.contains(key) {
            sources += 1;
            if let Some(workspace) = &source.workspace {
                gone.insert(workspace);
            }
        }
    }
    let mut reasons = Vec::new();
    if scan.dirty_at.is_some() {
        reasons.push("changed by crepath".to_string());
    }
    if global {
        reasons.push("global db changed".to_string());
    }
    if storage {
        reasons.push("storage.json changed".to_string());
    }
    if !changed.is_empty() {
        reasons.push(ui::plural(
            changed.len(),
            "workspace changed",
            "workspaces changed",
        ));
    }
    if added > 0 {
        reasons.push(ui::plural(added, "new workspace", "new workspaces"));
    }
    if !gone.is_empty() {
        reasons.push(ui::plural(
            gone.len(),
            "workspace removed",
            "workspaces removed",
        ));
    }
    Ok(Staleness { sources, reasons })
}

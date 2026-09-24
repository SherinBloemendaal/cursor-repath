//! Per-workspace `state.vscdb` chat selection and `~/.cursor/projects/<slug>` data.

use anyhow::{Context, Result, bail};
use rusqlite::{Connection, OptionalExtension};
use serde_json::{Map, Value as Json};
use std::collections::{HashMap, HashSet};
use std::fs;
use std::path::{Path, PathBuf};

use super::fsops::{copy_tree, exists};
use super::journal::Journal;
use super::sql;
use crate::cursor::registry::{LOCAL_COMPOSER_DATA_KEY, LOCAL_PINNED_KEY, LOCAL_SCHEMA};

pub const TRANSCRIPTS: &str = "agent-transcripts";

#[derive(Debug, Default, Clone, PartialEq, Eq)]
pub struct Selection {
    pub selected: Vec<String>,
    pub focused: Vec<String>,
    pub pinned: Vec<String>,
}

impl Selection {
    fn retain(&mut self, keep: impl Fn(&str) -> bool) {
        self.selected.retain(|id| keep(id));
        self.focused.retain(|id| keep(id));
        self.pinned.retain(|id| keep(id));
    }

    fn map(&mut self, map: &HashMap<String, String>) {
        for list in [&mut self.selected, &mut self.focused, &mut self.pinned] {
            *list = list.iter().filter_map(|id| map.get(id).cloned()).collect();
        }
    }

    fn absorb(&mut self, other: &Selection) {
        for (list, extra) in [
            (&mut self.selected, &other.selected),
            (&mut self.focused, &other.focused),
            (&mut self.pinned, &other.pinned),
        ] {
            for id in extra {
                if !list.contains(id) {
                    list.push(id.clone());
                }
            }
        }
    }
}

fn string_array(value: Option<&Json>) -> Vec<String> {
    value
        .and_then(|v| v.as_array())
        .map(|items| {
            items
                .iter()
                .filter_map(|item| item.as_str().map(str::to_string))
                .collect()
        })
        .unwrap_or_default()
}

fn item(conn: &Connection, key: &str) -> Result<Option<String>> {
    if !sql::table_exists(conn, "ItemTable")? {
        return Ok(None);
    }
    let value: Option<rusqlite::types::Value> = conn
        .query_row("SELECT value FROM ItemTable WHERE key = ?1", [key], |row| {
            row.get(0)
        })
        .optional()?;
    Ok(value.as_ref().and_then(sql::text).map(str::to_string))
}

fn read_selection(conn: &Connection) -> Result<(Selection, Map<String, Json>)> {
    let data = item(conn, LOCAL_COMPOSER_DATA_KEY)?
        .and_then(|raw| serde_json::from_str::<Json>(&raw).ok())
        .and_then(|json| json.as_object().cloned())
        .unwrap_or_default();
    let pinned =
        item(conn, LOCAL_PINNED_KEY)?.and_then(|raw| serde_json::from_str::<Json>(&raw).ok());
    Ok((
        Selection {
            selected: string_array(data.get("selectedComposerIds")),
            focused: string_array(data.get("lastFocusedComposerIds")),
            pinned: string_array(pinned.as_ref()),
        },
        data,
    ))
}

pub fn selection(workspace_dir: &Path) -> Result<Selection> {
    let db = workspace_dir.join("state.vscdb");
    if !db.exists() {
        return Ok(Selection::default());
    }
    let conn = sql::open_ro(&db)?;
    Ok(read_selection(&conn)?.0)
}

fn write_selection(
    conn: &Connection,
    data: &mut Map<String, Json>,
    next: &Selection,
) -> Result<()> {
    let (current, _) = read_selection(conn)?;
    if current.selected != next.selected || current.focused != next.focused {
        data.insert(
            "selectedComposerIds".to_string(),
            Json::Array(next.selected.iter().cloned().map(Json::String).collect()),
        );
        data.insert(
            "lastFocusedComposerIds".to_string(),
            Json::Array(next.focused.iter().cloned().map(Json::String).collect()),
        );
        conn.execute(
            "INSERT OR REPLACE INTO ItemTable (key, value) VALUES (?1, ?2)",
            rusqlite::params![
                LOCAL_COMPOSER_DATA_KEY,
                Json::Object(data.clone()).to_string()
            ],
        )?;
    }
    if current.pinned != next.pinned {
        conn.execute(
            "INSERT OR REPLACE INTO ItemTable (key, value) VALUES (?1, ?2)",
            rusqlite::params![
                LOCAL_PINNED_KEY,
                Json::Array(next.pinned.iter().cloned().map(Json::String).collect()).to_string()
            ],
        )?;
    }
    Ok(())
}

fn edit(
    journal: &mut Journal,
    workspace_dir: &Path,
    change: impl FnOnce(&mut Selection),
) -> Result<()> {
    let db = workspace_dir.join("state.vscdb");
    if !workspace_dir.exists() {
        return Ok(());
    }
    let (current, _) = if db.exists() {
        read_selection(&sql::open_ro(&db)?)?
    } else {
        (Selection::default(), Map::new())
    };
    let mut next = current.clone();
    change(&mut next);
    if next == current {
        return Ok(());
    }
    journal.save_sqlite(&db)?;
    let conn = Connection::open(&db).with_context(|| format!("failed to open {}", db.display()))?;
    conn.execute_batch(LOCAL_SCHEMA)?;
    let (_, mut data) = read_selection(&conn)?;
    write_selection(&conn, &mut data, &next)
}

/// Carry the tab and pin state of `ids` from `source_dir` to `target_dir`.
pub fn transfer_selection(
    journal: &mut Journal,
    source_dir: &Path,
    target_dir: &Path,
    ids: &[String],
    map: Option<&HashMap<String, String>>,
    remove_from_source: bool,
) -> Result<()> {
    let wanted: HashSet<&str> = ids.iter().map(String::as_str).collect();
    let mut carried = selection(source_dir)?;
    carried.retain(|id| wanted.contains(id));
    if let Some(map) = map {
        carried.map(map);
    }
    if remove_from_source && source_dir != target_dir {
        edit(journal, source_dir, |state| {
            state.retain(|id| !wanted.contains(id))
        })?;
    }
    edit(journal, target_dir, |state| state.absorb(&carried))
}

/// After copying a workspace directory: point its selection at the cloned ids only.
pub fn remap_selection(
    journal: &mut Journal,
    workspace_dir: &Path,
    map: &HashMap<String, String>,
) -> Result<()> {
    edit(journal, workspace_dir, |state| state.map(map))
}

pub fn forget_selection(journal: &mut Journal, workspace_dir: &Path, ids: &[String]) -> Result<()> {
    let wanted: HashSet<&str> = ids.iter().map(String::as_str).collect();
    edit(journal, workspace_dir, |state| {
        state.retain(|id| !wanted.contains(id))
    })
}

pub fn create_local_db(dir: &Path) -> Result<()> {
    let conn = Connection::open(dir.join("state.vscdb"))?;
    conn.execute_batch(LOCAL_SCHEMA)?;
    Ok(())
}

pub fn transcript_dir(projects: &Path, slug: &str, id: &str) -> PathBuf {
    projects.join(slug).join(TRANSCRIPTS).join(id)
}

/// Move or copy per-chat transcript directories between project slugs.
pub fn transfer_transcripts(
    journal: &mut Journal,
    projects: &Path,
    from_slug: &str,
    to_slug: &str,
    ids: &[String],
    map: Option<&HashMap<String, String>>,
) -> Result<()> {
    for id in ids {
        let from = transcript_dir(projects, from_slug, id);
        if !exists(&from) {
            continue;
        }
        let dest_id = map.and_then(|map| map.get(id)).unwrap_or(id);
        let dest = transcript_dir(projects, to_slug, dest_id);
        if from == dest {
            continue;
        }
        if exists(&dest) {
            bail!("collision: transcript already exists: {}", dest.display());
        }
        match map {
            None => journal.move_path(&from, &dest)?,
            Some(_) => {
                journal.created(&dest)?;
                copy_tree(&from, &dest)?;
            }
        }
    }
    Ok(())
}

pub fn remove_transcripts(
    journal: &mut Journal,
    projects: &Path,
    slug: &str,
    ids: &[String],
) -> Result<()> {
    for id in ids {
        journal.stash(&transcript_dir(projects, slug, id))?;
    }
    Ok(())
}

fn is_dir(path: &Path) -> bool {
    fs::symlink_metadata(path).is_ok_and(|meta| meta.is_dir())
}

/// Paths under `from` that would overwrite something under `to` in a merge.
pub fn merge_conflicts(from: &Path, to: &Path, skip_transcripts: bool) -> Result<Vec<PathBuf>> {
    let mut conflicts = Vec::new();
    if !exists(from) || !exists(to) {
        return Ok(conflicts);
    }
    if !is_dir(from) || !is_dir(to) {
        conflicts.push(to.to_path_buf());
        return Ok(conflicts);
    }
    for entry in fs::read_dir(from)? {
        let entry = entry?;
        if skip_transcripts && entry.file_name() == TRANSCRIPTS {
            continue;
        }
        let dest = to.join(entry.file_name());
        if exists(&dest) {
            conflicts.extend(merge_conflicts(&entry.path(), &dest, false)?);
        }
    }
    Ok(conflicts)
}

/// Move `from` into `to`, merging directories that already exist.
pub fn merge_move(journal: &mut Journal, from: &Path, to: &Path) -> Result<()> {
    if !exists(from) {
        return Ok(());
    }
    if !exists(to) {
        return journal.move_path(from, to);
    }
    if !is_dir(from) || !is_dir(to) {
        bail!("collision: {} already exists", to.display());
    }
    let mut children: Vec<_> = fs::read_dir(from)?.collect::<Result<_, _>>()?;
    children.sort_by_key(|entry| entry.file_name());
    for entry in children {
        merge_move(journal, &entry.path(), &to.join(entry.file_name()))?;
    }
    journal.stash(from)
}

/// Copy a project slug directory; transcripts are copied only for mapped chats, renamed.
/// With `skip_existing`, entries already present are left alone and returned.
pub fn copy_project(
    journal: &mut Journal,
    from: &Path,
    to: &Path,
    map: &HashMap<String, String>,
    skip_existing: bool,
) -> Result<Vec<PathBuf>> {
    let mut skipped = Vec::new();
    if !is_dir(from) {
        return Ok(skipped);
    }
    let mut children: Vec<_> = fs::read_dir(from)?.collect::<Result<_, _>>()?;
    children.sort_by_key(|entry| entry.file_name());
    for entry in children {
        let name = entry.file_name();
        if name == TRANSCRIPTS {
            let mut chats: Vec<_> = fs::read_dir(entry.path())?.collect::<Result<_, _>>()?;
            chats.sort_by_key(|entry| entry.file_name());
            for chat in chats {
                let id = chat.file_name().to_string_lossy().to_string();
                if let Some(new) = map.get(&id) {
                    let dest = to.join(TRANSCRIPTS).join(new);
                    if skip_existing && exists(&dest) {
                        skipped.push(dest);
                        continue;
                    }
                    journal.created(&dest)?;
                    copy_tree(&chat.path(), &dest)?;
                }
            }
            continue;
        }
        merge_copy(
            journal,
            &entry.path(),
            &to.join(name),
            skip_existing,
            &mut skipped,
        )?;
    }
    Ok(skipped)
}

fn merge_copy(
    journal: &mut Journal,
    from: &Path,
    to: &Path,
    skip_existing: bool,
    skipped: &mut Vec<PathBuf>,
) -> Result<()> {
    if !exists(to) {
        journal.created(to)?;
        return copy_tree(from, to);
    }
    if !is_dir(from) || !is_dir(to) {
        if skip_existing {
            skipped.push(to.to_path_buf());
            return Ok(());
        }
        bail!("collision: {} already exists", to.display());
    }
    let mut children: Vec<_> = fs::read_dir(from)?.collect::<Result<_, _>>()?;
    children.sort_by_key(|entry| entry.file_name());
    for entry in children {
        merge_copy(
            journal,
            &entry.path(),
            &to.join(entry.file_name()),
            skip_existing,
            skipped,
        )?;
    }
    Ok(())
}

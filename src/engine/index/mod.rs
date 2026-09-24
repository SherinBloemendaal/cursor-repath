//! Persistent index of every installation in `index.db`, refreshed in the background.
//!
//! Read commands render from the index and start a detached refresh. Write commands keep
//! reading live data, then mark the installation dirty so the next read refreshes it first.

mod lock;
mod refresh;
mod spawn;
mod store;

pub use lock::{Holder, Lock, clear_stale, holder, lock_path};
pub use refresh::{Outcome, Staleness, refresh, staleness};
pub use spawn::{
    Detached, LOG_KEEP, LOG_LIMIT, REFRESH_COMMAND, Spawner, last_line, log_path, trim_log,
};
pub use store::{Counts, Evidence, Index, SCHEMA, Scan, db_path, root_text};

use anyhow::{Result, anyhow};
use chrono::{Local, TimeZone};
use std::collections::HashSet;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::{Duration, Instant};

use super::facts::InstallFacts;
use super::{Runtime, Workspace};
use crate::ui::{self, DualProgress, Theme};

pub const THROTTLE_MS: i64 = 60_000;
const WAIT: Duration = Duration::from_secs(600);

pub fn now_ms() -> i64 {
    chrono::Utc::now().timestamp_millis()
}

#[derive(Clone)]
pub struct Config {
    pub spawner: Arc<dyn Spawner>,
    pub fresh: bool,
}

impl Config {
    pub fn new(spawner: Arc<dyn Spawner>) -> Self {
        Self {
            spawner,
            fresh: false,
        }
    }

    /// Production settings, or none when `CREPATH_NO_INDEX` holds anything but `0`.
    pub fn from_env(no_index: Option<&str>) -> Option<Self> {
        match no_index {
            Some(value) if !value.is_empty() && value != "0" => None,
            _ => Some(Self::new(Arc::new(Detached))),
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Note {
    pub as_of: i64,
    pub background: bool,
}

impl Note {
    pub fn text(&self, theme: Theme) -> String {
        let when = Local
            .timestamp_millis_opt(self.as_of)
            .single()
            .map(|stamp| {
                if stamp.date_naive() == Local::now().date_naive() {
                    stamp.format("%H:%M").to_string()
                } else {
                    stamp.format("%Y-%m-%d %H:%M").to_string()
                }
            })
            .unwrap_or_else(|| "an unknown time".to_string());
        let mut text = format!("as of {when}");
        if self.background {
            text.push_str(theme.icons().sep);
            text.push_str("refreshing in background");
        }
        text
    }
}

pub struct Loaded {
    pub installs: Vec<(Runtime, InstallFacts)>,
    pub note: Option<Note>,
}

fn home(rt: &Runtime) -> &Path {
    &rt.layout.crepath_home
}

pub fn index_path(rt: &Runtime) -> PathBuf {
    db_path(home(rt))
}

fn open(rt: &Runtime) -> Option<Index> {
    match Index::open(home(rt)) {
        Ok(index) => Some(index),
        Err(err) => {
            ui::warn(&format!(
                "the index is unavailable, reading live data: {err:#}"
            ));
            None
        }
    }
}

fn current(index: &Index, scopes: &[Runtime]) -> Result<Vec<Runtime>> {
    let scans = index.scans()?;
    Ok(scopes
        .iter()
        .filter(|scoped| {
            !scans
                .get(&scoped.layout.name)
                .is_some_and(|scan| scan.current(&scoped.layout))
        })
        .cloned()
        .collect())
}

/// Wait for a running refresh to release the lock, then take it.
pub fn wait_for_lock(home: &Path, quiet: bool) -> Result<Option<Lock>> {
    let deadline = Instant::now() + WAIT;
    let mut spinner = None;
    loop {
        match Lock::acquire(home)? {
            Ok(lock) => return Ok(Some(lock)),
            Err(holder) => {
                if Instant::now() >= deadline {
                    return Ok(None);
                }
                if spinner.is_none() {
                    spinner = Some(ui::spinner(
                        &format!("Waiting for the index refresh (pid {})", holder.pid),
                        quiet,
                    ));
                }
                std::thread::sleep(Duration::from_millis(200));
            }
        }
    }
}

/// Facts of every installation in scope from the index, building the ones it lacks or that
/// crepath changed. `None` means read live: the index is off, bypassed, or unavailable.
pub fn load(rt: &Runtime) -> Result<Option<Loaded>> {
    let Some(config) = &rt.index else {
        return Ok(None);
    };
    if config.fresh {
        return Ok(None);
    }
    let Some(index) = open(rt) else {
        return Ok(None);
    };
    let scopes = rt.scope();
    let mut built: HashSet<String> = HashSet::new();
    if !current(&index, &scopes)?.is_empty() {
        let Some(lock) = wait_for_lock(home(rt), rt.quiet)? else {
            ui::warn("another index refresh is still running, reading live data");
            return Ok(None);
        };
        let pending = current(&index, &scopes)?;
        if !pending.is_empty() {
            let progress = DualProgress::new("index", pending.len() as u64, rt.quiet);
            let outcome = refresh(&index, &pending, false, &progress);
            progress.finish();
            outcome?;
            built.extend(pending.iter().map(|scoped| scoped.layout.name.clone()));
        }
        drop(lock);
    }
    let scans = index.scans()?;
    let mut installs = Vec::new();
    let mut as_of: Option<i64> = None;
    for scoped in scopes {
        let facts = index.load(&scoped.layout)?.ok_or_else(|| {
            anyhow!(
                "the index has no rows for {}; run crepath cache scan",
                scoped.layout.name
            )
        })?;
        if !built.contains(&scoped.layout.name)
            && let Some(scan) = scans.get(&scoped.layout.name)
        {
            as_of = Some(as_of.map_or(scan.scanned_at, |seen| seen.min(scan.scanned_at)));
        }
        installs.push((scoped, facts));
    }
    let note = as_of.map(|as_of| Note {
        as_of,
        background: nudge(&index, rt, false),
    });
    Ok(Some(Loaded { installs, note }))
}

/// Refresh the installations in scope now, after a `--fresh` read.
pub fn sync(rt: &Runtime) -> Result<()> {
    if rt.index.is_none() {
        return Ok(());
    }
    let Some(index) = open(rt) else {
        return Ok(());
    };
    let Some(lock) = wait_for_lock(home(rt), rt.quiet)? else {
        ui::warn("the index was not refreshed: another refresh is still running");
        return Ok(());
    };
    let scopes = rt.scope();
    let progress = DualProgress::new("index", scopes.len() as u64, rt.quiet);
    let outcome = refresh(&index, &scopes, false, &progress);
    progress.finish();
    drop(lock);
    outcome.map(|_| ())
}

/// Start a background refresh unless one runs or one finished or started within a minute.
/// `force` skips the minute. Returns whether a refresh is running now.
fn nudge(index: &Index, rt: &Runtime, force: bool) -> bool {
    let Some(config) = &rt.index else {
        return false;
    };
    if holder(home(rt)).is_some_and(|holder| holder.alive) {
        return true;
    }
    let now = now_ms();
    if !force {
        let last = ["refreshed_at", "spawned_at"]
            .iter()
            .filter_map(|key| index.meta_millis(key).ok().flatten())
            .max();
        if last.is_some_and(|last| now - last < THROTTLE_MS) {
            return false;
        }
    }
    if let Err(err) = index.set_meta("spawned_at", &now.to_string()) {
        ui::warn(&format!("background refresh did not start: {err:#}"));
        return false;
    }
    match config.spawner.spawn(home(rt)) {
        Ok(()) => true,
        Err(err) => {
            ui::warn(&format!("background refresh did not start: {err:#}"));
            false
        }
    }
}

/// Picker rows from the index when the installation is current there.
pub fn workspaces(rt: &Runtime) -> Option<Vec<Workspace>> {
    let config = rt.index.as_ref()?;
    if config.fresh {
        return None;
    }
    let index = Index::open_existing(home(rt)).ok()??;
    let scan = index.scans().ok()?.remove(&rt.layout.name)?;
    if !scan.current(&rt.layout) {
        return None;
    }
    let found = index.workspaces(&rt.layout).ok()?;
    nudge(&index, rt, false);
    Some(found.into_iter().map(|facts| facts.workspace).collect())
}

/// Split evidence cached for `ids`, keyed by the `lastUpdatedAt` it was read at.
pub fn cached_evidence(rt: &Runtime, ids: &[String]) -> Evidence {
    if rt.index.is_none() {
        return Evidence::new();
    }
    match Index::open(home(rt)).and_then(|index| index.evidence(&rt.layout.name, ids)) {
        Ok(found) => found,
        Err(err) => {
            ui::warn(&format!("cached split evidence is unavailable: {err:#}"));
            Evidence::new()
        }
    }
}

pub fn keep_evidence(
    rt: &Runtime,
    workspace_id: &str,
    rows: &[(String, Option<i64>, Vec<PathBuf>)],
) {
    if rt.index.is_none() {
        return;
    }
    let outcome = Index::open(home(rt)).and_then(|index| {
        index.store_evidence(&rt.layout.name, workspace_id, rows)?;
        nudge(&index, rt, false);
        Ok(())
    });
    if let Err(err) = outcome {
        ui::warn(&format!("split evidence was not cached: {err:#}"));
    }
}

/// After a successful write: drop the evidence of every chat it touched, mark the
/// installation dirty, and start a refresh.
pub fn after_write(rt: &Runtime, chats: &HashSet<String>) {
    if rt.index.is_none() {
        return;
    }
    let outcome = (|| -> Result<()> {
        let Some(index) = Index::open_existing(home(rt))? else {
            return Ok(());
        };
        let ids: Vec<String> = chats.iter().cloned().collect();
        index.invalidate(&rt.layout.name, &ids, now_ms())?;
        nudge(&index, rt, true);
        Ok(())
    })();
    if let Err(err) = outcome {
        ui::warn(&format!(
            "the index was not updated after this change: {err:#}"
        ));
    }
}

pub fn summary(outcome: &Outcome) -> String {
    format!(
        "{}: {}, {}, {}, {}, {}, {}",
        outcome.install,
        ui::plural(outcome.workspaces, "workspace", "workspaces"),
        ui::plural(outcome.chats, "chat", "chats"),
        ui::plural(outcome.subagents, "subagent", "subagents"),
        ui::plural(outcome.changed, "changed source", "changed sources"),
        ui::plural(outcome.chats_read, "chat read", "chats read"),
        ui::duration(outcome.duration_ms)
    )
}

/// `crepath __refresh-index`: refresh every installation and log to stdout.
pub fn background(rt: &Runtime) -> Result<()> {
    let pid = std::process::id();
    let line = |text: &str| {
        println!(
            "{} pid {pid} {text}",
            Local::now().format("%Y-%m-%d %H:%M:%S")
        );
    };
    if rt.index.is_none() {
        line("skipped: CREPATH_NO_INDEX is set");
        return Ok(());
    }
    let lock = match Lock::acquire(home(rt))? {
        Ok(lock) => lock,
        Err(holder) => {
            line(&format!(
                "skipped: a refresh is already running (pid {})",
                holder.pid
            ));
            return Ok(());
        }
    };
    let started = Instant::now();
    line("refresh started");
    let outcome = (|| -> Result<Vec<Outcome>> {
        let index = Index::open(home(rt))?;
        let scopes = rt.scope();
        let progress = DualProgress::new("index", scopes.len() as u64, true);
        let outcomes = refresh(&index, &scopes, false, &progress)?;
        if !rt.pinned {
            let keep: Vec<String> = rt
                .installs
                .iter()
                .map(|layout| layout.name.clone())
                .collect();
            index.retain(&keep)?;
        }
        Ok(outcomes)
    })();
    drop(lock);
    match outcome {
        Ok(outcomes) => {
            for outcome in &outcomes {
                line(&summary(outcome));
            }
            line(&format!(
                "refresh done in {}",
                ui::duration(u64::try_from(started.elapsed().as_millis()).unwrap_or(u64::MAX))
            ));
        }
        Err(err) => line(&format!("refresh failed: {err:#}")),
    }
    Ok(())
}

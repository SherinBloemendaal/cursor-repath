//! `index.lock`: at most one refresh per crepath home, with stale-holder recovery.

use anyhow::{Context, Result};
use serde_json::{Value, json};
use std::fs::{self, OpenOptions};
use std::io::{ErrorKind, Write};
use std::path::{Path, PathBuf};
use std::time::{Duration, SystemTime};
use sysinfo::{Pid, ProcessRefreshKind, ProcessesToUpdate, System};

use super::now_ms;

const MAX_AGE_MS: i64 = 6 * 60 * 60 * 1000;
const UNREADABLE_GRACE: Duration = Duration::from_secs(10);

pub fn lock_path(home: &Path) -> PathBuf {
    home.join("index.lock")
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Holder {
    pub pid: u32,
    pub started: i64,
    pub alive: bool,
}

fn process_alive(pid: u32, started: i64) -> bool {
    if pid == std::process::id() {
        return true;
    }
    if pid == 0 || !sysinfo::IS_SUPPORTED_SYSTEM {
        return false;
    }
    let pid = Pid::from_u32(pid);
    let mut system = System::new();
    system.refresh_processes_specifics(
        ProcessesToUpdate::Some(&[pid]),
        true,
        ProcessRefreshKind::nothing(),
    );
    system.process(pid).is_some_and(|process| {
        let born = i64::try_from(process.start_time()).unwrap_or(i64::MAX);
        born.saturating_mul(1000) <= started + 2_000
    })
}

/// Who holds the lock, if anyone. A holder whose process is gone, was replaced by another
/// process with the same pid, or held it for hours is reported as not alive.
pub fn holder(home: &Path) -> Option<Holder> {
    let path = lock_path(home);
    let raw = fs::read_to_string(&path).ok()?;
    let parsed = serde_json::from_str::<Value>(&raw).ok().and_then(|json| {
        Some((
            u32::try_from(json.get("pid")?.as_u64()?).ok()?,
            json.get("started")?.as_i64()?,
        ))
    });
    let Some((pid, started)) = parsed else {
        let young = fs::metadata(&path)
            .and_then(|meta| meta.modified())
            .ok()
            .and_then(|modified| SystemTime::now().duration_since(modified).ok())
            .is_some_and(|age| age < UNREADABLE_GRACE);
        return Some(Holder {
            pid: 0,
            started: 0,
            alive: young,
        });
    };
    let alive = now_ms() - started < MAX_AGE_MS && process_alive(pid, started);
    Some(Holder {
        pid,
        started,
        alive,
    })
}

#[derive(Debug)]
pub struct Lock {
    path: PathBuf,
    pid: u32,
}

impl Lock {
    /// Take the lock, replacing a stale one. `Err(holder)` names the live refresh.
    pub fn acquire(home: &Path) -> Result<std::result::Result<Lock, Holder>> {
        fs::create_dir_all(home).with_context(|| format!("failed to create {}", home.display()))?;
        let path = lock_path(home);
        for _ in 0..3 {
            match OpenOptions::new().write(true).create_new(true).open(&path) {
                Ok(mut file) => {
                    let pid = std::process::id();
                    file.write_all(
                        json!({"pid": pid, "started": now_ms()})
                            .to_string()
                            .as_bytes(),
                    )?;
                    file.sync_all()?;
                    return Ok(Ok(Lock { path, pid }));
                }
                Err(err) if err.kind() == ErrorKind::AlreadyExists => match holder(home) {
                    Some(holder) if holder.alive => return Ok(Err(holder)),
                    _ => remove_stale(&path)?,
                },
                Err(err) => {
                    return Err(err)
                        .with_context(|| format!("failed to create {}", path.display()));
                }
            }
        }
        Ok(Err(holder(home).unwrap_or(Holder {
            pid: 0,
            started: 0,
            alive: true,
        })))
    }
}

fn remove_stale(path: &Path) -> Result<()> {
    match fs::remove_file(path) {
        Err(err) if err.kind() != ErrorKind::NotFound => {
            Err(err).with_context(|| format!("failed to remove stale {}", path.display()))
        }
        _ => Ok(()),
    }
}

/// Remove a lock whose holder is gone. Returns whether one was removed.
pub fn clear_stale(home: &Path) -> Result<bool> {
    match holder(home) {
        Some(holder) if !holder.alive => {
            remove_stale(&lock_path(home))?;
            Ok(true)
        }
        _ => Ok(false),
    }
}

impl Drop for Lock {
    fn drop(&mut self) {
        let ours = fs::read_to_string(&self.path)
            .ok()
            .and_then(|raw| serde_json::from_str::<Value>(&raw).ok())
            .and_then(|json| json.get("pid").and_then(Value::as_u64))
            == Some(u64::from(self.pid));
        if ours {
            let _ = fs::remove_file(&self.path);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn write(home: &Path, pid: u32, started: i64) {
        fs::write(
            lock_path(home),
            json!({"pid": pid, "started": started}).to_string(),
        )
        .unwrap();
    }

    #[test]
    fn one_holder_at_a_time_and_released_on_drop() {
        let temp = tempfile::tempdir().unwrap();
        let lock = Lock::acquire(temp.path()).unwrap().unwrap();
        let holder = Lock::acquire(temp.path()).unwrap().unwrap_err();
        assert_eq!(holder.pid, std::process::id());
        assert!(holder.alive);
        drop(lock);
        assert!(!lock_path(temp.path()).exists());
        assert!(Lock::acquire(temp.path()).unwrap().is_ok());
    }

    #[test]
    fn dead_old_and_garbled_holders_are_stale() {
        let temp = tempfile::tempdir().unwrap();
        let mut child = std::process::Command::new(std::env::current_exe().unwrap())
            .arg("--list")
            .stdout(std::process::Stdio::null())
            .spawn()
            .unwrap();
        let dead = child.id();
        child.wait().unwrap();
        write(temp.path(), dead, now_ms());
        assert!(!holder(temp.path()).unwrap().alive);
        write(temp.path(), std::process::id(), now_ms() - MAX_AGE_MS - 1);
        assert!(!holder(temp.path()).unwrap().alive);
        assert!(clear_stale(temp.path()).unwrap());
        assert!(holder(temp.path()).is_none());
        fs::write(lock_path(temp.path()), "").unwrap();
        assert!(holder(temp.path()).unwrap().alive);
        write(temp.path(), dead, now_ms());
        let lock = Lock::acquire(temp.path()).unwrap().unwrap();
        assert_eq!(holder(temp.path()).unwrap().pid, std::process::id());
        drop(lock);
    }
}

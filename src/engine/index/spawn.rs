//! Starting `crepath __refresh-index` detached from the terminal, logging to `refresh.log`.

use anyhow::{Context, Result};
use std::fs::{self, OpenOptions};
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};

use super::super::fsops::write_atomic;

pub const REFRESH_COMMAND: &str = "__refresh-index";
pub const LOG_LIMIT: u64 = 256 * 1024;
pub const LOG_KEEP: usize = 64 * 1024;

pub fn log_path(home: &Path) -> PathBuf {
    home.join("refresh.log")
}

pub trait Spawner: Send + Sync {
    /// Start a background refresh of the index in `home`.
    fn spawn(&self, home: &Path) -> Result<()>;
}

pub struct Detached;

impl Spawner for Detached {
    fn spawn(&self, home: &Path) -> Result<()> {
        fs::create_dir_all(home)?;
        let log = log_path(home);
        trim_log(&log, LOG_LIMIT, LOG_KEEP)?;
        let out = OpenOptions::new()
            .create(true)
            .append(true)
            .open(&log)
            .with_context(|| format!("failed to open {}", log.display()))?;
        let err = out.try_clone()?;
        let exe = std::env::current_exe().context("cannot locate the crepath binary")?;
        let mut command = Command::new(exe);
        command
            .arg(REFRESH_COMMAND)
            .env("CREPATH_HOME", home)
            .stdin(Stdio::null())
            .stdout(out)
            .stderr(err);
        detach(&mut command);
        command
            .spawn()
            .context("failed to start the background refresh")?;
        Ok(())
    }
}

#[cfg(unix)]
fn detach(command: &mut Command) {
    use std::os::unix::process::CommandExt;
    command.process_group(0);
}

#[cfg(windows)]
fn detach(command: &mut Command) {
    use std::os::windows::process::CommandExt;
    const DETACHED_PROCESS: u32 = 0x0000_0008;
    const CREATE_NEW_PROCESS_GROUP: u32 = 0x0000_0200;
    command.creation_flags(DETACHED_PROCESS | CREATE_NEW_PROCESS_GROUP);
}

/// Keep the log under `limit` bytes by dropping its oldest whole lines.
pub fn trim_log(path: &Path, limit: u64, keep: usize) -> Result<()> {
    let Ok(meta) = fs::metadata(path) else {
        return Ok(());
    };
    if meta.len() <= limit {
        return Ok(());
    }
    let bytes = fs::read(path)?;
    let tail = &bytes[bytes.len().saturating_sub(keep)..];
    let start = tail
        .iter()
        .position(|byte| *byte == b'\n')
        .map_or(0, |index| index + 1);
    write_atomic(path, &tail[start..])
}

pub fn last_line(home: &Path) -> Option<String> {
    let raw = fs::read_to_string(log_path(home)).ok()?;
    raw.lines()
        .rev()
        .find(|line| !line.trim().is_empty())
        .map(str::to_string)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_log_keeps_its_newest_whole_lines() {
        let temp = tempfile::tempdir().unwrap();
        let path = temp.path().join("refresh.log");
        let text: String = (0..2_000).map(|index| format!("line {index}\n")).collect();
        fs::write(&path, &text).unwrap();
        trim_log(&path, 1_000, 200).unwrap();
        let kept = fs::read_to_string(&path).unwrap();
        assert!(kept.len() <= 200);
        assert!(kept.starts_with("line "));
        assert!(kept.ends_with("line 1999\n"));
        let before = kept.clone();
        trim_log(&path, 1_000, 200).unwrap();
        assert_eq!(fs::read_to_string(&path).unwrap(), before);
        assert_eq!(last_line(temp.path()).as_deref(), Some("line 1999"));
    }
}

//! Running Cursor instances, read from the native process table.

use anyhow::{Result, bail};
use std::path::{Path, PathBuf};
use sysinfo::{ProcessRefreshKind, ProcessesToUpdate, System, UpdateKind};

use super::uri::normalize_path;

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ProcessInfo {
    pub pid: u32,
    pub name: String,
    pub exe: Option<PathBuf>,
    pub args: Vec<String>,
}

pub trait ProcessSource: Send + Sync {
    fn processes(&self) -> Result<Vec<ProcessInfo>>;
}

pub struct NativeProcesses;

impl ProcessSource for NativeProcesses {
    fn processes(&self) -> Result<Vec<ProcessInfo>> {
        if !sysinfo::IS_SUPPORTED_SYSTEM {
            bail!("this platform has no supported process table");
        }
        let mut system = System::new();
        system.refresh_processes_specifics(
            ProcessesToUpdate::All,
            true,
            ProcessRefreshKind::nothing()
                .with_exe(UpdateKind::Always)
                .with_cmd(UpdateKind::Always),
        );
        let list: Vec<ProcessInfo> = system
            .processes()
            .values()
            .map(|process| ProcessInfo {
                pid: process.pid().as_u32(),
                name: process.name().to_string_lossy().to_string(),
                exe: process.exe().map(Path::to_path_buf),
                args: process
                    .cmd()
                    .iter()
                    .map(|arg| arg.to_string_lossy().to_string())
                    .collect(),
            })
            .collect();
        let own = std::process::id();
        if !list.iter().any(|process| process.pid == own) {
            bail!(
                "the process table is incomplete ({} entries, crepath itself missing)",
                list.len()
            );
        }
        Ok(list)
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Instance {
    pub pid: u32,
    pub user_data_dir: Option<PathBuf>,
    pub main: bool,
}

fn stem(name: &str) -> String {
    let lower = name.to_lowercase();
    lower.strip_suffix(".exe").unwrap_or(&lower).to_string()
}

fn cursor_name(name: &str) -> bool {
    let stem = stem(name);
    stem == "cursor" || stem.starts_with("cursor helper")
}

fn is_main(process: &ProcessInfo) -> bool {
    let named = stem(&process.name) == "cursor"
        || process
            .exe
            .as_deref()
            .and_then(file_name)
            .is_some_and(|name| stem(&name) == "cursor");
    named && !process.args.iter().any(|arg| arg.starts_with("--type="))
}

fn file_name(path: &Path) -> Option<String> {
    path.file_name()
        .map(|name| name.to_string_lossy().to_string())
}

fn in_cursor_bundle(exe: &Path) -> bool {
    exe.components().any(|part| {
        let part = part.as_os_str().to_string_lossy().to_lowercase();
        part.starts_with("cursor") && part.ends_with(".app")
    })
}

pub fn user_data_dir(args: &[String]) -> Option<PathBuf> {
    let mut iter = args.iter();
    while let Some(arg) = iter.next() {
        if let Some(value) = arg.strip_prefix("--user-data-dir=") {
            return Some(normalize_path(Path::new(value)));
        }
        if arg == "--user-data-dir" {
            return iter.next().map(|value| normalize_path(Path::new(value)));
        }
    }
    None
}

pub fn is_cursor(process: &ProcessInfo, roots: &[PathBuf]) -> bool {
    let named = cursor_name(&process.name)
        || process
            .exe
            .as_deref()
            .and_then(file_name)
            .is_some_and(|name| cursor_name(&name))
        || process
            .args
            .first()
            .and_then(|arg| file_name(Path::new(arg)))
            .is_some_and(|name| cursor_name(&name));
    named
        || process.exe.as_deref().is_some_and(in_cursor_bundle)
        || user_data_dir(&process.args).is_some_and(|dir| roots.iter().any(|root| root == &dir))
}

pub fn instances(processes: &[ProcessInfo], roots: &[PathBuf]) -> Vec<Instance> {
    let roots: Vec<PathBuf> = roots.iter().map(|root| normalize_path(root)).collect();
    let mut found: Vec<Instance> = processes
        .iter()
        .filter(|process| is_cursor(process, &roots))
        .map(|process| Instance {
            pid: process.pid,
            user_data_dir: user_data_dir(&process.args),
            main: is_main(process),
        })
        .collect();
    found.sort_by_key(|instance| instance.pid);
    found
}

#[cfg(test)]
mod tests {
    use super::*;

    fn process(pid: u32, name: &str, exe: &str, args: &[&str]) -> ProcessInfo {
        ProcessInfo {
            pid,
            name: name.to_string(),
            exe: (!exe.is_empty()).then(|| PathBuf::from(exe)),
            args: args.iter().map(|arg| arg.to_string()).collect(),
        }
    }

    #[test]
    fn matches_every_cursor_process_and_nothing_else() {
        let roots = vec![PathBuf::from("/home/u/.cursor-custom")];
        let list = [
            process(
                10,
                "Cursor",
                "/Applications/Cursor.app/Contents/MacOS/Cursor",
                &["/Applications/Cursor.app/Contents/MacOS/Cursor"],
            ),
            process(
                11,
                "Cursor Helper (Renderer)",
                "/Applications/Cursor.app/Contents/Frameworks/Cursor Helper (Renderer).app/Contents/MacOS/Cursor Helper (Renderer)",
                &[
                    "Cursor Helper (Renderer)",
                    "--type=renderer",
                    "--user-data-dir=/Users/u/.cursor-resolved",
                ],
            ),
            process(
                12,
                "cursor",
                "/usr/share/cursor/cursor",
                &["/usr/share/cursor/cursor"],
            ),
            process(
                13,
                "Cursor.exe",
                "C:\\Program Files\\Cursor\\Cursor.exe",
                &[],
            ),
            process(
                14,
                "electron",
                "/opt/build/electron",
                &["electron", "--user-data-dir", "/home/u/.cursor-custom"],
            ),
            process(
                20,
                "CursorUIViewService",
                "/System/Library/PrivateFrameworks/TextInputUIMacHelper.framework/Versions/A/XPCServices/CursorUIViewService.xpc/Contents/MacOS/CursorUIViewService",
                &[],
            ),
            process(21, "cursor-agent", "/home/u/.local/bin/cursor-agent", &[]),
            process(
                22,
                "crepath",
                "/home/u/.crepath/bin/crepath",
                &["crepath", "mv"],
            ),
            process(
                23,
                "code",
                "/usr/bin/code",
                &["code", "--user-data-dir=/home/u/.vscode-other"],
            ),
        ];
        let pids: Vec<u32> = instances(&list, &roots)
            .iter()
            .map(|instance| instance.pid)
            .collect();
        assert_eq!(pids, [10, 11, 12, 13, 14]);
    }

    #[test]
    fn reads_the_user_data_dir_flag_in_both_spellings() {
        let joined = ["x".to_string(), "--user-data-dir=/a/b".to_string()];
        let split = [
            "x".to_string(),
            "--user-data-dir".to_string(),
            "/c/d".to_string(),
        ];
        assert_eq!(
            user_data_dir(&joined),
            Some(normalize_path(Path::new("/a/b")))
        );
        assert_eq!(
            user_data_dir(&split),
            Some(normalize_path(Path::new("/c/d")))
        );
        assert_eq!(user_data_dir(&["x".to_string()]), None);
    }

    #[test]
    fn helpers_are_not_main_processes() {
        let list = [
            process(
                5,
                "Cursor",
                "/Applications/Cursor.app/Contents/MacOS/Cursor",
                &[],
            ),
            process(
                6,
                "Cursor Helper",
                "/Applications/Cursor.app/Contents/Frameworks/Cursor Helper.app/Contents/MacOS/Cursor Helper",
                &["x", "--type=gpu-process"],
            ),
            process(
                7,
                "Cursor Helper (Plugin)",
                "/Applications/Cursor.app/Contents/Frameworks/Cursor Helper (Plugin).app/Contents/MacOS/Cursor Helper (Plugin)",
                &["x", "--dns-result-order=ipv4first", "gitWorker.js"],
            ),
            process(
                8,
                "cursor",
                "/usr/share/cursor/cursor",
                &["x", "--type=zygote"],
            ),
        ];
        let main: Vec<bool> = instances(&list, &[])
            .iter()
            .map(|instance| instance.main)
            .collect();
        assert_eq!(main, [true, false, false, false]);
    }

    #[test]
    fn the_native_table_includes_this_process() {
        let list = NativeProcesses.processes().unwrap();
        assert!(list.iter().any(|process| process.pid == std::process::id()));
    }
}

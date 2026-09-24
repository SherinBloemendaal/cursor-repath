//! Workspace storage ids, ported from VS Code `src/vs/platform/workspaces/node/workspaces.ts`.
//!
//! - Workspace files (`.code-workspace`, untitled `Workspaces/<ts>/workspace.json`):
//!   `md5(originalFSPath)`, lowercased except on Linux.
//! - Folders: `md5(fsPath + salt)`. The salt is `birthtime.getTime()` on macOS,
//!   `Math.floor(birthtimeMs)` on Windows, and the inode on Linux.

use anyhow::{Context, Result};
use serde_json::Value;
use std::fs;
use std::path::Path;

pub use super::uri::Platform;
use super::uri::{self, fs_path, original_fs_path};

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct FolderStat {
    pub birth_sec: i64,
    pub birth_nsec: u32,
    pub ino: u64,
}

impl FolderStat {
    pub fn read(path: &Path) -> Result<Self> {
        let metadata =
            fs::metadata(path).with_context(|| format!("failed to stat {}", path.display()))?;
        Self::from_metadata(&metadata)
            .with_context(|| format!("failed to read birth time of {}", path.display()))
    }

    #[cfg(unix)]
    fn from_metadata(metadata: &fs::Metadata) -> Result<Self> {
        use std::os::unix::fs::MetadataExt;
        let (birth_sec, birth_nsec) = if cfg!(target_os = "linux") {
            metadata
                .created()
                .ok()
                .map(split_system_time)
                .transpose()?
                .unwrap_or((0, 0))
        } else {
            split_system_time(metadata.created()?)?
        };
        Ok(Self {
            birth_sec,
            birth_nsec,
            ino: metadata.ino(),
        })
    }

    #[cfg(windows)]
    fn from_metadata(metadata: &fs::Metadata) -> Result<Self> {
        let (birth_sec, birth_nsec) = split_system_time(metadata.created()?)?;
        Ok(Self {
            birth_sec,
            birth_nsec,
            ino: 0,
        })
    }

    fn birthtime_ms(&self) -> f64 {
        self.birth_sec as f64 * 1000.0 + f64::from(self.birth_nsec) / 1_000_000.0
    }
}

fn split_system_time(time: std::time::SystemTime) -> Result<(i64, u32)> {
    match time.duration_since(std::time::UNIX_EPOCH) {
        Ok(after) => Ok((
            i64::try_from(after.as_secs()).context("birth time out of range")?,
            after.subsec_nanos(),
        )),
        Err(before) => {
            let before = before.duration();
            let secs = i64::try_from(before.as_secs()).context("birth time out of range")?;
            if before.subsec_nanos() == 0 {
                Ok((-secs, 0))
            } else {
                Ok((-secs - 1, 1_000_000_000 - before.subsec_nanos()))
            }
        }
    }
}

fn js_round(value: f64) -> f64 {
    let floor = value.floor();
    if value - floor >= 0.5 {
        floor + 1.0
    } else {
        floor
    }
}

fn md5_hex(input: &str) -> String {
    format!("{:x}", md5::compute(input.as_bytes()))
}

/// Folder id from an already sanitized path.
pub fn folder_id(platform: Platform, path: &str, stat: &FolderStat) -> String {
    let salt = match platform {
        Platform::Linux => stat.ino as i128,
        Platform::Macos => js_round(stat.birthtime_ms()) as i128,
        Platform::Windows => stat.birthtime_ms().floor() as i128,
    };
    let salt = if salt == 0 {
        String::new()
    } else {
        salt.to_string()
    };
    md5_hex(&format!("{}{salt}", fs_path(platform, path)))
}

/// Workspace-file id from an already sanitized config path.
pub fn workspace_file_id(platform: Platform, config_path: &str) -> String {
    let original = original_fs_path(platform, config_path);
    if platform == Platform::Linux {
        md5_hex(&original)
    } else {
        md5_hex(&original.to_lowercase())
    }
}

pub fn is_workspace_file(path: &Path) -> bool {
    if path.extension().and_then(|ext| ext.to_str()) == Some("code-workspace") {
        return true;
    }
    path.file_name().and_then(|name| name.to_str()) == Some("workspace.json")
        && path
            .parent()
            .and_then(Path::parent)
            .and_then(Path::file_name)
            .and_then(|name| name.to_str())
            == Some("Workspaces")
}

/// Workspace storage id Cursor computes for `path` on this machine.
pub fn compute_workspace_hash<P: AsRef<Path>>(path: P) -> Result<String> {
    let path = uri::normalize_path(path.as_ref());
    let platform = Platform::current();
    let text = path.to_string_lossy();
    if is_workspace_file(&path) {
        return Ok(workspace_file_id(platform, &text));
    }
    let stat = FolderStat::read(&path)?;
    Ok(folder_id(platform, &text, &stat))
}

/// Folder id for `dest` when it will inherit the identity of `source` (a rename keeps
/// birth time and inode).
pub fn compute_renamed_hash(source: &Path, dest: &Path) -> Result<String> {
    let dest = uri::normalize_path(dest);
    let platform = Platform::current();
    let text = dest.to_string_lossy();
    if is_workspace_file(&dest) {
        return Ok(workspace_file_id(platform, &text));
    }
    let stat = FolderStat::read(source)?;
    Ok(folder_id(platform, &text, &stat))
}

/// Read the primary workspace target URI from a workspace storage directory.
pub fn read_workspace_target_uri(workspace_dir: &Path) -> Result<Option<String>> {
    let workspace_json = workspace_dir.join("workspace.json");
    if !workspace_json.exists() {
        return Ok(None);
    }
    let content = fs::read_to_string(&workspace_json)
        .with_context(|| format!("Failed to read: {}", workspace_json.display()))?;
    let ws: Value = serde_json::from_str(&content)
        .with_context(|| format!("Failed to parse: {}", workspace_json.display()))?;
    Ok(ws
        .get("folder")
        .and_then(|value| value.as_str())
        .or_else(|| ws.get("workspace").and_then(|value| value.as_str()))
        .map(|value| value.to_string()))
}

#[cfg(test)]
mod tests {
    use super::*;
    use tempfile::TempDir;

    fn stat(sec: i64, nsec: u32, ino: u64) -> FolderStat {
        FolderStat {
            birth_sec: sec,
            birth_nsec: nsec,
            ino,
        }
    }

    #[test]
    fn macos_folder_rounds_birthtime() {
        let path = "/Users/me/projects/app";
        let expected = md5_hex("/Users/me/projects/app1790224225264");
        assert_eq!(
            folder_id(Platform::Macos, path, &stat(1_790_224_225, 263_600_000, 9)),
            expected
        );
        assert_eq!(
            folder_id(Platform::Macos, path, &stat(1_790_224_225, 263_500_000, 9)),
            expected
        );
        assert_ne!(
            folder_id(Platform::Macos, path, &stat(1_790_224_225, 263_499_000, 9)),
            expected
        );
    }

    #[test]
    fn windows_folder_floors_birthtime_and_lowercases_drive() {
        let expected = md5_hex("c:\\Users\\Me\\app1790224225263");
        assert_eq!(
            folder_id(
                Platform::Windows,
                "C:\\Users\\Me\\app",
                &stat(1_790_224_225, 263_900_000, 0)
            ),
            expected
        );
        assert_eq!(
            folder_id(
                Platform::Windows,
                "c:\\Users\\Me\\app",
                &stat(1_790_224_225, 263_000_000, 0)
            ),
            expected
        );
    }

    #[test]
    fn linux_folder_uses_inode() {
        assert_eq!(
            folder_id(
                Platform::Linux,
                "/home/me/app",
                &stat(1_790_224_225, 263_000_000, 424_242)
            ),
            md5_hex("/home/me/app424242")
        );
        assert_eq!(
            folder_id(Platform::Linux, "/home/me/app", &stat(5, 0, 0)),
            md5_hex("/home/me/app")
        );
    }

    #[test]
    fn workspace_files_hash_the_config_path() {
        let path = "/Users/Me/Projects/Verbleif/verbleif.code-workspace";
        assert_eq!(
            workspace_file_id(Platform::Macos, path),
            md5_hex("/users/me/projects/verbleif/verbleif.code-workspace")
        );
        assert_eq!(workspace_file_id(Platform::Linux, path), md5_hex(path));
        assert_eq!(
            workspace_file_id(Platform::Windows, "C:\\Work\\App.code-workspace"),
            md5_hex("c:\\work\\app.code-workspace")
        );
        let untitled =
            "/Users/me/Library/Application Support/Cursor/Workspaces/1765558213752/workspace.json";
        assert_eq!(
            workspace_file_id(Platform::Macos, untitled),
            md5_hex(&untitled.to_lowercase())
        );
    }

    #[test]
    fn detects_workspace_files() {
        assert!(is_workspace_file(Path::new("/a/app.code-workspace")));
        assert!(is_workspace_file(Path::new(
            "/c/Cursor/Workspaces/1765558213752/workspace.json"
        )));
        assert!(!is_workspace_file(Path::new("/a/workspace.json")));
        assert!(!is_workspace_file(Path::new("/a/app")));
    }

    #[test]
    fn compute_workspace_hash_ignores_trailing_separator() {
        let temp = TempDir::new().unwrap();
        let dir = temp.path().join("proj");
        fs::create_dir(&dir).unwrap();
        let plain = compute_workspace_hash(&dir).unwrap();
        let trailing = format!("{}{}", dir.display(), std::path::MAIN_SEPARATOR);
        assert_eq!(compute_workspace_hash(Path::new(&trailing)).unwrap(), plain);
        let dotted = dir.join(".");
        assert_eq!(compute_workspace_hash(&dotted).unwrap(), plain);
    }

    #[test]
    fn compute_workspace_hash_for_code_workspace_needs_no_stat() {
        let temp = TempDir::new().unwrap();
        let file = temp.path().join("App.code-workspace");
        let expected = workspace_file_id(
            Platform::current(),
            &uri::normalize_path(&file).to_string_lossy(),
        );
        assert_eq!(compute_workspace_hash(&file).unwrap(), expected);
    }

    #[test]
    fn read_workspace_target_uri_prefers_folder_then_workspace() {
        let temp_dir = TempDir::new().unwrap();
        let workspace_dir = temp_dir.path().join("ws");
        fs::create_dir(&workspace_dir).unwrap();
        fs::write(
            workspace_dir.join("workspace.json"),
            r#"{"folder":"file:///tmp/project","workspace":"file:///tmp/project.code-workspace"}"#,
        )
        .unwrap();
        let target = read_workspace_target_uri(&workspace_dir).unwrap();
        assert_eq!(target.as_deref(), Some("file:///tmp/project"));
    }
}

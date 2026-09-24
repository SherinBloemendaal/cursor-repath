//! Filesystem primitives shared by the engine and its journal.

use anyhow::{Context, Result, bail};
use std::collections::BTreeMap;
use std::fs;
use std::path::{Path, PathBuf};

/// Copy `source` (file or directory) to `dest`, which must not exist yet.
pub fn copy_tree(source: &Path, dest: &Path) -> Result<()> {
    if fs::symlink_metadata(dest).is_ok() {
        bail!("refusing to overwrite {}", dest.display());
    }
    let meta = fs::symlink_metadata(source)
        .with_context(|| format!("failed to stat {}", source.display()))?;
    if let Some(parent) = dest.parent() {
        fs::create_dir_all(parent)?;
    }
    if !meta.is_dir() {
        return copy_entry(source, dest, &meta);
    }
    for entry in walkdir::WalkDir::new(source).follow_links(false) {
        let entry = entry?;
        let rel = entry.path().strip_prefix(source)?;
        let target = dest.join(rel);
        let meta = fs::symlink_metadata(entry.path())?;
        if meta.is_dir() {
            fs::create_dir_all(&target)
                .with_context(|| format!("failed to create {}", target.display()))?;
        } else {
            copy_entry(entry.path(), &target, &meta)?;
        }
    }
    Ok(())
}

fn copy_entry(source: &Path, dest: &Path, meta: &fs::Metadata) -> Result<()> {
    if meta.file_type().is_symlink() {
        return copy_symlink(source, dest);
    }
    fs::copy(source, dest)
        .with_context(|| format!("failed to copy {} to {}", source.display(), dest.display()))?;
    Ok(())
}

#[cfg(unix)]
fn copy_symlink(source: &Path, dest: &Path) -> Result<()> {
    let link = fs::read_link(source)?;
    std::os::unix::fs::symlink(link, dest)
        .with_context(|| format!("failed to link {}", dest.display()))
}

#[cfg(windows)]
fn copy_symlink(source: &Path, dest: &Path) -> Result<()> {
    let resolved = fs::canonicalize(source)
        .with_context(|| format!("failed to resolve {}", source.display()))?;
    if resolved.is_dir() {
        copy_tree(&resolved, dest)
    } else {
        fs::copy(&resolved, dest)
            .map(|_| ())
            .with_context(|| format!("failed to copy {}", source.display()))
    }
}

pub fn remove_path(path: &Path) -> Result<()> {
    match fs::symlink_metadata(path) {
        Err(err) if err.kind() == std::io::ErrorKind::NotFound => Ok(()),
        Err(err) => Err(err).with_context(|| format!("failed to stat {}", path.display())),
        Ok(meta) if meta.is_dir() => {
            fs::remove_dir_all(path).with_context(|| format!("failed to remove {}", path.display()))
        }
        Ok(_) => {
            fs::remove_file(path).with_context(|| format!("failed to remove {}", path.display()))
        }
    }
}

pub fn exists(path: &Path) -> bool {
    fs::symlink_metadata(path).is_ok()
}

/// Rename, falling back to copy and delete across filesystems. `copied` runs after a
/// fallback copy completes and before the source is deleted.
pub fn move_path_with(source: &Path, dest: &Path, copied: impl FnOnce()) -> Result<()> {
    if exists(dest) {
        bail!("refusing to overwrite {}", dest.display());
    }
    if let Some(parent) = dest.parent() {
        fs::create_dir_all(parent)?;
    }
    if fs::rename(source, dest).is_ok() {
        copied();
        return Ok(());
    }
    if let Err(err) = copy_tree(source, dest) {
        remove_path(dest).ok();
        return Err(err);
    }
    copied();
    remove_path(source)
}

pub fn move_path(source: &Path, dest: &Path) -> Result<()> {
    move_path_with(source, dest, || {})
}

pub fn write_atomic(path: &Path, bytes: &[u8]) -> Result<()> {
    let parent = path.parent().context("path has no parent")?;
    fs::create_dir_all(parent)?;
    let mut temp = tempfile::NamedTempFile::new_in(parent)?;
    std::io::Write::write_all(&mut temp, bytes)?;
    temp.as_file().sync_all()?;
    temp.persist(path)
        .map_err(|err| err.error)
        .with_context(|| format!("failed to write {}", path.display()))?;
    Ok(())
}

pub fn dir_size(path: &Path) -> Result<u64> {
    let mut total = 0u64;
    if !exists(path) {
        return Ok(0);
    }
    for entry in walkdir::WalkDir::new(path).follow_links(false) {
        let entry = entry?;
        if entry.file_type().is_file() {
            total += entry.metadata()?.len();
        }
    }
    Ok(total)
}

fn existing_ancestor(path: &Path) -> Option<PathBuf> {
    let mut current = Some(path);
    while let Some(candidate) = current {
        if candidate.exists() {
            return Some(candidate.to_path_buf());
        }
        current = candidate.parent();
    }
    None
}

#[cfg(unix)]
pub fn free_bytes(path: &Path) -> Option<u64> {
    let stat = rustix::fs::statvfs(path).ok()?;
    Some(stat.f_bavail.saturating_mul(stat.f_frsize))
}

#[cfg(windows)]
pub fn free_bytes(path: &Path) -> Option<u64> {
    use std::os::windows::ffi::OsStrExt;
    use windows_sys::Win32::Storage::FileSystem::GetDiskFreeSpaceExW;

    let dir = if path.is_file() { path.parent()? } else { path };
    let wide: Vec<u16> = dir
        .as_os_str()
        .encode_wide()
        .chain(std::iter::once(0))
        .collect();
    let mut available = 0u64;
    let ok = unsafe {
        GetDiskFreeSpaceExW(
            wide.as_ptr(),
            &mut available,
            std::ptr::null_mut(),
            std::ptr::null_mut(),
        )
    };
    (ok != 0).then_some(available)
}

#[cfg(unix)]
fn volume_key(path: &Path) -> Option<String> {
    use std::os::unix::fs::MetadataExt;
    Some(fs::metadata(path).ok()?.dev().to_string())
}

#[cfg(windows)]
fn volume_key(path: &Path) -> Option<String> {
    path.components()
        .next()
        .map(|component| component.as_os_str().to_string_lossy().to_ascii_lowercase())
}

/// Bytes needed per location. Locations on the same volume are summed before comparing.
#[derive(Debug, Default)]
pub struct SpaceNeeds {
    items: Vec<(PathBuf, u64, String)>,
}

impl SpaceNeeds {
    pub fn add(&mut self, location: &Path, bytes: u64, what: impl Into<String>) {
        if bytes > 0 {
            self.items
                .push((location.to_path_buf(), bytes, what.into()));
        }
    }

    pub fn check(&self) -> Result<()> {
        let mut volumes: BTreeMap<String, (PathBuf, u64, Vec<String>)> = BTreeMap::new();
        for (location, bytes, what) in &self.items {
            let Some(anchor) = existing_ancestor(location) else {
                continue;
            };
            let key = volume_key(&anchor).unwrap_or_else(|| anchor.display().to_string());
            let slot = volumes
                .entry(key)
                .or_insert_with(|| (anchor.clone(), 0, Vec::new()));
            slot.1 = slot.1.saturating_add(*bytes);
            slot.2.push(what.clone());
        }
        for (anchor, needed, what) in volumes.values() {
            if let Some(free) = free_bytes(anchor)
                && free < *needed
            {
                bail!(
                    "not enough disk space on {}: need {needed} bytes for {}, {free} free",
                    anchor.display(),
                    what.join(", ")
                );
            }
        }
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn free_bytes_reports_space_for_directory() {
        let dir = tempfile::tempdir().unwrap();
        assert!(free_bytes(dir.path()).is_some_and(|free| free > 0));
    }

    #[test]
    fn free_bytes_reports_space_for_file() {
        let dir = tempfile::tempdir().unwrap();
        let file = dir.path().join("project.code-workspace");
        fs::write(&file, "{}").unwrap();
        assert!(free_bytes(&file).is_some_and(|free| free > 0));
    }

    #[test]
    fn free_bytes_is_none_for_missing_path() {
        let dir = tempfile::tempdir().unwrap();
        assert_eq!(free_bytes(&dir.path().join("missing").join("deeper")), None);
    }

    #[test]
    fn space_check_sums_one_volume() {
        let dir = tempfile::tempdir().unwrap();
        let free = free_bytes(dir.path()).unwrap();
        let mut needs = SpaceNeeds::default();
        needs.add(dir.path(), free / 2 + (1 << 29), "a");
        needs.add(&dir.path().join("missing/child"), free / 2 + (1 << 29), "b");
        let err = needs.check().unwrap_err().to_string();
        assert!(err.contains("a, b"), "{err}");
        let mut fits = SpaceNeeds::default();
        fits.add(dir.path(), 1, "a");
        fits.check().unwrap();
    }

    #[test]
    fn copy_tree_refuses_existing_destination_instead_of_nesting() {
        let dir = tempfile::tempdir().unwrap();
        let source = dir.path().join("src");
        fs::create_dir_all(source.join("inner")).unwrap();
        fs::write(source.join("inner/file"), "x").unwrap();
        let dest = dir.path().join("dest");
        copy_tree(&source, &dest).unwrap();
        assert_eq!(fs::read_to_string(dest.join("inner/file")).unwrap(), "x");
        assert!(copy_tree(&source, &dest).is_err());
        assert!(!dest.join("src").exists());
    }
}

//! Release check, self-update, and the GitHub shortcut.

use anyhow::{Context, Result, bail};
use comfy_table::{Attribute, Color};
use flate2::read::GzDecoder;
use semver::Version;
use serde::Deserialize;
use sha2::{Digest, Sha256};
use std::cmp::Ordering;
use std::fs::{self, File};
use std::io::{self, Read};
use std::path::{Path, PathBuf};
use std::process::Command;
use std::time::{Duration, SystemTime, UNIX_EPOCH};
use tar::Archive;

use crate::ui::{self, Theme};

pub const REPO_URL: &str = "https://github.com/SherinBloemendaal/cursor-repath";
const LATEST_API: &str =
    "https://api.github.com/repos/SherinBloemendaal/cursor-repath/releases/latest";
const CHECK_INTERVAL_SECS: i64 = 24 * 60 * 60;
const CHECK_TIMEOUT: Duration = Duration::from_secs(2);
const DOWNLOAD_TIMEOUT: Duration = Duration::from_secs(120);

#[derive(Debug, Deserialize)]
struct Release {
    tag_name: String,
    assets: Vec<Asset>,
}

#[derive(Debug, Deserialize)]
struct Asset {
    name: String,
    browser_download_url: String,
}

#[derive(Debug, serde::Serialize, serde::Deserialize)]
struct UpdateCache {
    checked_at: i64,
    latest: String,
}

pub fn notify_if_outdated() {
    if update_check_disabled() {
        return;
    }
    let Ok(path) = cache_path() else {
        return;
    };
    let now = unix_now();
    let Some(latest) = resolve_latest(&path, now, env!("CARGO_PKG_VERSION"), || {
        fetch_release(CHECK_TIMEOUT)
            .ok()
            .and_then(|release| display_version(&release.tag_name).ok())
    }) else {
        return;
    };
    ui::notice(&outdated_message(env!("CARGO_PKG_VERSION"), &latest));
}

pub fn run_update() -> Result<()> {
    let theme = Theme::stdout();
    ui::section("Checking for updates");
    let spinner = ui::spinner("Asking GitHub for the latest release", false);
    let release = fetch_release(Duration::from_secs(30))?;
    drop(spinner);
    let current = env!("CARGO_PKG_VERSION");
    let latest = display_version(&release.tag_name)?;
    if let Ok(path) = cache_path() {
        let _ = write_cache(&path, unix_now(), &latest);
    }
    let icons = theme.icons();
    if compare_versions(&latest, current)? != Ordering::Greater {
        println!(
            "{}",
            ui::panel(
                theme,
                vec![
                    ("Installed", theme.cell(current, None, &[Attribute::Bold])),
                    ("Latest", theme.cell(&latest, None, &[])),
                    (
                        "Status",
                        theme.cell(
                            format!("{} up to date", icons.check),
                            Some(Color::Green),
                            &[Attribute::Bold],
                        ),
                    ),
                ],
            )
        );
        return Ok(());
    }
    ui::section(&format!(
        "Updating crepath {} {} {}",
        theme.dim(current),
        theme.arrow(),
        theme.good(theme.bold(&latest))
    ));
    let asset = current_asset_name()?;
    let archive_url = asset_url(&release, asset)?;
    let sums_url = asset_url(&release, "SHA256SUMS")?;
    let tmp = tempfile::tempdir().context("could not create a temporary directory")?;
    let archive_path = tmp.path().join(asset);
    let spinner = ui::spinner(&format!("Downloading {asset}"), false);
    download(&archive_url, &archive_path, DOWNLOAD_TIMEOUT)
        .with_context(|| format!("download failed: {archive_url}"))?;
    spinner.set_message("Downloading SHA256SUMS");
    let sums_path = tmp.path().join("SHA256SUMS");
    download(&sums_url, &sums_path, Duration::from_secs(30))
        .with_context(|| format!("download failed: {sums_url}"))?;
    drop(spinner);
    let sums = fs::read_to_string(&sums_path).context("could not read SHA256SUMS")?;
    let dest = install_binary_path()?;
    install_verified_release(&archive_path, &sums, asset, &dest)?;
    println!(
        "{}",
        ui::panel(
            theme,
            vec![
                ("Previous", theme.cell(current, None, &[Attribute::Dim])),
                (
                    "Installed",
                    theme.cell(&latest, Some(Color::Green), &[Attribute::Bold]),
                ),
                ("Asset", theme.cell(asset, Some(Color::Cyan), &[])),
                (
                    "Checksum",
                    theme.cell(
                        format!("{} SHA-256 verified", icons.check),
                        Some(Color::Green),
                        &[],
                    ),
                ),
                (
                    "Location",
                    theme.cell(dest.display(), Some(Color::Cyan), &[]),
                ),
            ],
        )
    );
    ui::ok(&format!(
        "Updated crepath to {}",
        theme.good(theme.bold(&latest))
    ));
    Ok(())
}

pub fn open_github() -> Result<()> {
    let theme = Theme::stdout();
    let Some((program, prefix)) = browser_command(std::env::consts::OS) else {
        ui::info(&theme.path(REPO_URL));
        bail!("could not open {REPO_URL}");
    };
    let mut command = Command::new(program);
    command.args(prefix);
    command.arg(REPO_URL);
    match command.status() {
        Ok(status) if status.success() => {
            ui::ok(&format!("Opened {}", theme.path(REPO_URL)));
            Ok(())
        }
        _ => {
            ui::info(&theme.path(REPO_URL));
            bail!("could not open {REPO_URL}");
        }
    }
}

pub fn browser_command(os: &str) -> Option<(&'static str, &'static [&'static str])> {
    match os {
        "macos" => Some(("open", &[])),
        "linux" => Some(("xdg-open", &[])),
        "windows" => Some(("cmd", &["/c", "start"])),
        _ => None,
    }
}

pub fn compare_versions(left: &str, right: &str) -> Result<Ordering> {
    Ok(parse_version(left)?.cmp(&parse_version(right)?))
}

pub fn asset_name_for(os: &str, arch: &str) -> Result<&'static str> {
    match (os, arch) {
        ("macos", "aarch64") => Ok("crepath-aarch64-apple-darwin.tar.gz"),
        ("macos", "x86_64") => Ok("crepath-x86_64-apple-darwin.tar.gz"),
        ("linux", "x86_64") => Ok("crepath-x86_64-unknown-linux-gnu.tar.gz"),
        ("linux", "aarch64") => Ok("crepath-aarch64-unknown-linux-gnu.tar.gz"),
        ("windows", "x86_64") => Ok("crepath-x86_64-pc-windows-msvc.zip"),
        _ => bail!("unsupported platform: {os} {arch}"),
    }
}

pub fn outdated_message(current: &str, latest: &str) -> String {
    format!("crepath {current} is outdated. Latest is {latest}. Run crepath update.")
}

pub fn resolve_latest(
    cache_path: &Path,
    now: i64,
    current: &str,
    fetch: impl FnOnce() -> Option<String>,
) -> Option<String> {
    let cached = read_cache(cache_path);
    if let Some(cache) = &cached
        && cache_is_fresh(cache, now)
    {
        return newer_version(&cache.latest, current);
    }
    if let Some(latest) = fetch() {
        let _ = write_cache(cache_path, now, &latest);
        return newer_version(&latest, current);
    }
    cached.and_then(|cache| newer_version(&cache.latest, current))
}

pub fn install_verified_release(
    archive: &Path,
    sums: &str,
    asset: &str,
    dest: &Path,
) -> Result<()> {
    let expected = expected_checksum(sums, asset)?;
    let actual = sha256_file(archive)?;
    if actual != expected {
        bail!("checksum mismatch for {asset}: expected {expected} actual {actual}");
    }
    let parent = dest
        .parent()
        .context("install path has no parent directory")?;
    fs::create_dir_all(parent).with_context(|| format!("could not create {}", parent.display()))?;
    let mut stage = tempfile::NamedTempFile::new_in(parent)
        .context("could not create a temporary install file")?;
    extract_binary(archive, asset, stage.as_file_mut())?;
    let _ = stage.as_file_mut().sync_all();
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        fs::set_permissions(stage.path(), fs::Permissions::from_mode(0o755))?;
    }
    let stage_path = stage.into_temp_path().keep()?;
    if let Err(err) = replace_file(&stage_path, dest) {
        let _ = fs::remove_file(&stage_path);
        return Err(err);
    }
    Ok(())
}

pub fn expected_checksum(sums: &str, asset: &str) -> Result<String> {
    for line in sums.lines() {
        let line = line.trim();
        if line.is_empty() || line.starts_with('#') {
            continue;
        }
        let mut parts = line.split_whitespace();
        let Some(hash) = parts.next() else {
            continue;
        };
        let Some(name) = parts.next() else {
            continue;
        };
        let name = name.strip_prefix('*').unwrap_or(name);
        if name != asset {
            continue;
        }
        if hash.len() != 64 || !hash.chars().all(|c| c.is_ascii_hexdigit()) {
            bail!("SHA256SUMS has an invalid entry for {asset}");
        }
        return Ok(hash.to_ascii_lowercase());
    }
    bail!("SHA256SUMS has no entry for {asset}")
}

fn update_check_disabled() -> bool {
    std::env::var("CREPATH_NO_UPDATE_CHECK").ok().as_deref() == Some("1")
}

fn cache_path() -> Result<PathBuf> {
    Ok(crate::config::crepath_home()?.join("update-check.json"))
}

fn cache_is_fresh(cache: &UpdateCache, now: i64) -> bool {
    now >= cache.checked_at && now.saturating_sub(cache.checked_at) < CHECK_INTERVAL_SECS
}

fn newer_version(latest: &str, current: &str) -> Option<String> {
    let latest = parse_version(latest).ok()?;
    let current = parse_version(current).ok()?;
    if latest > current {
        Some(latest.to_string())
    } else {
        None
    }
}

fn read_cache(path: &Path) -> Option<UpdateCache> {
    let text = fs::read_to_string(path).ok()?;
    serde_json::from_str(&text).ok()
}

fn write_cache(path: &Path, now: i64, latest: &str) -> Result<()> {
    if let Some(parent) = path.parent() {
        fs::create_dir_all(parent)?;
    }
    let latest = display_version(latest).unwrap_or_else(|_| latest.to_string());
    let body = serde_json::to_string(&UpdateCache {
        checked_at: now,
        latest,
    })?;
    fs::write(path, body)?;
    Ok(())
}

fn parse_version(raw: &str) -> Result<Version> {
    let trimmed = raw.trim();
    let stripped = trimmed
        .strip_prefix('v')
        .or_else(|| trimmed.strip_prefix('V'))
        .unwrap_or(trimmed);
    Version::parse(stripped).with_context(|| format!("invalid version: {raw}"))
}

fn display_version(raw: &str) -> Result<String> {
    Ok(parse_version(raw)?.to_string())
}

fn current_asset_name() -> Result<&'static str> {
    asset_name_for(std::env::consts::OS, std::env::consts::ARCH)
}

pub(crate) fn install_binary_path() -> Result<PathBuf> {
    let dir = match std::env::var_os("CREPATH_INSTALL") {
        Some(dir) => PathBuf::from(dir),
        None => crate::config::crepath_home()?.join("bin"),
    };
    let name = if cfg!(windows) {
        "crepath.exe"
    } else {
        "crepath"
    };
    Ok(dir.join(name))
}

fn asset_url(release: &Release, name: &str) -> Result<String> {
    release
        .assets
        .iter()
        .find(|asset| asset.name == name)
        .map(|asset| asset.browser_download_url.clone())
        .with_context(|| format!("release {} has no {name}", release.tag_name))
}

fn http_agent(timeout: Duration) -> ureq::Agent {
    ureq::AgentBuilder::new()
        .timeout(timeout)
        .redirects(8)
        .build()
}

fn fetch_release(timeout: Duration) -> Result<Release> {
    let response = http_agent(timeout)
        .get(LATEST_API)
        .set("User-Agent", concat!("crepath/", env!("CARGO_PKG_VERSION")))
        .set("Accept", "application/vnd.github+json")
        .call()
        .map_err(|err| anyhow::anyhow!("could not reach GitHub: {err}"))?;
    let text = response
        .into_string()
        .context("could not read GitHub release")?;
    serde_json::from_str(&text).context("could not read GitHub release")
}

fn download(url: &str, dest: &Path, timeout: Duration) -> Result<()> {
    let response = http_agent(timeout)
        .get(url)
        .set("User-Agent", concat!("crepath/", env!("CARGO_PKG_VERSION")))
        .call()
        .map_err(|err| anyhow::anyhow!("{err}"))?;
    let mut file =
        File::create(dest).with_context(|| format!("could not write {}", dest.display()))?;
    io::copy(&mut response.into_reader(), &mut file)?;
    Ok(())
}

fn sha256_file(path: &Path) -> Result<String> {
    let mut file =
        File::open(path).with_context(|| format!("could not open {}", path.display()))?;
    let mut hasher = Sha256::new();
    let mut buf = [0_u8; 64 * 1024];
    loop {
        let read = file.read(&mut buf)?;
        if read == 0 {
            break;
        }
        hasher.update(&buf[..read]);
    }
    Ok(format!("{:x}", hasher.finalize()))
}

fn extract_binary(archive: &Path, asset: &str, output: &mut File) -> Result<()> {
    if asset.ends_with(".zip") {
        extract_zip(archive, output)
    } else if asset.ends_with(".tar.gz") {
        extract_tar_gz(archive, output)
    } else {
        bail!("unsupported archive: {asset}")
    }
}

fn extract_tar_gz(archive: &Path, output: &mut File) -> Result<()> {
    let file =
        File::open(archive).with_context(|| format!("could not open {}", archive.display()))?;
    let mut tar = Archive::new(GzDecoder::new(file));
    for entry in tar.entries().context("could not read tar archive")? {
        let mut entry = entry.context("could not read tar entry")?;
        let path = entry.path().context("could not read tar path")?;
        let Some(name) = path.file_name().and_then(|name| name.to_str()) else {
            continue;
        };
        if name != "crepath" && name != "crepath.exe" {
            continue;
        }
        io::copy(&mut entry, output).context("could not extract crepath")?;
        return Ok(());
    }
    bail!("archive does not contain crepath")
}

fn extract_zip(archive: &Path, output: &mut File) -> Result<()> {
    let file =
        File::open(archive).with_context(|| format!("could not open {}", archive.display()))?;
    let mut zip = zip::ZipArchive::new(file).context("could not read zip archive")?;
    for index in 0..zip.len() {
        let mut entry = zip.by_index(index).context("could not read zip entry")?;
        if entry.is_dir() {
            continue;
        }
        let name = entry.name().to_string();
        let file_name = Path::new(&name)
            .file_name()
            .and_then(|name| name.to_str())
            .unwrap_or("");
        if file_name != "crepath" && file_name != "crepath.exe" {
            continue;
        }
        io::copy(&mut entry, output).context("could not extract crepath")?;
        return Ok(());
    }
    bail!("archive does not contain crepath")
}

fn replace_file(stage: &Path, dest: &Path) -> Result<()> {
    #[cfg(unix)]
    {
        fs::rename(stage, dest).with_context(|| format!("could not replace {}", dest.display()))
    }
    #[cfg(windows)]
    {
        replace_windows(stage, dest)
    }
}

#[cfg(windows)]
fn replace_windows(stage: &Path, dest: &Path) -> Result<()> {
    if !dest.exists() {
        return fs::rename(stage, dest)
            .with_context(|| format!("could not replace {}", dest.display()));
    }
    let backup = dest.with_extension("old");
    let _ = fs::remove_file(&backup);
    fs::rename(dest, &backup).with_context(|| format!("could not replace {}", dest.display()))?;
    if let Err(err) = fs::rename(stage, dest) {
        let _ = fs::rename(&backup, dest);
        return Err(err).context(format!("could not replace {}", dest.display()));
    }
    let _ = fs::remove_file(&backup);
    Ok(())
}

fn unix_now() -> i64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|duration| duration.as_secs() as i64)
        .unwrap_or(0)
}

#[cfg(test)]
mod tests {
    use super::*;
    use flate2::Compression;
    use flate2::write::GzEncoder;
    use std::sync::atomic::{AtomicUsize, Ordering as AtomicOrdering};

    #[test]
    fn version_order_with_v_prefix() {
        assert_eq!(compare_versions("0.3.0", "0.4.0").unwrap(), Ordering::Less);
        assert_eq!(compare_versions("0.4.0", "0.4.0").unwrap(), Ordering::Equal);
        assert_eq!(
            compare_versions("0.5.0", "0.4.0").unwrap(),
            Ordering::Greater
        );
        assert_eq!(
            compare_versions("v0.4.0", "0.4.0").unwrap(),
            Ordering::Equal
        );
        assert_eq!(compare_versions("v0.3.0", "0.4.0").unwrap(), Ordering::Less);
        assert_eq!(
            compare_versions("v0.5.0", "v0.4.0").unwrap(),
            Ordering::Greater
        );
    }

    #[test]
    fn fresh_cache_skips_network() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("update-check.json");
        write_cache(&path, 1_700_000_000, "0.9.0").unwrap();
        let calls = AtomicUsize::new(0);
        let found = resolve_latest(&path, 1_700_000_000 + 60, "0.4.0", || {
            calls.fetch_add(1, AtomicOrdering::SeqCst);
            Some("1.0.0".to_string())
        });
        assert_eq!(calls.load(AtomicOrdering::SeqCst), 0);
        assert_eq!(found.as_deref(), Some("0.9.0"));
    }

    #[test]
    fn stale_cache_uses_fetch() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("update-check.json");
        write_cache(&path, 1_700_000_000, "0.9.0").unwrap();
        let calls = AtomicUsize::new(0);
        let found = resolve_latest(&path, 1_700_000_000 + CHECK_INTERVAL_SECS, "0.4.0", || {
            calls.fetch_add(1, AtomicOrdering::SeqCst);
            Some("1.2.0".to_string())
        });
        assert_eq!(calls.load(AtomicOrdering::SeqCst), 1);
        assert_eq!(found.as_deref(), Some("1.2.0"));
    }

    #[test]
    fn opener_command_per_os() {
        assert_eq!(browser_command("macos").unwrap().0, "open");
        assert!(browser_command("macos").unwrap().1.is_empty());
        assert_eq!(browser_command("linux").unwrap().0, "xdg-open");
        assert!(browser_command("linux").unwrap().1.is_empty());
        assert_eq!(browser_command("windows").unwrap().0, "cmd");
        assert_eq!(browser_command("windows").unwrap().1, &["/c", "start"][..]);
        assert!(browser_command("freebsd").is_none());
    }

    #[test]
    fn update_refuses_bad_checksum() {
        let dir = tempfile::tempdir().unwrap();
        let asset = "crepath-aarch64-apple-darwin.tar.gz";
        let archive = dir.path().join(asset);
        fs::write(&archive, b"not-the-archive").unwrap();
        let sums = "aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa  crepath-aarch64-apple-darwin.tar.gz\n";
        let dest = dir.path().join("bin").join("crepath");
        fs::create_dir_all(dest.parent().unwrap()).unwrap();
        fs::write(&dest, b"original").unwrap();
        let err = install_verified_release(&archive, sums, asset, &dest).unwrap_err();
        assert!(err.to_string().contains("checksum mismatch"));
        assert_eq!(fs::read(&dest).unwrap(), b"original");
    }

    #[test]
    fn verified_archive_replaces_destination() {
        let dir = tempfile::tempdir().unwrap();
        let asset = "crepath-aarch64-apple-darwin.tar.gz";
        let payload = dir.path().join("payload");
        fs::create_dir_all(&payload).unwrap();
        let binary = payload.join("crepath");
        fs::write(&binary, b"new-binary").unwrap();
        let archive = dir.path().join(asset);
        let file = File::create(&archive).unwrap();
        let encoder = GzEncoder::new(file, Compression::default());
        let mut builder = tar::Builder::new(encoder);
        builder.append_path_with_name(&binary, "crepath").unwrap();
        let encoder = builder.into_inner().unwrap();
        encoder.finish().unwrap();
        let sums = format!("{}  {asset}\n", sha256_file(&archive).unwrap());
        let dest = dir.path().join("bin").join("crepath");
        fs::create_dir_all(dest.parent().unwrap()).unwrap();
        fs::write(&dest, b"original").unwrap();
        install_verified_release(&archive, &sums, asset, &dest).unwrap();
        assert_eq!(fs::read(&dest).unwrap(), b"new-binary");
    }

    #[test]
    fn outdated_line() {
        assert_eq!(
            outdated_message("0.3.0", "0.4.0"),
            "crepath 0.3.0 is outdated. Latest is 0.4.0. Run crepath update."
        );
    }
}

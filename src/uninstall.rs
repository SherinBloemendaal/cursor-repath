//! Remove the managed crepath install and the PATH entry the installer added.

use anyhow::{Context, Result, bail};
use comfy_table::{Attribute, Color};
use std::fs;
use std::path::{Path, PathBuf};

use crate::cli::UninstallArgs;
use crate::engine::index::{self, Holder};
use crate::ui::{self, Theme};

pub fn run(args: &UninstallArgs) -> Result<()> {
    let home = dirs::home_dir().context("could not determine home directory")?;
    let binary = crate::update::install_binary_path()?;
    let state = crate::config::crepath_home()?;
    let spellings = dir_spellings(binary.parent().unwrap_or(Path::new("")));
    execute(args, &home, &binary, &state, &spellings)
}

fn execute(
    args: &UninstallArgs,
    home: &Path,
    binary: &Path,
    state: &Path,
    spellings: &[String],
) -> Result<()> {
    let theme = Theme::stdout();
    let outside = outside_note(binary);
    let edits = path_edits(home, spellings)?;
    let purge_block = if args.purge {
        live_refresh(state)
    } else {
        None
    };
    if let Some(note) = &outside {
        ui::info(note);
    }
    let binary_label = if binary.exists() { "remove" } else { "absent" };
    let path_label = path_plan_label(&edits);
    let purge_label = if args.purge && state.exists() && purge_block.is_none() {
        "yes"
    } else {
        "no"
    };
    if args.dry_run {
        ui::section("Uninstall (dry run)");
        print_table(theme, binary_label, &path_label, purge_label);
        if let Some(holder) = &purge_block {
            ui::warn(&lock_message(holder));
        }
        println!(
            "{}",
            ui::hint_line(theme, "Nothing was changed. Run again without -n to apply.",)
        );
        return Ok(());
    }
    let work = binary.exists() || !edits.is_empty() || (args.purge && state.exists());
    if work && !args.yes {
        ui::section("Uninstall");
        print_table(theme, binary_label, &path_label, purge_label);
        if !ui::confirm("Remove the crepath install?", false)? {
            bail!("aborted");
        }
    }
    let binary_result = remove_binary(binary)?;
    let edited = apply_path_edits(&edits)?;
    let (purge_result, purge_error) = if args.purge {
        match purge_state(state, purge_block) {
            Ok(yes) => (if yes { "yes" } else { "no" }, None),
            Err(err) => ("no", Some(err)),
        }
    } else {
        ("no", None)
    };
    ui::section("Uninstall");
    print_table(
        theme,
        binary_result.label,
        &edited_label(&edited),
        purge_result,
    );
    if let Some(err) = binary_result.error {
        return Err(err);
    }
    if let Some(err) = purge_error {
        return Err(err);
    }
    Ok(())
}

struct BinaryResult {
    label: &'static str,
    error: Option<anyhow::Error>,
}

fn remove_binary(binary: &Path) -> Result<BinaryResult> {
    if !binary.exists() {
        return Ok(BinaryResult {
            label: "absent",
            error: None,
        });
    }
    refuse_cursor(binary)?;
    match fs::remove_file(binary) {
        Ok(()) => Ok(BinaryResult {
            label: "removed",
            error: None,
        }),
        Err(err) if err.kind() == std::io::ErrorKind::NotFound => Ok(BinaryResult {
            label: "absent",
            error: None,
        }),
        Err(err) => Ok(BinaryResult {
            label: "failed",
            error: Some(
                anyhow::Error::from(err).context(format!("could not remove {}", binary.display())),
            ),
        }),
    }
}

fn purge_state(state: &Path, blocked: Option<Holder>) -> Result<bool> {
    if !state.exists() {
        return Ok(false);
    }
    if let Some(holder) = blocked {
        bail!("{}", lock_message(&holder));
    }
    refuse_cursor(state)?;
    fs::remove_dir_all(state).with_context(|| format!("could not delete {}", state.display()))?;
    Ok(true)
}

fn live_refresh(state: &Path) -> Option<Holder> {
    index::holder(state).filter(|holder| holder.alive)
}

fn lock_message(holder: &Holder) -> String {
    if holder.pid == 0 {
        "a background crepath __refresh-index holds the index lock, so local state was not deleted"
            .to_string()
    } else {
        format!(
            "a background crepath __refresh-index holds the index lock (pid {}), so local state was not deleted",
            holder.pid
        )
    }
}

fn outside_note(binary: &Path) -> Option<String> {
    if !binary.is_file() {
        return None;
    }
    let current = std::env::current_exe().ok()?;
    if same_file(&current, binary) {
        return None;
    }
    Some(format!(
        "This crepath is {}, outside {}. The managed install will still be removed.",
        current.display(),
        binary.display()
    ))
}

fn same_file(left: &Path, right: &Path) -> bool {
    match (fs::canonicalize(left), fs::canonicalize(right)) {
        (Ok(left), Ok(right)) => left == right,
        _ => left == right,
    }
}

fn print_table(theme: Theme, binary: &str, path: &str, purge: &str) {
    let cell = |text: &str| {
        let good = matches!(text, "removed" | "yes") || text.starts_with("edited ");
        if text == "failed" {
            theme.cell(
                format!("{} {text}", theme.icons().cross),
                Some(Color::Red),
                &[Attribute::Bold],
            )
        } else if good {
            theme.cell(
                format!("{} {text}", theme.icons().check),
                Some(Color::Green),
                &[Attribute::Bold],
            )
        } else if text == "remove" {
            theme.cell(text, Some(Color::Yellow), &[Attribute::Bold])
        } else {
            theme.cell(text, None, &[Attribute::Dim])
        }
    };
    println!(
        "{}",
        ui::panel(
            theme,
            vec![
                ("Binary", cell(binary)),
                ("PATH", cell(path)),
                ("Purge", cell(purge)),
            ],
        )
    );
}

struct PathEdit {
    path: PathBuf,
    next: String,
}

fn path_edits(home: &Path, spellings: &[String]) -> Result<Vec<PathEdit>> {
    #[cfg(windows)]
    {
        let _ = home;
        windows_plan(spellings)
    }
    #[cfg(not(windows))]
    {
        rc_plan(home, spellings)
    }
}

fn apply_path_edits(edits: &[PathEdit]) -> Result<Vec<PathBuf>> {
    #[cfg(windows)]
    {
        windows_apply(edits)
    }
    #[cfg(not(windows))]
    {
        let mut written = Vec::new();
        for edit in edits {
            fs::write(&edit.path, &edit.next)
                .with_context(|| format!("could not update {}", edit.path.display()))?;
            written.push(edit.path.clone());
        }
        Ok(written)
    }
}

fn path_plan_label(edits: &[PathEdit]) -> String {
    if edits.is_empty() {
        "unchanged".to_string()
    } else {
        format!("edit {}", join_labels(edits))
    }
}

fn edited_label(paths: &[PathBuf]) -> String {
    if paths.is_empty() {
        "unchanged".to_string()
    } else {
        format!(
            "edited {}",
            paths
                .iter()
                .map(|path| path.display().to_string())
                .collect::<Vec<_>>()
                .join(", ")
        )
    }
}

fn join_labels(edits: &[PathEdit]) -> String {
    edits
        .iter()
        .map(|edit| edit.path.display().to_string())
        .collect::<Vec<_>>()
        .join(", ")
}

#[cfg(not(windows))]
fn rc_plan(home: &Path, spellings: &[String]) -> Result<Vec<PathEdit>> {
    let mut edits = Vec::new();
    for (path, fish) in rc_files(home) {
        let Ok(text) = fs::read_to_string(&path) else {
            if path.exists() {
                bail!("could not read {}", path.display());
            }
            continue;
        };
        let next = strip_installer_blocks(&text, spellings, fish);
        if next != text {
            edits.push(PathEdit { path, next });
        }
    }
    Ok(edits)
}

#[cfg(not(windows))]
fn rc_files(home: &Path) -> Vec<(PathBuf, bool)> {
    vec![
        (home.join(".zshrc"), false),
        (home.join(".bashrc"), false),
        (home.join(".bash_profile"), false),
        (home.join(".config").join("fish").join("config.fish"), true),
    ]
}

pub(crate) fn strip_installer_blocks(text: &str, dirs: &[String], fish: bool) -> String {
    let mut out = text.to_string();
    for dir in dirs {
        if dir.is_empty() {
            continue;
        }
        let line = if fish {
            format!("fish_add_path --prepend \"{dir}\"")
        } else {
            format!("export PATH=\"{dir}:$PATH\"")
        };
        out = strip_exact_block(&out, &line);
    }
    out
}

fn strip_exact_block(text: &str, line: &str) -> String {
    let with_nl = format!("\n# crepath\n{line}\n");
    let at_eof = format!("\n# crepath\n{line}");
    let mut out = text.replace(&with_nl, "");
    if out.ends_with(&at_eof) {
        out.truncate(out.len() - at_eof.len());
    }
    out
}

pub(crate) fn dir_spellings(dir: &Path) -> Vec<String> {
    let mut out = Vec::new();
    push_spelling(&mut out, dir.to_string_lossy().into_owned());
    if let Ok(canon) = dir.canonicalize() {
        push_spelling(&mut out, canon.to_string_lossy().into_owned());
    }
    out
}

fn push_spelling(out: &mut Vec<String>, text: String) {
    let text = text.trim_end_matches(['/', '\\']).to_string();
    if !text.is_empty() && !out.iter().any(|existing| existing == &text) {
        out.push(text);
    }
}

#[cfg(any(windows, test))]
pub(crate) fn without_install_dirs(path: &str, dirs: &[String]) -> String {
    let needles: Vec<String> = dirs
        .iter()
        .map(|dir| win_key(dir))
        .filter(|dir| !dir.is_empty())
        .collect();
    path.split(';')
        .filter(|part| {
            let key = win_key(part);
            !needles.iter().any(|needle| needle == &key)
        })
        .collect::<Vec<_>>()
        .join(";")
}

#[cfg(any(windows, test))]
fn win_key(value: &str) -> String {
    value.trim().trim_end_matches('\\').to_ascii_lowercase()
}

fn refuse_cursor(path: &Path) -> Result<()> {
    if cursor_owned(path) {
        bail!(
            "refusing to delete {} because it is inside Cursor data",
            path.display()
        );
    }
    Ok(())
}

fn cursor_owned(path: &Path) -> bool {
    let path = normalize(path);
    if path.components().any(|part| part.as_os_str() == ".cursor") {
        return true;
    }
    let mut roots = Vec::new();
    if let Some(home) = dirs::home_dir() {
        roots.push(home.join(".cursor"));
    }
    if let Ok(config) = crate::config::cursor_config_dir() {
        roots.push(config);
    }
    roots.iter().any(|root| path.starts_with(normalize(root)))
}

fn normalize(path: &Path) -> PathBuf {
    let abs = if path.is_absolute() {
        path.to_path_buf()
    } else if let Ok(cwd) = std::env::current_dir() {
        cwd.join(path)
    } else {
        path.to_path_buf()
    };
    abs.canonicalize().unwrap_or(abs)
}

#[cfg(windows)]
fn windows_plan(spellings: &[String]) -> Result<Vec<PathEdit>> {
    let Some(current) = read_user_path()? else {
        return Ok(Vec::new());
    };
    let next = without_install_dirs(&current, spellings);
    if next == current {
        return Ok(Vec::new());
    }
    Ok(vec![PathEdit {
        path: PathBuf::from("user PATH"),
        next,
    }])
}

#[cfg(windows)]
fn windows_apply(edits: &[PathEdit]) -> Result<Vec<PathBuf>> {
    if edits.is_empty() {
        return Ok(Vec::new());
    }
    write_user_path(&edits[0].next)?;
    Ok(vec![PathBuf::from("user PATH")])
}

#[cfg(windows)]
fn read_user_path() -> Result<Option<String>> {
    use windows_sys::Win32::Foundation::{ERROR_FILE_NOT_FOUND, ERROR_SUCCESS};
    use windows_sys::Win32::System::Registry::{
        HKEY, HKEY_CURRENT_USER, KEY_READ, KEY_SET_VALUE, REG_EXPAND_SZ, REG_SZ, REG_VALUE_TYPE,
        RegCloseKey, RegOpenKeyExW, RegQueryValueExW,
    };

    let key = open_env_key(KEY_READ | KEY_SET_VALUE)?;
    let _close = Key(key);
    for name in ["Path", "PATH"] {
        let wide = wide_null(name);
        let mut kind: REG_VALUE_TYPE = 0;
        let mut size: u32 = 0;
        let status = unsafe {
            RegQueryValueExW(
                key,
                wide.as_ptr(),
                std::ptr::null(),
                &mut kind,
                std::ptr::null_mut(),
                &mut size,
            )
        };
        if status == ERROR_FILE_NOT_FOUND {
            continue;
        }
        if status != ERROR_SUCCESS {
            bail!("could not read the user PATH");
        }
        if kind != REG_SZ && kind != REG_EXPAND_SZ {
            bail!("user PATH is not a string");
        }
        let mut buf = vec![0_u8; size as usize];
        let status = unsafe {
            RegQueryValueExW(
                key,
                wide.as_ptr(),
                std::ptr::null(),
                &mut kind,
                buf.as_mut_ptr(),
                &mut size,
            )
        };
        if status != ERROR_SUCCESS {
            bail!("could not read the user PATH");
        }
        buf.truncate(size as usize);
        return Ok(Some(utf16_bytes(&buf)));
    }
    Ok(None)
}

#[cfg(windows)]
fn write_user_path(value: &str) -> Result<()> {
    use windows_sys::Win32::Foundation::ERROR_SUCCESS;
    use windows_sys::Win32::System::Registry::{
        HKEY_CURRENT_USER, KEY_READ, KEY_SET_VALUE, REG_EXPAND_SZ, REG_SZ, REG_VALUE_TYPE,
        RegQueryValueExW, RegSetValueExW,
    };
    use windows_sys::Win32::UI::WindowsAndMessaging::{
        HWND_BROADCAST, SMTO_ABORTIFHUNG, SendMessageTimeoutW, WM_SETTINGCHANGE,
    };

    let key = open_env_key(KEY_READ | KEY_SET_VALUE)?;
    let _close = Key(key);
    let mut chosen = "Path";
    let mut kind = REG_SZ;
    for name in ["Path", "PATH"] {
        let wide = wide_null(name);
        let mut found: REG_VALUE_TYPE = 0;
        let mut size: u32 = 0;
        let status = unsafe {
            RegQueryValueExW(
                key,
                wide.as_ptr(),
                std::ptr::null(),
                &mut found,
                std::ptr::null_mut(),
                &mut size,
            )
        };
        if status == ERROR_SUCCESS && (found == REG_SZ || found == REG_EXPAND_SZ) {
            chosen = name;
            kind = found;
            break;
        }
    }
    let wide_name = wide_null(chosen);
    let data = value
        .encode_utf16()
        .chain(std::iter::once(0))
        .collect::<Vec<_>>();
    let bytes = u32::try_from(data.len() * 2).unwrap_or(u32::MAX);
    let status = unsafe {
        RegSetValueExW(
            key,
            wide_name.as_ptr(),
            0,
            kind,
            data.as_ptr() as *const u8,
            bytes,
        )
    };
    if status != ERROR_SUCCESS {
        bail!("could not update the user PATH");
    }
    let notice = wide_null("Environment");
    unsafe {
        SendMessageTimeoutW(
            HWND_BROADCAST,
            WM_SETTINGCHANGE,
            0,
            notice.as_ptr() as isize,
            SMTO_ABORTIFHUNG,
            5000,
            std::ptr::null_mut(),
        );
    }
    Ok(())
}

#[cfg(windows)]
fn open_env_key(access: u32) -> Result<windows_sys::Win32::System::Registry::HKEY> {
    use windows_sys::Win32::Foundation::ERROR_SUCCESS;
    use windows_sys::Win32::System::Registry::{HKEY, HKEY_CURRENT_USER, RegOpenKeyExW};

    let name = wide_null("Environment");
    let mut key: HKEY = std::ptr::null_mut();
    let status = unsafe { RegOpenKeyExW(HKEY_CURRENT_USER, name.as_ptr(), 0, access, &mut key) };
    if status != ERROR_SUCCESS {
        bail!("could not open the user environment");
    }
    Ok(key)
}

#[cfg(windows)]
fn wide_null(value: &str) -> Vec<u16> {
    value.encode_utf16().chain(std::iter::once(0)).collect()
}

#[cfg(windows)]
fn utf16_bytes(buf: &[u8]) -> String {
    let units: Vec<u16> = buf
        .chunks_exact(2)
        .map(|chunk| u16::from_le_bytes([chunk[0], chunk[1]]))
        .collect();
    let end = units
        .iter()
        .rposition(|unit| *unit != 0)
        .map(|index| index + 1)
        .unwrap_or(0);
    String::from_utf16_lossy(&units[..end])
}

#[cfg(windows)]
struct Key(windows_sys::Win32::System::Registry::HKEY);

#[cfg(windows)]
impl Drop for Key {
    fn drop(&mut self) {
        if !self.0.is_null() {
            unsafe {
                windows_sys::Win32::System::Registry::RegCloseKey(self.0);
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn strips_only_the_installer_block() {
        let dir = "/tmp/crepath-bin";
        let text = format!(
            "export PATH=\"/usr/local/bin:$PATH\"\n# keep {dir}\n\n# crepath\nexport PATH=\"{dir}:$PATH\"\nalias ll='ls'\n"
        );
        let next = strip_installer_blocks(&text, &[dir.to_string()], false);
        assert_eq!(
            next,
            "export PATH=\"/usr/local/bin:$PATH\"\n# keep /tmp/crepath-bin\nalias ll='ls'\n"
        );
        let fish =
            format!("\n# crepath\nfish_add_path --prepend \"{dir}\"\nset -x PATH /usr/bin\n");
        assert_eq!(
            strip_installer_blocks(&fish, &[dir.to_string()], true),
            "set -x PATH /usr/bin\n"
        );
        assert_eq!(
            strip_installer_blocks(&text, &[dir.to_string()], true),
            text
        );
    }

    #[test]
    fn windows_path_drops_only_the_install_dir() {
        let dir = r"C:\Users\me\.crepath\bin";
        let path = r"C:\Users\me\.crepath\bin;C:\Windows;C:\Users\me\.crepath\bin\extra";
        assert_eq!(
            without_install_dirs(path, &[dir.to_string()]),
            r"C:\Windows;C:\Users\me\.crepath\bin\extra"
        );
        assert_eq!(
            without_install_dirs(path, &[format!("{dir}\\")]),
            r"C:\Windows;C:\Users\me\.crepath\bin\extra"
        );
        assert_eq!(
            without_install_dirs("A;B;", &[r"C:\missing".to_string()]),
            "A;B;"
        );
    }
}

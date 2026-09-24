use std::ffi::OsString;
use std::fs;
use std::path::{Path, PathBuf};
use std::sync::Mutex;

use clap::Parser;
use crepath::cli::{self, Cli};

static ENV_LOCK: Mutex<()> = Mutex::new(());

struct Saved(&'static str, Option<OsString>);

impl Drop for Saved {
    fn drop(&mut self) {
        match &self.1 {
            Some(value) => unsafe { std::env::set_var(self.0, value) },
            None => unsafe { std::env::remove_var(self.0) },
        }
    }
}

struct Layout {
    _root: tempfile::TempDir,
    home: PathBuf,
    install: PathBuf,
    state: PathBuf,
    _saved: Vec<Saved>,
}

fn binary_name() -> &'static str {
    if cfg!(windows) {
        "crepath.exe"
    } else {
        "crepath"
    }
}

fn set_var(key: &'static str, value: impl AsRef<std::ffi::OsStr>) -> Saved {
    let previous = std::env::var_os(key);
    unsafe { std::env::set_var(key, value) };
    Saved(key, previous)
}

fn layout() -> Layout {
    let root = tempfile::tempdir().unwrap();
    let home = root.path().join("home");
    let install = root.path().join("install");
    let state = root.path().join("state");
    fs::create_dir_all(&home).unwrap();
    fs::create_dir_all(&install).unwrap();
    fs::create_dir_all(&state).unwrap();
    let saved = vec![
        set_var("HOME", &home),
        set_var("USERPROFILE", &home),
        set_var("CREPATH_HOME", &state),
        set_var("CREPATH_INSTALL", &install),
        set_var("CREPATH_NO_UPDATE_CHECK", "1"),
        set_var("CREPATH_NO_INDEX", "1"),
    ];
    Layout {
        _root: root,
        home,
        install,
        state,
        _saved: saved,
    }
}

fn run(args: &[&str]) -> anyhow::Result<()> {
    let argv = std::iter::once("crepath").chain(args.iter().copied());
    cli::dispatch(Cli::try_parse_from(argv).unwrap(), None)
}

fn installer_block(dir: &Path) -> String {
    format!("\n# crepath\nexport PATH=\"{}:$PATH\"\n", dir.display())
}

fn write_rc(home: &Path, dir: &Path) -> String {
    let text = format!(
        "export PATH=\"/usr/local/bin:$PATH\"\n# notes about {}\n{}alias ll='ls -la'\n",
        dir.display(),
        installer_block(dir)
    );
    fs::write(home.join(".zshrc"), &text).unwrap();
    text
}

fn write_binary(install: &Path) -> PathBuf {
    let path = install.join(binary_name());
    fs::write(&path, b"crepath").unwrap();
    path
}

fn assert_rc(home: &Path, install: &Path, original: &str, stripped: bool) {
    let rc = fs::read_to_string(home.join(".zshrc")).unwrap();
    if cfg!(unix) && stripped {
        assert!(!rc.contains("# crepath"));
        assert!(!rc.contains(&format!("export PATH=\"{}:$PATH\"", install.display())));
        assert!(rc.contains("export PATH=\"/usr/local/bin:$PATH\""));
        assert!(rc.contains(&format!("# notes about {}", install.display())));
        assert!(rc.contains("alias ll='ls -la'"));
        assert_ne!(rc, original);
    } else {
        assert_eq!(rc, original);
    }
}

fn write_state(state: &Path) {
    fs::create_dir_all(state.join("backups")).unwrap();
    fs::write(state.join("backups").join("snap"), b"snap").unwrap();
    fs::write(state.join("history.jsonl"), b"{}\n").unwrap();
    fs::write(state.join("index.db"), b"db").unwrap();
    fs::write(state.join("update-check.json"), b"{}\n").unwrap();
}

#[test]
fn uninstall_removes_the_binary_and_only_the_installer_block() {
    let _lock = ENV_LOCK.lock().unwrap();
    let lay = layout();
    let binary = write_binary(&lay.install);
    let original = write_rc(&lay.home, &lay.install);
    write_state(&lay.state);
    let decoy_dir = lay.home.join("other-bin");
    fs::create_dir_all(&decoy_dir).unwrap();
    let decoy = decoy_dir.join(binary_name());
    fs::write(&decoy, b"other").unwrap();

    run(&["uninstall", "-y"]).unwrap();

    assert!(!binary.exists());
    assert_eq!(fs::read(&decoy).unwrap(), b"other");
    assert_rc(&lay.home, &lay.install, &original, true);
    assert!(lay.state.join("history.jsonl").exists());
    assert!(lay.state.join("index.db").exists());
    assert!(lay.state.join("backups").join("snap").exists());
    assert!(lay.state.join("update-check.json").exists());
}

#[test]
fn dry_run_changes_nothing() {
    let _lock = ENV_LOCK.lock().unwrap();
    let lay = layout();
    let binary = write_binary(&lay.install);
    let original = write_rc(&lay.home, &lay.install);
    write_state(&lay.state);

    run(&["uninstall", "-n", "--purge"]).unwrap();

    assert_eq!(fs::read(&binary).unwrap(), b"crepath");
    assert_rc(&lay.home, &lay.install, &original, false);
    assert!(lay.state.join("history.jsonl").exists());
    assert!(lay.state.join("index.db").exists());
}

#[test]
fn purge_removes_state_after_the_binary() {
    let _lock = ENV_LOCK.lock().unwrap();
    let lay = layout();
    let binary = write_binary(&lay.install);
    let original = write_rc(&lay.home, &lay.install);
    write_state(&lay.state);
    let nearby = lay.home.join("keep.txt");
    fs::write(&nearby, b"keep").unwrap();

    run(&["uninstall", "-y", "--purge"]).unwrap();

    assert!(!binary.exists());
    assert!(!lay.state.exists());
    assert_eq!(fs::read(&nearby).unwrap(), b"keep");
    assert_rc(&lay.home, &lay.install, &original, true);
}

#[test]
fn purge_refuses_a_live_index_lock() {
    let _lock = ENV_LOCK.lock().unwrap();
    let lay = layout();
    let binary = write_binary(&lay.install);
    let original = write_rc(&lay.home, &lay.install);
    write_state(&lay.state);
    fs::write(
        crepath::engine::index::lock_path(&lay.state),
        serde_json::json!({
            "pid": std::process::id(),
            "started": crepath::engine::index::now_ms(),
        })
        .to_string(),
    )
    .unwrap();

    let err = run(&["uninstall", "-y", "--purge"]).unwrap_err();
    let message = err.to_string();
    assert!(message.contains("__refresh-index"), "{message}");
    assert!(message.contains("not deleted"), "{message}");
    assert!(!binary.exists());
    assert!(lay.state.join("history.jsonl").exists());
    assert!(crepath::engine::index::lock_path(&lay.state).exists());
    assert_rc(&lay.home, &lay.install, &original, true);
}

#[test]
fn missing_binary_removal_fails() {
    let _lock = ENV_LOCK.lock().unwrap();
    let lay = layout();
    let binary = lay.install.join(binary_name());
    fs::create_dir_all(&binary).unwrap();

    let err = run(&["uninstall", "-y"]).unwrap_err();
    assert!(err.to_string().contains("could not remove"), "{err}");
    assert!(binary.is_dir());
}

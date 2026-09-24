//! `crepath cache clear|scan|stats`: look after the persistent index.

use anyhow::Result;
use comfy_table::{Attribute, Cell, CellAlignment, Color};
use rusqlite::{Connection, OpenFlags};
use std::fs;
use std::path::{Path, PathBuf};
use std::time::Instant;

use super::fsops::remove_path;
use super::index::{
    self, Counts, Holder, Index, Lock, Outcome, SCHEMA, Scan, Staleness, db_path, lock_path,
};
use super::{Report, Runtime};
use crate::cursor::install::DEFAULT;
use crate::ui::{self, Align, Sheet, Theme};

fn home(rt: &Runtime) -> &Path {
    &rt.layout.crepath_home
}

fn files(home: &Path) -> Vec<PathBuf> {
    let db = db_path(home);
    let mut out = vec![db.clone()];
    for suffix in ["-wal", "-shm"] {
        let mut name = db.file_name().unwrap_or_default().to_os_string();
        name.push(suffix);
        out.push(db.with_file_name(name));
    }
    out
}

fn index_size(home: &Path) -> u64 {
    files(home)
        .iter()
        .filter_map(|path| fs::metadata(path).ok())
        .map(|meta| meta.len())
        .sum()
}

fn clock(millis: i64) -> String {
    ui::datetime(millis).unwrap_or_else(|| "-".to_string())
}

fn running(holder: &Holder) -> anyhow::Error {
    ui::hinted(
        format!(
            "an index refresh is running (pid {}, started {})",
            holder.pid,
            clock(holder.started)
        ),
        "Wait for it to finish and retry. crepath cache stats shows whether it is still running.",
    )
}

/// The one installation `--profile` picked, or `None` for all of them.
fn chosen(rt: &Runtime) -> Result<Option<String>> {
    if let Some(profile) = &rt.profile
        && profile != DEFAULT
    {
        return Err(ui::hinted(
            format!(
                "the index is kept per Cursor installation, not per VS Code profile ({profile})"
            ),
            format!("Pass --profile {}.", rt.layout.name),
        ));
    }
    Ok(rt.pinned.then(|| rt.layout.name.clone()))
}

pub struct ClearPlan {
    pub install: Option<String>,
    pub items: Vec<String>,
    pub question: String,
}

pub fn clear_plan(rt: &Runtime) -> Result<ClearPlan> {
    let home = home(rt);
    if let Some(holder) = index::holder(home).filter(|holder| holder.alive) {
        return Err(running(&holder));
    }
    let install = chosen(rt)?;
    let mut items = Vec::new();
    let question = match &install {
        None => {
            for path in files(home) {
                if path.exists() {
                    items.push(format!("delete {}", path.display()));
                }
            }
            if index::holder(home).is_some() {
                items.push(format!("delete {}", lock_path(home).display()));
            }
            format!(
                "Delete the crepath index ({})?",
                ui::format_size(index_size(home))
            )
        }
        Some(name) => {
            if let Some(index) = Index::open_existing(home)?
                && index.installs()?.contains(name)
            {
                let counts = index.counts(name)?;
                items.push(format!(
                    "forget {} -> {}, {}",
                    rt.layout.cursor_root.display(),
                    ui::plural(counts.workspaces, "workspace", "workspaces"),
                    ui::plural(counts.chats + counts.subagents, "chat", "chats")
                ));
            }
            format!("Forget the index rows of {name}?")
        }
    };
    Ok(ClearPlan {
        install,
        items,
        question,
    })
}

pub fn clear(rt: &Runtime, plan: &ClearPlan) -> Result<Report> {
    let mut report = Report::default();
    if rt.dry_run {
        report.applied = plan
            .items
            .iter()
            .map(|item| format!("dry-run {item}"))
            .collect();
        return Ok(report);
    }
    if plan.items.is_empty() {
        return Ok(report);
    }
    let home = home(rt);
    let lock = match Lock::acquire(home)? {
        Ok(lock) => lock,
        Err(holder) => return Err(running(&holder)),
    };
    match &plan.install {
        None => {
            for path in files(home) {
                remove_path(&path)?;
            }
        }
        Some(name) => {
            if let Some(index) = Index::open_existing(home)? {
                index.forget(name)?;
            }
        }
    }
    drop(lock);
    report.applied = plan.items.clone();
    Ok(report)
}

pub struct ScanResult {
    pub outcomes: Vec<Outcome>,
    pub duration_ms: u64,
    pub full: bool,
    pub path: PathBuf,
}

pub fn scan(rt: &Runtime, full: bool) -> Result<ScanResult> {
    chosen(rt)?;
    let home = home(rt);
    let started = Instant::now();
    let lock = match Lock::acquire(home)? {
        Ok(lock) => lock,
        Err(holder) => {
            return Err(ui::hinted(
                format!("an index refresh is already running (pid {})", holder.pid),
                "Wait for it to finish and retry.",
            ));
        }
    };
    let index = Index::open(home)?;
    let scopes = rt.scope();
    let progress = ui::DualProgress::new("scan", scopes.len() as u64, rt.quiet);
    let outcome = index::refresh(&index, &scopes, full, &progress);
    progress.finish();
    let outcomes = outcome?;
    if !rt.pinned {
        let keep: Vec<String> = rt
            .installs
            .iter()
            .map(|layout| layout.name.clone())
            .collect();
        index.retain(&keep)?;
    }
    drop(lock);
    Ok(ScanResult {
        outcomes,
        duration_ms: u64::try_from(started.elapsed().as_millis()).unwrap_or(u64::MAX),
        full,
        path: index.path().to_path_buf(),
    })
}

fn number(theme: Theme, value: usize) -> Cell {
    theme
        .cell(ui::count(value as u64), None, &[Attribute::Bold])
        .set_alignment(CellAlignment::Right)
}

fn dim(theme: Theme, text: impl std::fmt::Display) -> Cell {
    theme.cell(text, None, &[Attribute::Dim])
}

fn name_cell(theme: Theme, name: &str) -> Cell {
    if name == DEFAULT {
        theme.cell(name, None, &[Attribute::Dim])
    } else {
        theme.cell(name, Some(Color::Cyan), &[Attribute::Bold])
    }
}

fn millis_cell(theme: Theme, millis: u64) -> Cell {
    dim(theme, ui::duration(millis)).set_alignment(CellAlignment::Right)
}

pub fn scan_sheet(theme: Theme, outcomes: &[Outcome], duration_ms: u64) -> Sheet {
    let mut sheet = Sheet::new(
        theme,
        &[
            ("installation", Align::Left),
            ("workspaces", Align::Right),
            ("chats", Align::Right),
            ("subagents", Align::Right),
            ("changed sources", Align::Right),
            ("duration", Align::Right),
        ],
    );
    for outcome in outcomes {
        sheet.row(vec![
            name_cell(theme, &outcome.install),
            number(theme, outcome.workspaces),
            number(theme, outcome.chats),
            theme.count_cell(outcome.subagents, Some(Color::Magenta)),
            theme.count_cell(outcome.changed, Some(Color::Yellow)),
            millis_cell(theme, outcome.duration_ms),
        ]);
    }
    if outcomes.len() > 1 {
        let sum = |pick: fn(&Outcome) -> usize| outcomes.iter().map(pick).sum::<usize>();
        sheet.total(vec![
            theme.cell("all installations", None, &[]),
            number(theme, sum(|outcome| outcome.workspaces)),
            number(theme, sum(|outcome| outcome.chats)),
            number(theme, sum(|outcome| outcome.subagents)),
            number(theme, sum(|outcome| outcome.changed)),
            millis_cell(theme, duration_ms),
        ]);
    }
    sheet
}

pub fn render_scan(theme: Theme, result: &ScanResult) -> String {
    let title = if result.full {
        "Index rebuild"
    } else {
        "Index scan"
    };
    let read: usize = result
        .outcomes
        .iter()
        .map(|outcome| outcome.chats_read)
        .sum();
    format!(
        "{}\n{}\n{}\n{}\n",
        ui::section_line(theme, title),
        scan_sheet(theme, &result.outcomes, result.duration_ms),
        ui::ok_line(
            theme,
            &format!(
                "{} in {}, {} from Cursor",
                ui::plural(
                    result.outcomes.len(),
                    "installation scanned",
                    "installations scanned"
                ),
                ui::duration(result.duration_ms),
                ui::plural(read, "chat read", "chats read")
            )
        ),
        ui::hint_line(
            theme,
            &ui::home_relative(&result.path.display().to_string())
        )
    )
}

pub struct InstallRow {
    pub name: String,
    pub root: String,
    pub found: bool,
    pub scan: Option<Scan>,
    pub counts: Counts,
    pub stale: Option<Staleness>,
}

pub struct CacheStats {
    pub path: PathBuf,
    pub exists: bool,
    pub size: u64,
    pub version: Option<i64>,
    pub refreshed_at: Option<i64>,
    pub spawned_at: Option<i64>,
    pub holder: Option<Holder>,
    pub log: Option<String>,
    pub installs: Vec<InstallRow>,
}

fn peek_version(path: &Path) -> Result<i64> {
    let conn = Connection::open_with_flags(
        path,
        OpenFlags::SQLITE_OPEN_READ_ONLY | OpenFlags::SQLITE_OPEN_NO_MUTEX,
    )?;
    Ok(conn.pragma_query_value(None, "user_version", |row| row.get(0))?)
}

/// Everything `cache stats` shows. Cursor's files are only stat'ed, never opened.
pub fn collect(rt: &Runtime) -> Result<CacheStats> {
    chosen(rt)?;
    let home = home(rt);
    let path = db_path(home);
    let exists = path.exists();
    let version = if exists {
        Some(peek_version(&path)?)
    } else {
        None
    };
    let index = if version == Some(SCHEMA) {
        Index::open_existing(home)?
    } else {
        None
    };
    let scans = match &index {
        Some(index) => index.scans()?,
        None => Default::default(),
    };
    let mut installs = Vec::new();
    for scoped in rt.scope() {
        let layout = &scoped.layout;
        let scan = scans.get(&layout.name).cloned();
        let counts = match &index {
            Some(index) => index.counts(&layout.name)?,
            None => Counts::default(),
        };
        let stale = match (&index, &scan) {
            (Some(index), Some(scan)) if scan.root == index::root_text(layout) => {
                Some(index::staleness(index, layout, scan)?)
            }
            (Some(_), Some(_)) => Some(Staleness {
                sources: 0,
                reasons: vec!["installation moved".to_string()],
            }),
            _ => None,
        };
        installs.push(InstallRow {
            name: layout.name.clone(),
            root: ui::home_relative(&layout.cursor_root.display().to_string()),
            found: true,
            scan,
            counts,
            stale,
        });
    }
    if !rt.pinned
        && let Some(index) = &index
    {
        for name in index.installs()? {
            if rt.installs.iter().any(|layout| layout.name == name) {
                continue;
            }
            installs.push(InstallRow {
                root: scans
                    .get(&name)
                    .map(|scan| ui::home_relative(&scan.root))
                    .unwrap_or_default(),
                scan: scans.get(&name).cloned(),
                counts: index.counts(&name)?,
                stale: None,
                found: false,
                name,
            });
        }
    }
    let meta = |key: &str| {
        index
            .as_ref()
            .and_then(|index| index.meta_millis(key).ok().flatten())
    };
    Ok(CacheStats {
        size: index_size(home),
        exists,
        version,
        refreshed_at: meta("refreshed_at"),
        spawned_at: meta("spawned_at"),
        holder: index::holder(home),
        log: index::last_line(home),
        installs,
        path,
    })
}

fn when(theme: Theme, millis: Option<i64>, now: i64) -> Cell {
    match millis.and_then(|at| ui::datetime(at).map(|stamp| (at, stamp))) {
        Some((at, stamp)) => theme.cell(format!("{stamp} ({})", ui::ago(at, now)), None, &[]),
        None => dim(theme, "never"),
    }
}

pub fn overview(theme: Theme, stats: &CacheStats, now: i64) -> Sheet {
    let path = ui::home_relative(&stats.path.display().to_string());
    let file = if stats.exists {
        theme.cell(path, Some(Color::Cyan), &[])
    } else {
        dim(theme, format!("{path} (not built yet)"))
    };
    let schema = match stats.version {
        Some(version) if version == SCHEMA => theme.cell(format!("version {version}"), None, &[]),
        Some(version) => theme.cell(
            format!("version {version}, rebuilt as version {SCHEMA} on next use"),
            Some(Color::Yellow),
            &[],
        ),
        None => dim(theme, format!("version {SCHEMA}")),
    };
    let background = match &stats.holder {
        Some(holder) if holder.alive => theme.cell(
            format!(
                "running (pid {}, started {})",
                holder.pid,
                clock(holder.started)
            ),
            Some(Color::Green),
            &[Attribute::Bold],
        ),
        Some(holder) => theme.cell(
            format!("stale lock (pid {}), cleared on next refresh", holder.pid),
            Some(Color::Yellow),
            &[],
        ),
        None => dim(theme, "idle"),
    };
    ui::panel(
        theme,
        vec![
            ("File", file),
            ("Size", theme.size_cell(stats.size)),
            ("Schema", schema),
            ("Last refresh", when(theme, stats.refreshed_at, now)),
            ("Last spawn", when(theme, stats.spawned_at, now)),
            ("Background", background),
            (
                "Last log line",
                match &stats.log {
                    Some(line) => dim(theme, line),
                    None => dim(theme, "none"),
                },
            ),
        ],
    )
}

pub fn installations(theme: Theme, stats: &CacheStats, now: i64) -> Sheet {
    let mut sheet = Sheet::new(
        theme,
        &[
            ("installation", Align::Left),
            ("last scan", Align::Left),
            ("scan time", Align::Right),
            ("workspaces", Align::Right),
            ("chats", Align::Right),
            ("subagents", Align::Right),
            ("stale sources", Align::Right),
            ("reason", Align::Left),
        ],
    )
    .flex(7);
    for row in &stats.installs {
        let (stale, reason) = match (&row.scan, &row.stale) {
            _ if !row.found => (
                dim(theme, "-").set_alignment(CellAlignment::Right),
                theme.cell("installation not found", Some(Color::Red), &[]),
            ),
            (None, _) => (
                dim(theme, "-").set_alignment(CellAlignment::Right),
                theme.cell("not scanned yet", Some(Color::Yellow), &[]),
            ),
            (Some(_), Some(stale)) if stale.reasons.is_empty() => (
                theme.count_cell(stale.sources, None),
                theme.cell("up to date", Some(Color::Green), &[]),
            ),
            (Some(_), Some(stale)) => (
                theme.count_cell(stale.sources, Some(Color::Yellow)),
                theme.cell(stale.reasons.join(", "), Some(Color::Yellow), &[]),
            ),
            (Some(_), None) => (
                dim(theme, "-").set_alignment(CellAlignment::Right),
                dim(theme, "schema changed"),
            ),
        };
        sheet.row(vec![
            name_cell(theme, &row.name),
            when(theme, row.scan.as_ref().map(|scan| scan.scanned_at), now),
            match &row.scan {
                Some(scan) => millis_cell(theme, u64::try_from(scan.duration_ms).unwrap_or(0)),
                None => dim(theme, "-").set_alignment(CellAlignment::Right),
            },
            theme.count_cell(row.counts.workspaces, None),
            theme.count_cell(row.counts.chats, None),
            theme.count_cell(row.counts.subagents, Some(Color::Magenta)),
            stale,
            reason,
        ]);
    }
    sheet
}

pub fn render_stats(theme: Theme, stats: &CacheStats, now: i64) -> String {
    let mut out = format!(
        "{}\n{}\n\n{}\n",
        ui::section_line(theme, "Index"),
        overview(theme, stats, now),
        ui::section_line(theme, "Installations")
    );
    let sheet = installations(theme, stats, now);
    if sheet.is_empty() {
        out.push_str(&ui::hint_line(theme, "none yet"));
        out.push('\n');
    } else {
        out.push_str(&format!("{sheet}\n"));
    }
    out
}

pub fn render_stale(theme: Theme, stats: &CacheStats) -> String {
    let mut sheet = Sheet::new(
        theme,
        &[
            ("installation", Align::Left),
            ("stale sources", Align::Right),
            ("reason", Align::Left),
        ],
    )
    .flex(2);
    for row in stats.installs.iter().filter(|row| row.found) {
        let (count, reason) = match (&row.scan, &row.stale) {
            (Some(_), Some(stale)) if !stale.reasons.is_empty() => (
                theme.count_cell(stale.sources, Some(Color::Yellow)),
                stale.reasons.join(", "),
            ),
            (Some(_), Some(stale)) => (theme.count_cell(stale.sources, None), "up to date".into()),
            _ => (
                dim(theme, "-").set_alignment(CellAlignment::Right),
                "not scanned yet".to_string(),
            ),
        };
        sheet.row(vec![
            name_cell(theme, &row.name),
            count,
            theme.cell(reason, None, &[]),
        ]);
    }
    format!(
        "{}\n{sheet}\n{}\n",
        ui::section_line(theme, "Index scan (dry run)"),
        ui::hint_line(theme, "Nothing was changed. Run again without -n to scan.")
    )
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::ui::is_bare_number;

    const UNITS: &[&str] = &[
        "workspaces",
        "chats",
        "subagents",
        "changed sources",
        "stale sources",
    ];

    fn assert_units(title: &str, sheet: &Sheet) {
        assert!(!sheet.is_empty(), "{title} is empty");
        for (index, header) in sheet.headers().iter().enumerate() {
            assert!(!header.is_empty(), "{title}");
            if sheet.column(index).iter().any(|cell| is_bare_number(cell)) {
                assert!(
                    UNITS.contains(&header.as_str()),
                    "{title}: bare numbers under {header:?}"
                );
            }
        }
        let rendered = sheet.render();
        let header_line = rendered.lines().nth(1).unwrap();
        for header in sheet.headers() {
            assert!(header_line.contains(header.as_str()), "{title}: {header}");
        }
    }

    fn outcome(install: &str, changed: usize) -> Outcome {
        Outcome {
            install: install.to_string(),
            workspaces: 183,
            chats: 3_531,
            subagents: 11_827,
            changed,
            chats_read: 4,
            duration_ms: 1_250,
        }
    }

    fn sample() -> CacheStats {
        let scan = |name: &str| Scan {
            name: name.to_string(),
            root: format!("/r/{name}"),
            scanned_at: 1_790_000_000_000,
            duration_ms: 900,
            dirty_at: None,
        };
        CacheStats {
            path: PathBuf::from("/h/.crepath/index.db"),
            exists: true,
            size: 12 * 1024 * 1024,
            version: Some(SCHEMA),
            refreshed_at: Some(1_790_000_000_000),
            spawned_at: None,
            holder: Some(Holder {
                pid: 4242,
                started: 1_790_000_060_000,
                alive: true,
            }),
            log: Some("2026-09-24 15:00:00 pid 4242 refresh started".to_string()),
            installs: vec![
                InstallRow {
                    name: "default".to_string(),
                    root: "~/Library/Application Support/Cursor".to_string(),
                    found: true,
                    scan: Some(scan("default")),
                    counts: Counts {
                        workspaces: 183,
                        chats: 3_531,
                        subagents: 11_827,
                    },
                    stale: Some(Staleness {
                        sources: 3,
                        reasons: vec![
                            "global db changed".to_string(),
                            "1 workspace changed".to_string(),
                        ],
                    }),
                },
                InstallRow {
                    name: "resolved".to_string(),
                    root: "~/.cursor-resolved".to_string(),
                    found: true,
                    scan: Some(scan("resolved")),
                    counts: Counts {
                        workspaces: 12,
                        chats: 40,
                        subagents: 0,
                    },
                    stale: Some(Staleness::default()),
                },
                InstallRow {
                    name: "debtt".to_string(),
                    root: "~/.cursor-debtt".to_string(),
                    found: true,
                    scan: None,
                    counts: Counts::default(),
                    stale: None,
                },
                InstallRow {
                    name: "gone".to_string(),
                    root: "~/.cursor-gone".to_string(),
                    found: false,
                    scan: Some(scan("gone")),
                    counts: Counts {
                        workspaces: 1,
                        chats: 2,
                        subagents: 0,
                    },
                    stale: None,
                },
            ],
        }
    }

    #[test]
    fn cache_tables_name_their_units() {
        let stats = sample();
        let now = 1_790_000_120_000;
        assert_units("installations", &installations(Theme::plain(), &stats, now));
        assert_units(
            "scan",
            &scan_sheet(
                Theme::plain(),
                &[outcome("default", 3), outcome("resolved", 0)],
                2_500,
            ),
        );
        assert_units("overview", &overview(Theme::plain(), &stats, now));
        let out = render_stats(Theme::plain(), &stats, now);
        for text in [
            "==> Index",
            "==> Installations",
            "12.0 MB",
            "version 1",
            "running (pid 4242",
            "refresh started",
            "│ installation │ last scan",
            "│ scan time │ workspaces │ chats │ subagents │ stale sources │ reason",
            "global db changed, 1 workspace changed",
            "up to date",
            "not scanned yet",
            "installation not found",
            "11,827",
            "(2m ago)",
            "900ms",
        ] {
            assert!(out.contains(text), "missing {text:?} in\n{out}");
        }
        assert_eq!(
            ui::layout::strip_ansi(&render_stats(Theme::colored(), &stats, now)),
            out
        );
        let scan = render_scan(
            Theme::plain(),
            &ScanResult {
                outcomes: vec![outcome("default", 3), outcome("resolved", 0)],
                duration_ms: 2_500,
                full: true,
                path: PathBuf::from("/h/.crepath/index.db"),
            },
        );
        for text in [
            "==> Index rebuild",
            "│ installation      │ workspaces │ chats │ subagents │ changed sources │ duration │",
            "7,062",
            "23,654",
            "all installations",
            "2 installations scanned in 2.5s, 8 chats read from Cursor",
        ] {
            assert!(scan.contains(text), "missing {text:?} in\n{scan}");
        }
        let stale = render_stale(Theme::plain(), &stats);
        assert!(
            stale.contains("│ installation │ stale sources │ reason"),
            "{stale}"
        );
        assert!(!stale.contains("gone"), "{stale}");
    }
}

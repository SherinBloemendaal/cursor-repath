//! Rendered views for `ls`, `ls <id>`, and `history`, plus the shared workspace naming.

use chrono::{DateTime, Local, Utc};
use comfy_table::{Attribute, Cell, CellAlignment, Color};
use owo_colors::Style;
use serde_json::Value;
use std::cmp::Reverse;
use std::path::{Path, PathBuf};

use super::{Kind, Workspace};
use crate::cursor::registry::ComposerHeader;
use crate::cursor::workspace::is_workspace_file;
use crate::ui::{self, Align, Sheet, Theme};

#[derive(Debug, Clone)]
pub struct ListRow {
    pub workspace: Workspace,
    pub chats: usize,
    pub subagents: usize,
    pub archived: usize,
    pub size: u64,
}

pub fn sort_rows(rows: &mut [ListRow]) {
    rows.sort_by(|a, b| {
        b.workspace
            .destination_missing
            .cmp(&a.workspace.destination_missing)
            .then_with(|| b.size.cmp(&a.size))
            .then_with(|| location(&a.workspace).cmp(&location(&b.workspace)))
            .then_with(|| a.workspace.id.cmp(&b.workspace.id))
    });
}

pub fn location(workspace: &Workspace) -> String {
    workspace
        .path
        .as_ref()
        .map(|path| path.display().to_string())
        .or_else(|| workspace.uri.clone())
        .unwrap_or_else(|| "-".to_string())
}

pub fn display_name(kind: Option<&Kind>, path: Option<&Path>, id: &str) -> String {
    match kind {
        Some(Kind::EmptyWindow) => "empty window".to_string(),
        Some(Kind::Unsaved) => format!("unsaved {}", unsaved_stamp(path, id)),
        Some(Kind::Remote) => path
            .map(|path| path.display().to_string())
            .unwrap_or_else(|| id.to_string()),
        Some(Kind::Folder | Kind::CodeWorkspace) | None => path
            .map(|path| ui::home_relative(&path.display().to_string()))
            .unwrap_or_else(|| id.to_string()),
    }
}

pub fn workspace_name(workspace: &Workspace) -> String {
    display_name(
        Some(&workspace.kind),
        workspace.path.as_deref(),
        &workspace.id,
    )
}

fn unsaved_stamp(path: Option<&Path>, id: &str) -> String {
    let digits = |text: &str| !text.is_empty() && text.chars().all(|ch| ch.is_ascii_digit());
    path.and_then(|path| {
        let dir = if path.file_name().and_then(|name| name.to_str()) == Some("workspace.json") {
            path.parent()?
        } else {
            path
        };
        dir.file_name()
            .and_then(|name| name.to_str())
            .filter(|name| digits(name))
            .map(str::to_string)
    })
    .unwrap_or_else(|| id.to_string())
}

pub fn header_workspace(header: &ComposerHeader) -> (Option<Kind>, Option<PathBuf>) {
    let id = header.workspace_id.as_str();
    if id == "empty-window" {
        return (Some(Kind::EmptyWindow), None);
    }
    let target = serde_json::from_str::<Value>(&header.value)
        .ok()
        .and_then(|json| {
            let identifier = json.get("workspaceIdentifier")?;
            [("uri", false), ("configPath", true)]
                .into_iter()
                .find_map(|(key, config)| {
                    let value = identifier.get(key)?;
                    let scheme = value
                        .get("scheme")
                        .and_then(Value::as_str)
                        .unwrap_or("file");
                    let remote = !scheme.eq_ignore_ascii_case("file");
                    let field = if remote { "path" } else { "fsPath" };
                    let path = value.get(field).and_then(Value::as_str)?;
                    Some((PathBuf::from(path), config, remote))
                })
        });
    match target {
        Some((path, _, true)) => (Some(Kind::Remote), Some(path)),
        Some((path, false, false)) => (Some(Kind::Folder), Some(path)),
        Some((path, true, false)) => {
            let untitled =
                path.file_name().and_then(|name| name.to_str()) == Some("workspace.json");
            let kind = if untitled && is_workspace_file(&path) {
                Kind::Unsaved
            } else {
                Kind::CodeWorkspace
            };
            (Some(kind), Some(path))
        }
        None if !id.is_empty() && id.chars().all(|ch| ch.is_ascii_digit()) => {
            (Some(Kind::Unsaved), None)
        }
        None => (None, None),
    }
}

pub fn short_hash(id: &str) -> &str {
    if id.len() == 32 && id.chars().all(|ch| ch.is_ascii_hexdigit()) {
        &id[..8]
    } else {
        id
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Destination {
    Present,
    Missing,
    None,
}

fn destination(workspace: &Workspace) -> Destination {
    match workspace.kind {
        Kind::Unsaved | Kind::EmptyWindow | Kind::Remote => Destination::None,
        Kind::Folder | Kind::CodeWorkspace if workspace.destination_missing => Destination::Missing,
        Kind::Folder | Kind::CodeWorkspace => Destination::Present,
    }
}

pub fn kind_cell(theme: Theme, kind: Option<&Kind>) -> Cell {
    let bullet = theme.icons().bullet;
    match kind {
        Some(kind) => {
            let (color, attributes): (Option<Color>, &[Attribute]) = match kind {
                Kind::Folder => (Some(Color::Blue), &[Attribute::Bold]),
                Kind::CodeWorkspace => (Some(Color::Magenta), &[Attribute::Bold]),
                Kind::Unsaved => (Some(Color::Yellow), &[Attribute::Bold]),
                Kind::Remote => (Some(Color::Green), &[Attribute::Bold]),
                Kind::EmptyWindow => (None, &[Attribute::Dim]),
            };
            theme.cell(format!("{bullet} {}", kind.label()), color, attributes)
        }
        None => theme.cell(format!("{bullet} unknown"), None, &[Attribute::Dim]),
    }
}

fn profile_cell(theme: Theme, profile: &str) -> Cell {
    if profile == "default" {
        theme.cell(profile, None, &[Attribute::Dim])
    } else {
        theme.cell(profile, Some(Color::Cyan), &[Attribute::Bold])
    }
}

fn status_cell(theme: Theme, workspace: &Workspace) -> Cell {
    let icons = theme.icons();
    match destination(workspace) {
        Destination::Present => theme.cell(icons.check, Some(Color::Green), &[Attribute::Bold]),
        Destination::Missing => theme.cell(icons.cross, Some(Color::Red), &[Attribute::Bold]),
        Destination::None => theme.cell(icons.hollow, None, &[Attribute::Dim]),
    }
}

fn path_cell(theme: Theme, workspace: &Workspace, text: String) -> Cell {
    match destination(workspace) {
        Destination::Present => theme.cell(text, Some(Color::Cyan), &[]),
        Destination::Missing => theme.cell(text, Some(Color::Red), &[]),
        Destination::None => theme.cell(text, None, &[Attribute::Dim]),
    }
}

pub fn hash_cell(theme: Theme, id: &str) -> Cell {
    theme.cell(id, Some(Color::Magenta), &[Attribute::Dim])
}

const LIST_COLUMNS: [(&str, Align); 8] = [
    ("dest", Align::Left),
    ("workspace", Align::Left),
    ("kind", Align::Left),
    ("profile", Align::Left),
    ("chats", Align::Right),
    ("subagents", Align::Right),
    ("size", Align::Right),
    ("hash", Align::Left),
];
fn path_room(theme: Theme, rows: &[ListRow], total: usize) -> Option<usize> {
    let icons = theme.icons();
    let column = |index: usize, cell: &dyn Fn(&ListRow) -> String| {
        let cells: Vec<String> = rows.iter().map(cell).collect();
        ui::column_width(LIST_COLUMNS[index].0, cells.iter().map(String::as_str))
    };
    let others = [
        ui::column_width(LIST_COLUMNS[0].0, [icons.check, icons.cross, icons.hollow]),
        column(2, &|row| {
            format!("{} {}", icons.bullet, row.workspace.kind.label())
        }),
        column(3, &|row| row.workspace.profile.clone()),
        column(4, &|row| ui::count(row.chats as u64)),
        column(5, &|row| ui::count(row.subagents as u64)),
        column(6, &|row| ui::format_size(row.size)),
        column(7, &|row| short_hash(&row.workspace.id).to_string()),
    ];
    ui::flex_room(total, &others)
}

pub fn render_list(theme: Theme, rows: &[ListRow]) -> String {
    render_list_at(theme, rows, ui::table_width())
}

pub fn render_list_at(theme: Theme, rows: &[ListRow], total: Option<usize>) -> String {
    if rows.is_empty() {
        return format!("{}\n", ui::info_line(theme, "No workspaces found."));
    }
    let room = total.and_then(|total| path_room(theme, rows, total));
    let mut sheet = Sheet::new(theme, &LIST_COLUMNS).flex(1);
    for row in rows {
        let workspace = &row.workspace;
        let name = workspace_name(workspace);
        let name = match room {
            Some(room) => ui::layout::keep_tail(&name, room, theme.icons().ellipsis),
            None => name,
        };
        sheet.row(vec![
            status_cell(theme, workspace),
            path_cell(theme, workspace, name),
            kind_cell(theme, Some(&workspace.kind)),
            profile_cell(theme, &workspace.profile),
            theme.count_cell(row.chats, None),
            theme.count_cell(row.subagents, Some(Color::Magenta)),
            theme.size_cell(row.size),
            hash_cell(theme, short_hash(&workspace.id)),
        ]);
    }
    format!(
        "{}\n{sheet}\n{}\n",
        ui::section_line(theme, "Workspaces"),
        list_footer(theme, rows)
    )
}

fn list_footer(theme: Theme, rows: &[ListRow]) -> String {
    let icons = theme.icons();
    let missing = rows
        .iter()
        .filter(|row| destination(&row.workspace) == Destination::Missing)
        .count();
    let chats: usize = rows.iter().map(|row| row.chats).sum();
    let subagents: usize = rows.iter().map(|row| row.subagents).sum();
    let size: u64 = rows.iter().map(|row| row.size).sum();
    let missing = if missing == 0 {
        format!(
            "{} {}",
            theme.good(icons.check),
            theme.good("no missing destinations")
        )
    } else {
        format!(
            "{} {}",
            theme.paint(icons.cross, Style::new().bold().red()),
            theme.bad(ui::plural(
                missing,
                "missing destination",
                "missing destinations"
            ))
        )
    };
    let parts = [
        format!(
            "{} {}",
            theme.paint(icons.square, Style::new().blue()),
            theme.bold(ui::plural(rows.len(), "workspace", "workspaces"))
        ),
        missing,
        format!(
            "{} {}, {}",
            theme.paint(icons.diamond, Style::new().cyan()),
            theme.bold(ui::plural(chats, "chat", "chats")),
            ui::plural(subagents, "subagent", "subagents")
        ),
        format!(
            "{} {} on disk",
            theme.paint(icons.bullet, Style::new().yellow()),
            theme.size(size)
        ),
    ];
    format!("  {}", parts.join(&theme.sep()))
}

pub fn render_detail(theme: Theme, row: &ListRow, headers: &[ComposerHeader]) -> String {
    let icons = theme.icons();
    let workspace = &row.workspace;
    let now = Utc::now().timestamp_millis();
    let last_active = headers
        .iter()
        .filter_map(|header| header.last_updated_at)
        .max();
    let destination_cell = match destination(workspace) {
        Destination::Present => {
            theme.cell(format!("{} exists", icons.check), Some(Color::Green), &[])
        }
        Destination::Missing => theme.cell(
            format!("{} missing on disk", icons.cross),
            Some(Color::Red),
            &[Attribute::Bold],
        ),
        Destination::None => theme.cell(
            format!("{} no destination", icons.hollow),
            None,
            &[Attribute::Dim],
        ),
    };
    let unit = |value: usize, one: &str, many: &str, color: Option<Color>| {
        theme.cell(
            ui::plural(value, one, many),
            color,
            if value == 0 {
                &[Attribute::Dim]
            } else {
                &[Attribute::Bold]
            },
        )
    };
    let mut rows = vec![
        (
            "Name",
            theme.cell(workspace_name(workspace), None, &[Attribute::Bold]),
        ),
        ("Path", path_cell(theme, workspace, location(workspace))),
        ("Destination", destination_cell),
        ("Kind", kind_cell(theme, Some(&workspace.kind))),
        ("Profile", profile_cell(theme, &workspace.profile)),
        ("Hash", theme.cell(&workspace.id, Some(Color::Magenta), &[])),
    ];
    if let Some(uri) = &workspace.uri {
        rows.push(("URI", theme.cell(uri, None, &[Attribute::Dim])));
    }
    rows.extend([
        (
            "Storage",
            theme.cell(workspace.dir.display(), Some(Color::Cyan), &[]),
        ),
        ("Chats", unit(row.chats, "chat", "chats", None)),
        (
            "Subagents",
            unit(
                row.subagents,
                "subagent chat",
                "subagent chats",
                Some(Color::Magenta),
            ),
        ),
        (
            "Archived",
            unit(
                row.archived,
                "archived chat",
                "archived chats",
                Some(Color::Yellow),
            ),
        ),
        ("Storage size", theme.size_cell(row.size)),
        (
            "Last active",
            match last_active.and_then(ui::datetime) {
                Some(stamp) => theme.cell(
                    format!("{stamp} ({})", ui::ago(last_active.unwrap_or(0), now)),
                    None,
                    &[],
                ),
                None => theme.cell("never", None, &[Attribute::Dim]),
            },
        ),
    ]);
    let mut out = format!(
        "{}\n{}\n",
        ui::section_line(theme, "Workspace"),
        ui::panel(theme, rows)
    );
    out.push('\n');
    out.push_str(&ui::section_line(
        theme,
        &format!("Chats ({})", ui::plural(headers.len(), "chat", "chats")),
    ));
    out.push('\n');
    if headers.is_empty() {
        out.push_str(&ui::hint_line(theme, "No chats in this workspace."));
        out.push('\n');
        return out;
    }
    let mut ordered: Vec<&ComposerHeader> = headers.iter().collect();
    ordered.sort_by_key(|header| {
        (
            Reverse(header.last_updated_at.unwrap_or(i64::MIN)),
            header.composer_id.clone(),
        )
    });
    let mut sheet = Sheet::new(
        theme,
        &[
            ("title", Align::Left),
            ("type", Align::Left),
            ("state", Align::Left),
            ("updated", Align::Left),
            ("chat id", Align::Left),
        ],
    )
    .flex(0);
    for header in ordered {
        let muted = header.is_subagent || header.is_archived;
        let title = match &header.title {
            Some(title) if muted => theme.cell(title, None, &[Attribute::Dim]),
            Some(title) => theme.cell(title, None, &[Attribute::Bold]),
            None => theme.cell("(untitled)", None, &[Attribute::Dim, Attribute::Italic]),
        };
        let kind = if header.is_subagent {
            theme.cell(
                format!("{} subagent", icons.diamond),
                Some(Color::Magenta),
                &[],
            )
        } else {
            theme.cell(format!("{} chat", icons.bullet), Some(Color::Cyan), &[])
        };
        let state = if header.is_archived {
            theme.cell("archived", Some(Color::Yellow), &[])
        } else {
            theme.cell("active", Some(Color::Green), &[])
        };
        let updated = match header.last_updated_at.and_then(ui::datetime) {
            Some(stamp) => theme.cell(stamp, None, &[]),
            None => theme.cell("never", None, &[Attribute::Dim]),
        };
        sheet.row(vec![
            title,
            kind,
            state,
            updated,
            hash_cell(theme, &header.composer_id),
        ]);
    }
    out.push_str(&format!("{sheet}\n"));
    let archived = headers.iter().filter(|header| header.is_archived).count();
    out.push_str(&format!(
        "  {}\n",
        [
            theme.bold(ui::plural(row.chats, "chat", "chats")),
            ui::plural(row.subagents, "subagent chat", "subagent chats"),
            ui::plural(archived, "archived", "archived"),
        ]
        .join(&theme.sep())
    ));
    out
}

struct HistoryEntry {
    at: Option<DateTime<Local>>,
    command: String,
    args: String,
    outcome: String,
    duration_ms: Option<u64>,
}

fn parse_history(line: &str) -> Option<HistoryEntry> {
    let json: Value = serde_json::from_str(line).ok()?;
    let at = json
        .get("at")
        .and_then(|v| v.as_str())
        .and_then(|raw| DateTime::parse_from_rfc3339(raw).ok())
        .map(|stamp| stamp.with_timezone(&Local));
    let args = json
        .get("args")
        .and_then(|v| v.as_array())
        .map(|items| {
            items
                .iter()
                .filter_map(|item| item.as_str())
                .filter(|item| !item.is_empty())
                .collect::<Vec<_>>()
                .join(" ")
        })
        .unwrap_or_default();
    Some(HistoryEntry {
        at,
        command: json
            .get("command")
            .and_then(|v| v.as_str())
            .unwrap_or("?")
            .to_string(),
        args,
        outcome: json
            .get("outcome")
            .and_then(|v| v.as_str())
            .unwrap_or("?")
            .to_string(),
        duration_ms: json.get("duration_ms").and_then(|v| v.as_u64()),
    })
}

pub fn render_history(theme: Theme, raw: Option<&str>) -> String {
    let icons = theme.icons();
    let lines: Vec<&str> = raw
        .map(|text| {
            text.lines()
                .filter(|line| !line.trim().is_empty())
                .collect()
        })
        .unwrap_or_default();
    if lines.is_empty() {
        return format!("{}\n", ui::info_line(theme, "No history yet."));
    }
    let mut sheet = Sheet::new(
        theme,
        &[
            ("result", Align::Left),
            ("when", Align::Left),
            ("command", Align::Left),
            ("args", Align::Left),
            ("duration", Align::Right),
        ],
    )
    .flex(3);
    let mut failures = 0usize;
    for line in &lines {
        let Some(entry) = parse_history(line) else {
            sheet.row(vec![
                theme.cell("? unreadable", None, &[Attribute::Dim]),
                theme.cell("-", None, &[Attribute::Dim]),
                theme.cell("-", None, &[Attribute::Dim]),
                theme.cell(line, None, &[Attribute::Dim]),
                theme.cell("-", None, &[Attribute::Dim]),
            ]);
            continue;
        };
        let result = match entry.outcome.as_str() {
            "ok" => theme.cell(
                format!("{} ok", icons.check),
                Some(Color::Green),
                &[Attribute::Bold],
            ),
            "error" => {
                failures += 1;
                theme.cell(
                    format!("{} error", icons.cross),
                    Some(Color::Red),
                    &[Attribute::Bold],
                )
            }
            other => theme.cell(other, Some(Color::Yellow), &[]),
        };
        sheet.row(vec![
            result,
            match entry.at {
                Some(stamp) => theme.cell(stamp.format("%Y-%m-%d %H:%M:%S"), None, &[]),
                None => theme.cell("-", None, &[Attribute::Dim]),
            },
            theme.cell(&entry.command, Some(Color::Cyan), &[Attribute::Bold]),
            if entry.args.is_empty() {
                theme.cell("-", None, &[Attribute::Dim])
            } else {
                theme.token_cell(&entry.args)
            },
            match entry.duration_ms {
                Some(ms) => theme
                    .cell(ui::duration(ms), None, &[Attribute::Dim])
                    .set_alignment(CellAlignment::Right),
                None => theme.cell("-", None, &[Attribute::Dim]),
            },
        ]);
    }
    let mut summary = vec![theme.bold(ui::plural(lines.len(), "command", "commands"))];
    summary.push(if failures == 0 {
        theme.good(format!("{} no failures", icons.check))
    } else {
        theme.bad(format!(
            "{} {}",
            icons.cross,
            ui::plural(failures, "failure", "failures")
        ))
    });
    format!(
        "{}\n{sheet}\n  {}\n",
        ui::section_line(theme, "History (last 30 days)"),
        summary.join(&theme.sep())
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    fn row(id: &str, missing: bool, size: u64) -> ListRow {
        ListRow {
            workspace: Workspace {
                id: id.to_string(),
                dir: PathBuf::from("/tmp/storage").join(id),
                kind: Kind::Folder,
                uri: Some(format!("file:///tmp/{id}")),
                path: Some(PathBuf::from(format!("/tmp/{id}"))),
                profile: "default".to_string(),
                destination_missing: missing,
            },
            chats: 1,
            subagents: 0,
            archived: 0,
            size,
        }
    }

    fn header(workspace: &str, value: &str) -> ComposerHeader {
        ComposerHeader {
            composer_id: "c1".to_string(),
            workspace_id: workspace.to_string(),
            created_at: None,
            last_updated_at: None,
            is_archived: false,
            is_subagent: false,
            recency: None,
            checkpoint_at: None,
            value: value.to_string(),
            subagent_type_name: None,
            title: None,
        }
    }

    #[test]
    fn missing_first_then_largest() {
        let mut rows = vec![
            row("small-ok", false, 10),
            row("big-missing", true, 500),
            row("big-ok", false, 900),
            row("small-missing", true, 5),
        ];
        sort_rows(&mut rows);
        let order: Vec<&str> = rows.iter().map(|row| row.workspace.id.as_str()).collect();
        assert_eq!(
            order,
            ["big-missing", "small-missing", "big-ok", "small-ok"]
        );
    }

    #[test]
    fn plain_list_has_markers_and_footer() {
        let mut rows = vec![row("aaa", false, 2048), row("bbb", true, 1024)];
        sort_rows(&mut rows);
        let out = render_list(Theme::plain(), &rows);
        assert!(!out.contains('\u{1b}'));
        assert!(out.starts_with("==> Workspaces\n╭"));
        assert!(out.contains("│ dest │ workspace │ kind"));
        assert!(out.contains("│ ✖    │ /tmp/bbb"));
        assert!(out.contains("│ ✔    │ /tmp/aaa"));
        assert!(out.contains("● folder"));
        assert!(out.contains("2 workspaces"));
        assert!(out.contains("1 missing destination"));
        assert!(out.contains("2 chats, 0 subagents"));
        assert!(out.contains("3.0 KB on disk"));
        assert!(out.find("/tmp/bbb").unwrap() < out.find("/tmp/aaa").unwrap());
    }

    #[test]
    fn list_shows_short_hashes_and_readable_names() {
        let mut unsaved = row("9d95c4710638ebad3cb7aaa3bec9a067", false, 10);
        unsaved.workspace.kind = Kind::Unsaved;
        unsaved.workspace.path = Some(PathBuf::from(
            "/x/Cursor/Workspaces/1778826556058/workspace.json",
        ));
        let mut empty = row("empty-window", false, 5);
        empty.workspace.kind = Kind::EmptyWindow;
        empty.workspace.path = None;
        let out = render_list(Theme::plain(), &[unsaved, empty]);
        assert!(out.contains("unsaved 1778826556058"));
        assert!(out.contains("│ 9d95c471     │"));
        assert!(!out.contains("9d95c4710638"));
        assert!(out.contains("empty window"));
        assert!(out.contains("│ empty-window │"));
    }

    #[test]
    fn colored_list_keeps_the_plain_layout() {
        let rows = [row("bbb", true, 1024), row("aaa", false, 2048)];
        let colored = render_list(Theme::colored(), &rows);
        assert!(colored.contains('\u{1b}'));
        assert_eq!(
            ui::layout::strip_ansi(&colored),
            render_list(Theme::plain(), &rows)
        );
    }

    #[test]
    fn narrow_terminals_shorten_paths_instead_of_wrapping() {
        let mut long = row("ccc", false, 4096);
        long.workspace.path = Some(PathBuf::from(format!(
            "/tmp/{}/deep/tail-part",
            "x".repeat(80)
        )));
        let rows = [long, row("aaa", false, 2048)];
        for total in [100, 120] {
            let out = render_list_at(Theme::plain(), &rows, Some(total));
            let table: Vec<&str> = out.lines().skip(1).take(6).collect();
            assert_eq!(table.len(), 6);
            for line in &table {
                assert_eq!(ui::layout::width(line), ui::layout::width(table[0]));
                assert!(ui::layout::width(line) <= total, "{line}");
            }
            assert!(out.contains("…"));
            assert!(out.contains("deep/tail-part"));
            assert!(out.contains("/tmp/aaa "));
        }
        let wide = render_list_at(Theme::plain(), &rows, None);
        assert!(wide.contains(&format!("/tmp/{}/deep/tail-part", "x".repeat(80))));
    }

    #[test]
    fn ascii_list_has_no_unicode() {
        let out = render_list(Theme::ascii(), &[row("bbb", true, 1024)]);
        assert!(out.is_ascii());
        assert!(out.contains("| x    |"));
    }

    #[test]
    fn header_workspaces_resolve_to_names() {
        let folder = header(
            "2a360a8fa4aad0701aad155ccadf46e7",
            r#"{"workspaceIdentifier":{"id":"2a360a8fa4aad0701aad155ccadf46e7","uri":{"fsPath":"/tmp/infra"}}}"#,
        );
        let (kind, path) = header_workspace(&folder);
        assert_eq!(kind, Some(Kind::Folder));
        assert_eq!(
            display_name(kind.as_ref(), path.as_deref(), "x"),
            "/tmp/infra"
        );
        let config = header(
            "9c7b190cfff266c8944e9777abb88ca9",
            r#"{"workspaceIdentifier":{"configPath":{"fsPath":"/tmp/r/resolved.code-workspace"}}}"#,
        );
        assert_eq!(header_workspace(&config).0, Some(Kind::CodeWorkspace));
        let untitled = header(
            "9d95c4710638ebad3cb7aaa3bec9a067",
            r#"{"workspaceIdentifier":{"configPath":{"fsPath":"/x/Cursor/Workspaces/1778826556058/workspace.json"}}}"#,
        );
        let (kind, path) = header_workspace(&untitled);
        assert_eq!(kind, Some(Kind::Unsaved));
        assert_eq!(
            display_name(kind.as_ref(), path.as_deref(), "x"),
            "unsaved 1778826556058"
        );
        let remote = header(
            "5f1c0e2a9b7d4c3e8f6a1b2c3d4e5f60",
            r#"{"workspaceIdentifier":{"uri":{"scheme":"vscode-remote","authority":"ssh-remote+box","path":"/srv/app"}}}"#,
        );
        let (kind, path) = header_workspace(&remote);
        assert_eq!(kind, Some(Kind::Remote));
        assert_eq!(
            display_name(kind.as_ref(), path.as_deref(), "x"),
            "/srv/app"
        );
        let unsaved = header(
            "1780204438555",
            r#"{"workspaceIdentifier":{"id":"1780204438555"}}"#,
        );
        let (kind, path) = header_workspace(&unsaved);
        assert_eq!(kind, Some(Kind::Unsaved));
        assert_eq!(
            display_name(kind.as_ref(), path.as_deref(), "1780204438555"),
            "unsaved 1780204438555"
        );
        let empty = header("empty-window", "{}");
        assert_eq!(
            display_name(header_workspace(&empty).0.as_ref(), None, "empty-window"),
            "empty window"
        );
        let unknown = header("abc", "not json");
        assert_eq!(header_workspace(&unknown), (None, None));
    }

    #[test]
    fn history_renders_a_table() {
        let raw = concat!(
            r#"{"at":"2026-09-24T10:00:00+00:00","command":"ls","args":[""],"outcome":"ok","duration_ms":12}"#,
            "\n",
            r#"{"at":"2026-09-24T10:01:00+00:00","command":"mv","args":["/a","/b"],"outcome":"error","duration_ms":1500}"#,
            "\n"
        );
        let out = render_history(Theme::plain(), Some(raw));
        assert!(out.starts_with("==> History"));
        assert!(out.contains("│ result  │ when"));
        assert!(out.contains("│ duration │"));
        assert!(out.contains("✔ ok"));
        assert!(out.contains("✖ error"));
        assert!(out.contains("/a /b"));
        assert!(out.contains("1.5s"));
        assert!(out.contains("2 commands"));
        assert!(out.contains("1 failure"));
        assert_eq!(render_history(Theme::plain(), None), "ℹ No history yet.\n");
    }
}

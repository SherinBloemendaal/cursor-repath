//! Aggregate Cursor usage from the local databases.

use anyhow::Result;
use chrono::{TimeZone, Utc};
use comfy_table::{Attribute, Cell, CellAlignment, Color};
use rusqlite::Connection;
use serde_json::Value;
use std::cmp::Reverse;
use std::collections::{BTreeMap, HashMap};
use std::fs;
use std::path::Path;

use super::view::{display_name, header_workspace, kind_cell, workspace_name};
use super::{Kind, Runtime, dir_size, discover};
use crate::cursor::registry::{ComposerHeader, load_headers};
use crate::engine::db;
use crate::ui::{self, Align, Sheet, Theme};

const TOP: usize = 10;

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Named {
    pub name: String,
    pub kind: Option<Kind>,
    pub value: u64,
}

#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct Usage {
    pub conversations: usize,
    pub cost_cents: u64,
    pub requests: u64,
    pub cost_chats: usize,
    pub context_tokens: u64,
    pub context_chats: usize,
    pub input_tokens: u64,
    pub output_tokens: u64,
    pub token_chats: usize,
}

pub struct Stats {
    pub profiles: usize,
    pub workspaces: usize,
    pub by_kind: Vec<(Kind, usize)>,
    pub chats: usize,
    pub subagents: usize,
    pub archived: usize,
    pub usage: Usage,
    pub models: BTreeMap<String, usize>,
    pub per_month: BTreeMap<String, usize>,
    pub busiest: Vec<Named>,
    pub global_db: Option<u64>,
    pub workspace_dbs: Vec<Named>,
}

pub fn render(rt: &Runtime, theme: Theme) -> Result<String> {
    Ok(render_stats(theme, &collect(rt)?))
}

pub fn collect(rt: &Runtime) -> Result<Stats> {
    let workspaces = discover(rt)?;
    let mut by_kind: Vec<(Kind, usize)> = Vec::new();
    for workspace in &workspaces {
        match by_kind.iter_mut().find(|(kind, _)| *kind == workspace.kind) {
            Some((_, count)) => *count += 1,
            None => by_kind.push((workspace.kind.clone(), 1)),
        }
    }
    by_kind.sort_by(|a, b| b.1.cmp(&a.1).then_with(|| a.0.label().cmp(b.0.label())));
    let conn = if rt.layout.global_db().exists() {
        Some(db::open_ro(&rt.layout.global_db())?)
    } else {
        None
    };
    let headers = match &conn {
        Some(conn) => load_headers(conn)?,
        None => Vec::new(),
    };
    let chats = headers.iter().filter(|header| !header.is_subagent).count();
    let subagents = headers.iter().filter(|header| header.is_subagent).count();
    let archived = headers.iter().filter(|header| header.is_archived).count();
    let mut per_month: BTreeMap<String, usize> = BTreeMap::new();
    let mut per_workspace: BTreeMap<String, (usize, &ComposerHeader)> = BTreeMap::new();
    for header in &headers {
        if header.is_subagent {
            continue;
        }
        per_workspace
            .entry(header.workspace_id.clone())
            .or_insert((0, header))
            .0 += 1;
        if let Some(created) = header.created_at
            && let Some(stamp) = Utc.timestamp_millis_opt(created).single()
        {
            *per_month
                .entry(stamp.format("%Y-%m").to_string())
                .or_default() += 1;
        }
    }
    let (usage, models) = match &conn {
        Some(conn) => usage(conn, &headers)?,
        None => (
            Usage {
                conversations: headers.len(),
                ..Usage::default()
            },
            BTreeMap::new(),
        ),
    };
    let known: HashMap<&str, (String, Kind)> = workspaces
        .iter()
        .map(|workspace| {
            (
                workspace.id.as_str(),
                (workspace_name(workspace), workspace.kind.clone()),
            )
        })
        .collect();
    let mut busiest: Vec<Named> = per_workspace
        .into_iter()
        .map(|(id, (count, header))| {
            let (name, kind) = match known.get(id.as_str()) {
                Some((name, kind)) => (name.clone(), Some(kind.clone())),
                None => {
                    let (kind, path) = header_workspace(header);
                    (display_name(kind.as_ref(), path.as_deref(), &id), kind)
                }
            };
            Named {
                name,
                kind,
                value: count as u64,
            }
        })
        .collect();
    busiest.sort_by(|a, b| b.value.cmp(&a.value).then_with(|| a.name.cmp(&b.name)));
    let mut workspace_dbs: Vec<Named> = workspaces
        .iter()
        .filter_map(|workspace| {
            file_len(&workspace.dir.join("state.vscdb")).map(|size| Named {
                name: workspace_name(workspace),
                kind: Some(workspace.kind.clone()),
                value: size,
            })
        })
        .collect();
    workspace_dbs.sort_by(|a, b| b.value.cmp(&a.value).then_with(|| a.name.cmp(&b.name)));
    Ok(Stats {
        profiles: profile_count(rt),
        workspaces: workspaces.len(),
        by_kind,
        chats,
        subagents,
        archived,
        usage,
        models,
        per_month,
        busiest,
        global_db: file_len(&rt.layout.global_db()),
        workspace_dbs,
    })
}

fn left(theme: Theme, text: impl std::fmt::Display) -> Cell {
    theme.cell(text, None, &[])
}

fn number(theme: Theme, value: u64) -> Cell {
    theme
        .cell(ui::count(value), None, &[Attribute::Bold])
        .set_alignment(CellAlignment::Right)
}

fn dim(theme: Theme, text: impl std::fmt::Display) -> Cell {
    theme.cell(text, None, &[Attribute::Dim])
}

fn money(cents: u64) -> String {
    format!("${}.{:02}", ui::count(cents / 100), cents % 100)
}

fn coverage(part: usize, whole: usize) -> String {
    format!(
        "recorded in {} of {}",
        ui::count(part as u64),
        ui::plural(whole, "conversation", "conversations")
    )
}

fn kind_summary(stats: &Stats) -> String {
    stats
        .by_kind
        .iter()
        .map(|(kind, count)| {
            let label = match kind {
                Kind::EmptyWindow => "empty window",
                other => other.label(),
            };
            format!("{} {label}", ui::count(*count as u64))
        })
        .collect::<Vec<_>>()
        .join(", ")
}

fn overview(theme: Theme, stats: &Stats) -> Sheet {
    let mut sheet = Sheet::new(
        theme,
        &[
            ("metric", Align::Left),
            ("value", Align::Right),
            ("detail", Align::Left),
        ],
    )
    .flex(2);
    let usage = &stats.usage;
    let metric = |name: &str| theme.cell(name, Some(Color::Blue), &[Attribute::Bold]);
    let value = |text: String, color: Option<Color>| {
        theme
            .cell(text, color, &[Attribute::Bold])
            .set_alignment(CellAlignment::Right)
    };
    let workspace_db_total: u64 = stats.workspace_dbs.iter().map(|named| named.value).sum();
    let mut rows = vec![
        (
            "Profiles",
            value(ui::plural(stats.profiles, "profile", "profiles"), None),
            "Cursor profiles in storage.json".to_string(),
        ),
        (
            "Workspaces",
            value(
                ui::plural(stats.workspaces, "workspace", "workspaces"),
                None,
            ),
            kind_summary(stats),
        ),
        (
            "Chats",
            value(ui::plural(stats.chats, "chat", "chats"), None),
            "top-level chats, subagents not included".to_string(),
        ),
        (
            "Subagents",
            value(
                ui::plural(stats.subagents, "subagent chat", "subagent chats"),
                Some(Color::Magenta),
            ),
            "chats started by an agent".to_string(),
        ),
        (
            "Archived",
            value(
                ui::plural(stats.archived, "archived chat", "archived chats"),
                Some(Color::Yellow),
            ),
            "chats and subagent chats marked archived".to_string(),
        ),
        (
            "Usage cost",
            value(money(usage.cost_cents), Some(Color::Green)),
            format!(
                "{}, {}",
                ui::plural(usage.requests as usize, "request", "requests"),
                coverage(usage.cost_chats, usage.conversations)
            ),
        ),
        (
            "Context size",
            value(
                if usage.context_chats == 0 {
                    "none recorded".to_string()
                } else {
                    format!(
                        "avg {} tokens",
                        ui::count(usage.context_tokens / usage.context_chats as u64)
                    )
                },
                Some(Color::Cyan),
            ),
            format!(
                "context window in use at the last turn, {}",
                coverage(usage.context_chats, usage.conversations)
            ),
        ),
        (
            "Message tokens",
            value(
                format!(
                    "{} in, {} out",
                    ui::count(usage.input_tokens),
                    ui::count(usage.output_tokens)
                ),
                Some(Color::Cyan),
            ),
            format!(
                "inline counts only, {}; newer chats store tokens per message",
                coverage(usage.token_chats, usage.conversations)
            ),
        ),
    ];
    if let Some((model, count)) = top_model(&stats.models) {
        rows.push((
            "Top model",
            value(model.to_string(), Some(Color::Cyan)),
            ui::plural(count, "chat", "chats"),
        ));
    }
    if let Some(size) = stats.global_db {
        rows.push((
            "Global db",
            value(ui::format_size(size), None),
            "globalStorage/state.vscdb".to_string(),
        ));
    }
    if !stats.workspace_dbs.is_empty() {
        rows.push((
            "Workspace dbs",
            value(ui::format_size(workspace_db_total), None),
            ui::plural(
                stats.workspace_dbs.len(),
                "state.vscdb file",
                "state.vscdb files",
            ),
        ));
    }
    for (name, cell, detail) in rows {
        sheet.row(vec![metric(name), cell, dim(theme, detail)]);
    }
    sheet
}

fn kinds(theme: Theme, stats: &Stats) -> Sheet {
    let mut sheet = Sheet::new(
        theme,
        &[
            ("kind", Align::Left),
            ("workspaces", Align::Right),
            ("share", Align::Right),
            ("distribution", Align::Left),
        ],
    );
    let total: u64 = stats.by_kind.iter().map(|(_, count)| *count as u64).sum();
    let max = stats
        .by_kind
        .iter()
        .map(|(_, count)| *count as u64)
        .max()
        .unwrap_or(0);
    for (kind, count) in &stats.by_kind {
        sheet.row(vec![
            kind_cell(theme, Some(kind)),
            number(theme, *count as u64),
            theme.share_cell(*count as u64, total),
            theme.bar_cell(*count as u64, max, Color::Blue),
        ]);
    }
    if !sheet.is_empty() {
        sheet.total(vec![
            left(theme, "all kinds"),
            number(theme, total),
            theme.share_cell(total, total),
            left(theme, ""),
        ]);
    }
    sheet
}

fn models(theme: Theme, stats: &Stats) -> Sheet {
    let mut sheet = Sheet::new(
        theme,
        &[
            ("model", Align::Left),
            ("chats", Align::Right),
            ("share", Align::Right),
            ("distribution", Align::Left),
        ],
    );
    let mut models: Vec<(&String, u64)> = stats
        .models
        .iter()
        .map(|(name, count)| (name, *count as u64))
        .collect();
    models.sort_by_key(|(name, count)| (Reverse(*count), (*name).clone()));
    let total: u64 = models.iter().map(|(_, count)| count).sum();
    let max = models.first().map(|(_, count)| *count).unwrap_or(0);
    for (name, count) in models.iter().take(TOP) {
        sheet.row(vec![
            theme.cell(name, Some(Color::Cyan), &[]),
            number(theme, *count),
            theme.share_cell(*count, total),
            theme.bar_cell(*count, max, Color::Cyan),
        ]);
    }
    if models.len() > TOP {
        let rest: u64 = models.iter().skip(TOP).map(|(_, count)| count).sum();
        sheet.row(vec![
            dim(
                theme,
                ui::plural(models.len() - TOP, "other model", "other models"),
            ),
            number(theme, rest),
            theme.share_cell(rest, total),
            theme.bar_cell(rest, max, Color::Cyan),
        ]);
    }
    if !sheet.is_empty() {
        sheet.total(vec![
            left(theme, "all models"),
            number(theme, total),
            theme.share_cell(total, total),
            left(theme, ""),
        ]);
    }
    sheet
}

fn named(
    theme: Theme,
    items: &[Named],
    unit: &str,
    color: Color,
    format: fn(u64) -> String,
) -> Sheet {
    named_at(theme, items, unit, color, format, ui::table_width())
}

fn named_at(
    theme: Theme,
    items: &[Named],
    unit: &str,
    color: Color,
    format: fn(u64) -> String,
    width: Option<usize>,
) -> Sheet {
    let kinds: Vec<String> = items
        .iter()
        .map(|item| kind_cell(theme, item.kind.as_ref()).content())
        .collect();
    let values: Vec<String> = items
        .iter()
        .map(|item| format(item.value))
        .chain([format(items.iter().map(|item| item.value).sum())])
        .collect();
    let room = width.and_then(|width| {
        ui::flex_room(
            width,
            &[
                ui::column_width("kind", kinds.iter().map(String::as_str)),
                ui::column_width(unit, values.iter().map(String::as_str)),
                ui::column_width("share", ["100.0%"]),
                ui::column_width("distribution", [" ".repeat(ui::BAR_SPAN).as_str()]),
            ],
        )
    });
    let fit = |name: &str| match room {
        Some(room) => ui::layout::keep_tail(name, room, theme.icons().ellipsis),
        None => name.to_string(),
    };
    let mut sheet = Sheet::new(
        theme,
        &[
            ("workspace", Align::Left),
            ("kind", Align::Left),
            (unit, Align::Right),
            ("share", Align::Right),
            ("distribution", Align::Left),
        ],
    )
    .flex(0);
    let total: u64 = items.iter().map(|item| item.value).sum();
    let max = items.first().map(|item| item.value).unwrap_or(0);
    let value = |amount: u64| {
        theme
            .cell(format(amount), None, &[Attribute::Bold])
            .set_alignment(CellAlignment::Right)
    };
    for item in items.iter().take(TOP) {
        sheet.row(vec![
            theme.cell(fit(&item.name), Some(Color::Cyan), &[]),
            kind_cell(theme, item.kind.as_ref()),
            value(item.value),
            theme.share_cell(item.value, total),
            theme.bar_cell(item.value, max, color),
        ]);
    }
    if items.len() > TOP {
        let rest: u64 = items.iter().skip(TOP).map(|item| item.value).sum();
        sheet.row(vec![
            dim(
                theme,
                ui::plural(items.len() - TOP, "other workspace", "other workspaces"),
            ),
            left(theme, ""),
            value(rest),
            theme.share_cell(rest, total),
            theme.bar_cell(rest, max, color),
        ]);
    }
    if !sheet.is_empty() {
        sheet.total(vec![
            left(
                theme,
                format!("all {}", ui::plural(items.len(), "workspace", "workspaces")),
            ),
            left(theme, ""),
            value(total),
            theme.share_cell(total, total),
            left(theme, ""),
        ]);
    }
    sheet
}

fn months(theme: Theme, stats: &Stats) -> Sheet {
    let mut sheet = Sheet::new(
        theme,
        &[
            ("month", Align::Left),
            ("chats", Align::Right),
            ("share", Align::Right),
            ("distribution", Align::Left),
        ],
    );
    let total: u64 = stats.per_month.values().map(|count| *count as u64).sum();
    let max = stats
        .per_month
        .values()
        .map(|count| *count as u64)
        .max()
        .unwrap_or(0);
    for (month, count) in &stats.per_month {
        sheet.row(vec![
            left(theme, month),
            number(theme, *count as u64),
            theme.share_cell(*count as u64, total),
            theme.bar_cell(*count as u64, max, Color::Blue),
        ]);
    }
    if !sheet.is_empty() {
        sheet.total(vec![
            left(
                theme,
                format!(
                    "all {}",
                    ui::plural(stats.per_month.len(), "month", "months")
                ),
            ),
            number(theme, total),
            theme.share_cell(total, total),
            left(theme, ""),
        ]);
    }
    sheet
}

pub fn sections(theme: Theme, stats: &Stats) -> Vec<(&'static str, Sheet)> {
    vec![
        ("Overview", overview(theme, stats)),
        ("Workspaces by kind", kinds(theme, stats)),
        ("Top models", models(theme, stats)),
        (
            "Busiest workspaces",
            named(theme, &stats.busiest, "chats", Color::Green, ui::count),
        ),
        ("Chats per month", months(theme, stats)),
        (
            "Largest workspace databases",
            named(
                theme,
                &stats.workspace_dbs,
                "db size",
                Color::Yellow,
                ui::format_size,
            ),
        ),
    ]
}

pub fn render_stats(theme: Theme, stats: &Stats) -> String {
    let mut out = String::new();
    for (index, (title, sheet)) in sections(theme, stats).into_iter().enumerate() {
        if index > 0 {
            out.push('\n');
        }
        out.push_str(&ui::section_line(theme, title));
        out.push('\n');
        if sheet.is_empty() {
            out.push_str(&ui::hint_line(theme, "none yet"));
            out.push('\n');
        } else {
            out.push_str(&format!("{sheet}\n"));
        }
    }
    out
}

fn top_model(models: &BTreeMap<String, usize>) -> Option<(&str, usize)> {
    models
        .iter()
        .max_by(|a, b| a.1.cmp(b.1).then_with(|| b.0.cmp(a.0)))
        .map(|(name, count)| (name.as_str(), *count))
}

fn usage(
    conn: &Connection,
    headers: &[ComposerHeader],
) -> Result<(Usage, BTreeMap<String, usize>)> {
    let mut usage = Usage {
        conversations: headers.len(),
        ..Usage::default()
    };
    let mut models = BTreeMap::new();
    for header in headers {
        let Some(raw) = db::read_text(conn, &format!("composerData:{}", header.composer_id))?
        else {
            continue;
        };
        let Ok(json) = serde_json::from_str::<Value>(&raw) else {
            continue;
        };
        add_usage(&mut usage, &json);
        if header.is_subagent {
            continue;
        }
        if let Some(name) = json
            .pointer("/modelConfig/modelName")
            .and_then(|v| v.as_str())
            .or_else(|| json.get("modelConfig").and_then(|v| v.as_str()))
        {
            *models.entry(name.to_string()).or_default() += 1;
        }
    }
    Ok((usage, models))
}

pub fn add_usage(usage: &mut Usage, json: &Value) {
    let field = |value: &Value, key: &str| value.get(key).and_then(Value::as_u64).unwrap_or(0);
    if let Some(entries) = json.get("usageData").and_then(Value::as_object) {
        let (cents, requests) = entries.values().fold((0, 0), |(cents, requests), entry| {
            (
                cents + field(entry, "costInCents"),
                requests + field(entry, "amount"),
            )
        });
        if cents > 0 || requests > 0 {
            usage.cost_cents += cents;
            usage.requests += requests;
            usage.cost_chats += 1;
        }
    }
    let context = field(json, "contextTokensUsed");
    if context > 0 {
        usage.context_tokens += context;
        usage.context_chats += 1;
    }
    if let Some(messages) = json.get("conversation").and_then(Value::as_array) {
        let (input, output) = messages.iter().fold((0, 0), |(input, output), message| {
            let count = message.get("tokenCount").unwrap_or(&Value::Null);
            (
                input + field(count, "inputTokens"),
                output + field(count, "outputTokens"),
            )
        });
        if input > 0 || output > 0 {
            usage.input_tokens += input;
            usage.output_tokens += output;
            usage.token_chats += 1;
        }
    }
}

fn profile_count(rt: &Runtime) -> usize {
    let path = rt.layout.storage_json();
    if !path.exists() {
        return 1;
    }
    let Ok(raw) = fs::read_to_string(path) else {
        return 1;
    };
    let Ok(json) = serde_json::from_str::<Value>(&raw) else {
        return 1;
    };
    super::profiles_from_storage(&json).len()
}

fn file_len(path: &Path) -> Option<u64> {
    fs::metadata(path).ok().map(|meta| meta.len()).or_else(|| {
        if path.is_dir() {
            dir_size(path).ok()
        } else {
            None
        }
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::ui::is_bare_number;
    use serde_json::json;

    const UNITS: &[&str] = &["workspaces", "chats", "requests", "tokens", "subagents"];

    fn item(name: &str, kind: Kind, value: u64) -> Named {
        Named {
            name: name.to_string(),
            kind: Some(kind),
            value,
        }
    }

    fn sample() -> Stats {
        let mut busiest: Vec<Named> = (0..12)
            .map(|index| item(&format!("~/app{index}"), Kind::Folder, 20 - index))
            .collect();
        busiest.push(Named {
            name: "unsaved 1780204438555".to_string(),
            kind: Some(Kind::Unsaved),
            value: 1,
        });
        Stats {
            profiles: 2,
            workspaces: 3,
            by_kind: vec![(Kind::Folder, 2), (Kind::Unsaved, 1)],
            chats: 1_200,
            subagents: 40,
            archived: 7,
            usage: Usage {
                conversations: 1_240,
                cost_cents: 123_456,
                requests: 789,
                cost_chats: 12,
                context_tokens: 300_000,
                context_chats: 3,
                input_tokens: 5_000,
                output_tokens: 700,
                token_chats: 2,
            },
            models: BTreeMap::from([("gpt".to_string(), 3), ("opus".to_string(), 9)]),
            per_month: BTreeMap::from([("2026-08".to_string(), 4), ("2026-09".to_string(), 8)]),
            busiest,
            global_db: Some(3 * 1024 * 1024 * 1024),
            workspace_dbs: vec![
                item("~/app", Kind::Folder, 2048),
                item("~/lib", Kind::CodeWorkspace, 1024),
            ],
        }
    }

    #[test]
    fn every_section_has_named_columns_and_units() {
        let stats = sample();
        for (title, sheet) in sections(Theme::plain(), &stats) {
            assert!(!sheet.is_empty(), "{title} is empty");
            assert!(
                sheet.headers().iter().all(|header| !header.is_empty()),
                "{title}: {:?}",
                sheet.headers()
            );
            for (index, header) in sheet.headers().iter().enumerate() {
                let cells = sheet.column(index);
                if cells.iter().any(|cell| is_bare_number(cell)) {
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
    }

    #[test]
    fn stats_render_sections_units_and_totals() {
        let out = render_stats(Theme::plain(), &sample());
        for section in [
            "==> Overview",
            "==> Workspaces by kind",
            "==> Top models",
            "==> Busiest workspaces",
            "==> Chats per month",
            "==> Largest workspace databases",
        ] {
            assert!(out.contains(section), "missing {section}");
        }
        for text in [
            "│ metric ",
            "2 profiles",
            "3 workspaces",
            "2 folder, 1 unsaved",
            "1,200 chats",
            "40 subagent chats",
            "7 archived chats",
            "$1,234.56",
            "789 requests, recorded in 12 of 1,240 conversations",
            "avg 100,000 tokens",
            "5,000 in, 700 out",
            "recorded in 2 of 1,240 conversations",
            "opus",
            "9 chats",
            "3.0 GB",
            "75.0%",
            "all kinds",
            "all models",
            "3 other workspaces",
            "all 13 workspaces",
            "all 2 months",
            "db size",
            "2.0 KB",
            "● code-workspace",
            "█",
        ] {
            assert!(out.contains(text), "missing {text:?}");
        }
        assert!(!out.contains("unsaved 1780204438555"));
        assert!(!out.contains('\u{1b}'));
        let opus = out
            .lines()
            .position(|line| line.contains("│ opus "))
            .unwrap();
        let gpt = out
            .lines()
            .position(|line| line.contains("│ gpt "))
            .unwrap();
        assert!(opus < gpt);
        let colored = render_stats(Theme::colored(), &sample());
        assert_eq!(ui::layout::strip_ansi(&colored), out);
    }

    #[test]
    fn long_workspace_names_are_shortened_to_fit() {
        let long = format!("~/{}/deep/verbleif.code-workspace", "x".repeat(90));
        let items = [
            item(&long, Kind::CodeWorkspace, 1_092),
            item("~/a", Kind::Folder, 9),
        ];
        for width in [100, 120] {
            let out = named_at(
                Theme::plain(),
                &items,
                "chats",
                Color::Green,
                ui::count,
                Some(width),
            )
            .render();
            let lines: Vec<&str> = out.lines().collect();
            assert!(
                lines
                    .iter()
                    .all(|line| ui::layout::width(line) == ui::layout::width(lines[0]))
            );
            assert!(ui::layout::width(lines[0]) <= width);
            assert!(out.contains("…"));
            assert!(out.contains("deep/verbleif.code-workspace"));
            assert_eq!(lines.len(), 7);
        }
    }

    #[test]
    fn empty_stats_say_none_yet() {
        let stats = Stats {
            profiles: 1,
            workspaces: 0,
            by_kind: Vec::new(),
            chats: 0,
            subagents: 0,
            archived: 0,
            usage: Usage::default(),
            models: BTreeMap::new(),
            per_month: BTreeMap::new(),
            busiest: Vec::new(),
            global_db: None,
            workspace_dbs: Vec::new(),
        };
        let out = render_stats(Theme::plain(), &stats);
        assert!(out.contains("none yet"));
        assert!(out.contains("none recorded"));
    }

    #[test]
    fn usage_reads_cost_context_and_inline_tokens_only() {
        let mut usage = Usage::default();
        add_usage(
            &mut usage,
            &json!({
                "usageData": {
                    "claude-4-opus-thinking": {"costInCents": 5768, "amount": 202},
                    "premium-tool-call": {"costInCents": 5, "amount": 7}
                },
                "contextTokensUsed": 63408,
                "contextTokenLimit": 200000,
                "conversation": [
                    {"tokenCount": {"inputTokens": 1200, "outputTokens": 300}},
                    {"tokenCount": {"inputTokens": 0, "outputTokens": 0}},
                    {"text": "no counts"}
                ]
            }),
        );
        add_usage(
            &mut usage,
            &json!({"usageData": {}, "contextTokensUsed": 0}),
        );
        assert_eq!(
            usage,
            Usage {
                conversations: 0,
                cost_cents: 5773,
                requests: 209,
                cost_chats: 1,
                context_tokens: 63408,
                context_chats: 1,
                input_tokens: 1200,
                output_tokens: 300,
                token_chats: 1,
            }
        );
        assert_eq!(money(5773), "$57.73");
        assert_eq!(money(123_456_789), "$1,234,567.89");
    }
}

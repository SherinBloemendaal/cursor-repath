//! The top-level `crepath` help screen: banner, then one strictly aligned grid.

use owo_colors::Style;
use std::io::{self, Write};

use super::banner::banner;
use super::layout::{Grid, wrap_indented};
use super::{Theme, section_line, term};

const INDENT: usize = 2;

pub struct Entry {
    pub icon: (&'static str, &'static str),
    pub name: &'static str,
    pub aliases: &'static [&'static str],
    pub args: &'static str,
    pub about: &'static str,
}

pub struct Group {
    pub title: &'static str,
    pub style: fn() -> Style,
    pub entries: &'static [Entry],
}

pub struct Flag {
    pub short: &'static str,
    pub long: &'static str,
    pub value: &'static str,
    pub about: &'static str,
}

const fn entry(
    icon: (&'static str, &'static str),
    name: &'static str,
    aliases: &'static [&'static str],
    args: &'static str,
    about: &'static str,
) -> Entry {
    Entry {
        icon,
        name,
        aliases,
        args,
        about,
    }
}

const fn flag(
    short: &'static str,
    long: &'static str,
    value: &'static str,
    about: &'static str,
) -> Flag {
    Flag {
        short,
        long,
        value,
        about,
    }
}

pub const GROUPS: &[Group] = &[
    Group {
        title: "Migrate",
        style: || Style::new().cyan(),
        entries: &[
            entry(
                ("➜", ">"),
                "mv",
                &["move"],
                "[FROM] [TO]",
                "Repath workspace metadata after a folder moved",
            ),
            entry(
                ("✚", "+"),
                "cp",
                &["copy"],
                "[FROM] [TO]",
                "Copy a workspace and duplicate its chats",
            ),
            entry(
                ("✎", "*"),
                "save",
                &[],
                "[ID] [TO]",
                "Attach an unsaved Workspaces/<ts> session to a folder",
            ),
        ],
    },
    Group {
        title: "Chats",
        style: || Style::new().magenta(),
        entries: &[
            entry(
                ("⇉", "<"),
                "split",
                &[],
                "[SOURCE] [TARGETS...]",
                "Copy chats from one workspace into separate projects",
            ),
            entry(
                ("⊕", "+"),
                "combine",
                &[],
                "[TARGET] [SOURCES...]",
                "Bring chats from several sources into one workspace",
            ),
            entry(
                ("✗", "x"),
                "rm",
                &[],
                "[TARGET]",
                "Remove workspace metadata or a single chat",
            ),
        ],
    },
    Group {
        title: "Inspect",
        style: || Style::new().green(),
        entries: &[
            entry(
                ("≡", "="),
                "ls",
                &["list"],
                "[ID]",
                "List workspaces, missing destinations first, then by size",
            ),
            entry(
                ("▤", "#"),
                "stats",
                &[],
                "",
                "Profiles, chats, tokens, and models at a glance",
            ),
            entry(
                ("↺", "~"),
                "history",
                &[],
                "",
                "The local command log of the last 30 days",
            ),
        ],
    },
    Group {
        title: "Archive",
        style: || Style::new().yellow(),
        entries: &[
            entry(
                ("⇧", "^"),
                "export",
                &[],
                "[TARGET] [FILE]",
                "Write a workspace and its chats to a .crepath archive",
            ),
            entry(
                ("⇩", "v"),
                "import",
                &[],
                "[FILE] [TO]",
                "Restore a .crepath archive, optionally into a new folder",
            ),
        ],
    },
    Group {
        title: "Maintenance",
        style: || Style::new().blue(),
        entries: &[
            entry(
                ("↻", "@"),
                "rx",
                &["reindex"],
                "[TARGET]",
                "Rebuild registry refs, rewrite stale paths, clear caches",
            ),
            entry(
                ("◫", "%"),
                "cache",
                &[],
                "clear|scan|stats",
                "Clear, rescan, or inspect the persistent index",
            ),
            entry(
                ("✦", "*"),
                "update",
                &[],
                "",
                "Download, verify, and install the latest release",
            ),
            entry(
                ("⌫", "-"),
                "uninstall",
                &[],
                "",
                "Remove the installed binary and its PATH entry",
            ),
            entry(
                ("⌂", "&"),
                "github",
                &[],
                "",
                "Open the GitHub repository in the browser",
            ),
            entry(
                ("?", "?"),
                "help",
                &[],
                "[COMMAND]",
                "This screen, or every option of one command",
            ),
        ],
    },
];

pub const FLAGS: &[Flag] = &[
    flag("-n", "--dry-run", "", "Show the plan and change nothing"),
    flag("-y", "--yes", "", "Skip the confirmation prompt"),
    flag(
        "",
        "--profile",
        "NAME",
        "Limit work to one Cursor installation, or NAME/PROFILE",
    ),
    flag(
        "",
        "--replace",
        "FROM TO",
        "Rewrite every matching path from FROM to TO",
    ),
    flag(
        "",
        "--regex",
        "",
        "Treat --replace FROM as a regular expression",
    ),
    flag(
        "",
        "--unsaved",
        "",
        "Only consider unsaved Workspaces/<ts> sessions",
    ),
    flag(
        "",
        "--project",
        "",
        "With mv or cp, also move or copy the real project folder",
    ),
    flag(
        "",
        "--move",
        "",
        "With split or combine, move chats instead of copying them",
    ),
    flag(
        "",
        "--copy",
        "",
        "With combine, keep chats in the sources (the default)",
    ),
    flag(
        "",
        "--overwrite",
        "",
        "With import, replace chats that already exist",
    ),
    flag(
        "",
        "--fresh",
        "",
        "With ls or stats, read live data and refresh the index",
    ),
    flag(
        "",
        "--full",
        "",
        "With cache scan, rebuild the index from scratch",
    ),
    flag(
        "",
        "--purge",
        "",
        "With uninstall, also delete history, index, and backups",
    ),
    flag(
        "",
        "--color",
        "WHEN",
        "Color output: auto, always, or never",
    ),
    flag("-h", "--help", "", "Show this help"),
    flag("-V", "--version", "", "Print the version"),
];

pub const EXAMPLES: &[(&str, &str)] = &[
    ("crepath ls", "Every workspace, missing ones on top"),
    (
        "crepath ls ~/code/app",
        "One workspace and all of its chats",
    ),
    (
        "crepath mv ~/old/app ~/new/app",
        "Repath after moving a project folder",
    ),
    (
        "crepath mv -n --replace ~/a ~/b",
        "Preview a bulk rename without changing anything",
    ),
    (
        "crepath cp ~/app ~/app2 --project",
        "Copy the folder together with its chats",
    ),
    (
        "crepath split ~/mono ~/mono/api",
        "Spread monorepo chats over sub-projects",
    ),
    (
        "crepath export ~/app app.crepath",
        "Write a portable backup archive",
    ),
    ("crepath help mv", "Every option of one command"),
];

struct Row {
    cells: Vec<String>,
    about: &'static str,
}

#[derive(Clone, Copy, PartialEq, Eq)]
enum Kind {
    Commands,
    Flags,
    Examples,
}

struct Section {
    title: &'static str,
    kind: Kind,
    rows: Vec<Row>,
}

fn command_rows(theme: Theme, group: &Group) -> Vec<Row> {
    let style = (group.style)();
    group
        .entries
        .iter()
        .map(|entry| {
            let icon = if theme.unicode() {
                entry.icon.0
            } else {
                entry.icon.1
            };
            let mut name = theme.paint(entry.name, style.bold());
            for alias in entry.aliases {
                name.push_str(&theme.dim(format!(", {alias}")));
            }
            Row {
                cells: vec![theme.paint(icon, style), name, theme.dim(entry.args)],
                about: entry.about,
            }
        })
        .collect()
}

fn flag_rows(theme: Theme) -> Vec<Row> {
    FLAGS
        .iter()
        .map(|flag| {
            let name = if flag.short.is_empty() {
                format!("    {}", theme.flag(flag.long))
            } else {
                format!(
                    "{}{} {}",
                    theme.flag(flag.short),
                    theme.dim(","),
                    theme.flag(flag.long)
                )
            };
            Row {
                cells: vec![name, theme.dim(flag.value)],
                about: flag.about,
            }
        })
        .collect()
}

fn example_rows(theme: Theme) -> Vec<Row> {
    EXAMPLES
        .iter()
        .map(|(command, about)| Row {
            cells: vec![theme.dim("$"), highlight(theme, command)],
            about,
        })
        .collect()
}

fn sections(theme: Theme) -> Vec<Section> {
    let mut out: Vec<Section> = GROUPS
        .iter()
        .map(|group| Section {
            title: group.title,
            kind: Kind::Commands,
            rows: command_rows(theme, group),
        })
        .collect();
    out.push(Section {
        title: "Flags",
        kind: Kind::Flags,
        rows: flag_rows(theme),
    });
    out.push(Section {
        title: "Examples",
        kind: Kind::Examples,
        rows: example_rows(theme),
    });
    out
}

fn fit(sections: &[Section], kind: Kind) -> Grid {
    Grid::fit(
        INDENT,
        sections
            .iter()
            .filter(|section| section.kind == kind)
            .flat_map(|section| section.rows.iter().map(|row| row.cells.as_slice())),
    )
}

pub fn help_text(theme: Theme, total: usize) -> String {
    let sections = sections(theme);
    let mut commands = fit(&sections, Kind::Commands);
    let mut flags = fit(&sections, Kind::Flags);
    let mut examples = fit(&sections, Kind::Examples);
    let values = commands.column_start(2).max(flags.column_start(1));
    commands.align_column_to(2, values);
    flags.align_column_to(1, values);
    let column = [&commands, &flags, &examples]
        .iter()
        .map(|grid| grid.text_column())
        .max()
        .unwrap_or(0);
    for grid in [&mut commands, &mut flags, &mut examples] {
        grid.align_text_to(column);
    }
    let mut out = banner(theme, total);
    for section in &sections {
        let grid = match section.kind {
            Kind::Commands => &commands,
            Kind::Flags => &flags,
            Kind::Examples => &examples,
        };
        out.push('\n');
        out.push_str(&section_line(theme, section.title));
        out.push('\n');
        for row in &section.rows {
            out.push_str(
                &grid.render(&row.cells, row.about, total, |line| match section.kind {
                    Kind::Examples => theme.dim(line),
                    Kind::Commands | Kind::Flags => line.to_string(),
                }),
            );
        }
    }
    out.push('\n');
    let star = if theme.unicode() { "★" } else { "*" };
    out.push_str(&wrap_indented(
        &format!("{star} {}", crate::update::REPO_URL),
        INDENT,
        total,
        |line| match line.split_once(' ') {
            Some((icon, rest)) if icon == star => {
                format!(
                    "{} {}",
                    theme.paint(icon, Style::new().yellow()),
                    theme.paint(rest, Style::new().cyan().underline())
                )
            }
            _ => theme.paint(line, Style::new().cyan().underline()),
        },
    ));
    out
}

pub fn print_help() {
    let mut stdout = io::stdout().lock();
    let _ = write!(stdout, "{}", help_text(Theme::stdout(), term::width()));
}

fn highlight(theme: Theme, command: &str) -> String {
    command
        .split(' ')
        .enumerate()
        .map(|(index, word)| match index {
            0 => theme.paint(word, Style::new().bold().green()),
            1 => theme.command(word),
            _ if word.starts_with('-') => theme.flag(word),
            _ => theme.token(word),
        })
        .collect::<Vec<_>>()
        .join(" ")
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::ui::layout::{strip_ansi, width};
    use crate::ui::term::Depth;

    fn themes() -> [Theme; 4] {
        [
            Theme::plain(),
            Theme::colored(),
            Theme::colored().with_depth(Depth::TrueColor),
            Theme::colored().with_depth(Depth::Ansi256),
        ]
    }

    fn offset(line: &str, needle: &str) -> usize {
        let index = line
            .find(needle)
            .unwrap_or_else(|| panic!("{needle:?} not in {line:?}"));
        width(&line[..index])
    }

    fn first_word(text: &str) -> &str {
        text.split(' ').next().unwrap()
    }

    fn command_line<'a>(lines: &'a [&'a str], name: &str) -> &'a str {
        lines
            .iter()
            .find(|line| {
                let cells: Vec<&str> = line.split_whitespace().collect();
                cells.len() > 1 && cells[1].trim_end_matches(',') == name
            })
            .unwrap_or_else(|| panic!("no row for {name}"))
    }

    fn assert_logo_gap(text: &str) {
        let lines: Vec<&str> = text.lines().collect();
        let tag = lines
            .iter()
            .position(|line| line.contains("Repath Cursor workspaces and chats."))
            .expect("tagline");
        assert!(tag >= 2, "logo, blank line, tagline");
        assert_eq!(
            lines[tag - 1],
            "",
            "blank line between the logo and the tagline"
        );
        let logo = lines[tag - 2];
        assert!(
            logo.contains('╚') || logo.contains('|') || logo.contains('_'),
            "logo line above the blank line: {logo:?}"
        );
    }

    #[test]
    fn every_row_shares_one_description_column() {
        for total in [80, 100, 120] {
            let plain = help_text(Theme::plain(), total);
            assert_logo_gap(&plain);
            for theme in themes() {
                let text = strip_ansi(&help_text(theme, total));
                assert_eq!(text, plain, "colors changed the layout at {total}");
                let lines: Vec<&str> = text.lines().collect();
                let mut columns = Vec::new();
                for group in GROUPS {
                    for entry in group.entries {
                        let line = command_line(&lines, entry.name);
                        assert_eq!(offset(line, entry.name), INDENT + 1 + 2);
                        if !entry.args.is_empty() {
                            columns.push(("args", offset(line, entry.args)));
                        }
                        columns.push(("about", offset(line, first_word(entry.about))));
                    }
                }
                for flag in FLAGS {
                    let line = lines
                        .iter()
                        .find(|line| {
                            line.contains(&format!(" {} ", flag.long)) || line.ends_with(flag.long)
                        })
                        .filter(|line| line.trim_start().starts_with('-'))
                        .unwrap_or_else(|| panic!("no row for {}", flag.long));
                    if !flag.value.is_empty() {
                        columns.push(("args", offset(line, flag.value)));
                    }
                    columns.push(("about", offset(line, first_word(flag.about))));
                }
                for (command, about) in EXAMPLES {
                    let line = lines.iter().find(|line| line.contains(command)).unwrap();
                    columns.push(("about", offset(line, first_word(about))));
                }
                let about: Vec<usize> = columns
                    .iter()
                    .filter(|(kind, _)| *kind == "about")
                    .map(|(_, column)| *column)
                    .collect();
                assert!(about.iter().all(|column| *column == about[0]), "{about:?}");
                let args: Vec<usize> = columns
                    .iter()
                    .filter(|(kind, _)| *kind == "args")
                    .map(|(_, column)| *column)
                    .collect();
                assert!(args.iter().all(|column| *column == args[0]), "{args:?}");
                for line in &lines {
                    assert!(width(line) <= total, "{} > {total}: {line:?}", width(line));
                }
                let grid_start = lines
                    .iter()
                    .position(|line| line.starts_with("==> "))
                    .unwrap();
                for line in &lines[grid_start..] {
                    let lead = line.len() - line.trim_start().len();
                    if lead > INDENT && !line.trim_start().starts_with("--") {
                        assert_eq!(lead, about[0], "ragged continuation: {line:?}");
                    }
                }
            }
        }
    }

    #[test]
    fn sections_are_separated_by_one_blank_line() {
        let text = help_text(Theme::plain(), 100);
        let lines: Vec<&str> = text.lines().collect();
        for (index, line) in lines.iter().enumerate() {
            if line.starts_with("==> ") {
                assert_eq!(lines[index - 1], "", "{line}");
                assert_ne!(lines[index - 2], "", "{line}");
            }
        }
        for title in [
            "Migrate",
            "Chats",
            "Inspect",
            "Archive",
            "Maintenance",
            "Flags",
            "Examples",
        ] {
            assert!(lines.contains(&format!("==> {title}").as_str()), "{title}");
        }
        assert!(!text.contains("\n\n\n"));
    }

    #[test]
    fn help_lists_every_command_alias_and_flag() {
        let text = help_text(Theme::plain(), 100);
        for group in GROUPS {
            for entry in group.entries {
                assert!(text.contains(entry.about), "{}", entry.name);
                for alias in entry.aliases {
                    assert!(text.contains(&format!("{}, {alias}", entry.name)));
                }
            }
        }
        for flag in FLAGS {
            assert!(text.contains(flag.long), "{}", flag.long);
        }
        assert!(!text.contains("--move-chats"));
        assert!(text.contains(crate::update::REPO_URL));
        assert!(!text.contains('\u{1b}'));
        assert!(help_text(Theme::colored(), 100).contains('\u{1b}'));
    }

    #[test]
    fn ascii_help_keeps_the_grid() {
        let text = help_text(Theme::ascii(), 80);
        assert_logo_gap(&text);
        assert!(text.is_ascii());
        let lines: Vec<&str> = text.lines().collect();
        let columns: Vec<usize> = GROUPS
            .iter()
            .flat_map(|group| group.entries.iter())
            .map(|entry| offset(command_line(&lines, entry.name), first_word(entry.about)))
            .collect();
        assert!(columns.iter().all(|column| *column == columns[0]));
    }
}

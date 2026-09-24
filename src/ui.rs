//! Terminal presentation: brew-style status, tables, and dual progress.

use comfy_table::{Table, presets::UTF8_FULL};
use indicatif::{MultiProgress, ProgressBar, ProgressDrawTarget, ProgressStyle};
use owo_colors::OwoColorize;
use std::io::{self, IsTerminal, Write};
use std::time::Duration;

pub fn ok(message: &str) {
    println!("{} {message}", "✔".green());
}

pub fn warn(message: &str) {
    eprintln!("{} {message}", "!".yellow());
}

pub fn info(message: &str) {
    println!("{} {message}", "i".blue());
}

pub fn err(message: &str) {
    eprintln!("{} {message}", "x".red());
}

pub fn format_size(bytes: u64) -> String {
    const KB: u64 = 1024;
    const MB: u64 = KB * 1024;
    const GB: u64 = MB * 1024;
    if bytes >= GB {
        format!("{:.1} GB", bytes as f64 / GB as f64)
    } else if bytes >= MB {
        format!("{:.1} MB", bytes as f64 / MB as f64)
    } else if bytes >= KB {
        format!("{:.1} KB", bytes as f64 / KB as f64)
    } else {
        format!("{bytes} B")
    }
}

pub fn table(headers: &[&str]) -> Table {
    let mut table = Table::new();
    table.load_preset(UTF8_FULL).set_header(headers);
    table
}

pub fn stdout_is_tty() -> bool {
    io::stdout().is_terminal()
}

pub fn confirm(prompt: &str, yes: bool) -> anyhow::Result<bool> {
    if yes {
        return Ok(true);
    }
    if !io::stdin().is_terminal() {
        anyhow::bail!("{prompt} Re-run with -y to continue.");
    }
    let choice = dialoguer::Confirm::new()
        .with_prompt(prompt)
        .default(false)
        .interact()?;
    Ok(choice)
}

pub fn multi_select(prompt: &str, items: &[String]) -> anyhow::Result<Vec<usize>> {
    if !io::stdin().is_terminal() {
        anyhow::bail!("pass arguments, or run {prompt} in a terminal");
    }
    let picked = dialoguer::MultiSelect::new()
        .with_prompt(prompt)
        .items(items)
        .interact()?;
    Ok(picked)
}

pub fn input(prompt: &str) -> anyhow::Result<String> {
    if !io::stdin().is_terminal() {
        anyhow::bail!("pass arguments, or run in a terminal");
    }
    Ok(dialoguer::Input::new()
        .with_prompt(prompt)
        .interact_text()?)
}

pub fn select(prompt: &str, items: &[&str]) -> anyhow::Result<usize> {
    if !io::stdin().is_terminal() {
        anyhow::bail!("pass arguments, or run in a terminal");
    }
    Ok(dialoguer::Select::new()
        .with_prompt(prompt)
        .items(items)
        .interact()?)
}

pub fn validation(title: &str, rows: &[Vec<String>], warnings: &[String]) -> String {
    let mut table = table(&["action", "detail"]);
    for row in rows {
        table.add_row(row);
    }
    let mut out = format!("{}\n{table}\n", title.bold());
    for warning in warnings {
        out.push_str(&format!("{} {warning}\n", "!".yellow()));
    }
    out
}

pub struct DualProgress {
    steps: ProgressBar,
    rows: ProgressBar,
    _multi: Option<MultiProgress>,
}

impl DualProgress {
    pub fn new(steps: u64, quiet: bool) -> Self {
        let style_steps = ProgressStyle::with_template("{bar:40.green} {pos}/{len} {msg}")
            .unwrap_or_else(|_| ProgressStyle::default_bar())
            .progress_chars("=>-");
        let style_rows = ProgressStyle::with_template("{bar:40.cyan} {pos}/{len} {eta} {msg}")
            .unwrap_or_else(|_| ProgressStyle::default_bar())
            .progress_chars("=>-");
        if quiet || !io::stderr().is_terminal() {
            let steps =
                ProgressBar::with_draw_target(Some(steps.max(1)), ProgressDrawTarget::hidden());
            let rows = ProgressBar::with_draw_target(Some(1), ProgressDrawTarget::hidden());
            steps.set_style(style_steps);
            rows.set_style(style_rows);
            rows.enable_steady_tick(Duration::from_millis(250));
            return Self {
                steps,
                rows,
                _multi: None,
            };
        }
        let multi = MultiProgress::new();
        let steps_bar = multi.add(ProgressBar::new(steps.max(1)));
        let rows_bar = multi.add(ProgressBar::new(1));
        steps_bar.set_style(style_steps);
        rows_bar.set_style(style_rows);
        rows_bar.enable_steady_tick(Duration::from_millis(250));
        Self {
            steps: steps_bar,
            rows: rows_bar,
            _multi: Some(multi),
        }
    }

    pub fn step(&self, index: u64, message: &str) {
        self.steps.set_position(index);
        self.steps.set_message(message.to_string());
    }

    pub fn rows(&self, done: u64, total: u64, message: &str) {
        self.rows.set_length(total.max(1));
        self.rows.set_position(done.min(total.max(1)));
        self.rows.set_message(message.to_string());
    }

    pub fn finish(&self) {
        self.steps.finish_and_clear();
        self.rows.finish_and_clear();
    }
}

pub fn print_help() {
    let mut stdout = io::stdout().lock();
    let _ = writeln!(
        stdout,
        "{}\nRepath Cursor workspaces and chats.",
        "crepath".bold().green()
    );
    let _ = writeln!(
        stdout,
        "
  {} {} {}   repath workspace metadata
  {} {} {}   duplicate a workspace and its chats
  {} {} {}  attach an unsaved Workspaces/<ts> session
  {} {} {} split chats into project workspaces
  {} {} {} merge chats into one workspace
  {} {} {}    rebuild registry refs and clear caches
  {} {} {}    list workspaces
  {} {} {}    remove a workspace or chat
  {} {} {} export a .crepath archive
  {} {} {} import a .crepath archive
  {}              local command log
  {}                profiles, chats, tokens, models
  {}                 this help

Shared flags: {} dry-run, {} skip the warning prompt, {} NAME,
{} FROM TO, {}, {}.
{} on mv/cp also moves the real folder.
split and combine copy unless {}.
",
        "mv".cyan(),
        "[FROM]".dimmed(),
        "[TO]".dimmed(),
        "cp".cyan(),
        "[FROM]".dimmed(),
        "[TO]".dimmed(),
        "save".cyan(),
        "[ID]".dimmed(),
        "[TO]".dimmed(),
        "split".cyan(),
        "[SOURCE]".dimmed(),
        "[TARGETS...]".dimmed(),
        "combine".cyan(),
        "[TARGET]".dimmed(),
        "[SOURCES...]".dimmed(),
        "rx".cyan(),
        "[TARGET]".dimmed(),
        "".dimmed(),
        "ls".cyan(),
        "[ID]".dimmed(),
        "".dimmed(),
        "rm".cyan(),
        "[TARGET]".dimmed(),
        "".dimmed(),
        "export".cyan(),
        "[TARGET]".dimmed(),
        "[FILE]".dimmed(),
        "import".cyan(),
        "[FILE]".dimmed(),
        "[TO]".dimmed(),
        "history".cyan(),
        "stats".cyan(),
        "help".cyan(),
        "-n".yellow(),
        "-y".yellow(),
        "--profile".yellow(),
        "--replace".yellow(),
        "--regex".yellow(),
        "--unsaved".yellow(),
        "--project".yellow(),
        "--move".yellow(),
    );
}

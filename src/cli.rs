//! Command surface for crepath.

use anyhow::{Result, bail};
use clap::builder::styling::{AnsiColor, Effects, Styles};
use clap::{ArgAction, Args, ColorChoice, CommandFactory, FromArgMatches, Parser, Subcommand};
use std::collections::BTreeSet;
use std::io::IsTerminal;
use std::path::PathBuf;
use std::sync::Arc;
use std::time::Instant;

use crate::config;
use crate::cursor::install::{self, DEFAULT};
use crate::cursor::process::NativeProcesses;
use crate::engine::{
    self, FixedProbe, Layout, Runtime, SystemProbe, Workspace, cache, combine_workspaces,
    copy_paths, export_workspace, import_archive, index, move_paths, record_history, reindex,
    remove_targets, save_unsaved, show_history, split_workspace, suggest_split,
};
use crate::ui::{self, ColorMode, Theme, validation};

fn styles() -> Styles {
    Styles::styled()
        .header(AnsiColor::Blue.on_default().effects(Effects::BOLD))
        .usage(AnsiColor::Blue.on_default().effects(Effects::BOLD))
        .literal(AnsiColor::Cyan.on_default().effects(Effects::BOLD))
        .placeholder(AnsiColor::BrightBlack.on_default())
        .error(AnsiColor::Red.on_default().effects(Effects::BOLD))
        .valid(AnsiColor::Green.on_default())
        .invalid(AnsiColor::Yellow.on_default().effects(Effects::BOLD))
}

#[derive(Parser)]
#[command(
    name = "crepath",
    version,
    about = "Repath Cursor workspaces and chats",
    styles = styles(),
    disable_help_subcommand = true,
    disable_help_flag = true
)]
pub struct Cli {
    /// Show help.
    #[arg(short = 'h', long = "help", action = ArgAction::SetTrue)]
    pub help: bool,
    /// Color output: auto, always, or never.
    #[arg(long, global = true, value_enum, value_name = "WHEN", default_value_t = ColorMode::Auto)]
    pub color: ColorMode,
    #[command(subcommand)]
    pub command: Option<Command>,
}

#[derive(Subcommand)]
pub enum Command {
    /// Repath workspace metadata. Alias: move.
    #[command(alias = "move")]
    Mv(PathArgs),
    /// Copy a workspace and duplicate its chats. Alias: copy.
    #[command(alias = "copy")]
    Cp(PathArgs),
    /// Attach an unsaved Workspaces/<ts> session to a real folder.
    Save(SaveArgs),
    /// Copy chats from one workspace into separate projects.
    Split(SplitArgs),
    /// Bring chats from several sources into one workspace.
    Combine(CombineArgs),
    /// Rebuild registry refs, rewrite leftover paths, and clear caches. Alias: reindex.
    #[command(alias = "reindex")]
    Rx(TargetArgs),
    /// List workspaces. Alias: list.
    #[command(alias = "list")]
    Ls(ListArgs),
    /// Remove workspace metadata or a single chat.
    Rm(TargetArgs),
    /// Write a .crepath gzip archive.
    Export(ExportArgs),
    /// Restore a .crepath archive.
    Import(ImportArgs),
    /// Show the local command log.
    History(CommonArgs),
    /// Show profiles, chats, tokens, and models.
    Stats(StatsArgs),
    /// Inspect, rebuild, or clear the persistent index.
    Cache(CacheArgs),
    #[command(name = "__refresh-index", hide = true)]
    RefreshIndex,
    /// Download and install the latest release.
    Update,
    /// Open the GitHub repository in the browser.
    Github,
    /// Show help, or every option of one command.
    Help(HelpArgs),
}

#[derive(Args, Debug, Clone)]
pub struct HelpArgs {
    pub command: Option<String>,
}

#[derive(Args, Debug, Clone)]
pub struct CommonArgs {
    /// Show the plan and change nothing.
    #[arg(short = 'n', long)]
    pub dry_run: bool,
    /// Skip the single warning prompt.
    #[arg(short = 'y', long)]
    pub yes: bool,
    /// Limit work to one Cursor installation, or NAME/PROFILE for one VS Code profile in it.
    #[arg(long)]
    pub profile: Option<String>,
    /// Rewrite every matching path from FROM to TO.
    #[arg(long, num_args = 2, value_names = ["FROM", "TO"])]
    pub replace: Vec<String>,
    /// Treat --replace FROM as a regular expression.
    #[arg(long)]
    pub regex: bool,
    /// Only consider unsaved Workspaces/<ts> sessions.
    #[arg(long)]
    pub unsaved: bool,
}

#[derive(Args, Debug, Clone)]
pub struct PathArgs {
    #[command(flatten)]
    pub common: CommonArgs,
    pub from: Option<String>,
    pub to: Option<String>,
    /// Also move or copy the real project folder.
    #[arg(long)]
    pub project: bool,
}

#[derive(Args, Debug, Clone)]
pub struct SaveArgs {
    #[command(flatten)]
    pub common: CommonArgs,
    pub id: Option<String>,
    pub to: Option<String>,
}

#[derive(Args, Debug, Clone)]
pub struct SplitArgs {
    #[command(flatten)]
    pub common: CommonArgs,
    pub source: Option<String>,
    pub targets: Vec<String>,
    /// Remove chats from the source after they land on the targets.
    #[arg(long = "move")]
    pub move_chats: bool,
}

#[derive(Args, Debug, Clone)]
pub struct CombineArgs {
    #[command(flatten)]
    pub common: CommonArgs,
    pub target: Option<String>,
    pub sources: Vec<String>,
    /// Keep the chats in the sources (default).
    #[arg(long, conflicts_with = "move_chats")]
    pub copy: bool,
    /// Remove chats from the sources.
    #[arg(long = "move")]
    pub move_chats: bool,
}

#[derive(Args, Debug, Clone)]
pub struct TargetArgs {
    #[command(flatten)]
    pub common: CommonArgs,
    pub target: Option<String>,
}

#[derive(Args, Debug, Clone)]
pub struct ListArgs {
    #[command(flatten)]
    pub common: CommonArgs,
    pub id: Option<String>,
    /// Read live data instead of the index, then refresh the index.
    #[arg(long)]
    pub fresh: bool,
}

#[derive(Args, Debug, Clone)]
pub struct StatsArgs {
    #[command(flatten)]
    pub common: CommonArgs,
    /// Read live data instead of the index, then refresh the index.
    #[arg(long)]
    pub fresh: bool,
}

#[derive(Args, Debug, Clone)]
pub struct CacheArgs {
    #[command(subcommand)]
    pub action: CacheAction,
}

#[derive(Subcommand, Debug, Clone)]
pub enum CacheAction {
    /// Delete the index, or with --profile only that installation's rows.
    Clear(CommonArgs),
    /// Refresh the index now, re-reading only what changed.
    Scan(ScanArgs),
    /// Show index size, freshness, and the background refresh.
    Stats(CommonArgs),
}

impl CacheAction {
    fn name(&self) -> &'static str {
        match self {
            Self::Clear(_) => "clear",
            Self::Scan(_) => "scan",
            Self::Stats(_) => "stats",
        }
    }

    fn common(&self) -> &CommonArgs {
        match self {
            Self::Clear(common) | Self::Stats(common) => common,
            Self::Scan(args) => &args.common,
        }
    }
}

#[derive(Args, Debug, Clone)]
pub struct ScanArgs {
    #[command(flatten)]
    pub common: CommonArgs,
    /// Ignore what is stored and rebuild from scratch.
    #[arg(long)]
    pub full: bool,
}

#[derive(Args, Debug, Clone)]
pub struct ExportArgs {
    #[command(flatten)]
    pub common: CommonArgs,
    pub target: Option<String>,
    pub file: Option<String>,
}

#[derive(Args, Debug, Clone)]
pub struct ImportArgs {
    #[command(flatten)]
    pub common: CommonArgs,
    pub file: Option<String>,
    pub to: Option<String>,
    /// Replace chats that already exist instead of skipping them.
    #[arg(long)]
    pub overwrite: bool,
}

pub fn run() -> Result<()> {
    let settings = ui::term::init(ui::term::mode_from_args(std::env::args_os()));
    let matches = Cli::command()
        .color(clap_color(settings.stderr))
        .get_matches();
    let cli = Cli::from_arg_matches(&matches).unwrap_or_else(|err| err.exit());
    dispatch(cli, None)
}

fn clap_color(enabled: bool) -> ColorChoice {
    if enabled {
        ColorChoice::Always
    } else {
        ColorChoice::Never
    }
}

pub fn dispatch(cli: Cli, runtime: Option<Runtime>) -> Result<()> {
    let command = match cli.command {
        Some(command) if !cli.help => command,
        _ => {
            crate::update::notify_if_outdated();
            ui::print_help();
            return Ok(());
        }
    };
    if matches!(command, Command::RefreshIndex) {
        let rt = match runtime {
            Some(runtime) => runtime,
            None => production_runtime(&common_of(&command))?,
        };
        return index::background(&rt);
    }
    if matches!(command, Command::Update) {
        return crate::update::run_update();
    }
    crate::update::notify_if_outdated();
    if matches!(command, Command::Github) {
        return crate::update::open_github();
    }
    if let Command::Help(args) = &command {
        return show_help(args.command.as_deref());
    }
    let common = common_of(&command);
    let rt = match runtime {
        Some(runtime) => configure(runtime, &common)?,
        None => production_runtime(&common)?,
    };
    let started = Instant::now();
    let name = command_name(&command);
    let args = command_args(&command);
    let result = execute(&rt, command);
    let outcome = if result.is_ok() { "ok" } else { "error" };
    record_history(&rt, name, &args, outcome, started);
    result
}

fn execute(rt: &Runtime, command: Command) -> Result<()> {
    match command {
        Command::Mv(args) => run_path(rt, args, false),
        Command::Cp(args) => run_path(rt, args, true),
        Command::Save(args) => run_save(rt, args),
        Command::Split(args) => run_split(rt, args),
        Command::Combine(args) => run_combine(rt, args),
        Command::Rx(args) => {
            let (rt, target) = one_target(rt, args.target, "Reindex which workspace?")?;
            let report = reindex(&rt, &target)?;
            finish(&rt, "Reindex", report);
            Ok(())
        }
        Command::Ls(args) => {
            let rt = if args.fresh { rt.fresh() } else { rt.clone() };
            let (text, note) = engine::render_workspaces_noted(
                &rt,
                args.common.unsaved,
                args.id.as_deref(),
                Theme::stdout(),
            )?;
            print!("{text}");
            print_note(note);
            Ok(())
        }
        Command::Rm(args) => {
            let (rt, targets) = match args.target {
                Some(target) => (resolve_install(rt, &[target.as_str()])?, vec![target]),
                None => pick_many(rt, "Remove which workspaces?")?,
            };
            let rt = &rt;
            if !rt.dry_run && !rt.yes {
                let rows: Vec<Vec<String>> = targets
                    .iter()
                    .map(|target| vec![target.clone(), removal_label(rt, target)])
                    .collect();
                print!(
                    "{}",
                    validation(
                        Theme::stdout(),
                        "Remove plan",
                        &["target", "location"],
                        &rows,
                        &[]
                    )
                );
            }
            if !rt.dry_run && !ui::confirm("Remove the selected Cursor metadata?", rt.yes)? {
                bail!("aborted");
            }
            let report = remove_targets(rt, &targets)?;
            finish(rt, "Remove", report);
            Ok(())
        }
        Command::Export(args) => {
            let (rt, target) = one_target(rt, args.target, "Export which workspace?")?;
            let file = args.file.unwrap_or_else(|| {
                ui::input("Archive path").unwrap_or_else(|_| "export.crepath".into())
            });
            let report = export_workspace(&rt, &target, PathBuf::from(file).as_path())?;
            finish(&rt, "Export", report);
            Ok(())
        }
        Command::Import(args) => {
            let file = args
                .file
                .map(PathBuf::from)
                .or_else(|| ui::input("Archive path").ok().map(PathBuf::from))
                .ok_or_else(|| anyhow::anyhow!("a file is required"))?;
            let rt = import_install(rt, args.to.as_deref())?;
            let dest = args.to.map(PathBuf::from);
            let report = import_archive(&rt, &file, dest.as_deref(), args.overwrite)?;
            finish(&rt, "Import", report);
            Ok(())
        }
        Command::History(_) => {
            print!("{}", show_history(rt)?);
            Ok(())
        }
        Command::Stats(args) => {
            let rt = if args.fresh { rt.fresh() } else { rt.clone() };
            let (text, note) = engine::render_stats_noted(&rt, Theme::stdout())?;
            print!("{text}");
            print_note(note);
            Ok(())
        }
        Command::Cache(args) => run_cache(rt, &args.action),
        Command::RefreshIndex => index::background(rt),
        Command::Update => crate::update::run_update(),
        Command::Github => crate::update::open_github(),
        Command::Help(args) => show_help(args.command.as_deref()),
    }
}

fn print_note(note: Option<index::Note>) {
    if let Some(note) = note {
        let theme = Theme::stderr();
        eprintln!("{}", ui::hint_line(theme, &note.text(theme)));
    }
}

fn run_cache(rt: &Runtime, action: &CacheAction) -> Result<()> {
    match action {
        CacheAction::Clear(_) => {
            let plan = cache::clear_plan(rt)?;
            if !plan.items.is_empty() && !rt.dry_run && !ui::confirm(&plan.question, rt.yes)? {
                bail!("aborted");
            }
            let report = cache::clear(rt, &plan)?;
            finish(rt, "Cache clear", report);
        }
        CacheAction::Scan(_) if rt.dry_run => {
            print!(
                "{}",
                cache::render_stale(Theme::stdout(), &cache::collect(rt)?)
            );
        }
        CacheAction::Scan(args) => {
            let result = cache::scan(rt, args.full)?;
            print!("{}", cache::render_scan(Theme::stdout(), &result));
        }
        CacheAction::Stats(_) => {
            print!(
                "{}",
                cache::render_stats(Theme::stdout(), &cache::collect(rt)?, index::now_ms())
            );
        }
    }
    Ok(())
}

fn show_help(command: Option<&str>) -> Result<()> {
    let Some(name) = command else {
        ui::print_help();
        return Ok(());
    };
    let mut root = Cli::command().color(clap_color(ui::term::settings().stdout));
    root.build();
    let Some(sub) = root.find_subcommand_mut(name) else {
        return Err(ui::hinted(
            format!("unknown command: {name}"),
            "Run crepath help to see every command.",
        ));
    };
    sub.print_help()?;
    Ok(())
}

fn removal_label(rt: &Runtime, target: &str) -> String {
    match engine::find_workspace(rt, target) {
        Ok(Some(workspace)) => workspace
            .path
            .map(|path| path.display().to_string())
            .unwrap_or_else(|| workspace.kind.label().to_string()),
        _ => "chat".to_string(),
    }
}

fn run_path(rt: &Runtime, args: PathArgs, copy: bool) -> Result<()> {
    let replace = replace_pair(&args.common)?;
    let (rt, pairs) = match (&args.from, &args.to, &replace) {
        (Some(from), Some(to), None) => (
            resolve_install(rt, &[from.as_str()])?,
            vec![(from.clone(), to.clone())],
        ),
        (_, _, Some((from, to))) => (
            replace_install(rt, from, to, args.common.regex)?,
            Vec::new(),
        ),
        _ => pick_path_pairs(rt, copy)?,
    };
    let rt = &rt;
    let report = if copy {
        copy_paths(
            rt,
            &pairs,
            replace
                .as_ref()
                .map(|(from, to)| (from.as_str(), to.as_str())),
            args.common.regex,
            args.project,
        )?
    } else {
        move_paths(
            rt,
            &pairs,
            replace
                .as_ref()
                .map(|(from, to)| (from.as_str(), to.as_str())),
            args.common.regex,
            args.project,
        )?
    };
    finish(rt, if copy { "Copy" } else { "Move" }, report);
    Ok(())
}

fn run_save(rt: &Runtime, args: SaveArgs) -> Result<()> {
    let (rt, id) = match args.id {
        Some(id) => (resolve_install(rt, &[id.as_str()])?, id),
        None => pick_unsaved(rt)?,
    };
    let to = args
        .to
        .map(PathBuf::from)
        .or_else(|| ui::input("Destination folder").ok().map(PathBuf::from))
        .ok_or_else(|| anyhow::anyhow!("a destination is required"))?;
    let report = save_unsaved(&rt, &id, &to)?;
    finish(&rt, "Save", report);
    Ok(())
}

fn run_split(rt: &Runtime, args: SplitArgs) -> Result<()> {
    let (rt, source) = one_target(rt, args.source, "Split which workspace?")?;
    let rt = &rt;
    let targets = if args.targets.is_empty() {
        ui::input("Target folders, separated by commas")?
            .split(',')
            .map(|part| part.trim().to_string())
            .filter(|part| !part.is_empty())
            .collect()
    } else {
        args.targets
    };
    let target_paths: Vec<PathBuf> = targets.iter().map(PathBuf::from).collect();
    let workspace = engine::find_workspace(rt, &source)?
        .ok_or_else(|| anyhow::anyhow!("no workspace matches {source}"))?;
    let mode = if ui::stdout_is_tty() && !rt.quiet {
        match ui::select("Assignment", &["Auto-suggest", "All to all", "Manual"])? {
            1 => "all",
            2 => "manual",
            _ => "auto",
        }
    } else {
        "auto"
    };
    let suggestion = suggest_split(rt, &workspace, &target_paths)?;
    let mut assigned = suggestion.assigned.clone();
    if mode == "all" {
        for id in suggestion.titles.keys() {
            assigned.insert(id.clone(), target_paths.clone());
        }
    }
    if mode == "manual" || !suggestion.unassigned.is_empty() {
        for id in suggestion.unassigned.iter().chain(
            if mode == "manual" {
                suggestion.titles.keys().cloned().collect::<Vec<_>>()
            } else {
                Vec::new()
            }
            .iter(),
        ) {
            if assigned.contains_key(id) && mode != "manual" {
                continue;
            }
            let labels: Vec<String> = target_paths
                .iter()
                .map(|path| path.display().to_string())
                .collect();
            let picked = ui::multi_select(&format!("Targets for {id}"), &labels)?;
            let chosen = picked
                .into_iter()
                .map(|index| target_paths[index].clone())
                .collect();
            assigned.insert(id.clone(), chosen);
        }
    }
    let rows: Vec<Vec<String>> = assigned
        .iter()
        .map(|(id, dests)| {
            vec![
                id.clone(),
                dests
                    .iter()
                    .map(|path| path.display().to_string())
                    .collect::<Vec<_>>()
                    .join(", "),
            ]
        })
        .collect();
    print!(
        "{}",
        validation(
            Theme::stdout(),
            "Split plan",
            &["chat", "targets"],
            &rows,
            &[]
        )
    );
    if !ui::confirm("Continue with this split?", rt.yes)? {
        bail!("aborted");
    }
    let report = split_workspace(rt, &source, &target_paths, &assigned, args.move_chats)?;
    finish(rt, "Split", report);
    Ok(())
}

fn run_combine(rt: &Runtime, args: CombineArgs) -> Result<()> {
    let target = args
        .target
        .or_else(|| ui::input("Target folder or workspace").ok())
        .ok_or_else(|| anyhow::anyhow!("a target is required"))?;
    let (rt, sources) = if args.sources.is_empty() {
        pick_many(rt, "Combine which sources?")?
    } else {
        let specs: Vec<&str> = args.sources.iter().map(String::as_str).collect();
        (resolve_install(rt, &specs)?, args.sources)
    };
    let rt = &rt;
    let rows = sources
        .iter()
        .map(|source| vec![source.clone(), target.clone()])
        .collect::<Vec<_>>();
    print!(
        "{}",
        validation(
            Theme::stdout(),
            "Combine plan",
            &["source", "target"],
            &rows,
            &[]
        )
    );
    if !ui::confirm("Continue with this combine?", rt.yes)? {
        bail!("aborted");
    }
    let report = combine_workspaces(
        rt,
        PathBuf::from(&target).as_path(),
        &sources,
        args.move_chats,
    )?;
    finish(rt, "Combine", report);
    Ok(())
}

fn pick_path_pairs(rt: &Runtime, copy: bool) -> Result<(Runtime, Vec<(String, String)>)> {
    let (rt, ids) = pick_many(
        rt,
        if copy {
            "Copy which workspaces?"
        } else {
            "Move which workspaces?"
        },
    )?;
    let rt = &rt;
    let mut pairs = Vec::new();
    if ids.len() == 1 {
        let dest = ui::input("Destination path")?;
        pairs.push((ids[0].clone(), dest));
    } else {
        let from = ui::input("Replace from")?;
        let to = ui::input("Replace to")?;
        for id in &ids {
            if let Some(workspace) = engine::find_workspace(rt, id)?
                && let Some(path) = workspace.path
            {
                let updated = path.to_string_lossy().replace(&from, &to);
                pairs.push((id.clone(), updated));
            }
        }
    }
    let rows = pairs
        .iter()
        .map(|(from, to)| vec![from.clone(), to.clone()])
        .collect::<Vec<_>>();
    print!(
        "{}",
        validation(
            Theme::stdout(),
            if copy { "Copy plan" } else { "Move plan" },
            &["workspace", "destination"],
            &rows,
            &[]
        )
    );
    if !ui::confirm(
        if copy {
            "Continue with this copy?"
        } else {
            "Continue with this move?"
        },
        rt.yes,
    )? {
        bail!("aborted");
    }
    Ok((rt.clone(), pairs))
}

fn pin(rt: &Runtime) -> Runtime {
    let mut rt = rt.clone();
    rt.pinned = true;
    rt
}

fn mixed(first: (&str, &str), second: (&str, &str)) -> anyhow::Error {
    ui::hinted(
        format!(
            "{} is in profile {}, but {} is in profile {}",
            first.0, first.1, second.0, second.1
        ),
        "crepath never mixes Cursor installations. Run the command once per profile with --profile NAME.",
    )
}

fn ask_install(rt: &Runtime, names: &[String], why: &str) -> Result<Runtime> {
    let name = if names.is_empty() {
        bail!("{why}: no Cursor installation was found");
    } else if names.len() == 1 {
        names[0].clone()
    } else if rt.quiet || !std::io::stdin().is_terminal() {
        return Err(ui::hinted(
            why.to_string(),
            format!("Pass --profile with one of: {}.", names.join(", ")),
        ));
    } else {
        let items: Vec<&str> = names.iter().map(String::as_str).collect();
        names[ui::select(&format!("{why}. Which Cursor profile?"), &items)?].clone()
    };
    let layout = rt
        .installs
        .iter()
        .find(|layout| layout.name == name)
        .ok_or_else(|| anyhow::anyhow!("unknown Cursor profile {name}"))?;
    Ok(pin(&rt.scoped(layout)))
}

/// The one installation that holds every spec, asking when several do.
fn resolve_install(rt: &Runtime, specs: &[&str]) -> Result<Runtime> {
    if rt.installs.len() <= 1 {
        return Ok(rt.clone());
    }
    if rt.pinned {
        for spec in specs {
            if !engine::locate(rt, spec)?.is_empty() {
                continue;
            }
            let mut owners = Vec::new();
            for sibling in rt.siblings() {
                if !engine::locate(&sibling, spec)?.is_empty() {
                    owners.push(sibling.layout.name);
                }
            }
            if !owners.is_empty() {
                return Err(ui::hinted(
                    format!(
                        "{spec} belongs to profile {}, not {}",
                        owners.join(", "),
                        rt.layout.name
                    ),
                    "crepath never moves data between Cursor installations. Pass the profile that holds it.",
                ));
            }
        }
        return Ok(rt.clone());
    }
    let mut shared: Option<BTreeSet<String>> = None;
    let mut seen: Vec<(&str, BTreeSet<String>)> = Vec::new();
    for spec in specs {
        let names: BTreeSet<String> = engine::locate(rt, spec)?
            .into_iter()
            .map(|layout| layout.name)
            .collect();
        if names.is_empty() {
            continue;
        }
        let next: BTreeSet<String> = match &shared {
            Some(prev) => prev.intersection(&names).cloned().collect(),
            None => names.clone(),
        };
        if next.is_empty()
            && let Some((first, owners)) = seen.first()
        {
            let join = |set: &BTreeSet<String>| set.iter().cloned().collect::<Vec<_>>().join(", ");
            return Err(mixed((first, &join(owners)), (spec, &join(&names))));
        }
        seen.push((spec, names));
        shared = Some(next);
    }
    let Some(shared) = shared else {
        return Ok(rt.clone());
    };
    let names: Vec<String> = shared.into_iter().collect();
    ask_install(
        rt,
        &names,
        &format!(
            "{} exists in profiles {}",
            specs.join(", "),
            names.join(", ")
        ),
    )
}

fn replace_install(rt: &Runtime, from: &str, to: &str, regex: bool) -> Result<Runtime> {
    if rt.pinned || rt.installs.len() <= 1 {
        return Ok(rt.clone());
    }
    let mut names = Vec::new();
    for scoped in rt.scope() {
        if !engine::replaced(&scoped, from, to, regex)?.is_empty() {
            names.push(scoped.layout.name);
        }
    }
    if names.is_empty() {
        return Ok(rt.clone());
    }
    ask_install(
        rt,
        &names,
        &format!(
            "--replace {from} matches workspaces in profiles {}",
            names.join(", ")
        ),
    )
}

fn import_install(rt: &Runtime, to: Option<&str>) -> Result<Runtime> {
    if rt.pinned || rt.installs.len() <= 1 {
        return Ok(rt.clone());
    }
    if let Some(to) = to {
        let names: Vec<String> = engine::locate(rt, to)?
            .into_iter()
            .map(|layout| layout.name)
            .collect();
        if !names.is_empty() {
            return ask_install(
                rt,
                &names,
                &format!("{to} exists in profiles {}", names.join(", ")),
            );
        }
    }
    let names: Vec<String> = rt
        .installs
        .iter()
        .map(|layout| layout.name.clone())
        .collect();
    ask_install(
        rt,
        &names,
        "import needs one Cursor installation to write into",
    )
}

fn pick_from(
    rt: &Runtime,
    prompt: &str,
    keep: impl Fn(&Workspace) -> bool,
) -> Result<(Runtime, Vec<String>)> {
    let scopes = rt.scope();
    let labelled = scopes.len() > 1;
    let mut found: Vec<(Runtime, Workspace)> = Vec::new();
    for scoped in scopes {
        for workspace in engine::picker_workspaces(&scoped)? {
            let wanted = scoped
                .profile
                .as_ref()
                .is_none_or(|profile| &workspace.profile == profile);
            if wanted && keep(&workspace) {
                found.push((scoped.clone(), workspace));
            }
        }
    }
    let theme = Theme::stderr();
    let labels: Vec<String> = found
        .iter()
        .map(|(_, workspace)| {
            let location = workspace
                .path
                .as_ref()
                .map(|path| path.display().to_string())
                .unwrap_or_else(|| workspace.kind.label().to_string());
            let location = if workspace.destination_missing {
                format!("{} {}", theme.bad(theme.icons().cross), theme.bad(location))
            } else {
                theme.path(location)
            };
            if labelled {
                format!(
                    "{}  {}  {location}",
                    theme.id(&workspace.id),
                    theme.bold(workspace.profile_label())
                )
            } else {
                format!("{}  {location}", theme.id(&workspace.id))
            }
        })
        .collect();
    let picked = ui::multi_select(prompt, &labels)?;
    let mut chosen: Option<(Runtime, String)> = None;
    let mut ids = Vec::new();
    for index in picked {
        let (scoped, workspace) = &found[index];
        match &chosen {
            Some((existing, first)) if existing.layout.name != scoped.layout.name => {
                return Err(mixed(
                    (first, &existing.layout.name),
                    (&workspace.id, &scoped.layout.name),
                ));
            }
            Some(_) => {}
            None => chosen = Some((pin(scoped), workspace.id.clone())),
        }
        ids.push(workspace.id.clone());
    }
    Ok((chosen.map_or_else(|| rt.clone(), |(scoped, _)| scoped), ids))
}

fn pick_many(rt: &Runtime, prompt: &str) -> Result<(Runtime, Vec<String>)> {
    pick_from(rt, prompt, |_| true)
}

fn one_target(rt: &Runtime, target: Option<String>, prompt: &str) -> Result<(Runtime, String)> {
    match target {
        Some(target) => Ok((resolve_install(rt, &[target.as_str()])?, target)),
        None => {
            let (rt, mut ids) = pick_many(rt, prompt)?;
            let target = ids
                .pop()
                .ok_or_else(|| anyhow::anyhow!("a target is required"))?;
            Ok((rt, target))
        }
    }
}

fn pick_unsaved(rt: &Runtime) -> Result<(Runtime, String)> {
    let (rt, ids) = pick_from(rt, "Unsaved workspace", |workspace| {
        workspace.kind == engine::Kind::Unsaved
    })?;
    let id = ids
        .into_iter()
        .next()
        .ok_or_else(|| anyhow::anyhow!("an unsaved workspace id is required"))?;
    Ok((rt, id))
}

fn replace_pair(common: &CommonArgs) -> Result<Option<(String, String)>> {
    if common.replace.is_empty() {
        return Ok(None);
    }
    if common.replace.len() != 2 {
        bail!("--replace needs FROM and TO");
    }
    Ok(Some((common.replace[0].clone(), common.replace[1].clone())))
}

fn finish(rt: &Runtime, title: &str, report: engine::Report) {
    print!(
        "{}",
        finish_text(Theme::stdout(), title, rt.dry_run, &report)
    );
    let theme = Theme::stderr();
    for warning in &report.warnings {
        eprintln!("{}", ui::warn_line(theme, warning));
    }
    print!("{}", finish_summary(Theme::stdout(), rt.dry_run, &report));
}

pub fn finish_text(theme: Theme, title: &str, dry_run: bool, report: &engine::Report) -> String {
    let heading = if dry_run {
        format!("{title} (dry run)")
    } else {
        title.to_string()
    };
    let mut out = format!("{}\n", ui::section_line(theme, &heading));
    if !report.applied.is_empty() || !report.skipped.is_empty() {
        let sheet = ui::results(
            theme,
            &title.to_lowercase(),
            &report.applied,
            &report.skipped,
        );
        out.push_str(&format!("{sheet}\n"));
    }
    if !report.rewritten_keys.is_empty() {
        out.push_str(&ui::info_line(
            theme,
            &format!(
                "rewrote {} storage keys",
                theme.number(ui::count(report.rewritten_keys.len() as u64))
            ),
        ));
        out.push('\n');
    }
    out
}

pub fn finish_summary(theme: Theme, dry_run: bool, report: &engine::Report) -> String {
    if report.applied.is_empty() && report.skipped.is_empty() {
        return format!("{}\n", ui::info_line(theme, "Nothing to do."));
    }
    let applied = report.applied.len();
    let mut parts = vec![theme.good(if dry_run {
        ui::plural(applied, "item planned", "items planned")
    } else {
        ui::plural(applied, "item applied", "items applied")
    })];
    if !report.skipped.is_empty() {
        parts.push(theme.caution(ui::plural(
            report.skipped.len(),
            "item skipped",
            "items skipped",
        )));
    }
    if !report.warnings.is_empty() {
        parts.push(theme.caution(ui::plural(report.warnings.len(), "warning", "warnings")));
    }
    let mut out = format!("  {}\n", parts.join(&theme.sep()));
    if dry_run {
        out.push_str(&ui::hint_line(
            theme,
            "Nothing was changed. Run again without -n to apply.",
        ));
        out.push('\n');
    }
    out
}

fn common_of(command: &Command) -> CommonArgs {
    match command {
        Command::Mv(args) | Command::Cp(args) => args.common.clone(),
        Command::Save(args) => args.common.clone(),
        Command::Split(args) => args.common.clone(),
        Command::Combine(args) => args.common.clone(),
        Command::Rx(args) | Command::Rm(args) => args.common.clone(),
        Command::Ls(args) => args.common.clone(),
        Command::Export(args) => args.common.clone(),
        Command::Import(args) => args.common.clone(),
        Command::History(args) => args.clone(),
        Command::Stats(args) => args.common.clone(),
        Command::Cache(args) => args.action.common().clone(),
        Command::Update | Command::Github | Command::Help(_) | Command::RefreshIndex => {
            CommonArgs {
                dry_run: false,
                yes: true,
                profile: None,
                replace: Vec::new(),
                regex: false,
                unsaved: false,
            }
        }
    }
}

/// Index settings from `CREPATH_NO_INDEX`.
pub fn index_config() -> Option<index::Config> {
    index::Config::from_env(std::env::var("CREPATH_NO_INDEX").ok().as_deref())
}

pub fn production_runtime(common: &CommonArgs) -> Result<Runtime> {
    let projects_dir = config::cursor_projects_dir()?;
    let crepath_home = config::crepath_home()?;
    let installs: Vec<Layout> = install::discover(&install::Roots::system()?)
        .into_iter()
        .map(|found| Layout {
            name: found.name,
            cursor_root: found.root,
            projects_dir: projects_dir.clone(),
            crepath_home: crepath_home.clone(),
        })
        .collect();
    let layout = installs
        .first()
        .cloned()
        .ok_or_else(|| anyhow::anyhow!("no Cursor installation was found"))?;
    let roots = installs
        .iter()
        .map(|layout| layout.cursor_root.clone())
        .collect();
    configure(
        Runtime {
            layout,
            installs,
            pinned: false,
            dry_run: false,
            yes: false,
            profile: None,
            probe: Arc::new(SystemProbe::new(NativeProcesses, roots)),
            quiet: false,
            index: index_config(),
        },
        common,
    )
}

/// Apply the shared flags to a runtime; `--profile` pins one installation.
pub fn configure(mut rt: Runtime, common: &CommonArgs) -> Result<Runtime> {
    rt.dry_run |= common.dry_run;
    rt.yes |= common.yes;
    match &common.profile {
        Some(selector) => select_profile(rt, selector),
        None => Ok(rt),
    }
}

fn profile_labels(rt: &Runtime) -> Result<Vec<String>> {
    let mut labels = Vec::new();
    for layout in &rt.installs {
        labels.push(layout.name.clone());
        for profile in engine::user_profiles(&rt.scoped(layout))? {
            labels.push(format!("{}/{}", layout.name, profile.name));
        }
    }
    Ok(labels)
}

fn unknown_profile(rt: &Runtime, selector: &str) -> Result<anyhow::Error> {
    Ok(ui::hinted(
        format!("no Cursor profile named {selector}"),
        format!("Known profiles: {}.", profile_labels(rt)?.join(", ")),
    ))
}

/// `NAME` picks an installation, `NAME/PROFILE` a VS Code profile inside it, and a bare
/// VS Code profile name works when exactly one installation has it.
pub fn select_profile(mut rt: Runtime, selector: &str) -> Result<Runtime> {
    let (install_name, profile) = match selector.split_once('/') {
        Some((install_name, profile)) => (install_name, Some(profile)),
        None => (selector, None),
    };
    if let Some(layout) = rt
        .installs
        .iter()
        .find(|layout| layout.name.eq_ignore_ascii_case(install_name))
        .cloned()
    {
        let profile = match profile {
            None => None,
            Some(wanted) if wanted.eq_ignore_ascii_case(DEFAULT) => Some(DEFAULT.to_string()),
            Some(wanted) => {
                let found = engine::user_profiles(&rt.scoped(&layout))?
                    .into_iter()
                    .find(|found| found.name.eq_ignore_ascii_case(wanted) || found.id == wanted);
                match found {
                    Some(found) => Some(found.name),
                    None => return Err(unknown_profile(&rt, selector)?),
                }
            }
        };
        rt.layout = layout;
        rt.pinned = true;
        rt.profile = profile;
        return Ok(rt);
    }
    if profile.is_none() {
        let mut hits = Vec::new();
        for layout in &rt.installs {
            for found in engine::user_profiles(&rt.scoped(layout))? {
                if found.name.eq_ignore_ascii_case(selector) || found.id == selector {
                    hits.push((layout.clone(), found.name));
                }
            }
        }
        if hits.len() > 1 {
            return Err(ui::hinted(
                format!(
                    "VS Code profile {selector} exists in {}",
                    hits.iter()
                        .map(|(layout, _)| layout.name.as_str())
                        .collect::<Vec<_>>()
                        .join(", ")
                ),
                format!("Pass --profile INSTALLATION/{selector}."),
            ));
        }
        if let Some((layout, name)) = hits.pop() {
            rt.layout = layout;
            rt.pinned = true;
            rt.profile = Some(name);
            return Ok(rt);
        }
    }
    Err(unknown_profile(&rt, selector)?)
}

pub fn test_runtime(layout: Layout, running: bool, dry_run: bool) -> Runtime {
    Runtime {
        installs: vec![layout.clone()],
        layout,
        pinned: false,
        dry_run,
        yes: true,
        profile: None,
        probe: Arc::new(FixedProbe(running)),
        quiet: true,
        index: None,
    }
}

fn command_name(command: &Command) -> &'static str {
    match command {
        Command::Mv(_) => "mv",
        Command::Cp(_) => "cp",
        Command::Save(_) => "save",
        Command::Split(_) => "split",
        Command::Combine(_) => "combine",
        Command::Rx(_) => "rx",
        Command::Ls(_) => "ls",
        Command::Rm(_) => "rm",
        Command::Export(_) => "export",
        Command::Import(_) => "import",
        Command::History(_) => "history",
        Command::Stats(_) => "stats",
        Command::Cache(_) => "cache",
        Command::RefreshIndex => index::REFRESH_COMMAND,
        Command::Update => "update",
        Command::Github => "github",
        Command::Help(_) => "help",
    }
}

fn command_args(command: &Command) -> Vec<String> {
    match command {
        Command::Mv(args) | Command::Cp(args) => vec![
            args.from.clone().unwrap_or_default(),
            args.to.clone().unwrap_or_default(),
        ],
        Command::Save(args) => vec![
            args.id.clone().unwrap_or_default(),
            args.to.clone().unwrap_or_default(),
        ],
        Command::Split(args) => std::iter::once(args.source.clone().unwrap_or_default())
            .chain(args.targets.clone())
            .collect(),
        Command::Combine(args) => std::iter::once(args.target.clone().unwrap_or_default())
            .chain(args.sources.clone())
            .collect(),
        Command::Rx(args) | Command::Rm(args) => vec![args.target.clone().unwrap_or_default()],
        Command::Ls(args) => vec![args.id.clone().unwrap_or_default()],
        Command::Export(args) => vec![
            args.target.clone().unwrap_or_default(),
            args.file.clone().unwrap_or_default(),
        ],
        Command::Import(args) => vec![
            args.file.clone().unwrap_or_default(),
            args.to.clone().unwrap_or_default(),
        ],
        Command::Cache(args) => vec![args.action.name().to_string()],
        Command::History(_)
        | Command::Stats(_)
        | Command::Update
        | Command::Github
        | Command::Help(_)
        | Command::RefreshIndex => Vec::new(),
    }
}

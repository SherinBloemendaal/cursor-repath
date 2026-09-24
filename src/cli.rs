//! Command surface for crepath.

use anyhow::{Result, bail};
use clap::{Args, Parser, Subcommand};
use std::path::PathBuf;
use std::sync::Arc;
use std::time::Instant;

use crate::config;
use crate::engine::{
    self, FixedProbe, Layout, Runtime, SystemProbe, combine_workspaces, copy_paths, discover,
    export_workspace, import_archive, list_workspaces, move_paths, record_history, reindex,
    remove_targets, save_unsaved, show_history, show_stats, split_workspace, suggest_split,
};
use crate::ui::{self, validation};

#[derive(Parser)]
#[command(
    name = "crepath",
    version,
    about = "Repath Cursor workspaces and chats"
)]
pub struct Cli {
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
    Stats(CommonArgs),
    /// Show help.
    Help,
}

#[derive(Args, Debug, Clone)]
pub struct CommonArgs {
    /// Show the plan and change nothing.
    #[arg(short = 'n', long)]
    pub dry_run: bool,
    /// Skip the single warning prompt.
    #[arg(short = 'y', long)]
    pub yes: bool,
    /// Limit work to one Cursor profile.
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
    #[arg(long)]
    pub move_chats: bool,
}

#[derive(Args, Debug, Clone)]
pub struct CombineArgs {
    #[command(flatten)]
    pub common: CommonArgs,
    pub target: Option<String>,
    pub sources: Vec<String>,
    /// Remove chats from the sources.
    #[arg(long)]
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
}

pub fn run() -> Result<()> {
    let cli = Cli::parse();
    dispatch(cli, None)
}

pub fn dispatch(cli: Cli, runtime: Option<Runtime>) -> Result<()> {
    let Some(command) = cli.command else {
        ui::print_help();
        return Ok(());
    };
    if matches!(command, Command::Help) {
        ui::print_help();
        return Ok(());
    }
    let rt = match runtime {
        Some(runtime) => runtime,
        None => production_runtime(&common_of(&command))?,
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
            let target = args
                .target
                .or_else(|| pick_one(rt, "Reindex which workspace?"))
                .ok_or_else(|| anyhow::anyhow!("a target is required"))?;
            let report = reindex(rt, &target)?;
            finish(report);
            Ok(())
        }
        Command::Ls(args) => {
            println!(
                "{}",
                list_workspaces(rt, args.common.unsaved, args.id.as_deref())?
            );
            Ok(())
        }
        Command::Rm(args) => {
            let targets = match args.target {
                Some(target) => vec![target],
                None => pick_many(rt, "Remove which workspaces?")?,
            };
            if !ui::confirm("Remove the selected Cursor metadata?", rt.yes)? {
                bail!("aborted");
            }
            let report = remove_targets(rt, &targets)?;
            finish(report);
            Ok(())
        }
        Command::Export(args) => {
            let target = args
                .target
                .or_else(|| pick_one(rt, "Export which workspace?"))
                .ok_or_else(|| anyhow::anyhow!("a target is required"))?;
            let file = args.file.unwrap_or_else(|| {
                ui::input("Archive path").unwrap_or_else(|_| "export.crepath".into())
            });
            let report = export_workspace(rt, &target, PathBuf::from(file).as_path())?;
            finish(report);
            Ok(())
        }
        Command::Import(args) => {
            let file = args
                .file
                .map(PathBuf::from)
                .or_else(|| ui::input("Archive path").ok().map(PathBuf::from))
                .ok_or_else(|| anyhow::anyhow!("a file is required"))?;
            let dest = args.to.map(PathBuf::from);
            let report = import_archive(rt, &file, dest.as_deref())?;
            finish(report);
            Ok(())
        }
        Command::History(_) => {
            print!("{}", show_history(rt)?);
            Ok(())
        }
        Command::Stats(_) => {
            print!("{}", show_stats(rt)?);
            Ok(())
        }
        Command::Help => {
            ui::print_help();
            Ok(())
        }
    }
}

fn run_path(rt: &Runtime, args: PathArgs, copy: bool) -> Result<()> {
    let replace = replace_pair(&args.common)?;
    let pairs = match (&args.from, &args.to, replace.is_some()) {
        (Some(from), Some(to), false) => vec![(from.clone(), to.clone())],
        (_, _, true) => Vec::new(),
        _ => pick_path_pairs(rt, copy)?,
    };
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
    finish(report);
    Ok(())
}

fn run_save(rt: &Runtime, args: SaveArgs) -> Result<()> {
    let id = args
        .id
        .or_else(|| pick_unsaved(rt))
        .ok_or_else(|| anyhow::anyhow!("an unsaved workspace id is required"))?;
    let to = args
        .to
        .map(PathBuf::from)
        .or_else(|| ui::input("Destination folder").ok().map(PathBuf::from))
        .ok_or_else(|| anyhow::anyhow!("a destination is required"))?;
    let report = save_unsaved(rt, &id, &to)?;
    finish(report);
    Ok(())
}

fn run_split(rt: &Runtime, args: SplitArgs) -> Result<()> {
    let source = args
        .source
        .or_else(|| pick_one(rt, "Split which workspace?"))
        .ok_or_else(|| anyhow::anyhow!("a source workspace is required"))?;
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
    print!("{}", validation("Split", &rows, &[]));
    if !ui::confirm("Continue with this split?", rt.yes)? {
        bail!("aborted");
    }
    let report = split_workspace(rt, &source, &target_paths, &assigned, args.move_chats)?;
    finish(report);
    Ok(())
}

fn run_combine(rt: &Runtime, args: CombineArgs) -> Result<()> {
    let target = args
        .target
        .or_else(|| ui::input("Target folder or workspace").ok())
        .ok_or_else(|| anyhow::anyhow!("a target is required"))?;
    let sources = if args.sources.is_empty() {
        pick_many(rt, "Combine which sources?")?
    } else {
        args.sources
    };
    let rows = sources
        .iter()
        .map(|source| vec![source.clone(), target.clone()])
        .collect::<Vec<_>>();
    print!("{}", validation("Combine", &rows, &[]));
    if !ui::confirm("Continue with this combine?", rt.yes)? {
        bail!("aborted");
    }
    let report = combine_workspaces(
        rt,
        PathBuf::from(&target).as_path(),
        &sources,
        args.move_chats,
    )?;
    finish(report);
    Ok(())
}

fn pick_path_pairs(rt: &Runtime, copy: bool) -> Result<Vec<(String, String)>> {
    let ids = pick_many(
        rt,
        if copy {
            "Copy which workspaces?"
        } else {
            "Move which workspaces?"
        },
    )?;
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
        validation(if copy { "Copy" } else { "Move" }, &rows, &[])
    );
    if !ui::confirm("Continue?", rt.yes)? {
        bail!("aborted");
    }
    Ok(pairs)
}

fn pick_many(rt: &Runtime, prompt: &str) -> Result<Vec<String>> {
    let workspaces = discover(rt)?;
    let labels: Vec<String> = workspaces
        .iter()
        .map(|workspace| {
            format!(
                "{}  {}",
                workspace.id,
                workspace
                    .path
                    .as_ref()
                    .map(|path| path.display().to_string())
                    .unwrap_or_else(|| workspace.kind.label().to_string())
            )
        })
        .collect();
    let picked = ui::multi_select(prompt, &labels)?;
    Ok(picked
        .into_iter()
        .map(|index| workspaces[index].id.clone())
        .collect())
}

fn pick_one(rt: &Runtime, prompt: &str) -> Option<String> {
    pick_many(rt, prompt).ok().and_then(|mut ids| ids.pop())
}

fn pick_unsaved(rt: &Runtime) -> Option<String> {
    discover(rt).ok().and_then(|workspaces| {
        let labels: Vec<String> = workspaces
            .iter()
            .filter(|workspace| workspace.kind == engine::Kind::Unsaved)
            .map(|workspace| workspace.id.clone())
            .collect();
        let picked = ui::multi_select("Unsaved workspace", &labels).ok()?;
        picked.first().map(|index| labels[*index].clone())
    })
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

fn finish(report: engine::Report) {
    for warning in &report.warnings {
        ui::warn(warning);
    }
    for skipped in &report.skipped {
        ui::warn(&format!("skipped {skipped}"));
    }
    for applied in &report.applied {
        ui::ok(applied);
    }
    if !report.rewritten_keys.is_empty() {
        ui::info(&format!(
            "rewrote {} storage keys",
            report.rewritten_keys.len()
        ));
    }
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
        Command::History(args) | Command::Stats(args) => args.clone(),
        Command::Help => CommonArgs {
            dry_run: false,
            yes: true,
            profile: None,
            replace: Vec::new(),
            regex: false,
            unsaved: false,
        },
    }
}

pub fn production_runtime(common: &CommonArgs) -> Result<Runtime> {
    Ok(Runtime {
        layout: Layout {
            cursor_root: config::cursor_config_dir()?,
            projects_dir: config::cursor_projects_dir()?,
            crepath_home: config::crepath_home()?,
        },
        dry_run: common.dry_run,
        yes: common.yes,
        profile: common.profile.clone(),
        probe: Arc::new(SystemProbe),
        quiet: false,
    })
}

pub fn test_runtime(layout: Layout, running: bool, dry_run: bool) -> Runtime {
    Runtime {
        layout,
        dry_run,
        yes: true,
        profile: None,
        probe: Arc::new(FixedProbe(running)),
        quiet: true,
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
        Command::Help => "help",
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
        Command::History(_) | Command::Stats(_) | Command::Help => Vec::new(),
    }
}

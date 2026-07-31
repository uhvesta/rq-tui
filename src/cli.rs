use std::io::IsTerminal as _;
use std::path::PathBuf;
use std::process::Command as ProcessCommand;
use std::sync::mpsc::{self, RecvTimeoutError, Sender};
use std::thread::JoinHandle;
use std::time::{Duration, Instant};

use anyhow::{bail, Context, Result};
use clap::{ArgGroup, Args, Parser, Subcommand};

use crate::app::AppState;
use crate::config::AppPaths;
use crate::remote::{resolve_remote, PrReference};
use crate::storage::Storage;
use crate::work_item::{combine_resolved, resolve_local};

#[derive(Debug, Parser)]
#[command(
    name = "rq-tui",
    version,
    about = "A conversational code-review harness",
    arg_required_else_help = true,
    subcommand_required = true
)]
pub(crate) struct Cli {
    #[command(subcommand)]
    pub(crate) command: Command,
}

#[derive(Debug, Subcommand)]
pub(crate) enum Command {
    /// Open an interactive code review.
    Review(ReviewArgs),
    /// Check local dependencies and storage without entering TUI mode.
    Doctor,
    /// List previously opened work items without entering TUI mode.
    History {
        /// Emit machine-readable JSON.
        #[arg(long)]
        json: bool,
    },
    /// Render a deterministic mocked UI state for visual inspection.
    #[command(hide = true)]
    UiSnapshot {
        /// State name: review, ask, command, composer, quiet, queue, side, model, settings, markdown, tiny, or all.
        #[arg(long, default_value = "all")]
        state: String,
        #[arg(long, default_value_t = 100)]
        width: u16,
        #[arg(long, default_value_t = 28)]
        height: u16,
    },
    /// Drive the headless TUI harness with a scripted key/text sequence read
    /// from stdin and print the resulting frame(s) for e2e exploration.
    #[command(hide = true)]
    UiScript {
        /// Fixture diff: default, foldheavy, manyfiles, longlines, or unicode.
        #[arg(long, default_value = "default")]
        fixture: String,
        #[arg(long, default_value_t = 100)]
        width: u16,
        #[arg(long, default_value_t = 28)]
        height: u16,
    },
}

#[derive(Debug, Args)]
#[command(group(
    ArgGroup::new("target")
        .required(true)
        .multiple(true)
        .args(["path", "prs"])
))]
pub(crate) struct ReviewArgs {
    /// Local repository or folder containing repositories.
    #[arg(value_name = "PATH")]
    pub(crate) path: Option<PathBuf>,

    /// Remote PR reference, repeatable for a multi-repo work item.
    #[arg(long = "pr", value_name = "OWNER/REPO#NUMBER")]
    pub(crate) prs: Vec<String>,

    /// Override the base branch for local repositories.
    #[arg(long)]
    pub(crate) base: Option<String>,
}

pub(crate) fn run_review(args: ReviewArgs, paths: AppPaths) -> Result<()> {
    require_interactive_terminal(io_is_interactive())?;
    let mut progress = StartupProgress::begin("Opening review storage");
    let storage = Storage::open(&paths.database)?;
    let workspace = args.path.as_deref().map(resolve_invocation_path);
    let resolved = match (args.path.as_deref(), args.prs.is_empty()) {
        (Some(_), true) => {
            progress.stage("Inspecting local repositories and computing diffs");
            resolve_local(
                workspace.as_deref().expect("path was present"),
                args.base.as_deref(),
                &paths,
                &storage,
            )?
        }
        (None, false) => {
            progress.stage("Fetching pull-request metadata and revisions");
            let references = args
                .prs
                .iter()
                .map(|reference| PrReference::parse(reference))
                .collect::<Result<Vec<_>>>()?;
            resolve_remote(&references, &paths, &storage)?
        }
        (Some(_), false) => {
            progress.stage("Inspecting local repositories and computing diffs");
            let local = resolve_local(
                workspace.as_deref().expect("path was present"),
                args.base.as_deref(),
                &paths,
                &storage,
            )?;
            progress.stage("Fetching pull-request metadata and revisions");
            let references = args
                .prs
                .iter()
                .map(|reference| PrReference::parse(reference))
                .collect::<Result<Vec<_>>>()?;
            let remote = resolve_remote(&references, &paths, &storage)?;
            combine_resolved(local, remote, &paths, &storage)?
        }
        (None, true) => unreachable!("clap requires a target"),
    };
    crate::ui::run(AppState::new(resolved), &storage, &paths, progress)
}

enum StartupSignal {
    Stage(String),
    Stop,
}

pub(crate) struct StartupProgress {
    started: Instant,
    sender: Sender<StartupSignal>,
    worker: Option<JoinHandle<()>>,
}

impl StartupProgress {
    const HEARTBEAT: Duration = Duration::from_secs(2);

    fn begin(stage: &str) -> Self {
        let started = Instant::now();
        let (sender, receiver) = mpsc::channel();
        let mut current_stage = stage.to_owned();
        eprintln!("{}", startup_message(&current_stage, Duration::ZERO));
        let worker = std::thread::spawn(move || loop {
            match receiver.recv_timeout(Self::HEARTBEAT) {
                Ok(StartupSignal::Stage(stage)) => current_stage = stage,
                Ok(StartupSignal::Stop) | Err(RecvTimeoutError::Disconnected) => break,
                Err(RecvTimeoutError::Timeout) => {
                    eprintln!("{}", startup_message(&current_stage, started.elapsed()));
                }
            }
        });
        Self {
            started,
            sender,
            worker: Some(worker),
        }
    }

    pub(crate) fn stage(&mut self, stage: &str) {
        eprintln!("{}", startup_message(stage, self.started.elapsed()));
        let _ = self.sender.send(StartupSignal::Stage(stage.to_owned()));
    }

    pub(crate) fn finish(mut self) {
        self.stop();
        eprintln!(
            "rq-tui · review ready in {} · entering TUI",
            format_elapsed(self.started.elapsed())
        );
    }

    fn stop(&mut self) {
        let _ = self.sender.send(StartupSignal::Stop);
        if let Some(worker) = self.worker.take() {
            let _ = worker.join();
        }
    }
}

impl Drop for StartupProgress {
    fn drop(&mut self) {
        self.stop();
    }
}

fn startup_message(stage: &str, elapsed: Duration) -> String {
    format!(
        "rq-tui · {stage} · {} elapsed · Ctrl-C cancels",
        format_elapsed(elapsed)
    )
}

fn format_elapsed(elapsed: Duration) -> String {
    let seconds = elapsed.as_secs();
    if seconds < 60 {
        format!("{seconds}s")
    } else {
        format!("{}m {:02}s", seconds / 60, seconds % 60)
    }
}

fn io_is_interactive() -> bool {
    std::io::stdin().is_terminal() && std::io::stdout().is_terminal()
}

fn require_interactive_terminal(interactive: bool) -> Result<()> {
    if !interactive {
        bail!(
            "interactive review requires a TTY on stdin and stdout; use `rq-tui doctor` or `rq-tui history` for non-interactive CLI operation"
        );
    }
    Ok(())
}

fn resolve_invocation_path(path: &std::path::Path) -> PathBuf {
    if path.is_absolute() {
        return path.to_path_buf();
    }
    std::env::var_os("BUILD_WORKSPACE_DIRECTORY")
        .map(PathBuf::from)
        .unwrap_or_else(|| std::env::current_dir().unwrap_or_else(|_| PathBuf::from(".")))
        .join(path)
}

pub(crate) fn run_doctor(paths: AppPaths) -> Result<()> {
    let storage = Storage::open(&paths.database)?;
    let checks = [
        ("git", command_version("git", &["--version"])),
        ("gh", command_version("gh", &["--version"])),
        ("copilot", command_version("copilot", &["--version"])),
    ];

    println!("rq-tui doctor");
    println!("  database  {} (ok)", storage.path().display());
    println!("  cache     {}", paths.cache.display());
    println!("  data      {}", paths.data.display());
    for (name, result) in checks {
        match result {
            Ok(version) => println!("  {name:<9} {version}"),
            Err(error) => println!("  {name:<9} unavailable ({error})"),
        }
    }
    Ok(())
}

pub(crate) fn run_history(paths: AppPaths, json: bool) -> Result<()> {
    let storage = Storage::open(&paths.database)?;
    let items = storage.list_work_items()?;
    if json {
        println!("{}", serde_json::to_string_pretty(&items)?);
    } else if items.is_empty() {
        println!("No reviews have been opened.");
    } else {
        for item in items {
            println!(
                "{}\t{}\t{}",
                item.id,
                item.last_opened_at.as_deref().unwrap_or("never"),
                item.workspace_root.display()
            );
        }
    }
    Ok(())
}

fn command_version(program: &str, args: &[&str]) -> Result<String> {
    let output = ProcessCommand::new(program)
        .args(args)
        .output()
        .with_context(|| format!("failed to run {program}"))?;
    if !output.status.success() {
        bail!("exited with {}", output.status);
    }
    Ok(String::from_utf8_lossy(&output.stdout)
        .lines()
        .next()
        .unwrap_or("ok")
        .trim()
        .to_owned())
}

#[cfg(test)]
mod tests {
    use std::time::Duration;

    use clap::Parser;

    use super::{format_elapsed, require_interactive_terminal, startup_message, Cli, Command};

    #[test]
    fn incomplete_review_is_rejected_before_tui_startup() {
        assert!(Cli::try_parse_from(["rq-tui", "review"]).is_err());
    }

    #[test]
    fn review_rejects_non_interactive_terminal_before_tui_startup() {
        let error = require_interactive_terminal(false).unwrap_err().to_string();
        assert!(error.contains("requires a TTY"));
        assert!(error.contains("doctor"));
        assert!(require_interactive_terminal(true).is_ok());
    }

    #[test]
    fn local_review_is_accepted() {
        let cli = Cli::try_parse_from(["rq-tui", "review", "."]).unwrap();
        assert!(matches!(cli.command, Command::Review(_)));
    }

    #[test]
    fn remote_review_is_accepted() {
        let cli = Cli::try_parse_from(["rq-tui", "review", "--pr", "acme/api#42"]).unwrap();
        assert!(matches!(cli.command, Command::Review(_)));
    }

    #[test]
    fn mixed_local_and_remote_review_is_accepted() {
        let cli = Cli::try_parse_from(["rq-tui", "review", ".", "--pr", "acme/api#42"]).unwrap();
        assert!(matches!(cli.command, Command::Review(_)));
    }

    #[test]
    fn startup_progress_names_the_stage_elapsed_time_and_escape_hatch() {
        assert_eq!(format_elapsed(Duration::from_secs(7)), "7s");
        assert_eq!(format_elapsed(Duration::from_secs(125)), "2m 05s");
        assert_eq!(
            startup_message(
                "Inspecting local repositories and computing diffs",
                Duration::from_secs(3)
            ),
            "rq-tui · Inspecting local repositories and computing diffs · 3s elapsed · Ctrl-C cancels"
        );
    }
}

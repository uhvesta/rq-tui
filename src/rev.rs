use std::io::IsTerminal as _;
use std::path::{Path, PathBuf};

use anyhow::{bail, Context, Result};
use clap::{Parser, Subcommand};

use crate::config::AppPaths;
use crate::export::{CommentExport, ReviewArchive};
use crate::storage::Storage;
use crate::work_item::resolve_local;

#[derive(Debug, Parser)]
#[command(
    name = "rev",
    version,
    about = "A small, persistent workspace code-review TUI",
    long_about = "Review one workspace containing one or more Git repositories. \
                  Questions use isolated Copilot sessions; comments and answers \
                  are retained in SQLite until explicitly cleared.",
    arg_required_else_help = true
)]
struct RevCli {
    /// Open this workspace directly. Equivalent to `rev review PATH`.
    #[arg(value_name = "PATH")]
    path: Option<PathBuf>,

    /// Override the base branch while opening a workspace directly.
    #[arg(long, global = true)]
    base: Option<String>,

    #[command(subcommand)]
    command: Option<RevCommand>,
}

#[derive(Debug, Subcommand)]
enum RevCommand {
    /// Open the review TUI.
    Review {
        #[arg(default_value = ".")]
        path: PathBuf,
    },
    /// Print persisted comments and question/answer history.
    History {
        path: Option<PathBuf>,
        #[arg(long)]
        json: bool,
    },
    /// Print a structured, paste-ready agent feedback prompt.
    Feedback {
        #[arg(default_value = ".")]
        path: PathBuf,
    },
    /// Export the paste-ready feedback prompt, optionally to a file.
    Export {
        #[arg(default_value = ".")]
        path: PathBuf,
        #[arg(long)]
        output: Option<PathBuf>,
    },
    /// Clear comments and questions but retain the workspace entry.
    Clear {
        #[arg(default_value = ".")]
        path: PathBuf,
        #[arg(long)]
        yes: bool,
    },
    /// Permanently remove the current workspace and all local review history.
    Delete {
        #[arg(default_value = ".")]
        path: PathBuf,
        /// Save a Markdown archive before deletion.
        #[arg(long)]
        export: bool,
        #[arg(long)]
        yes: bool,
    },
    /// Render a deterministic mock frame without opening a terminal.
    #[command(hide = true)]
    UiSnapshot {
        #[arg(long, default_value_t = 100)]
        width: u16,
        #[arg(long, default_value_t = 28)]
        height: u16,
        /// review, split, expanded, command, composer, history, or streaming
        #[arg(long, default_value = "review")]
        state: String,
    },
}

pub(crate) fn run() -> Result<()> {
    let cli = RevCli::parse();
    let paths = AppPaths::discover()?;
    match (cli.path, cli.command) {
        (Some(path), None) => open(path, cli.base.as_deref(), paths),
        (None, Some(RevCommand::Review { path })) => open(path, cli.base.as_deref(), paths),
        (None, Some(RevCommand::History { path, json })) => history(path, json, paths),
        (None, Some(RevCommand::Feedback { path })) => feedback(path, paths),
        (None, Some(RevCommand::Export { path, output })) => export(path, output, paths),
        (None, Some(RevCommand::Clear { path, yes })) => clear(path, yes, paths),
        (None, Some(RevCommand::Delete { path, export, yes })) => delete(path, export, yes, paths),
        (
            None,
            Some(RevCommand::UiSnapshot {
                width,
                height,
                state,
            }),
        ) => {
            print!("{}", crate::rev_ui::render_snapshot(width, height, &state)?);
            Ok(())
        }
        (Some(_), Some(_)) => bail!("provide either a workspace path or a subcommand, not both"),
        (None, None) => unreachable!("clap requires a path or subcommand"),
    }
}

fn open(path: PathBuf, base: Option<&str>, paths: AppPaths) -> Result<()> {
    require_tty()?;
    let workspace = invocation_path(&path);
    eprintln!(
        "rev · inspecting workspace {} · Ctrl-C cancels · progress updates every second",
        workspace.display()
    );
    let worker_workspace = workspace.clone();
    let worker_base = base.map(str::to_owned);
    let worker_paths = paths.clone();
    let (sender, receiver) = std::sync::mpsc::sync_channel(1);
    std::thread::spawn(move || {
        let result = Storage::open(&worker_paths.database).and_then(|storage| {
            resolve_local(
                &worker_workspace,
                worker_base.as_deref(),
                &worker_paths,
                &storage,
            )
        });
        sender.send(result).ok();
    });
    let started = std::time::Instant::now();
    let resolved = loop {
        match receiver.recv_timeout(std::time::Duration::from_secs(1)) {
            Ok(result) => break result?,
            Err(std::sync::mpsc::RecvTimeoutError::Timeout) => eprintln!(
                "rev · still inspecting local Git repositories · {}s elapsed · Ctrl-C cancels",
                started.elapsed().as_secs()
            ),
            Err(std::sync::mpsc::RecvTimeoutError::Disconnected) => {
                bail!("workspace inspection worker stopped unexpectedly")
            }
        }
    };
    let storage = Storage::open(&paths.database)?;
    eprintln!(
        "rev · workspace ready in {}ms · opening terminal",
        started.elapsed().as_millis()
    );
    crate::rev_ui::run(resolved, &storage, &paths)
}

fn history(path: Option<PathBuf>, json: bool, paths: AppPaths) -> Result<()> {
    let storage = Storage::open(&paths.database)?;
    if let Some(path) = path {
        let item = current_item(&storage, &path)?;
        let archive = ReviewArchive::load(&storage, &item)?;
        if json {
            println!("{}", serde_json::to_string_pretty(&archive)?);
        } else {
            print!("{}", archive.markdown());
        }
        return Ok(());
    }
    let items = storage.list_work_items()?;
    if json {
        println!("{}", serde_json::to_string_pretty(&items)?);
    } else if items.is_empty() {
        println!("No persisted reviews.");
    } else {
        for item in items {
            let counts = ReviewArchive::load(&storage, &item)?.annotations.len();
            if counts > 0 {
                println!(
                    "{}\t{} review item(s)\t{}",
                    item.last_opened_at.as_deref().unwrap_or("never"),
                    counts,
                    item.workspace_root.display()
                );
            }
        }
    }
    Ok(())
}

fn feedback(path: PathBuf, paths: AppPaths) -> Result<()> {
    let storage = Storage::open(&paths.database)?;
    let item = current_item(&storage, &path)?;
    let export = CommentExport::load(&storage, &item)?;
    if export.comments.is_empty() {
        println!("No feedback has been recorded for this workspace.");
    } else {
        print!("{}", export.structured_agent_prompt());
    }
    Ok(())
}

fn export(path: PathBuf, output: Option<PathBuf>, paths: AppPaths) -> Result<()> {
    let storage = Storage::open(&paths.database)?;
    let item = current_item(&storage, &path)?;
    let export = CommentExport::load(&storage, &item)?;
    if export.comments.is_empty() {
        println!("No feedback has been recorded for this workspace.");
        return Ok(());
    }
    let prompt = export.structured_agent_prompt();
    if let Some(output) = output {
        std::fs::write(&output, prompt)?;
        println!("Exported feedback prompt to {}.", output.display());
    } else {
        print!("{prompt}");
    }
    Ok(())
}

fn clear(path: PathBuf, confirmed: bool, paths: AppPaths) -> Result<()> {
    if !confirmed {
        bail!("refusing to clear review history without --yes");
    }
    let storage = Storage::open(&paths.database)?;
    let item = current_item(&storage, &path)?;
    storage.clear_review_history(&item.id)?;
    println!(
        "Cleared comments and questions for {}. Workspace metadata was retained.",
        item.workspace_root.display()
    );
    Ok(())
}

fn delete(path: PathBuf, export: bool, confirmed: bool, paths: AppPaths) -> Result<()> {
    if !confirmed {
        bail!("refusing permanent workspace deletion without --yes");
    }
    let storage = Storage::open(&paths.database)?;
    let item = current_item(&storage, &path)?;
    eprintln!(
        "rev · deleting durable Copilot sessions before local history · Ctrl-C cancels safely"
    );
    let outcome = crate::copilot::prune_work_item_permanently(
        &paths,
        &item.id,
        export,
        |label, done, total| {
            if total == 0 {
                eprintln!("rev · {label}");
            } else {
                eprintln!("rev · [{done}/{total}] {label}");
            }
        },
    )?;
    if let Some(error) = outcome.error {
        bail!("permanent deletion failed; local history was retained: {error}");
    }
    println!(
        "Deleted Copilot sessions, local review history, and workspace metadata for {}.",
        item.workspace_root.display()
    );
    Ok(())
}

fn current_item(storage: &Storage, path: &Path) -> Result<crate::domain::WorkItem> {
    let workspace = invocation_path(path)
        .canonicalize()
        .with_context(|| format!("cannot resolve workspace {}", path.display()))?;
    for candidate in workspace.ancestors() {
        if let Some(item) = storage.work_item_by_root(candidate)? {
            return Ok(item);
        }
    }
    Err(anyhow::anyhow!(
        "no persisted review for {} or any parent workspace",
        workspace.display()
    ))
}

fn invocation_path(path: &Path) -> PathBuf {
    if path.is_absolute() {
        return path.to_path_buf();
    }
    std::env::var_os("BUILD_WORKSPACE_DIRECTORY")
        .map(PathBuf::from)
        .unwrap_or_else(|| std::env::current_dir().unwrap_or_else(|_| PathBuf::from(".")))
        .join(path)
}

fn require_tty() -> Result<()> {
    if !std::io::stdin().is_terminal() || !std::io::stdout().is_terminal() {
        bail!(
            "interactive review requires a TTY; use `rev history`, `rev feedback`, \
             `rev clear`, or `rev delete` for normal CLI operation"
        );
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use clap::Parser;

    use super::{RevCli, RevCommand};

    #[test]
    fn bare_rev_prints_help_instead_of_entering_the_tui() {
        assert!(RevCli::try_parse_from(["rev"]).is_err());
    }

    #[test]
    fn workspace_path_is_the_short_review_form() {
        let cli = RevCli::try_parse_from(["rev", "."]).unwrap();
        assert!(cli.path.is_some());
        assert!(cli.command.is_none());
    }

    #[test]
    fn destructive_commands_require_an_explicit_command() {
        let cli = RevCli::try_parse_from(["rev", "delete", ".", "--yes"]).unwrap();
        assert!(matches!(
            cli.command,
            Some(RevCommand::Delete { yes: true, .. })
        ));
    }

    #[test]
    fn export_is_a_normal_noninteractive_cli_command() {
        let cli =
            RevCli::try_parse_from(["rev", "export", ".", "--output", "feedback.md"]).unwrap();
        assert!(matches!(
            cli.command,
            Some(RevCommand::Export {
                output: Some(_),
                ..
            })
        ));
    }
}

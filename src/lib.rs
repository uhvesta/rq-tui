#[cfg(test)]
pub(crate) mod agent_state;
pub(crate) mod annotations;
pub(crate) mod app;
pub(crate) use rq_tui_text::chat_render;
pub(crate) use rq_tui_text::chat_selection;
pub(crate) mod cli;
pub(crate) mod cmux;
pub(crate) mod config;
#[allow(dead_code)] // Bounded model slice; UI/storage integration follows separately.
pub(crate) mod context_editor;
pub(crate) mod copilot;
pub(crate) mod diff;
pub(crate) mod domain;
pub(crate) mod export;
pub(crate) mod git;
pub(crate) use rq_tui_text::highlight;
pub(crate) use rq_tui_text::markdown;
pub(crate) mod process_control;
pub(crate) mod prune;
pub(crate) mod remote;
pub(crate) mod rev;
pub(crate) mod rev_ui;
pub(crate) mod review_stream;
pub(crate) mod storage;
pub(crate) use rq_tui_text::terminal_text;
#[doc(hidden)]
pub mod testing;
pub(crate) mod ui;
pub(crate) mod work_item;

use anyhow::Result;
use clap::Parser;

use crate::cli::{Cli, Command};
use crate::config::AppPaths;

pub fn run() -> Result<()> {
    let cli = Cli::parse();
    let paths = AppPaths::discover()?;

    match cli.command {
        Command::Review(args) => crate::cli::run_review(args, paths),
        Command::Doctor => crate::cli::run_doctor(paths),
        Command::History { json } => crate::cli::run_history(paths, json),
        Command::UiSnapshot {
            state,
            width,
            height,
        } => {
            print!(
                "{}",
                crate::testing::render_ui_scenario(&state, width, height)?
            );
            Ok(())
        }
        Command::UiScript {
            fixture,
            width,
            height,
        } => {
            use std::io::Read;
            let mut script = String::new();
            std::io::stdin().read_to_string(&mut script)?;
            print!(
                "{}",
                crate::testing::run_ui_script(&fixture, width, height, &script)?
            );
            Ok(())
        }
    }
}

pub fn run_rev() -> Result<()> {
    crate::rev::run()
}

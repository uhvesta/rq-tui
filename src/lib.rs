pub(crate) mod annotations;
pub(crate) mod app;
pub(crate) mod chat_render;
pub(crate) mod chat_selection;
pub(crate) mod cli;
pub(crate) mod config;
pub(crate) mod copilot;
pub(crate) mod diff;
pub(crate) mod domain;
pub(crate) mod export;
pub(crate) mod git;
pub(crate) mod highlight;
pub(crate) mod markdown;
pub(crate) mod remote;
pub(crate) mod review_stream;
pub(crate) mod storage;
pub(crate) mod terminal_text;
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

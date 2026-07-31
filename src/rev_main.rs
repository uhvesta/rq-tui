use std::process::ExitCode;

fn main() -> ExitCode {
    match rq_tui::run_rev() {
        Ok(()) => ExitCode::SUCCESS,
        Err(error) => {
            eprintln!("error: {error:#}");
            ExitCode::FAILURE
        }
    }
}

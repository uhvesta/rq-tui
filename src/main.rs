use std::{env, io, process::Command, time::Duration};

use crossterm::{
    event::{self, Event, KeyCode, KeyEvent, KeyModifiers},
    execute,
    terminal::{disable_raw_mode, enable_raw_mode, EnterAlternateScreen, LeaveAlternateScreen},
};
use ratatui::{
    backend::CrosstermBackend,
    widgets::{Block, Borders, Paragraph, Wrap},
    Terminal,
};

fn main() -> Result<(), Box<dyn std::error::Error>> {
    let content = review_content();
    enable_raw_mode()?;
    let mut stdout = io::stdout();
    execute!(stdout, EnterAlternateScreen)?;
    let backend = CrosstermBackend::new(stdout);
    let mut terminal = Terminal::new(backend)?;

    let result = run(&mut terminal, &content);

    disable_raw_mode()?;
    execute!(terminal.backend_mut(), LeaveAlternateScreen)?;
    terminal.show_cursor()?;
    result
}

fn run<B: ratatui::backend::Backend>(
    terminal: &mut Terminal<B>,
    content: &str,
) -> Result<(), Box<dyn std::error::Error>> {
    let mut scroll = 0;

    loop {
        terminal.draw(|frame| {
            let area = frame.area();
            let paragraph = Paragraph::new(content)
                .block(
                    Block::default()
                        .title("rq-tui · review")
                        .borders(Borders::ALL),
                )
                .scroll((scroll, 0))
                .wrap(Wrap { trim: false });
            frame.render_widget(paragraph, area);
        })?;

        if event::poll(Duration::from_millis(250))? {
            if let Event::Key(key) = event::read()? {
                match key {
                    KeyEvent {
                        code: KeyCode::Char('q'),
                        modifiers: KeyModifiers::NONE,
                        ..
                    }
                    | KeyEvent {
                        code: KeyCode::Esc, ..
                    } => return Ok(()),
                    KeyEvent {
                        code: KeyCode::Char('j') | KeyCode::Down,
                        ..
                    } => scroll = scroll.saturating_add(1),
                    KeyEvent {
                        code: KeyCode::Char('k') | KeyCode::Up,
                        ..
                    } => scroll = scroll.saturating_sub(1),
                    KeyEvent {
                        code: KeyCode::PageDown,
                        ..
                    } => scroll = scroll.saturating_add(10),
                    KeyEvent {
                        code: KeyCode::PageUp,
                        ..
                    } => scroll = scroll.saturating_sub(10),
                    _ => {}
                }
            }
        }
    }
}

fn review_content() -> String {
    let mut args = env::args().skip(1);
    match args.next().as_deref() {
        Some("review") => match args.next().as_deref() {
            Some("--pr") => match args.next() {
                Some(pr) => format!(
                    "Remote PR: {pr}\n\nRemote checkout and Copilot session wiring are next.\n\nPress j/k or ↑/↓ to scroll · q or Esc to quit"
                ),
                None => "Missing PR reference after `--pr`.\n\nUsage: rq-tui review --pr owner/repo#123".into(),
            },
            Some(path) => local_diff(path),
            None => local_diff("."),
        },
        _ => "rq-tui\n\nUsage:\n  rq-tui review [PATH]\n  rq-tui review --pr OWNER/REPO#NUMBER\n\nPress q or Esc to quit".into(),
    }
}

fn local_diff(path: &str) -> String {
    match Command::new("git")
        .args(["-C", path, "diff", "--no-ext-diff", "--unified=3"])
        .output()
    {
        Ok(output) if output.status.success() => {
            let diff = String::from_utf8_lossy(&output.stdout);
            if diff.is_empty() {
                format!("No working-tree changes found in {path}.\n\nPress q or Esc to quit")
            } else {
                format!("{path}\n\n{diff}\nPress q or Esc to quit")
            }
        }
        Ok(output) => format!(
            "Unable to read git diff for {path}: {}\n\nPress q or Esc to quit",
            String::from_utf8_lossy(&output.stderr).trim()
        ),
        Err(error) => format!("Unable to run git: {error}\n\nPress q or Esc to quit"),
    }
}

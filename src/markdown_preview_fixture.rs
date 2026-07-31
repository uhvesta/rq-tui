#[allow(dead_code)]
mod markdown_preview;

use std::io::{self, BufRead};

use anyhow::Result;
use markdown_preview::MarkdownPreviewServer;

fn main() -> Result<()> {
    let revision = "browser-fixture-1";
    let document = serde_json::to_string(&serde_json::json!({
        "revision": revision,
        "path": "docs/review.md",
        "previous": "# Review plan\n\nThe old introduction.\n\n## Removed section\n\n- first item\n- removed item\n\n```rust\nfn old() {}\n```\n",
        "current": "# Review plan\n\nThe improved introduction.\n\n## Added section\n\n- first item\n- added item\n\n```rust\nfn current() {\n    println!(\"rich diff\");\n}\n```\n",
        "additions": [3, 5, 8, 10, 11, 12],
        "deletions": [3, 5, 8, 10],
        "browser_fixture": true,
        "notes": [{
            "kind": "feedback",
            "side": "new",
            "line_start": 5,
            "line_end": 5,
            "text": "Keep this heading specific."
        }],
    }))?;
    let focus = serde_json::to_string(&serde_json::json!({
        "revision": revision,
        "focus_side": "new",
        "focus_line": 5,
        "viewport_top": 0,
        "cursor_fraction": 0.28,
    }))?;
    let server = MarkdownPreviewServer::start(document, focus)?;
    println!("{}", server.url());
    println!("Enter `old|new LINE [VIEWPORT_TOP] [CURSOR_FRACTION]`, or `quit`.");
    for line in io::stdin().lock().lines() {
        let line = line?;
        if line == "quit" {
            break;
        }
        let mut parts = line.split_whitespace();
        let Some(side @ ("old" | "new")) = parts.next() else {
            continue;
        };
        let Some(line) = parts.next().and_then(|line| line.parse::<usize>().ok()) else {
            continue;
        };
        let viewport_top = parts
            .next()
            .and_then(|value| value.parse::<usize>().ok())
            .unwrap_or(0);
        let cursor_fraction = parts
            .next()
            .and_then(|value| value.parse::<f64>().ok())
            .unwrap_or(0.28);
        server.update_focus(serde_json::to_string(&serde_json::json!({
            "revision": revision,
            "focus_side": side,
            "focus_line": line,
            "viewport_top": viewport_top,
            "cursor_fraction": cursor_fraction,
        }))?);
    }
    Ok(())
}

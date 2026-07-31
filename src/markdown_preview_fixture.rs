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
    }))?;
    let server = MarkdownPreviewServer::start(document, focus)?;
    println!("{}", server.url());
    println!("Enter `old LINE`, `new LINE`, or `quit`.");
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
        server.update_focus(serde_json::to_string(&serde_json::json!({
            "revision": revision,
            "focus_side": side,
            "focus_line": line,
        }))?);
    }
    Ok(())
}

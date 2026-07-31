# rev Markdown preview fork

This directory vendors the browser-rendering pieces used by
[`iamcco/markdown-preview.nvim`](https://github.com/iamcco/markdown-preview.nvim)
at upstream commit `a923f5f`.

`rev` does not embed Neovim or the upstream Node/socket.io server. Its Rust
process serves one local-only browser page and publishes review state as JSON.
The fork keeps the upstream Markdown presentation, Mermaid renderer, source-line
metadata, and cursor-to-rendered-document scroll model, then adds:

- current/previous rendered revisions with added and removed block treatment;
- review-comment markers;
- immediate, non-animated movement to the active diff line;
- one stable cmux browser surface instead of reopening Markdown files.

The upstream MIT license is in `LICENSE`. The vendored Markdown-It 12.3.2
runtime and its license are under `markdown-it/`. The Mermaid 10.2.3 license is
stored alongside the runtime under `static/MERMAID-LICENSE`. Highlight.js
10.4.1 and its BSD license are also stored under `static/`.

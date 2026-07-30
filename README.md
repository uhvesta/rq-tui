# rq-tui

`rq-tui` is a keyboard-first code-review harness for local changes, GitHub pull
requests, and multi-repository Work Items. It renders diffs with ratatui,
stores review state in SQLite, highlights source with syntect, and uses a
persistent read-only GitHub Copilot SDK session for grounded questions and
chat.

## Requirements

- Bazel 9 through Bazelisk; `.bazelversion` pins `9.2.0`.
- `git` for local review targets.
- `gh` authenticated with GitHub for remote PR reviews.
- `copilot` on `PATH`, or `COPILOT_CLI_PATH=/absolute/path/to/copilot`, for
  Copilot asks and chat.

The repository uses bzlmod through `MODULE.bazel` and its checked-in lockfile.
`.bazelrc` makes bzlmod and `--lockfile_mode=error` the default, so ordinary
commands cannot silently rewrite dependency resolution. During an intentional
dependency update, pass `--lockfile_mode=update` explicitly and commit the
resulting lockfile. Rust compilation, formatting, linting, tests, and release
builds are driven through Bazel targets only. Target and Rust-module visibility
is intentionally kept narrow.

Bootstrap a pinned, checksum-verified Bazelisk on macOS or Linux (arm64 or
x86_64) without installing anything globally:

```sh
./bootstrap-bazelisk.sh
export PATH="$PWD/.tools/bin:$PATH"
```

The script installs Bazelisk under the git-ignored `.tools/bin/` directory.
It pins and verifies the platform checksum, rejects symlinked install paths,
uses bounded HTTPS downloads, and installs atomically under a process lock.
Bazelisk then reads `.bazelversion` and downloads the repository's pinned Bazel
release. Running the script again is idempotent.

## CLI behavior

The normal CLI remains a normal CLI for non-interactive commands:

```sh
bazel run //:rq-tui -- --help
bazel run //:rq-tui -- doctor
bazel run //:rq-tui -- history
bazel run //:rq-tui -- history --json
```

Only `review` with a valid target can enter the TUI. Clap validates that a
local path, one or more `--pr OWNER/REPO#NUMBER` references, or both are
present before storage resolution, Copilot startup, raw mode, or alternate
screen setup:

```sh
bazel run //:rq-tui -- review .
bazel run //:rq-tui -- review . --base origin/develop
bazel run //:rq-tui -- review --pr acme/api#482
bazel run //:rq-tui -- review --pr acme/api#482 --pr acme/web#91
bazel run //:rq-tui -- review . --pr acme/api#482
```

`rq-tui review` without a path or PR is rejected as a CLI error and never
initializes the TUI.

## Review and Chat

Review mode is the diff workspace. `j/k`, `gg/G`, page and half-page movement
navigate the focused pane; `h/l` changes files; `t` toggles the file tree;
`v` selects rows; `a` asks a grounded question; and `c` records a local
comment. Unified is the default layout, while `:diff split` keeps the
old/new side-by-side view. In both layouts, asks and comments are full-width
inline blocks—there is no annotation rail. `y`/`yy` first use the native
clipboard and fall back to OSC 52. `Tab` toggles Review/Chat, while `gc`
always opens Chat and `gr` always opens Review in normal mode.

Chat deliberately has explicit modes so the input state is never ambiguous:

- `NORMAL`: read and navigate the transcript; press `i` to edit, `:` for the
  command palette, `/` for search, and `h/j/k/l`, word, line, page, or
  document motions to navigate rendered text.
- `INSERT`: edit the sticky composer. `Enter`, `Ctrl-J`, `Ctrl-M`, or `Ctrl-S`
  submits; `Shift-Enter` inserts a newline. `Esc` returns to normal mode and
  preserves a Chat draft. During an active response, the first `Ctrl-C` stops
  Copilot and preserves the draft; while idle, `Ctrl-C` discards it.
- `COMMAND`: the palette is the only active input surface. `↑/↓` changes the
  selected suggestion, `PgUp/PgDn` scrolls the actual visible page, `Tab`
  completes, `Enter` runs, and `Esc` cancels.
- `SEARCH`: `/` searches the focused transcript or diff; `Enter` accepts and
  `Esc` cancels.
- `VISUAL`: in a diff, `v` selects source rows. In Chat, `v`, `V`, and
  `Ctrl-V` select rendered text character-wise, line-wise, and block-wise.
  Selection remains attached to source text across Markdown wrapping,
  terminal resize, and streaming updates. `y` copies the exact semantic
  selection and `Esc` returns to normal mode.

Chat scrolling is based on rendered terminal rows, including wrapped Markdown
and long single messages, rather than skipping from message to message. `G`
returns to the latest content and resumes live-following. The application owns
mouse wheel events for Chat and diff scrolling. To select and copy arbitrary
terminal text with the terminal emulator, hold `Shift` while dragging; the
terminal then receives the selection gesture instead of the TUI.

`gm` previews Markdown with the persisted Settings default. Inline preview is
the default and scrolls inside the TUI; `o` requests the system browser while
keeping the inline preview open as a reliable fallback. Settings also persists
the launch diff layout and whether the file tree starts open.

The Chat composer is a sticky bordered rectangle. It wraps, expands with the
draft, supports multiple lines, moves through wrapped visual rows, keeps its
own vertical scroll position, and shows an insertion cursor. It is capped at a
terminal-sized height so a long draft does not cover the complete transcript.
`Ctrl-C` while a Copilot turn is active stops that response and preserves the
draft; after the turn is idle, `Ctrl-C` discards the draft. `:stop`, `:abort`,
or `s` in the agent-status overlay provide the same stop path.

Responses stream through ephemeral deltas and then reconcile with the final
SDK message, so a dropped delta does not corrupt the visible transcript.
Persisted MAIN session history is restored on resume, and MAIN and SIDE
transcripts are kept separate. Tool and skill activity is shown as typed
events—planning, tool start/progress/complete, retries, queue position, and
completion—without displaying hidden model reasoning.

The Chat progress panel is always visible. It reports the lane, phase, elapsed
time, last SDK-event age, event count, queue depth, current operation, and
detail. After a quiet interval it explicitly says the SDK has been quiet while
still connected; after a longer interval it adds a warning and points to
`:agent-status`. That overlay provides connection state, lane, phase, elapsed
time, last event, queue depth, outbound ID, and a scrollable recent activity
timeline. Use `j/k` to inspect it, `s` to stop the current turn, and `q`/`Esc`
to return.

Questions submitted during an active turn continue in the background. They are
shown by `:queue` and delivered FIFO; `j/k` selects an entry, `d` cancels a
selected waiting prompt, and `s` stops the active prompt. `/steer <correction>`
(or `:steer <correction>`) sends an immediate correction to the active Copilot
loop. `:model` opens a staged runtime-capability picker for model, reasoning
effort, and context tier.

### `/side`

`/side [question]` in the Chat composer creates an ephemeral, isolated SIDE
conversation from the persistent MAIN session. The SIDE view is visibly
labelled, receives its own transcript and progress lane, and is constrained
to answer the side question without continuing MAIN's task. The first SIDE
message includes a reference-only MAIN-history boundary; subsequent SIDE
messages stay in that lane.

`/main` or `/side-exit` closes SIDE, discards its transcript, and restores the
unchanged MAIN transcript. The equivalent command-palette forms are
`:side [question]` and `:main`. A second SIDE cannot be created until the
current one is exited. SIDE creation is recorded durably before the SDK fork:
if rq-tui exits during fork, open, or deletion, the next owning process
reconciles the named fork and retries bounded cleanup in the background
without ever treating it as MAIN. A per-Work-Item lease with an independent
heartbeat and fenced storage transitions prevents concurrent rq-tui processes from
deleting each other's live SIDE; MAIN remains usable while lease recovery and
cleanup progress are shown in the agent timeline.

## Skills and plugins

Copilot receives global skills from the data directory's `skills/` folder and
repository skills from `.rq-tui/skills`. Plugin directories follow the same
pattern through the global `plugins/` folder and repository
`.rq-tui/plugins`. Hook, tool, skill, MCP-tool, and subagent lifecycle activity
is routed into the visible progress timeline; the read-only permission handler
still rejects shell and write requests.

## Markdown and syntax highlighting

Chat messages use terminal Markdown semantics rather than printing raw markup:
headings, unordered/ordered/task lists, blockquotes, emphasis, inline code,
blank lines, wrapping, and fenced code blocks are rendered as terminal rows.
Fenced code is passed through the same lazy, cached syntax-highlighting
interface used by diffs. Language fences create a synthetic filename, while
diff files are detected by path. The deterministic tests cover Rust, Python,
TypeScript, JavaScript, Go, JSON, YAML, TOML, Markdown, shell, C/C++, Java,
C#, Ruby, PHP, SQL, HTML, CSS, and Swift.

## Persistence and recovery

State lives under the platform data directory (normally
`~/.local/share/rq-tui`) and cache directory (normally `~/.cache/rq-tui`).
These can be overridden with:

```sh
RQ_TUI_DATA_DIR=/path/to/data
RQ_TUI_CACHE_DIR=/path/to/cache
RQ_TUI_DATABASE=/path/to/rq-tui.db
```

SQLite uses WAL mode, foreign keys, migrations, and per-version annotation
placements. Local annotations pin Git snapshots without moving `HEAD` or the
index. Remote versions use a bare cache plus detached worktrees. New versions
carry annotations forward by content, follow renames, and mark missing or
ambiguous anchors.

Outbound MAIN Chat prompts, asks, comment batches, and accepted context are
persisted as pending before send and acknowledged when delivery begins. Queue
edits update the Chat outbox atomically and cancellation removes the matching
record. A restart never silently replays pending work: a recovery screen
requires an explicit resend, discard, or defer decision. SIDE remains
deliberately ephemeral and is not recovered as MAIN work.

## Deterministic UI inspection and tests

The hidden `ui-snapshot` command renders production UI code with a temporary
database and deterministic fake agent, without a real TTY, Copilot process,
network, GitHub account, or repository:

```sh
bazel run //:rq-tui -- ui-snapshot --state all --width 100 --height 28
bazel run //:rq-tui -- ui-snapshot --state quiet --width 110 --height 26
```

The `all` gallery includes `review`, `ask`, `command`, `composer`, `quiet`,
`queue`, `side`, `model`, `settings`, `markdown`, and `tiny` states. It prints
the terminal buffer, making mode labels, palette selection, composer wrapping,
persisted preferences, Markdown rows, progress diagnostics, lane isolation,
and minimum-terminal behavior easy to inspect or snapshot in a test harness.

For multi-step inspection, the hidden `ui-script` command reads a deterministic
script from stdin. Commands include `key`, `type`, `resize`, `stream`,
`stream-start`, `stream-delta`, `stream-complete`, `stream-abort`, `fail`,
`history`, typed `activity`, `models`, `quiet`, `disconnect`, `side-start`,
`side-exit`, and `snapshot`. The final dump includes exact yank bytes and
captured effects:

```sh
printf '%s\n' \
  'key Tab' \
  'key i' \
  'type inspect the cleanup path' \
  'key Enter' \
  'snapshot queued' \
  'stream-start deterministic ' \
  'snapshot partial' \
  'stream-delta answer' \
  'stream-complete' \
  'snapshot complete' |
  bazel run //:rq-tui -- ui-script --fixture unicode --width 80 --height 20
```

`TuiHarness` is the public headless testing surface. Its reducer/effect loop
uses a temporary SQLite database and a fake agent behind the small agent
interface; it can inject streaming, tool, skill, failure, liveness, and SIDE
events, restart from the same database, inspect persisted annotations, and
inspect rendered styles. The controlled compiled agent is enabled only with
`RQ_TUI_CONTROLLED_AGENT=1` and is useful for pseudo-terminal audits without
production credentials.

## Build and test with Bazel

```sh
bazel build //:rq-tui
bazel test //:rq_tui_tests
```

The test suite includes the reducer, diff parsing and anchoring, SQLite
migrations, Git integration, language detection/highlighting, read/search-only
Copilot permissions, Markdown rendering, deterministic UI frames, Chat row
scrolling, command-palette scrolling, sticky-composer editing/cancellation,
SDK liveness diagnostics, and MAIN/SIDE isolation. The suite also runs the
Rust formatting and Clippy targets through the root test suite.

The current regular verification baseline is recorded in
[`docs/behavioral-audit.md`](docs/behavioral-audit.md). The opt-in authenticated
test and compiled controlled-agent PTY workflows were also rerun on
2026-07-30.

The authenticated live test is opt-in and ignored by default. It checks real
Copilot streaming, completion, SIDE lifecycle, SIDE boundary behavior,
disconnect, session resume, and restored MAIN history:

```sh
RQ_TUI_LIVE_COPILOT=1 \
COPILOT_CLI_PATH="$(command -v copilot)" \
bazel test //src:rq_tui_tests \
  --test_arg=live_copilot_streams_and_resumes_persisted_history \
  --test_arg=--ignored \
  --test_env=RQ_TUI_LIVE_COPILOT \
  --test_env=COPILOT_CLI_PATH \
  --test_env=HOME \
  --test_env=PATH \
  --test_output=streamed \
  --strategy=TestRunner=local
```

The local test strategy is required on macOS because the sandbox blocks the
Copilot CLI's network access. The authenticated test remains opt-in and is not
part of the regular baseline.

## Releases

Pull requests and `main` run Bazel tests and builds on Linux x86_64, Linux
aarch64, macOS x86_64, and macOS aarch64. Tags matching `v*` publish a
compressed binary and SHA-256 file for each of those four targets through
GitHub Actions. Windows is intentionally not part of the matrix.

The authoritative product contract is
[`docs/spec-v2-consolidated.md`](docs/spec-v2-consolidated.md). Historical
implementation and Copilot SDK audit trails are in
[`docs/behavioral-audit.md`](docs/behavioral-audit.md) and
[`docs/copilot-sdk-audit.md`](docs/copilot-sdk-audit.md). Current compiled
pseudo-terminal evidence is recorded in
[`docs/audit/pty-smoke.md`](docs/audit/pty-smoke.md).

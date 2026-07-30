# Code Review Harness — Spec (v2, consolidated)

## Document status

This is the single, self-contained specification to build against. It
replaces and folds together three prior sources, in priority order when they
disagree:

1. **New owner requirements** (2026-07-30, verbatim intent below) — highest
   priority; where they contradict either of the other two sources, they win
   and the change is called out inline as `Changed from v2:`.
2. **[`chat-interaction-spec.md`](chat-interaction-spec.md)** — the agreed,
   detailed specification for Chat-mode feel and interaction quality
   (rendered-text selection, focus visibility, composer behavior, conversation
   execution, MAIN/SIDE sessions, progressive model configuration). Folded
   directly into this document's body, not linked.
3. **The original Spec (v2)** (conversation-only, never filed) — the
   architectural backbone: Work Items, local/remote PRs, versioning and
   snapshots, annotation identity/anchoring, the modal keybinding model, the
   SQLite schema, the FSM, and SDK capability notes. Everything from it that
   isn't superseded below is carried forward unchanged.

Nobody implementing against this document should need to go back and read any
of the three source documents. `behavioral-audit.md` and `copilot-sdk-audit.md`
remain useful as historical audit trail against the *previous* build, but they
describe a prior implementation's gaps, not requirements of this spec.

---

## 1. Overview

A personal terminal UI (TUI) for reviewing code, built as a harness on top of
the GitHub Copilot SDK/CLI — not a wrapper around GitHub's own review UI.
GitHub (`gh` CLI) is used only as a data source for fetching remote PRs. All
review interaction (asking questions, leaving comments, chatting) happens
through one persistent Copilot SDK session per review, giving the tool
"harness" behavior: read-only Q&A, structured comments, and an actual
conversational agent, all grounded in the diff being reviewed.

The defining interaction principle of this v2: **everything lives in one
linear, scrollable stream.** There is no side rail, no third column, no pane
you have to tab into to see what you asked or noted. Comments and asks render
as bordered blocks woven directly into the diff at their anchor line — the
same visual idiom Claude Code, Codex CLI, and the Copilot CLI use to weave a
tool-call or diagnostic block into a scrolling terminal transcript that you
page through with ordinary `j`/`k`, not a side panel you switch into. Chat
mode gets the same treatment in reverse: it must feel like those same CLIs —
a transcript you scroll and select text in like a real editor, not a
message-index list.

**Scope note — read this before touching diff-pane navigation.** The
existing build's core diff-pane navigation is already right and is
explicitly *not* being redesigned here: file-to-file movement (`h`/`l`),
line-to-line movement (`j`/`k`, including moving straight through an
expanded inline block and back out, §3.4), fold expand/collapse (`o`/`O`,
§9), `gg`/`G`, and in-pane search (`/`, `n`/`N`, §9) all already feel
correct and should be carried forward as-is, mechanically unchanged. Every
requirement in this document is about what surrounds that navigation, not
about replacing it: killing the annotation rail (§2.5, §3.4), Chat's feel
and rendered-text selection (§7), focus visibility and window scope (§4.2,
§4.3), composer growth (§7.5), inline Markdown preview (§3.11), and the
crate/workspace split (§1.1). If an implementer finds themselves rewriting
how the cursor moves through diff lines, that's a sign they've drifted
outside this document's scope, not a sign the spec asked for it.

**Stack:** Rust, [ratatui](https://ratatui.rs) (TUI framework), `syntect`
(syntax highlighting, lazy per-viewport — see §9), `github-copilot-sdk` (Rust
crate, JSON-RPC to the `copilot` CLI in server mode), `gh` CLI shelled out to
for remote PR metadata; git plumbing (clone/worktree/write-tree) for all
checkout and snapshot mechanics. Build/test gate is Bazel (Bzlmod), macOS/
Linux only.

**Prefer existing ratatui-ecosystem widgets over bespoke ones.** Before
hand-rolling a widget, check the [`tui-widgets`](https://crates.io/crates/tui-widgets)
family and the broader ratatui ecosystem for something that already does
the job — this build has repeatedly reinvented things (wrapped-text editing
with cursor tracking for composers, §7.5; scrollable viewports with a
follow/pause model, §7.4; markdown rendering, §3.11 below) that community
widgets likely already solve, tested, with fewer edge cases than a
from-scratch implementation. Concretely worth evaluating first: a
maintained textarea/editor widget for the Chat and inline Ask/Comment
composers (wrapped-row cursor movement, multi-line editing — §7.5) instead
of the bespoke wrapped-line cursor math this codebase has carried since v1;
a scrollview widget for any pane that needs viewport+scrollbar+follow
semantics; and a markdown-rendering widget for §3.11's inline preview.
Adopt a widget wholesale where it fits; fall back to bespoke only where the
inline-block/no-rail interaction model (§3.4) genuinely has no off-the-shelf
equivalent — don't rewrite something a maintained crate already gets right.

### 1.1 Crate layout: a Cargo workspace, not one crate

**Decision:** this build starts as a Cargo workspace with several member
crates instead of one monolithic crate. The goal is twofold: force a real
API boundary between layers (so e.g. the reducer can't reach into rendering
internals, or storage into UI state, by accident), and make the highest-
value logic — the modal reducer — unit-testable with zero TUI/IO
dependencies at all, not just via the headless `TestBackend` harness
(§7.9) which still pulls in all of ratatui.

Proposed member crates (names indicative, not final):

| Crate | Contents | Depends on | Why split out |
|---|---|---|---|
| `rq-tui-domain` | Work Item / Repo / Version / Annotation / Placement types (§8), diff parsing (unified-diff → hunks/lines) | — | Pure data + pure parsing, zero I/O. The shared vocabulary every other crate imports; changing it is a deliberate, visible event. |
| `rq-tui-git` | Worktree/clone/snapshot plumbing (`git stash create` / `write-tree` trick, §2.3), rename detection for re-anchoring (§2.4) | `rq-tui-domain` | Isolates every `git` subprocess call behind one API; lets higher layers be tested against a fake instead of a real repo. |
| `rq-tui-storage` | SQLite schema, migrations, queries (§8) | `rq-tui-domain` | The persistence boundary — swappable/mockable independent of everything else. |
| `rq-tui-remote` | `gh` PR fetch/resolve, cache-dir layout (§2.2) | `rq-tui-domain`, `rq-tui-git` | Network/subprocess-heavy; isolating it keeps the reducer and storage tests free of any dependency on `gh` being installed or authenticated. |
| `rq-tui-copilot` | Copilot SDK session management, the `AgentSink`/`AgentEvent` abstraction, MAIN/SIDE/fork session topology (§2.6) | `rq-tui-domain` | The agent boundary — already partway there via the `AgentSink` trait; a real crate boundary makes the fake-agent-backed tests (§7.9) enforced by the compiler, not just by convention. |
| `rq-tui-app` | The modal reducer: `AppState`, `handle_key`, `Effect` (§4) | `rq-tui-domain` | **The highest-value split.** This is pure `(state, key) -> (state, effects)` logic with no ratatui, no SQLite, no subprocess calls — it should be unit-testable by feeding key sequences and asserting on emitted `Effect`s and resulting state, at workspace-build speed, with no `TestBackend` or temp SQLite file required for logic-only tests. |
| `rq-tui-ui` | ratatui rendering (§3, §7), syntax highlighting, markdown rendering (§3.11) | `rq-tui-app`, `rq-tui-domain` | Presentation-only; consumes `AppState` read-only and never mutates it directly, enforced by the crate boundary instead of by convention. |
| `rq-tui` (bin) | CLI parsing, wiring the above together, `main` | all of the above | Thin — if this crate has meaningful logic in it, something leaked out of its proper layer. |

This directly serves §7.9's acceptance standard ("a deterministic end-to-end
test... not only emitted effects") by giving the reducer a test tier
*below* end-to-end: pure unit tests in `rq-tui-app` with no I/O at all,
sitting underneath the existing headless-harness integration tests, sitting
underneath the PTY-driven compiled-binary tests. Each tier catches what the
one below it can't (a bad reducer branch; a rendering bug the reducer
can't see; a real-terminal escape-sequence surprise the harness can't see).

---

## 2. Core Concepts

### 2.1 Work Item
The top-level unit of review. Backed by:
- A **workspace root**: either a single git repo, or a folder containing
  multiple git repos (multi-repo change spanning several PRs, one per repo).
- One or more **PRs** (local uncommitted/branch diffs, and/or remote PRs
  fetched via `gh`), grouped under the Work Item. A remote PR is one review
  item; a workspace containing multiple repos is also one review item.
- One **active Copilot SDK session** (MAIN — see §2.6) — persisted,
  resumable, forkable — shared across every repo/PR in the Work Item. All
  asks and submitted comments flow through the active session (see §2.6 for
  session topology and §6.2 for delivery guarantees).

In a multi-repo Work Item, repos are listed everywhere (file tree groups,
launch screen, metadata panel) **ordered by last git-tracked activity,
descending**: per repo, the max of (a) newest mtime among files `git status
--porcelain` reports as modified/added/staged — tracked changes only,
untracked files and build artifacts don't count — and (b) the commit
timestamp of the branch tip. The repo you were just working in floats to the
top; a repo with no diff against its base doesn't appear at all. Computed at
Work Item open and on `:sync`, cached in `repos.last_activity_at`.

### 2.2 Local vs Remote PRs
- **Local**: diff of the current branch(es) against a configurable **base
  branch** (per-repo, since a multi-repo Work Item may use different base
  branches per repo — e.g. `main` in one, `develop` in another). Defaults to
  each repo's detected default branch (`git remote show origin` / `main`/
  `master` fallback), overridable per Work Item or globally in Settings. Read
  directly from the working tree(s) under the workspace root. Represented as
  an auto-maintained `v0 (working tree)` version plus pinned **snapshots**
  (see §2.3).
- **Remote**: fetched via `gh` into a shared cache dir as a **real checkout**:
  `~/.cache/rq-tui/prs/<org>_<repo>_<pr#>/repo.git` (bare clone, fetched on
  `:sync`) plus `worktrees/vN` (a detached worktree at that version's head
  SHA). Diffs are computed locally against the merge-base with the PR's base
  branch. Each fetch that detects new commits creates a new version (`vN`)
  rather than overwriting the last one. Real checkouts (not just `gh pr diff`
  output) are required so the session's read-only ask tools can actually read
  surrounding files.

For multi-repo Work Items, the harness creates a **synthetic root** per Work
Item at `~/.local/share/rq-tui/roots/<work_item_id>/`, containing symlinks to
each repo's worktree (remote) or local path (local). The Copilot session is
rooted there, so one session cwd can see every repo in the Work Item.

### 2.3 Versioning, Snapshots & History

**Everything is a version.** Every annotation hangs off a version row:

- **Remote versions** (`v1, v2, ...`): one per fetch that found new commits.
  Backed by a worktree in the cache dir.
- **Working tree** (`v0`): one auto-maintained row per local repo that always
  reflects current state — the live display view.
- **Snapshots**: pinned, reproducible captures of the local working tree,
  created *without touching HEAD or the index* via the `git stash create` /
  `git write-tree` (temp index) trick, then pinned with
  `refs/rq-tui/snapshots/<version_id>` so gc can't collect them. Near-zero
  disk cost (git dedupes blobs). Triggers: first annotation against a changed
  working tree, explicit `:snapshot`, and automatically before `:export`.
  Reproducing an old local state = temp worktree at the snapshot ref —
  identical mechanics to reopening a remote `vN`.

Snapshots are what make local history queries real: "which version did I
leave this comment on" has a stable answer (the snapshot), not a moving `v0`.
The `anchor_snippet` (below) is the human-readable fallback when even the
snapshot's file has since moved on.

Reopening a remote PR that has changed since your last review triggers an
update prompt. You can:
- Review the latest version.
- Reopen an older cached version, including the comments/asks placed against
  that exact version (exact per-version placements — see §2.4).
- Diff only what changed between two versions *(interdiff — deferred, see
  §12)*.

### 2.4 Annotations: Identity vs Placement

An annotation (ask or comment) is **one identity with per-version
placements**:

- The **annotation** row holds what it *is*: kind, file path, anchor
  snippet, anchor hash, text, submitted state.
- A **placement** row per version holds where it *sits*: line range, which
  side of the diff that range belongs to, and an outdated flag, for that
  version.

**Carry-forward** = inserting a new placement against a newly fetched remote
version (or new snapshot): the annotation is re-anchored against the new
content — if its code moved, the placement follows; if its code is gone, a
placement is still created but flagged **outdated** (still readable, still
exported, clearly marked in its inline block). Reopening `v2` = querying
placements for `v2`, so the Version History Picker's counts are exact and
history is preserved. Editing edits the single annotation; `:export` batches
unsubmitted annotations.

**Anchoring mechanics:**
- The anchor is content-based: `anchor_snippet` (selected lines + a few
  lines of context) fuzzy-matched against new content is the real
  re-anchoring mechanism. `anchor_hash` (hash of the containing hunk) is only
  a fast-path equality check — hunks are unstable, so a hash miss just means
  "do the fuzzy match," not "outdated."
- Line numbers (and which side they belong to — old/new, see §8) are display
  hints, updated on re-anchor; never the source of truth.
- **Renames are followed**: re-anchoring consults git's rename detection
  before declaring an annotation outdated; on a followed rename the
  annotation's `file_path` is updated.
- **Ambiguous matches**: if the snippet matches in multiple places (copied
  code), the placement anchors to the match nearest the previous location
  and is flagged for review (shown with an `≈` marker on its inline block);
  `e` on it lets you re-pin manually.

*(Changed from v2: the original spec's "shown with an `≈` marker in the
rail" language is retained in meaning only — there is no rail. The marker now
appears on the inline block itself, per §3.)*

### 2.5 Ask (`a`) vs Comment (`c`) — both render inline, only Ask replies inline

- **Ask**: a question about a selected code range, sent to the active
  Copilot session with **read-only tool access** (search/read only, no file
  edits, no shell). It renders as a bordered block directly under its anchor
  line (§3.3) and its answer streams back **inside that same block**, right
  below the question. The block is a live sub-conversation, not a one-shot
  Q&A: you can keep typing follow-ups in it without leaving the diff, and it
  stays attached to that code location. Each follow-up is a self-contained
  message (below); if another ask is currently streaming, this one queues
  using the same execution/queueing model as Chat (§7.5) and the block shows
  its queue state as durable typed activity, not a bare spinner.
- **Comment**: your own note on a code range — not sent to the model
  immediately. It renders in the **identical bordered block** as an Ask
  (same title-bar shape, same body styling — see §3.2) but does **not**
  stream a reply, because it isn't sent yet. Comments accumulate locally
  through the review. At any point (or at the end), you explicitly submit
  the batch of comments into the harness session (`:export`), where they
  become context the agent can read, respond to, or act on.

**Self-contained ask messages.** Every ask and every follow-up is sent as a
message that carries its own grounding: annotation id, file path, the anchor
snippet, and — on follow-ups — the local thread tail from `ask_messages`.
The model never needs to "remember" ask #1 across ask #5; the harness
re-supplies context each time. This keeps one linear session correct and
cheap (snippets are small), and makes concurrency trivial: asks **queue
serially** into the session. (Per-ask session forks were considered and
rejected: prefix caches have short TTLs so resumed threads re-pay the full
prefix, each fork accumulates its own suffix, quota is per-session-turn, and
forking creates a "which session do new asks target" problem for zero UX
benefit.)

*(Changed from v2: the original's split-view rail (old §3.2) is gone —
there is exactly one annotation treatment, always: the inline block. What's
new is that this treatment, previously exclusive to unified view (old
§3.3's "inline fold blocks" carve-out), now applies to split view too —
split itself is not removed, only its rail. See §3.2/§3.3.)*

### 2.6 Sessions: MAIN, SIDE, and Forking

A Work Item has one **active** session at a time, called **MAIN** — the
persistent, resumable, canonical session for that Work Item. Every ask,
follow-up, and comment-batch submission targets MAIN unless a SIDE session
is currently active (below). Sessions are tracked in a `sessions` table
(`id, work_item_id, parent_id, active, ephemeral`).

Two ways to branch:

- **`/side` (composer command) or `:side`** — creates a **SIDE**: a
  visibly isolated, ephemeral child session, forked from MAIN's current
  point, that uses MAIN's history as reference-only context. Messages
  submitted while the fork is starting stay queued for SIDE and never leak
  into MAIN. Switching to SIDE is a first-class mode of Chat (§7.6), meant
  for a quick tangent you don't want polluting the main review transcript.
  Only one SIDE is active at a time.
- **`/main` or `:main`** — interrupts an active SIDE response immediately
  (does not wait for a long SIDE turn to finish) and returns to MAIN,
  restoring MAIN's transcript, pending queue, scroll position, and draft
  exactly as left. Closing SIDE deletes its persisted SDK session when the
  SDK supports session deletion (row removed, `ephemeral=1` marks it for
  this cleanup); if the SDK doesn't support deletion, the same manual-path
  fallback as `:prune` (§3.10) applies.
- **`:fork`** — the general-purpose, *non-ephemeral* branch: creates a child
  session from the current point and makes it active, exactly as the
  original v2 spec described. Unlike SIDE, a forked session persists and is
  browsable later; use it when you want to keep exploring a branch
  indefinitely rather than pop back to MAIN. `:fork` sessions never get
  auto-deleted by `/main`.

SIDE creation, cancellation, failure, active work, and teardown must each
produce explicit progress text in the status area — never a silent state
change (see §7.5's durable-activity requirement, which applies here too).

### 2.7 Structured Context for Local Reviews
Remote PRs typically arrive with a description; local (uncommitted /
not-yet-PR'd) changes usually don't. To keep review quality consistent, the
harness can generate the same structured context remote PRs have, for local
changes too:

- On opening a local Work Item without existing structure, you can run
  `:generate-context` (or it's offered automatically).
- This invokes the Copilot session, read-only, against the local diff to
  draft: **Title / What / Why / How / Considerations / Other approaches**
  (one-line each) — the same shape a human would write when opening a PR.
- The draft is shown in an editable buffer before being attached to the Work
  Item — you're never forced to accept the agent's framing verbatim.
- This structure is stored alongside the Work Item and is always shown in
  the metadata panel during review.

**Descriptions in multi-repo Work Items:** each remote PR's own description
is PR metadata, stored per-repo (`pr_meta_json`) and displayed per-repo in
the metadata panel. The `contexts` record is the single **Work-Item-level**
summary — generated, manual, or (for the single-remote-PR case) mirrored
from that PR. Multi-repo Work Items therefore show each PR's own description
plus one optional generated rollup.

---

## 3. UI Modes

### 3.1 Launch / Work Item Resolution
```
$ rq-tui review --pr github.com/acme/api-server#482

  Fetching PR #482 (acme/api-server)...
  ✓ worktree at ~/.cache/rq-tui/prs/acme_api-server_482/worktrees/v3
  ⚠ new commits since your last review (v2, reviewed 2 days ago)

  [1] Review latest (v3)
  [2] View v2 (your prior comments/asks, exact placements)
  [3] Diff v2 → v3 only  (deferred, §12)

  >
```

### 3.2 Diff layout: split and unified both stay — only the rail is gone

**Decision:** both diff layouts from the original spec are **kept** —
split (old|new side-by-side) and unified (single `+`/`-` stream) remain
togglable via `:diff split` / `:diff unified` (§5), same as v1. What
changes is orthogonal to layout: **there is no annotation rail in either
layout, ever.** Comments and asks always render as the inline block
described in §3.4, injected directly beneath their anchor row(s) — in
unified that's one row below the single anchor line; in split it's one
full-width row below the aligned old/new pair, breaking out of the
two-column grid for just that block (§3.3). `:diff expand` (context
expansion) is unaffected by any of this.

**Reasoning:** the owner's screenshot shows unified with an inline comment
box, which is the concrete reference for *how inline blocks look and
behave* — but the owner was explicit that removing split entirely was the
wrong read of "I want the diff view to look like it does right now": split
is still wanted as an available layout, only the old rail-based annotation
UI is unwanted. So the fix here is narrower than the first draft of this
document assumed: kill the rail, keep both layouts. `j`/`k` moving through
inline block rows "as one linear scroll" (§3.4) works identically in both
layouts — a block's rows are just more rows in whichever stream you're
currently in.

*(Changed from the first draft of this consolidated doc: an earlier pass
removed split entirely, reasoning that split's only remaining reason to
exist was pairing with the rail. The owner corrected this — split has value
independent of the rail (seeing old and new code side by side) and stays.
Default layout is unified, matching the reference screenshot; `:diff
split` switches to split at any time and keeps working exactly as in v1.)*

### 3.3 Review Mode — Files pane + diff (unified default, split available)
The left pane is the file tree (toggle with `t`, §4). The right pane holds
the diff in whichever layout is active (§3.2):

- **Unified** (default): one continuous scroll — every changed file's
  header, hunk header(s), and lines, back to back, no visual gap between
  files. Added lines carry a full-row green background wash; removed lines
  a full-row red wash (not just a glyph). The line-number gutter shows the
  number appropriate to that row's side — new-file numbers for
  context/added rows, old-file numbers for removed rows (this is what
  "side" means throughout: §2.4, §8).
- **Split** (`:diff split`): old and new render in two columns as in v1,
  each with its own gutter. There is exactly one cursor, not one per
  column: `j`/`k` (and every other vertical movement — `C-u`/`C-d`,
  `gg`/`G`, block traversal) advances both columns in lockstep by paired
  row, the same single-cursor model as unified, just displayed across two
  columns. This is what makes the "one linear scroll, `j`/`k` goes through
  everything including open blocks" principle (§3.4) hold in split too —
  there's still only one thing to scroll, it's just drawn twice per row.
  Inline blocks (§3.4) render as one continuous stream conceptually — a
  block spans the full width beneath its anchor row, underneath both
  columns, rather than living inside either one.

Either way, the currently selected row gets a solid highlighted full-row
background and a `❯` chevron in the gutter — the one selection convention
used everywhere in this UI.

```
┌─ new-auth ───────────────────────────────────────────────────────── [Review] Chat: ● ─┐
│ Files (2)                    │ api-server > README.md                unified · ln 5/12 │
├───────────────────────────────┼──────────────────────────────────────────────────────────┤
│  A GETTING_STARTED.md +5  -0  │ GETTING_STARTED.md  +5  -0                                │
│❯ A README.md          +7  -0  │ @@ -0,0 +1,5 @@                                            │
│                                │  1 +   # Getting Started                                  │
│                                │  2 +                                                       │
│                                │  3 +   1. Clone the repository.                            │
│                                │  4 +   2. Open the project in your editor.                 │
│                                │  5 +   3. Add your code and run it from here.               │
│                                │ README.md  +7  -0                                          │
│                                │ @@ -0,0 +1,7 @@                                            │
│                                │  1 +   # README                                            │
│                                │  2 +                                                       │
│                                │  3 +   ## Overview                                         │
│                                │  4 +                                                       │
│                                │❯ 5 +   ## Getting started                                  │
│                                │       ╭──────────────────────────────────────────╮        │
│                                │       │ Comment · README.md R5                    │        │
│                                │       │ ❯ This is what commenting looks like▏     │        │
│                                │       ╰──────────────────────────────────────────╯        │
│                                │  6 +                                                       │
│                                │  7 +   Open the repository and add your code here.          │
├───────────────────────────────┴──────────────────────────────────────────────────────────┤
│ INSERT  comment · README.md R5   Enter submit  Esc cancel                                  │
└──────────────────────────────────────────────────────────────────────────────────────────┘
```

The Files pane row format: status badge (`A`/`M`/`D`, colored — green for
added), filename, then right-aligned `+added  -removed`. The selected file
is shown in the accent color with the `❯` chevron. Repo groups (multi-repo
Work Items) are ordered per §2.1, each with a compact relative-activity
timestamp; `h`/`l` on a group row collapses/expands it, same as v2.

Same content in split (`:diff split`) — the inline block still spans full
width beneath the anchor row, breaking out of the two-column grid:

```
┌─ new-auth ───────────────────────────────────────────────────────── [Review] Chat: ● ─┐
│ Files (2)    │- old                              │+ new                              split │
├───────────────┼──────────────────────────────────┴──────────────────────────────────────────┤
│❯ A README.md  │  84  fn validate(tok: &str) {      84  fn validate(tok: &str) {               │
│               │  85    let claims = decode(tok);   85    let claims = decode(tok);            │
│               │  86    if claims.exp < now() {      86    if claims.exp < now() {              │
│               │❯ 87      return Err(Expired)      ❯ 87      return Err(Expired)                │
│               │       ╭──────────────────────────────────────────────────────────╮            │
│               │       │ Ask · session.rs R87                                       │            │
│               │       │ ❯ why does this not log before returning?▏                 │            │
│               │       ╰──────────────────────────────────────────────────────────╯            │
│               │  88  }                              88  }                                     │
├───────────────┴──────────────────────────────────────────────────────────────────────────────┤
│ INSERT  ask · session.rs R87   Enter submit  Esc cancel                                        │
└──────────────────────────────────────────────────────────────────────────────────────────────┘
```

### 3.4 Inline block: Comment vs Ask, and "go up and down around it"

Both kinds use the same bordered-box shape, injected directly beneath their
anchor line, pushing everything below it down by exactly the box's rendered
height — no separate rail, no side column, anywhere in the layout:

- **Comment** title bar: `Comment · <file> <side><line>` (e.g.
  `Comment · README.md R5` for the new/right side, `Comment · session.rs
  L88` for the old/left side of a removed line). Body: a single
  `❯`-prefixed live-typed line with a visible cursor while composing.
- **Ask** title bar: `Ask · <file> <side><line>`. Body: the `❯`-prefixed
  question, then the model's streamed reply directly below it (no
  intermediate blank line, no "switch to Chat to see the answer"), then a
  fresh `❯` prompt row for a follow-up:

```
   4 +   #include "session.h"
   5 +   // refresh window: 15m
   6 +   if claims.exp - now() < Duration::minutes(15) {
       ╭──────────────────────────────────────────────────────╮
       │ Ask · session.rs R91                                   │
       │ ❯ why 15min not 1h?                                    │
       │ 🤖 Short-lived refresh windows limit exposure if a     │
       │    token leaks. 15m is a common default...             │
       │ ❯ does this match our other services?▏                 │
       ╰──────────────────────────────────────────────────────╯
   7 +      refresh_token(tok)?;
   8 +   }
```

**Navigation matches the Copilot CLI: you go up and down *around* it, not
into a separate mode.** When the cursor reaches a block's rows (expanded or
collapsed), `j`/`k`, `C-u`/`C-d`, `C-f`/`C-b`, and `gg`/`G` treat those rows
as first-class members of the one linear scrollable stream — moving off the
top or bottom of a block continues straight into the surrounding diff rows,
exactly the way a tool-call or diagnostic block is woven into a Claude Code
/ Codex / Copilot CLI transcript that you page through with ordinary
up/down. `]a`/`[a` still exist as a fast-jump shortcut layered on top (jump
straight to the next/previous block, wrapping across files) — but the block
itself is never a separate navigation mode or focus target; it is just more
rows.

Folding: `za` collapses a block to a single summary row inline at its
anchor position (e.g. `▸ [c] R102 · consider extracting into helper`),
still part of the same scroll — collapsing only changes how many rows the
block occupies, never where it lives. An `≈` prefix marks an
ambiguously-re-anchored block (§2.4); `e` re-pins it.

Entering INSERT on a block's prompt row (`i`, or automatically when you
press `a`/`c` on a fresh Visual selection) lets you type; `Esc` returns to
NORMAL without leaving the diff. Ask follow-ups obey the same queueing/
steering/cancellation model as Chat (§7.5) — a follow-up sent while another
ask is streaming shows a queued state directly in the block, not a modal
spinner.

*(This section replaces the original spec's §3.2 split-view rail and
generalizes its §3.3 unified-view inline-fold-block treatment — that
treatment is now the only one, for both kinds of annotation, per §2.5 and
§3.2's diff-layout decision above.)*

### 3.5 Chat Mode (full panel)
Full Chat-mode behavior — selection, composer, execution, MAIN/SIDE
switching, model configuration — is specified in full in §7; this is the
layout reference.

```
┌─ new-auth — Chat (MAIN) ───────────────────────────────────── model: gpt-5 ▾   [Chat] Review: ● ─┐
│  💬 you  (session.rs:89-94)                                                                        │
│  why 15min not 1h?                                                                                 │
│                                                                                                     │
│  🤖 copilot                                                                                        │
│  Short-lived refresh windows limit exposure if a token leaks. 15m is a common default...           │
│                                                                                                     │
│  📝 comments submitted (batch, 4 items) — session.rs, LoginForm.tsx                                 │
│  1. ln102 consider extracting into helper                                                          │
│  2. ln140 missing null check                                                                        │
│  3. LoginForm.tsx:22 rename to isSubmitting                                                         │
│  4. LoginForm.tsx:55 duplicate validation logic                                                     │
│                                                                                                     │
│  🤖 copilot  ⏺ searching repo · 0:04 elapsed · last event 0:01 ago                                  │
│  ▌                                                                                                 │
├─────────────────────────────────────────────────────────────────────────────────────────────────┤
│ > _                                                                        [/side][/compact][/fork]│
├─────────────────────────────────────────────────────────────────────────────────────────────────┤
│ INSERT  Esc→normal, then Tab⇄review  gm preview                                                   │
└─────────────────────────────────────────────────────────────────────────────────────────────────┘
```

The `Focus: files → diff` breadcrumb (§4.2) is a Review-only affordance —
Chat mode is a full-panel transcript with a single focus target (the
transcript itself), so it has no breadcrumb of its own. Chat replaces
Review's screen entirely when active (toggled with `Tab`/`gc`/`gr`, §4.3);
it is not a docked sidebar and not reachable via `Ctrl-w`. MAIN/SIDE state
and the durable activity line live inside it (§7).

### 3.6 Command Palette (`:`, autocomplete)
```
├─────────────────────────────────────────────────────────────────────────────────────────────────┤
│ :mod█                                                                                              │
│   :model                                                                                           │
│   :main                                                                                            │
└─────────────────────────────────────────────────────────────────────────────────────────────────┘
```

### 3.7 Settings
```
┌─ Settings ──────────────────────────────────────────────────────────────────────────────────────┐
│  Model              gpt-5 · reasoning: medium · context: 128k     ▸ staged picker (§7.7)          │
│  Ask tool scope     read/search only (fixed for `a`)                                              │
│  Diff layout        unified (default)          ▸ split available anytime via :diff split (§3.2)  │
│  Base branch        auto-detect (per-repo)     ▸ override globally or per-repo                   │
│  Cache dir          ~/.cache/rq-tui/prs                                                           │
│  gh auth            ✓ authenticated as octocat                                                    │
│  Keybindings        vim (default)              ▸ view/edit                                        │
│  Diff context       6 lines (o expand step: 10)                                                   │
│  File tree default  open                       ▸ open/closed on launch (`t` toggles at runtime)   │
│  Markdown preview   inline (default)           ▸ inline / browser fallback (§3.11)                │
│  Skills dir         ./ .rq-tui/skills, ~/.rq-tui/skills                                            │
│  Storage            SQLite (WAL) — ~/.local/share/rq-tui/rq-tui.db                                 │
├─────────────────────────────────────────────────────────────────────────────────────────────────┤
│ j/k select  Enter edit  q back                                                                    │
└─────────────────────────────────────────────────────────────────────────────────────────────────┘
```
"Diff layout" sets the *default* on launch; `:diff split` / `:diff unified`
switch it at any time within a review without touching this setting.

### 3.8 Version History Picker
Counts come straight from per-version placement rows, so they're exact.
Snapshots appear alongside remote versions for local repos.
```
┌─ new-auth — Versions ──────────────────────────────────────────────────────────────────────────┐
│  v3  (latest)     fetched today            47 files    12 asks · 4 comments (unsubmitted)         │
│  v2                fetched 2 days ago       41 files    9 asks · 4 comments (submitted)            │
│  v1                fetched 5 days ago       30 files    3 asks · 2 comments (submitted)            │
│  s2  (snapshot)    taken yesterday          local        2 asks · 1 comment                        │
├─────────────────────────────────────────────────────────────────────────────────────────────────┤
│ j/k select  Enter open  d diff-vs-current (deferred, §12)  q back                                  │
└─────────────────────────────────────────────────────────────────────────────────────────────────┘
```

### 3.9 Generated Context Editor (local PRs)
```
┌─ Generate Context — new-auth (local) ─────────────────────────────────────────────────────────┐
│ Title           15-minute token refresh window                                           [edit] │
│ What             Adds a silent refresh check before token expiry.                          [edit] │
│ Why              Prevents users being logged out mid-session.                               [edit] │
│ How              validate() now checks exp - now() < 15m and calls refresh_token().         [edit] │
│ Considerations   Refresh window duration is hardcoded; no jitter/backoff on refresh failure. [edit] │
│ Other approaches - Sliding-window session cookie (simpler, less precise)                     │
│                   - Client-driven refresh polling (more client complexity)                   │
├──────────────────────────────────────────────────────────────────────────────────────────────┤
│ e edit field  a accept & attach  r regenerate  q discard                                       │
└──────────────────────────────────────────────────────────────────────────────────────────────┘
```

### 3.10 Prune — Review History
`:prune` opens a **review-history** view scoped to things you actually
reviewed (rows in SQLite with at least one open event or annotation) — it
does not crawl the cache dir listing every fetched artifact. Ordered oldest
last-reviewed first. Versions that were fetched but never opened are garbage
by definition and their worktrees are deleted automatically once a newer
version has been reviewed — no prompt, they never appear here.
```
┌─ Prune — reviewed items (oldest first) ────────────────────────────────────────────────────────┐
│ [x] acme/api-server#301   merged 3w ago    last reviewed 3w ago    412 MB   3 versions · 14 annot │
│ [x] old-workspace (local) path missing     last reviewed 2w ago     88 MB   2 snapshots · 5 annot │
│ [ ] acme/api-server#482   open             last reviewed today     390 MB   3 versions · 16 annot │
├─────────────────────────────────────────────────────────────────────────────────────────────────┤
│ space select  d delete selected  x export-then-delete  q back                                     │
└─────────────────────────────────────────────────────────────────────────────────────────────────┘
```
- PR state comes from `gh pr view --json state,mergedAt` — merged/closed PRs
  and orphans (workspace path no longer exists) float to the top as the
  obvious safe deletes.
- Deleting drops worktrees, snapshot refs, placements, and annotations. If a
  deletion would leave annotations with zero placements, a one-shot markdown
  export is offered first (`x`).
- Pruning a Work Item also deletes/abandons its Copilot sessions, MAIN and
  any forks/SIDEs (the CLI keeps transcripts in its own data dir containing
  the same code snippets). If the SDK doesn't expose session deletion, the
  prune summary prints the session storage path so it can be removed
  manually.

### 3.11 Markdown preview: inline by default, with real scrolling

**Changed from v2:** the original spec's `gm` always shelled out to an
external browser. That remains available, but the *default* is an inline
overlay rendered directly in the TUI — no context switch to a browser tab
for the common case of previewing a README, a comment body, or an ask
thread.

- `gm` (§4.4 for full referent table) opens a full-height overlay panel
  with the target's Markdown rendered semantically (headings, lists, fenced
  code with syntax highlighting, tables, links shown as `text (url)` or
  similar terminal-safe form) — the same rendering quality already
  required for Chat's transcript (§7.2's Markdown-aware selection implies
  the renderer already exists; this reuses it).
- The overlay is independently, fully scrollable: `j`/`k` line-by-line,
  `C-u`/`C-d` half-page, `C-f`/`C-b` full-page, `gg`/`G` top/bottom — same
  muscle memory as every other pane (§9), not a one-shot static render.
  Content wider than the overlay wraps; content taller than the overlay
  scrolls, with a `lines X-Y/Z` indicator when it doesn't all fit (same
  idiom as the inline-block scroll indicator, §3.4).
- `q` or `Esc` closes the overlay and returns focus exactly where it was
  (diff cursor position, chat cursor position, or composer draft, per the
  referent that opened it).
- The **browser fallback** (`:settings` → Markdown preview → browser, or a
  one-key toggle from inside the overlay) remains for cases where the
  inline render is insufficient — very large documents, or content the
  terminal genuinely can't represent (complex tables, embedded images).
  Settings' "Markdown preview" row (§3.7) becomes `inline (default) /
  browser` instead of always `browser`.
- Implementation should evaluate an existing ratatui-ecosystem Markdown
  widget before writing a bespoke renderer (§1) — the requirement is the
  behavior above, not a specific implementation.

---

## 4. Modal Model & Keybindings

### 4.1 Modes

The app is modal, vim-style. Exactly one mode is active at a time, always
shown in the status bar alongside the focus breadcrumb (§4.2). Three modes:

- **NORMAL (command)** — the default and the mode you return to with `Esc`.
  All navigation lives here: moving through the diff (including through
  inline ask/comment blocks, §3.4), scrolling up through chat history,
  jumping between panes, folding, searching. Single keys are commands
  (`j/k`, `]a`, `yy`, `t`, `gm`, `Tab`, …), and it's the launch point for the
  other modes: `i` (or starting to type in an input context) enters INSERT,
  `v`/`V`/`Ctrl-V` enter VISUAL (character/line/block), `:` opens the
  command palette. Yanking whole units without a selection (`yy` — current
  diff line or current chat message) happens directly from NORMAL.
- **INSERT** — typing text into an input: the chat composer, an inline ask
  follow-up, a comment body, a context-editor field, search input. Keys are
  literal (including `Tab`). `Esc` returns to NORMAL. Mode toggles like
  `Tab`⇄chat never fire from INSERT — you `Esc` first.
- **VISUAL** — entered from NORMAL to select a range in the focused pane,
  with three sub-modes sharing one grammar everywhere (diff pane or chat
  pane alike): `v` character-wise, `V` line-wise, `Ctrl-V` rectangular
  block. In the diff pane, `a`/`c` on a selection always operate on the
  full line range spanned by the selection (annotations anchor at line
  granularity regardless of sub-line character selection) and drop you into
  INSERT to type; `y` yanks the exact selected text (see §7.3 for the copy
  semantics that apply in both panes). `Esc` cancels back to NORMAL.

*(Changed from v2: Visual mode gains explicit character/line/block
sub-modes, via `v`/`V`/`Ctrl-V`, in every pane — not only Chat. The original
spec's single undifferentiated `v` is generalized so the "same grammar
everywhere" promise below still holds once Chat requires full rendered-text
selection, §7.2.)*

```
            i / a / c / :ask-follow-up            v / V / Ctrl-V
   NORMAL ─────────────────────────► INSERT      NORMAL ────► VISUAL
     ▲                                  │            ▲            │
     └────────────── Esc ───────────────┘            └── Esc/y/a/c┘
```

Typical chat-history flow, all from NORMAL in the chat pane: `k k k` up
through past messages → `V` + motion to select a run of lines from an
answer → `y` to copy it → `G` back to the latest message (resumes
auto-follow) → `i` to type a reply.

### 4.2 Focus and pending-key visibility

At every instant the user must be able to answer both "where is focus?" and
"is the app waiting for another key?" This applies within Review; Chat is a
separate full-screen mode with its own focus model (§7), not a window you
`Ctrl-w` into (§4.3):

- Both Review windows (file tree, diff) have a visibly distinct focused
  border and title.
- The status area always shows a breadcrumb while in Review, e.g.
  `Focus: files → diff`.
- Starting a `Ctrl-W` chord immediately shows
  `CTRL-W · h/j/k/l or arrows · Esc cancel` in the status area.
- `Ctrl-W h/j/k/l` and `Ctrl-W` plus arrow keys behave identically.
- An incomplete chord times out (1.5s of no second key) and visibly reports
  that it was cancelled, returning to whatever mode/pane was active.
- A direction with no destination reports it, e.g. `No window to the
  right` — it never silently clears the visible cursor or no-ops
  invisibly. Since Review has only two windows, `Ctrl-w j`/`Ctrl-w k` (no
  vertical neighbor) and the far side of `h`/`l` always report a dead end;
  this is expected, not a bug.
- Focus movement always places a visible cursor or selected row in the
  destination window.
- Resizing or closing an overlay never leaves focus pointing at a hidden
  window.

### 4.3 Window navigation: `Ctrl-w h/j/k/l`, two windows — Chat is not one of them

**Decision:** the `Ctrl-w`-navigable window set inside Review is exactly
**two**: **file tree** and **diff**. Inline ask/comment blocks are
*content inside the diff window* (§3.4), never separate focusable windows,
consistent with "you go up and down around it."

**Chat is deliberately not part of this ring.** It's a separate,
full-screen mode you reach with `Tab` / `gc` / `gr` (§3.5, §7.1) — the
owner was explicit: "for chat I want a separate screen all together that
takes up everything," not a third `Ctrl-w` pane living alongside a
shrunken Review. So there is no `Ctrl-w`-to-Chat path in either direction;
`Tab`/`gc`/`gr` is the only door.

| From | `Ctrl-w l` | `Ctrl-w h` |
|---|---|---|
| file tree | → diff | *(no window to the left — reports `No window to the left`)* |
| diff | *(no window to the right — reports `No window to the right`; use `Tab`/`gc` to reach Chat)* | → file tree (opens it if closed) |

`t` toggles the file tree window open/closed (§4.4) — this is the new
primary binding for that action. *(Changed from v2: this supersedes the
original spec's `-` / `,e` bindings, which are retired to avoid two
bindings for one action.)*

*(Changed from the first draft of this consolidated doc: an earlier pass
put Chat into the `Ctrl-w` ring as a third window, reasoning that
"universal Ctrl-w for all windows" implied including it. The owner
corrected this — Chat should stay a distinct full-screen destination
reached only via `Tab`/`gc`/`gr`, exactly as in the original v2 spec;
"universal Ctrl-w" describes uniform behavior *among Review's windows*,
not that every screen in the app is a Ctrl-w-reachable pane.)*

### 4.4 Keybindings (summary)

Full pane-scoped breakdown (movement, search, yank per pane) is in §9 — this
is the quick-reference cheat sheet. Unless marked otherwise, keys below are
NORMAL-mode commands.

| Key | Context | Action |
|---|---|---|
| `j` / `k` | Focused pane | Move within pane (line/message/entry) — includes rows belonging to an inline ask/comment block, §3.4 |
| `h` / `l` | Diff pane | Move to prev/next changed file |
| `h` / `l` | File tree, on a repo group row | Collapse/expand that group |
| `C-u` / `C-d` | Any pane | Half-page scroll up/down (always — never overloaded) |
| `C-f` / `C-b` | Any pane | Full-page scroll |
| `o` / `O` | Diff pane, cursor on a fold line | Expand 10 more context lines / expand the whole gap |
| `gg` / `G` | Focused pane | Jump to top/bottom (chat: oldest/latest, `G` resumes auto-follow) |
| `Ctrl-w h/j/k/l` | Review | Move focus between file tree ⇄ diff (§4.3); shows chord prompt, times out, reports dead-end directions (§4.2). Chat is reached via `Tab`/`gc`/`gr`, not `Ctrl-w` |
| `t` | Review | Toggle file tree pane open/closed |
| `i` | NORMAL, input context (chat composer, ask thread, comment, editor field) | Enter INSERT mode |
| `Esc` | INSERT / VISUAL / search | Back to NORMAL (also cancels selection / closes search bar) |
| `v` / `V` / `Ctrl-V` | NORMAL, focused pane | Enter VISUAL: character-wise / line-wise / block |
| `y` / `yy` | VISUAL / NORMAL | Yank selection / yank current line or message (no selection needed) |
| `/`, `n`/`N` | NORMAL, focused pane | Search within pane, next/prev match |
| `a` | VISUAL selection (or NORMAL: current line) | Ask about selection (read-only) — drops into INSERT to type; inline block, ongoing conversation, streamed reply (§3.4) |
| `c` | VISUAL selection (or NORMAL: current line) | Leave comment on selection — drops into INSERT to type; inline block, no reply until `:export` |
| `]a` / `[a` | Diff pane | Fast-jump to next/previous annotation block (ask or comment), wraps across files |
| `e` | Cursor on annotation block | Edit the comment text / your last ask message / re-pin an `≈` ambiguous anchor |
| `dd` | Cursor on annotation block | Delete annotation (undo toast, `u` restores) |
| `u` | Diff pane | Undo last annotation delete |
| `gm` | Review/Chat | Markdown preview of the thing under the cursor (see §9 for referents) |
| `za` | Review | Toggle fold on inline ask/comment block |
| `Tab` / `gc` / `gr` | Global, **NORMAL mode only** | Toggle Review ↔ Chat mode (in INSERT, `Tab` is a literal tab; `Esc` first) |
| `:` | Global | Open command mode (autocomplete) |
| `/side`, `/main` | Chat/Ask composer, or `:side`/`:main` in command mode | Create/enter a SIDE session; interrupt SIDE and return to MAIN (§2.6) |
| `q` | Overlays only | Close current overlay/screen — `q` never quits the app; quitting is always explicit via `:q` |

---

## 5. Command Mode (`:`) — Commands

| Command | Effect |
|---|---|
| `:diff split` / `:diff unified` | Switch diff layout (§3.2); default on launch is unified, set in Settings |
| `:diff expand` | Expand full context for current file |
| `:model` | Open the staged model picker (model → reasoning → context tier → extra options, §7.7) |
| `:fork` | Fork the active session into a new persistent branch and make it active (§2.6) |
| `:side [name]` | Create/enter a SIDE session (§2.6) |
| `:main` | Interrupt SIDE if active and return to MAIN (§2.6) |
| `:steer <text>` | Steer the currently streaming turn (§7.5); falls back to enqueue if nothing is streaming, and reports which happened |
| `:compact [instructions]` | Manually trigger compaction, optionally guided |
| `:versions` | Open Version History Picker |
| `:snapshot` | Pin a snapshot of the current working tree (§2.3) |
| `:generate-context` | Draft Title/What/Why/How/Considerations/Alternatives for a local PR |
| `:export [format]` | Export/submit accumulated comments (see §6). Auto-snapshots first for local repos |
| `:prune` | Open review-history prune view (§3.10) |
| `:settings` | Open Settings screen |
| `:sync` | Re-fetch remote PR (bare-clone fetch), checking for new commits; refresh `last_activity_at` |
| `:base [branch] [--repo <name>]` | Override base branch for local diff comparison (current repo, or a named repo in a multi-repo Work Item) |
| `:q` / `:quit` | Quit the app — the only way to exit from top-level Review/Chat. Warns first if there are unsubmitted comments or a streaming/queued ask in flight (`:q!` skips the warning). `q` alone only ever closes overlays. |

`/side`, `/main`, and `/steer` are also available as **composer-native
commands**: typed directly into the Chat or inline-ask composer starting
with `/`, intercepted by the harness before send (never forwarded to the
model) — the same idiom Claude Code/Codex/Copilot CLI use for their own
slash commands. The `:` palette form and the `/` composer form are
equivalent; use whichever your fingers are already on.

---

## 6. Comment Export & Session Delivery

### 6.1 Export / Submission
- Comments (`c`) accumulate locally per file, inline, non-blocking (§2.5,
  §3.4).
- `:export` (or an explicit "submit" action) batches all pending comments
  and:
  1. Posts them as one structured message into the active (MAIN, unless a
     SIDE is active — see §2.6) Copilot session (visible in Chat mode,
     becomes context for the agent going forward).
  2. Optionally writes them to disk in a chosen format (markdown, JSON) for
     external use.
  3. For local repos, pins a snapshot first (§2.3) so the exported comments
     reference a reproducible state.
- Export formats to support:
  - **Markdown** — human-readable summary, grouped by file/repo.
  - **JSON** — structured, for scripting/tooling.
  - *(Future/optional)* `gh pr review`-compatible format, to actually post
    comments back to GitHub — not required for v1 but the annotations/
    placements data model stays compatible with it.

### 6.2 Delivery Guarantees — never resend
Reopening a PR, Work Item, or the app must never replay history into a
session. Rules:

- Reopening always **resumes** the active `session_ref` (MAIN, or a
  previously active fork); the harness never re-posts prior asks, comments,
  or context. The session's own persistence is the record of what it has
  seen.
- Every outbound message (ask, follow-up, comment batch, attached context)
  gets a **delivery state** in SQLite: written as `pending` before send,
  flipped to `sent` on ack in the same transaction that records the
  response start. Concretely: `sent` flag on `ask_messages`, `submitted` on
  comment annotations, `attached_to_session` on `contexts`.
- On startup, anything stuck in `pending` is **prompted, never silently
  resent**: "this ask may not have been delivered — resend / discard?" A
  crash mid-send is the only ambiguous case; a prompt beats guessing wrong
  in either direction.
- `:sync` and new-version fetches post **nothing** automatically.
  Carry-forward is a local re-anchoring operation only; the session hears
  about a new version only when you next ask or submit something. This is
  what keeps a long-lived Work Item session from ballooning with duplicate
  diff context on every reopen.

---

## 7. Chat Mode — Full Specification

This section is the authoritative Chat-mode spec, folded in from
`chat-interaction-spec.md` in full — not summarized as a side reference.
Everything here is a normative requirement of this v2 build, restated
directly rather than framed as an audit finding against a prior build.

### 7.1 Product model

Review is the primary workspace; Chat is not a destination you have to
discover an escape route from. Concretely:

- Code-anchored asks and comments live inline in Review (§3.4) — they are
  never something you have to switch to Chat to see or continue. This is
  the resolution of the original chat-interaction-spec's "navigable
  conversation rail" requirement: instead of a rail *inside* Review,
  everything the rail would have held is inline, directly in the diff,
  which satisfies the same goal (never leaving Review to see your own
  threads) more completely.
- The full-screen Chat view (§3.5) is where the Work-Item-level running
  conversation lives (general questions, comment-batch submissions, model
  replies not anchored to a specific line) — reached and left with `Tab` /
  `gc` / `gr`.
- The footer always advertises the reverse action: `Tab → Review` while in
  Chat, `Tab → Chat` while in Review.
- Named navigation is deterministic: `gr` always means Review, `gc` always
  means Chat — neither is a toggle to "the opposite screen."
- Returning to Review preserves the current file, diff position, block
  fold/expansion state, and any in-progress chat draft.

### 7.2 Rendered-text selection

Chat selection operates on **rendered text**, not message indexes — the
defining "feels like neovim" requirement.

| Key | Required behavior |
|---|---|
| `v` | Enter character-wise Visual mode at the current rendered cell. |
| `V` | Enter line-wise Visual mode at the current rendered row. |
| `Ctrl-V` | Enter rectangular block selection where the terminal supports it. |
| `h` / `l` | Move by a displayed grapheme/cell. |
| `j` / `k` | Move by one rendered row while retaining the preferred column. |
| `0` / `$` | Move to the beginning/end of the rendered row. |
| `w` / `b` | Move by semantic word boundaries in selectable text. |
| `PgUp` / `PgDn` | Extend the selection by a viewport while autoscrolling. |
| `gg` / `G` | Extend to the transcript beginning/end. |
| `y` | Copy exactly the selected text and return to Normal mode. |
| `Esc` | Clear the selection and return to Normal mode without copying. |

The mode indicator distinguishes `VISUAL`, `VISUAL LINE`, and `VISUAL
BLOCK`. The footer shows the relevant movement and yank actions.

**Selection model.** Selection cannot be stored as viewport row numbers,
because Markdown wrapping changes after resize. Each endpoint uses stable
semantic coordinates:

```text
message id
  → semantic block id
    → source-text byte/grapheme offset
```

The renderer maintains a temporary mapping from each rendered terminal cell
back to that semantic coordinate. This is what keeps visual highlighting and
movement correct across:

- terminal resize and Markdown reflow;
- scrolling and live transcript growth;
- syntax-highlighted fenced code;
- wide Unicode graphemes and combining characters;
- tabs and wrapped links;
- collapsed or expanded tool/activity blocks.

If a selected message is removed because an ephemeral SIDE conversation is
closed (§2.6), selection clears with an explicit status message.

### 7.3 Copy semantics

- Character-wise selection copies the exact selected plain text.
- Line-wise selection copies complete semantic/rendered lines joined by
  newlines and ends with a newline.
- A selection may cross Markdown blocks and message boundaries.
- Crossing messages inserts one blank line and preserves a lightweight
  speaker label unless the user disables labels in Settings.
- Markdown decoration used only for display never leaks into copied text.
- Fenced code copies its source text without border characters, line-number
  columns, or syntax-style escape sequences.
- Soft terminal wrapping never adds newlines to copied text. Explicit
  Markdown/code newlines are retained.
- Copy tries a native clipboard backend where available and falls back to
  OSC 52 (this applies to diff-pane yanks too, per §9). The status line
  says which backend was used or reports failure explicitly — it never
  claims success solely because bytes were written to stdout.
- Shift-drag terminal selection remains available as a fallback, not the
  implementation of this requirement.

### 7.4 Interaction with streaming and the composer

- The transcript remains scrollable and selectable while Copilot streams.
- Entering Visual mode pauses automatic follow-to-bottom.
- New deltas may append without moving either selection endpoint.
- `G` in Normal mode returns to the latest content and resumes following.
- The sticky composer remains visible while the transcript is selected.
- Selection keys never edit the composer unless Insert mode is active.
- Copying never cancels, steers, or queues an agent turn.

### 7.5 Composer behavior

- The Chat and contextual Ask/Comment composers grow according to
  **wrapped visual rows**, not only explicit newline count.
- Both composers have a safe height cap and independently scroll to keep
  the insertion cursor visible.
- `Up`/`Down` move through wrapped visual rows, including within a single
  long logical line.
- The current internal row range is visible when content exceeds the cap.
- `Esc` leaves Insert mode and preserves a Chat draft.
- With an active Copilot response, the first `Ctrl-C` stops the response
  and preserves the draft; a subsequent `Ctrl-C` may discard the draft.
- Contextual Ask/Comment cancellation returns to the exact prior
  Review/Visual state.

### 7.6 Conversation execution

- Submitting a question never blocks input.
- Additional questions enter a visible FIFO queue and remain editable or
  cancellable until delivery begins. This queue is shared machinery between
  the Chat composer and inline Ask follow-ups (§2.5, §3.4) — there is one
  execution/queueing model, not two.
- The queue inspector shows lane, position, state, short identifier, and a
  prompt preview.
- Steering is a distinct operation from queueing (`:steer` / `/steer`,
  §5). The UI exposes an explicit steer action, shows whether the SDK
  accepted it as immediate steering or fell back to enqueueing, and retains
  that event in the timeline.
- Cancellation identifies which active turn was stopped and leaves the
  session reusable.
- Tool calls, skills, subagents, retries, queue movement, and current
  operation remain visible as durable typed activity — not transient
  spinner text (see the `⏺ searching repo · 0:04 elapsed` line in §3.5's
  mockup).
- A quiet-but-connected interval is distinguishable from a disconnect. The
  user always has a visible way to inspect elapsed time and the most recent
  SDK event.

### 7.7 MAIN and SIDE

(Full session mechanics in §2.6; this is the Chat-mode-visible contract.)

- `/side` creates a visibly isolated ephemeral conversation using MAIN
  history as reference-only context.
- Messages submitted while the fork starts remain in SIDE and never leak
  into MAIN.
- `/main` interrupts an active SIDE response and returns promptly; it does
  not wait for a long SIDE turn to finish naturally.
- Returning to MAIN restores its transcript, pending queue, scroll
  position, and draft unchanged.
- Closing SIDE removes its persisted SDK session when supported.
- SIDE creation, cancellation, failure, active work, and teardown each have
  explicit progress text.

### 7.8 Progressive model configuration

The model picker (`:model`) drills down one decision at a time:

1. Model, using the SDK runtime model list.
2. Reasoning/thinking effort supported by that model.
3. Context tier/window options reported for that model.
4. Any additional model-specific options exposed by the SDK.

The picker never guesses unsupported values. The final choice persists and
is applied to new and resumed sessions (MAIN, forks, and SIDE alike).
Back/Esc returns to the previous stage without changing the active model.

### 7.9 Acceptance standard for Chat mode

A feature in this section is not complete merely because a reducer branch,
keybinding, or renderer exists — it needs a deterministic end-to-end test
and, for terminal behavior, a recorded pseudo-terminal reproduction against
the Bazel-built binary. Minimum required deterministic states, exercised
through the production renderer used by the `ui-snapshot` gallery, with
exact copied bytes and final focus asserted, not just emitted effects:

- Review with inline conversation blocks visible and navigable as ordinary
  scroll content;
- pending `Ctrl-W` chord and each focused window;
- character, line, and block Chat selection;
- selection across wrapped Markdown, fenced code, Unicode, and messages;
- selection while new streaming deltas arrive;
- selection preserved across resize/reflow;
- successful native copy, OSC 52 fallback, and total copy failure;
- long single-line and multiline composers at and beyond their caps;
- active cancellation with a preserved draft;
- multiple queued questions, steering, and queue cancellation;
- staged model selection;
- SIDE start, active turn, immediate exit, and MAIN restoration;
- quiet, warning, retry, tool, skill, subagent, disconnect, and resume
  states;
- minimum supported terminal size and resize recovery.

The compiled Bazel binary is additionally driven in a pseudo-terminal for:
the focus chord, line-wise copy, long composer, background queue,
cancellation, and SIDE-exit workflows.

---

## 8. Data Model — SQLite

All persistent state is stored in a single SQLite database at
`~/.local/share/rq-tui/rq-tui.db` — chosen over flat files/JSON so that
history queries (e.g. "show me all my open asks across every Work Item," or
"which version did I leave this comment on") are simple indexed queries, and
so multiple concurrent processes can safely read/write. **Every connection
opens with `PRAGMA journal_mode=WAL` and a `busy_timeout`.** Large content
(worktrees, diff blobs) lives in the cache dir / git object store and is
referenced by path/SHA, keeping the DB small.

```sql
-- Work Items: top-level review container
CREATE TABLE work_items (
  id             TEXT PRIMARY KEY,
  name           TEXT NOT NULL,
  workspace_root TEXT NOT NULL,
  created_at     TEXT NOT NULL,
  updated_at     TEXT NOT NULL,
  last_opened_at TEXT               -- backs the :prune review-history ordering
);

-- Copilot sessions: MAIN (ephemeral=0, active=1) per Work Item, plus
-- persistent :fork branches (ephemeral=0) and SIDE forks (ephemeral=1).
CREATE TABLE sessions (
  id            TEXT PRIMARY KEY,   -- Copilot SDK session id
  work_item_id  TEXT NOT NULL REFERENCES work_items(id),
  parent_id     TEXT REFERENCES sessions(id),
  active        INTEGER NOT NULL DEFAULT 0,
  ephemeral     INTEGER NOT NULL DEFAULT 0,  -- 1 = SIDE: deleted on /main teardown
  created_at    TEXT NOT NULL
);

-- Repos belonging to a Work Item (1+ per Work Item, 1 PR per repo)
CREATE TABLE repos (
  id               TEXT PRIMARY KEY,
  work_item_id     TEXT NOT NULL REFERENCES work_items(id),
  path             TEXT NOT NULL,        -- local path, or cache checkout root for remote
  remote_pr_url    TEXT,                 -- null for local-only
  pr_meta_json     TEXT,                 -- remote PR title/description/state (per-PR description, §2.7)
  base_branch      TEXT,                 -- override; null = auto-detect default branch
  base_branch_source TEXT NOT NULL DEFAULT 'auto', -- 'auto' | 'global' | 'per_repo'
  last_activity_at TEXT                  -- cached last git-tracked modification (§2.1)
);

-- Work-Item-level structured context (Title/What/Why/How/Considerations/Alternatives)
CREATE TABLE contexts (
  work_item_id  TEXT PRIMARY KEY REFERENCES work_items(id),
  title         TEXT, what TEXT, why TEXT, how TEXT,
  considerations TEXT,
  alternatives  TEXT,                -- newline-delimited one-liners
  source        TEXT NOT NULL,       -- 'generated' | 'manual' | 'remote_pr'
  attached_to_session INTEGER NOT NULL DEFAULT 0  -- delivery state (§6.2)
);

-- Versions: remote fetches (v1..vN), the live working tree (v0), and pinned snapshots.
CREATE TABLE versions (
  id            TEXT PRIMARY KEY,
  repo_id       TEXT NOT NULL REFERENCES repos(id),
  version_num   INTEGER NOT NULL,        -- 0 reserved for working_tree; snapshots use s-numbering in UI
  kind          TEXT NOT NULL,           -- 'remote' | 'working_tree' | 'snapshot'
  created_at    TEXT NOT NULL,           -- fetch time / snapshot time / last re-anchor scan for v0
  head_sha      TEXT NOT NULL,           -- remote: PR head; snapshot: pinned tree/commit; v0: merge-base
  worktree_path TEXT,                    -- remote: cache worktree; null for v0 (read live) and snapshots (materialized on demand)
  last_opened_at TEXT                    -- null = fetched-but-never-reviewed → auto-cleaned (§3.10)
);

-- Annotations: ONE identity per ask/comment...
CREATE TABLE annotations (
  id             TEXT PRIMARY KEY,
  repo_id        TEXT NOT NULL REFERENCES repos(id),
  kind           TEXT NOT NULL,          -- 'ask' | 'comment'
  file_path      TEXT NOT NULL,          -- updated when a rename is followed (§2.4)
  anchor_snippet TEXT NOT NULL,          -- selected lines + context: the real re-anchor mechanism
  anchor_hash    TEXT NOT NULL,          -- containing-hunk hash: fast-path equality check only
  text           TEXT,                   -- comment body; null for asks (thread lives in ask_messages)
  submitted      INTEGER NOT NULL DEFAULT 0,  -- delivery state for comments (§6.2)
  created_at     TEXT NOT NULL
);

-- ...with per-version placements (carry-forward inserts a row here per new version)
CREATE TABLE placements (
  annotation_id  TEXT NOT NULL REFERENCES annotations(id),
  version_id     TEXT NOT NULL REFERENCES versions(id),
  side           TEXT NOT NULL DEFAULT 'new',  -- 'old' | 'new' — which half of the unified diff line_start/line_end number against (§3.3, §3.4)
  line_start     INTEGER NOT NULL,       -- display hint, set on (re-)anchor
  line_end       INTEGER NOT NULL,
  outdated       INTEGER NOT NULL DEFAULT 0,  -- anchor not found in this version's content
  ambiguous      INTEGER NOT NULL DEFAULT 0,  -- snippet matched in multiple places; needs verify (≈)
  PRIMARY KEY (annotation_id, version_id)
);

-- Ongoing conversation within one ask (also backs inline reply rendering, §3.4)
CREATE TABLE ask_messages (
  id            TEXT PRIMARY KEY,
  annotation_id TEXT NOT NULL REFERENCES annotations(id),
  seq           INTEGER NOT NULL,        -- authoritative ordering (timestamps can tie)
  role          TEXT NOT NULL,           -- 'user' | 'assistant'
  text          TEXT NOT NULL,
  sent          INTEGER NOT NULL DEFAULT 0,  -- delivery state (§6.2); assistant rows: 1
  ts            TEXT NOT NULL,
  UNIQUE (annotation_id, seq)
);

-- App-level settings (model, ask tool scope, global base branch default, etc.)
CREATE TABLE settings (
  key           TEXT PRIMARY KEY,
  value         TEXT NOT NULL
);
```

*(Changed from v2: `placements.side` is a new column — with the split
old|new diff layout gone, unified rows need an explicit `old`/`new` tag to
know which side's line numbers a given annotation's `line_start`/`line_end`
refer to, since a single gutter column now shows either number depending on
row type, §3.3.)*

Indexes worth adding early: `versions(repo_id, version_num)`,
`placements(version_id)`, `annotations(repo_id, submitted)`,
`ask_messages(annotation_id, seq)`, `work_items(last_opened_at)`,
`sessions(work_item_id, active)` — these back the Version History Picker,
inline-block rendering per file, `:export` batch query, ask-thread
rendering, `:prune` ordering, and MAIN/SIDE lookup respectively.

Editing a **submitted** comment, or an ask message the model has already
replied to, doesn't rewrite session history — the session already saw the
old text. The edit is appended as a correction marker ("comment #3 revised:
…") so the agent's context stays truthful while the local annotation shows
only the latest text.

---

## 9. Keybindings, Search & Navigation (pane-scoped, keyboard-only)

No mouse dependency. Two orthogonal axes drive everything: the **mode**
(NORMAL / INSERT / VISUAL, §4.1) says what keys mean; **where you are**
says what they act on — the focused window within Review (file tree or
diff, §4.3), or the Chat transcript when that full-screen mode is active
(§7). All tables in this section describe NORMAL-mode behavior unless
noted — search, scroll, and yank resolve relative to whatever you're
currently looking at, the same way vim splits work. The Chat column below
describes Chat-mode movement for completeness even though Chat isn't a
`Ctrl-w`-reachable window (§4.3).

**Within-pane movement (scoped to whichever window has focus)**
| Key | Diff pane | Chat pane | File tree |
|---|---|---|---|
| `j` / `k` | Line down/up (through code and inline blocks alike, §3.4) | Rendered row down/up (§7.2) | Entry down/up |
| `h` / `l` | Prev/next changed file | Move by grapheme/cell (§7.2) | Collapse/expand repo group |
| `C-d` / `C-u` | Half-page down/up | Half-page down/up | Half-page down/up |
| `C-f` / `C-b` | Full-page down/up | Full-page down/up | Full-page down/up |
| `gg` / `G` | Top/bottom of file | Oldest/latest message (`G` also resumes auto-follow while streaming) | Top/bottom of tree |

**Expanding diff context (diff pane)**
Collapsed unchanged regions render as **fold lines** in the diff itself:

```
  ··· 46 unchanged lines ··· (o expand 10 · O expand all) ···
```

With the cursor on a fold line:
| Key | Action |
|---|---|
| `o` | Expand N more context lines (N = "expand step" in Settings, default 10) toward the cursor's approach direction |
| `O` | Expand the entire gap |

`:diff expand` remains the "expand everything in the current file" command.
This keeps `C-u`/`C-d` unambiguous (always scroll, in every pane) and makes
expansion a property of the gap you're looking at — the same mental model
as GitHub's expand arrows — rather than a mode-dependent key overload.

**Annotation navigation & editing (diff pane — inline blocks, no rail)**
| Key | Action |
|---|---|
| `]a` / `[a` | Fast-jump to next/previous annotation block (asks and comments alike), wrapping across files — the "second pass" sweep before `:export`. Ordinary `j`/`k` also walks through block rows as part of the linear scroll (§3.4) — `]a`/`[a` is a shortcut layered on top, not a separate mode |
| `e` | (cursor on annotation block) Edit the comment text, or your last message in an ask thread; on an `≈` ambiguous placement, re-pin the anchor manually |
| `dd` | (cursor on annotation block) Delete it — no confirmation dialog; a one-line undo toast appears and `u` restores |
| `za` | Toggle fold on the block under the cursor — folding only changes its rendered height, never its position in the stream |

**Search (`/`), scoped to the focused window**
| Key | Action |
|---|---|
| `/` | Open search within the focused window (code text in diff, message text in chat, filename in file tree) |
| `n` / `N` | Next / previous match, within that same window |
| `*` | (diff pane) search forward for word under cursor, vim-style |
| `Esc` | Clear search / close search bar |
| File tree `/` | Fuzzy-filters the tree live as you type, not just literal match |

Search state is per-window and independent — searching in chat doesn't
disturb your position in the diff, and switching focus back to the diff
resumes where you left off.

**Markdown preview (`gm`) — what it previews**
`gm` always previews *the thing under the cursor*, in a scrollable inline
overlay by default (browser fallback available, §3.11):
| Context | Referent |
|---|---|
| Diff pane, cursor in code | The current file, if it's a markdown file (otherwise no-op with a hint) |
| Diff pane, cursor on an annotation block | That annotation's text (comment body or full ask thread) |
| Chat pane | The message under the cursor |
| Any input box while composing | Your current draft |

**Yank / copy (keyboard-only, no clipboard mouse dependency)**
- `v` / `V` / `Ctrl-V` — visual select in the focused window (character,
  line, or block — §4.1, §7.2).
- `y` — yank the current visual selection to the system clipboard, trying a
  native clipboard backend first and falling back to **OSC 52** (works over
  SSH/tmux, not just local sessions); the status line reports which backend
  succeeded, or that copy failed (§7.3).
- `yy` — yank the whole current line (diff) or whole current message
  (chat), vim `yy`-style, no visual mode needed.

**Rendering performance**
`syntect` highlighting runs lazily per visible viewport with a small LRU of
highlighted lines — never eagerly across a whole large diff. If profiling
shows it's still the bottleneck on very large files, budget a later move to
tree-sitter; the diff renderer should keep highlighting behind a trait for
that reason.

---

## 10. Finite State Machine — UX Flow

```
                                   ┌────────────────────┐
                                   │   [Launch/Resolve]  │
                                   │ rq-tui review [...] │
                                   └─────────┬────────────┘
                                             │ resolves Work Item
                                             │ (local path or --pr, checks cache/version,
                                             │  refreshes repo ordering by git activity)
                                             ▼
                        ┌───────────────────────────────────────────┐
                        │           [Version Picker]                 │◄───────────┐
                        │  (shown only if cached versions exist      │            │
                        │   and remote has changed)                  │            │
                        └───────────┬─────────────────────┬──────────┘            │
                                    │ select version        │ 'q' cancel           │
                                    ▼                        ▼                      │
                        ┌────────────────────────────────────────────┐             │
              ┌────────►│              [REVIEW MODE]                 │             │
              │         │  unified diff · file tree ('t') · line nav │             │
              │         └───┬───────┬─────────┬─────────┬────────────┘             │
              │             │       │         │         │                          │
              │      'v/V/^V'  'gm' md   ':' cmd    'Tab'/'gc'                      │
              │       visual    preview   mode      (NORMAL only)                  │
              │             ▼       │         │         │                          │
              │   ┌──────────────┐  │         │         ▼                          │
              │   │ [VISUAL SEL] │  │         │   ┌─────────────────────────┐       │
              │   └──┬───────┬───┘  │         │   │      [CHAT MODE]         │       │
              │      │       │      │         │   │  MAIN/SIDE, full view,   │       │
              │   'a' ask  'c'      │         │   │  rendered-text selection,│       │
              │      │    comment   │         │   │  model picker / compact  │       │
              │      ▼       ▼      ▼         ▼   └──────┬────────────────────┘      │
              │  ┌────────┐┌───────┐┌──────┐┌─────────┐  │ 'Tab'/'gr' back           │
              │  │ [ASK   ││[COMMENT││[MD  ││[COMMAND │  └───────────────►(REVIEW)   │
              │  │ INLINE ││ INLINE ││ PRE- ││ PALETTE]│                              │
              │  │ BLOCK] ││ BLOCK] ││ VIEW]││autocomp.│                              │
              │  │ read-  ││ local  ││opens ││ runs :  │                              │
              │  │ only,  ││ note,  ││browser││commands │                              │
              │  │ queued,││ no     ││        ││ (:diff expand, :model, :settings,     │
              │  │ inline ││ reply  ││        ││  :export, :versions, :snapshot,       │
              │  │ reply  ││ until  ││        ││  :prune, :generate-context, :side,    │
              │  │ (§3.4) ││ export ││        ││  :main, :fork, :steer)                │
              │  └───┬────┘└───┬────┘└──┬─────┘└────┬────┘                             │
              │      │         │        │            │ resolves to one of:              │
              │      │         │        │            ├──► [SETTINGS]  (own screen)      │
              │      │         │        │            ├──► [VERSION PICKER] (loop above)  │
              │      │         │        │            ├──► [GENERATE CONTEXT] (local only)│
              │      │         │        │            ├──► [EXPORT COMMENTS] (below)      │
              │      │         │        │            ├──► [PRUNE] (review history)       │
              │      │         │        │            └──► toggles model / SIDE etc.      │
              │      └────┬────┴────────┘                 (stays in current mode)        │
              │           │ 'Esc' back to normal, or 'j'/'k' scroll straight through      │
              │           │  the block into surrounding diff rows (§3.4)                 │
              └───────────┴──────────────────────────────────────────────────────────────┘
                          │
                          │ (any time) ':export' or explicit "submit comments" action
                          ▼
              ┌───────────────────────────────────────────┐
              │        [EXPORT / SUBMIT COMMENTS]           │
              │  local repos: pin snapshot first (§2.3)     │
              │  batches all pending 'c' comments,          │
              │  posts them as ONE message into the         │
              │  active (MAIN) Copilot session               │
              │  (delivery-tracked)                          │
              └───────────────────┬─────────────────────────┘
                                  │
                                  ▼
                         back to [CHAT MODE]
                     (comments now visible as context,
                      Copilot can respond/act on them)

  Global, from any mode:
    ':settings'          → [SETTINGS]  → 'q' back to previous mode
    ':generate-context'  → [GENERATE CONTEXT] (local Work Items) → accept/discard → back to REVIEW
    ':prune'             → [PRUNE] → 'q' back to previous mode
    '/side' / '/main'    → switch MAIN ⇄ SIDE within Chat mode (§2.6, §7.7)
    'q'                  → close current overlay only (never quits the app)
    ':q' / ':quit'       → quit the app (warns on unsubmitted comments or in-flight/queued asks; ':q!' forces)
```

---

## 11. SDK Capability Notes (confirmed feasible)

- **Tool auto-approval / scoping**: SDK defaults to CLI's `--allow-all`
  behavior but every tool call still passes through a permission handler —
  this is how `a` (Ask) is restricted to read-only tools while a future
  edit-capable mode could be added later without re-architecting.
- **`/fork`**: sessions can be branched from any point — backs `:fork`,
  `/side`, and the `sessions` table (§2.6).
- **`/compact`**: manual and automatic context compaction supported,
  including guided compaction ("keep X, forget Y").
- **`/skills`**: skills load from directories and remain effective across
  compaction — used for the `Skills dir` setting (e.g. repo-specific review
  conventions in `SKILL.md`).
- **Streaming**: 40+ event types available for real-time UI updates
  (needed for inline ask streaming, the serial ask/chat queue's durable
  activity display, and Chat mode generally).
- **Session persistence**: sessions resume across restarts — backs the
  never-resend reopening behavior in §6.2.
- **Session deletion**: needed by `:prune` (§3.10) and SIDE teardown
  (§2.6) — confirm the SDK exposes it; if not, both fall back to printing
  the CLI's session-storage path for manual removal.
- **Runtime model/capability listing**: needed for the staged model picker
  (§7.8) — model list, per-model reasoning-effort options, context tiers,
  and any additional model-specific options must be queryable from the SDK
  rather than hardcoded, so the picker never offers an unsupported value.
- Architecture: app → Rust SDK client → JSON-RPC → `copilot` CLI (server
  mode); CLI process lifecycle is managed by the SDK. One session cwd per
  Work Item, rooted at the synthetic root for multi-repo (§2.2).

---

## 12. Open Items

- **Interdiff view (vN → vM only)** — deferred. Diffing two diffs is
  genuinely hard to render well and has no clean answer for annotation
  display; annotations are hidden in that view when it lands.
- SQLite migration strategy (embedded migrations at startup vs. a separate
  CLI subcommand) as the schema evolves.
- Conflict handling when the local working tree changes *during* an open
  review session (file watcher vs manual `:sync`), and how that interacts
  with `base_branch` overrides (e.g. base branch itself moves upstream).
  Snapshots (§2.3) bound the damage — annotations are always recoverable
  against their snapshot — but live re-anchor cadence is still undecided.
- Precise diff algorithm/library choice for multi-repo unified views.
- Which ratatui-ecosystem widget (if any) to adopt for the inline Markdown
  overlay (§3.11) and for composer text editing (§1) — needs a short
  evaluation pass against `tui-widgets` and similar before committing.
  Browser-fallback launch mechanism (system `open`/`xdg-open` vs. a local
  static server) is a smaller, secondary decision.
- Whether `settings` stores the global default base branch as a single row
  (e.g. `key='base_branch.default'`) or needs a small dedicated table if
  per-language/per-org defaults are wanted later.
- Fuzzy re-anchor tuning: match threshold, context window size, and how
  aggressively to auto-accept nearest-match on ambiguity (§2.4) before
  flagging `≈`.
- Whether snapshot worktrees are materialized eagerly on open or lazily on
  first ask that needs file reads.
- Full SDK event-shape coverage for durable activity display (§7.6) —
  tool/skill/subagent event kinds beyond the ones already prototyped.
- Markdown rendering completeness in Chat: links, tables, and nested
  structures beyond fenced code and basic inline formatting, plus their
  selection/copy mappings (§7.2–§7.3).
- Whether `:fork` branches should get their own lightweight picker (a
  `:sessions` command) now that MAIN/SIDE/fork are three distinct concepts,
  or whether Version-History-Picker-style browsing is unnecessary until
  someone actually accumulates more than one or two forks.
- Clipboard-failure injection and native-backend detection across the
  target terminals (§7.3, §7.9) — exact backend list to test against.

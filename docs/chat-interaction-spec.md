# Chat interaction and review integration specification

Status: superseded by [`spec-v2-consolidated.md`](spec-v2-consolidated.md)
Date: 2026-07-30

This document records the interaction requirements raised after the original
Code Review Harness v2 specification. Several behaviors exist at a coarse
level while the requested editor-grade behavior remains incomplete.

The words **must**, **should**, and **may** are normative. A feature is not
complete merely because a reducer branch, keybinding, or renderer exists. It
must have a deterministic end-to-end test and, for terminal behavior, a
recorded pseudo-terminal reproduction against the Bazel-built binary.

## 1. Product model

Review is the primary workspace. Chat must not feel like a destination from
which the user has to discover an escape route.

- The split Review layout must have a navigable conversation rail containing
  code-anchored asks, comments, and relevant general-chat messages.
- Selecting a rail item must reveal its thread inline and, when grounded to
  code, move the diff cursor to its anchor.
- The full-screen Chat view may remain as an expanded transcript view, but it
  must not be the only place where the conversation is readable.
- `Tab` must switch between Review and expanded Chat in Normal mode.
- The footer must always advertise the reverse action: `Tab → Review` while in
  Chat and `Tab → Chat` while in Review.
- Named navigation must be deterministic. `gr` means Review and `gc` means
  Chat; neither may merely toggle to the opposite screen.
- Returning to Review must preserve the current file, diff position,
  annotation expansion state, and chat draft.

## 2. Focus and pending-key visibility

At every instant the user must be able to answer both “where is focus?” and
“is the application waiting for another key?”

- Every focusable pane must have a visibly distinct focused border and title.
- The status area must show a breadcrumb such as
  `Focus: files → diff → conversation`.
- Starting a `Ctrl-W` chord must immediately show
  `CTRL-W · h/j/k/l or arrows · Esc cancel`.
- `Ctrl-W h/j/k/l` and `Ctrl-W` plus arrow keys must behave consistently.
- An incomplete chord must time out and visibly report that it was cancelled.
- A direction without a destination must report, for example,
  `No pane to the right`; it must not silently clear the visible cursor.
- Focus movement must place a visible cursor or selected row in the
  destination pane.
- Resizing, closing a popup, or switching Review/Chat must never leave focus
  pointing at a hidden pane.

## 3. Rendered-text selection

### 3.1 Required modes

Chat selection must operate on rendered text, not message indexes.

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

The mode indicator must distinguish `VISUAL`, `VISUAL LINE`, and
`VISUAL BLOCK`. The footer must show the relevant movement and yank actions.

### 3.2 Selection model

Selection cannot be stored as viewport row numbers because Markdown wrapping
changes after resize. Each endpoint must use stable semantic coordinates:

```text
message id
  → semantic block id
    → source-text byte/grapheme offset
```

The renderer must maintain a temporary mapping from each rendered terminal
cell back to that semantic coordinate. This permits visual highlighting and
movement while keeping the actual selection stable across:

- terminal resize and Markdown reflow;
- scrolling and live transcript growth;
- syntax-highlighted fenced code;
- wide Unicode graphemes and combining characters;
- tabs and wrapped links;
- collapsed or expanded tool/activity blocks.

If a selected message is removed because an ephemeral SIDE conversation is
closed, selection must clear with an explicit status message.

### 3.3 Copy semantics

- Character-wise selection copies the exact selected plain text.
- Line-wise selection copies complete semantic/rendered lines joined by
  newlines and ends with a newline.
- A selection may cross Markdown blocks and message boundaries.
- Crossing messages inserts one blank line and preserves a lightweight speaker
  label unless the user disables labels in settings.
- Markdown decoration used only for display must not leak into copied text.
- Fenced code copies its source text without border characters, line-number
  columns, or syntax-style escape sequences.
- Soft terminal wrapping must not add newlines to copied text. Explicit
  Markdown/code newlines must be retained.
- Copy must try a native clipboard backend where available and fall back to
  OSC 52. The status must say which backend was used or report failure; it must
  not claim success solely because bytes were written to stdout.
- Shift-drag terminal selection remains a fallback, not the implementation of
  this requirement.

### 3.4 Interaction with streaming and the composer

- The transcript remains scrollable and selectable while Copilot streams.
- Entering Visual mode pauses automatic follow-to-bottom.
- New deltas may append without moving either selection endpoint.
- `G` in Normal mode returns to the latest content and resumes following.
- The sticky composer remains visible while the transcript is selected.
- Selection keys must never edit the composer unless Insert mode is active.
- Copying must not cancel, steer, or queue an agent turn.

## 4. Composer behavior

- The Chat and contextual Ask/Comment composers must grow according to
  **wrapped visual rows**, not only explicit newline count.
- Both composers must have a safe height cap and independently scroll to keep
  the insertion cursor visible.
- `Up`/`Down` must move through wrapped visual rows, including a single long
  logical line.
- The current internal row range must be visible when content exceeds the cap.
- `Esc` leaves Insert mode and preserves a Chat draft.
- With an active Copilot response, the first `Ctrl-C` stops the response and
  preserves the draft; a subsequent `Ctrl-C` may discard the draft.
- Contextual Ask/Comment cancellation must return to the exact prior
  Review/Visual state.

## 5. Conversation execution

- Submitting a question must never block input.
- Additional questions enter a visible FIFO queue and remain editable or
  cancellable until delivery begins.
- The queue inspector must show lane, position, state, short identifier, and a
  prompt preview.
- Steering must be a distinct operation from queueing. The UI must expose an
  explicit steer action, show whether the SDK accepted it as immediate
  steering or fell back to enqueueing, and retain that event in the timeline.
- Cancellation must identify which active turn was stopped and leave the
  session reusable.
- Tool calls, skills, subagents, retries, queue movement, and current operation
  must remain visible as durable typed activity—not transient spinner text.
- A quiet-but-connected interval must be distinguishable from a disconnect.
  The user must always have a visible way to inspect elapsed time and the most
  recent SDK event.

## 6. MAIN and SIDE behavior

- `/side` creates a visibly isolated ephemeral conversation using MAIN history
  as reference-only context.
- Messages submitted while the fork starts must remain in SIDE and must never
  leak into MAIN.
- `/main` must interrupt an active SIDE response and return promptly; it must
  not wait for a long SIDE turn to finish naturally.
- Returning to MAIN restores its transcript, pending queue, scroll position,
  and draft unchanged.
- Closing SIDE must remove its persisted SDK session when supported.
- SIDE creation, cancellation, failure, active work, and teardown must each
  have explicit progress text.

## 7. Progressive model configuration

The model picker must drill down one decision at a time:

1. Model, using the SDK runtime model list.
2. Reasoning/thinking effort supported by that model.
3. Context tier/window options reported for that model.
4. Any additional model-specific options exposed by the SDK.

The picker must not guess unsupported values. The final choice must persist and
be applied to new and resumed sessions. Back/Esc returns to the previous stage
without changing the active model.

## 8. Deterministic inspection and acceptance tests

The generic agent interface and controlled backend must support every state
without production data. Required deterministic states include:

- Review with an integrated conversation rail;
- pending `Ctrl-W` chord and each focused pane;
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
- quiet, warning, retry, tool, skill, subagent, disconnect, and resume states;
- minimum supported terminal and resize recovery.

Each state must be available through the production renderer used by the
hidden `ui-snapshot` gallery. Tests must assert semantic state, rendered
styles, copied bytes, and final focus—not only emitted effects.

The compiled Bazel binary must also be driven in a pseudo-terminal for the
focus chord, line-wise copy, long composer, background queue, cancellation,
and SIDE-exit workflows.

## 9. Consolidated explicitly requested gaps

This table is an implementation inventory, not evidence that any item works.
It records requests that are absent, incomplete, misleading, or not yet
validated to the standard above.

| Explicit request | Current gap |
|---|---|
| Review-side messages inline and navigable | Code-anchored Ask threads exist, but general Chat remains a separate full-screen destination rather than a navigable Review conversation rail. |
| Clear route from Chat back to Review | `Tab → Review` is shown in Chat and `gc`/`gr` are named Chat/Review destinations. Review-integrated general-chat navigation remains open. |
| Always-visible focus during `Ctrl-W` navigation | Focus breadcrumb, focused-pane titles/color, arrow/letter chords, cancellation, and invalid-destination feedback are visible. A timed chord expiry remains open. |
| Line/character/block selection in Chat | Character, line, and block modes operate on source-mapped rendered cells and pass deterministic Markdown, code, Unicode, wrap, resize, streaming, and cross-message tests. PTY-24 verifies exact character- and line-mode clipboard bytes; block mode remains deterministic-renderer evidence because rectangular terminal input is terminal-dependent. |
| Reliable arbitrary-text copy | Built-in Visual copy is fine-grained and omits Markdown decoration and soft-wrap newlines. Native copy and OSC 52 fallback exist; PTY-24 forces OSC 52 and decodes exact character/line payloads. A writer-injected total OSC 52 failure verifies the visible `Action failed` path; host terminal consumption remains terminal-dependent. |
| Composer that fits and scrolls all content | Dynamic wrapped-row sizing, visual-row cursor movement, capped independent scrolling, and cancellation precedence pass deterministic tests; PTY-21 validates an oversized contextual composer. |
| Chat scrolling after content exceeds the viewport | Rendered-row keyboard, page, and mouse scrolling exist. PTY-24 verifies all three against visible row ranges while streaming, plus selection and narrow/wide resize reflow. |
| Background questions rather than a blocked UI | FIFO queueing and selected waiting-prompt cancellation are visible and PTY-validated. Queue editing, SDK-native queue introspection, and durable restart recovery remain open. |
| Explicit steering versus queueing | `/steer`/`:steer` use immediate delivery during an active turn, report fallback when idle, and retain visible timeline/transcript evidence; PTY-17 validates the active path. |
| Staged model → thinking → context picker | The runtime capability-driven model, reasoning, and context stages are persisted and compiled-PTY validated. Additional SDK model-specific option kinds are not currently exposed. |
| Cancel a prompt while retaining usability | PTY-18 validates active-response cancellation with a preserved draft; PTY-20 validates immediate active-SIDE interruption and MAIN restoration. |
| See tool calls, skills, subagents, and progress | Typed activity is deterministic and PTY-25 verifies scrollable skill, subagent, retry, operation, outbound, and queue evidence with sticky controls. Coverage of future SDK event shapes remains an adapter-maintenance concern. |
| Never wonder whether Copilot is stuck | Progress exposes phase, elapsed time, last-event age, operation detail, queue depth, outbound ID, durable activity, and a sticky diagnostics footer. PTY-25 verifies quiet → stopping → cancelled transitions and independent queued-work cancellation; broader real-world event-shape coverage remains prudent. |
| `/side` isolation and prompt return to MAIN | Deterministic, compiled-PTY, and authenticated tests cover isolation, active interruption, MAIN restoration, and SDK-session deletion attempts; deletion failure remains visibly attached to MAIN status/progress. |
| Generic Copilot abstraction for exhaustive tests | A controlled agent, full effect harness, snapshot gallery, and script driver cover partial streaming/deltas/completion/abort/failure, queueing/cancellation, steering, model stages, SIDE, resize, Unicode, exact rendered-text selection, native clipboard timeout, OSC 52 bytes, and total output failure. |
| Screenshot/mock render for agent inspection | `ui-snapshot all` exposes review, ask, command, composer, quiet, queue, side, model, markdown, and tiny states; `ui-script` creates operation-specific reproducible frames. An image artifact is optional rather than required for inspection. |
| Markdown-quality terminal rendering | Semantic Markdown, fenced syntax highlighting, and source-to-rendered selection/copy mappings exist. Links, tables, nested structures, and collapsed activity blocks remain incomplete. |
| Complete Copilot SDK feature audit and Rust/Go parity decision | [`copilot-sdk-audit.md`](copilot-sdk-audit.md) checks all 21 requested pages against SDK 1.0.8 and records application wiring, optional scope, and remaining gaps. |
| Codex/Claude-like interaction quality | Editor-grade Chat selection, explicit focus, composer navigation, and interruption semantics are implemented and deterministic. Conversation integration and several terminal-polish details remain below that bar. |

### 9.1 Original v2 requirements still open

The original v2 matrix in
[`behavioral-audit.md`](behavioral-audit.md) also records these explicitly
requested areas as partial, untested, or missing:

- complete multi-repository discovery, ordering, synthetic-root, and base
  override workflows;
- authenticated remote PR fetch, mixed local/remote Work Items, version
  updates, old-version materialization, and carry-forward across renames;
- a complete version-history picker workflow and folder hierarchy in the file
  picker;
- live authenticated `:fork`, compaction, model switching, remote read-only
  permissions, streaming resume, and delivered-message correction workflows;
- complete structured-context generation, editing, acceptance, persistence,
  and multi-repo metadata presentation;
- editable keybindings and the currently informational Settings preferences;
- prune behavior that also removes Copilot transcripts or reports their
  storage path;
- interdiff and live file watching/re-anchoring, which the original
  specification explicitly deferred rather than completed.

Those rows remain requirements unless deliberately removed from product scope.
Source presence alone must not be used to promote them to “working.”

### 9.2 Explicit requests that are substantially represented

The following requests are present enough that they are not categorized as
wholly ignored, although their integration must remain under test:

- Bazel 9, Bzlmod through `MODULE.bazel`, Bazel-only Rust gates, and narrow
  visibility;
- macOS/Linux release targets without Windows;
- a normal non-TUI CLI unless a valid `review` target is supplied;
- syntax highlighting and semantic Markdown rendering;
- a scrollable command palette and visible Command mode;
- rendered-row Chat scrolling and a sticky dynamically sized composer;
- a generic controlled agent interface and deterministic text snapshot
  gallery;
- streaming MAIN/SIDE transcripts, persisted MAIN resume, progress UI, and
  typed activity events.

## 10. Completion criteria

This specification is complete only when:

- every requirement above has a Bazel test or a recorded reason it is not
  applicable;
- exact copied text is asserted for character-, line-, and block-wise cases;
- all focusable states have deterministic rendered evidence;
- the full Bazel build, formatting, lint, and test gate passes;
- the compiled binary passes the required pseudo-terminal workflows;
- a final adversarial audit finds no P0/P1 interaction defect, or each
  remaining blocker is reported honestly.

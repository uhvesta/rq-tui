# Code Review Harness v2 behavioral audit

Audit date: 2026-07-30

This report compares the v2 specification, the repository implementation, the
subsequent Visual/composer/palette/chat feedback, the full headless effect
loop, and the Bazel-built binary. “Working end-to-end” is used only where a
test crosses reducer, effect execution, persistence/agent events, and final
rendering, or where the compiled binary was driven in a pseudo-terminal.

Evidence abbreviations:

- `WF::<name>` means a test in `src/testing.rs`.
- `PTY-xx` refers to [`audit/pty-smoke.md`](audit/pty-smoke.md).
- Other test names identify their Rust module directly.

Current-evidence boundary: the deterministic harness baseline is 104 regular
tests passing with 1 authenticated live test ignored by default. The ignored
stream/resume test was also run explicitly against the installed Copilot CLI
and passed. PTY-16 through PTY-21 are current compiled-binary evidence; earlier
PTY references remain useful historical records.

## Findings remediated during this audit

| Priority | Finding | Resolution and evidence |
|---|---|---|
| P0 | Split view paired deletions/additions into fewer rows than the reducer used, so cursor and persisted anchors could diverge. | Split rendering is now one canonical parsed row per display row. `WF::file_switch_clears_visual_mode_and_split_unified_render_selection`. |
| P0 | Placements could not distinguish old-file and new-file line numbers. | Migration 2 adds placement `side`; deleted selections persist as `old`, new/context selections as `new`. `WF::deleted_line_annotation_records_the_old_side`. |
| P0 | Fold/meta selections could persist as line 0; replacement selections could mix incompatible old/new coordinates. | Annotation validation rejects metadata and mixed-side ranges with an explanatory status. `WF::folds_and_mixed_old_new_ranges_fail_safely`. |
| P0 | Local Ask/Comment always failed while pinning the first snapshot because `:` from the version ID was placed in a Git ref. | Snapshot refs use a deterministic Git-safe SHA-256 name. `git::tests::snapshot_preserves_head_and_index_and_pins_the_worktree`, PTY-04. |
| P0 | Compiled-terminal Enter/control sequences could leave composers and the command palette impossible to submit. | Enter plus terminal-equivalent `Ctrl-J`/`Ctrl-M` are accepted; `Ctrl-S` remains supported. `app::tests::terminal_ctrl_j_equivalent_submits_composers_and_commands`, PTY-04/05/08. |
| P0 | The public headless harness captured effect strings but did not execute effects. | It now uses a temporary SQLite database, fake agent, full effect runner, event injection, restart, persisted-row inspection, and styled-frame inspection. All `WF::*` tests use it. |
| P0 | Persistence errors discarded the composer draft and could terminate the interaction loop. | Effect failures are contained; contextual mode, range, target, and draft are restored. `WF::failed_persistence_restores_the_contextual_composer_and_draft`. |
| P0 | A closed agent command channel could restore an already-persisted Ask composer, allowing a duplicate on retry. | The single Ask remains visibly failed and pending for explicit recovery; the composer is not recreated. `WF::closed_agent_channel_keeps_one_pending_ask_without_duplicate_retry`. |
| P0 | Some terminal setup failures could skip raw/alternate-screen restoration. | Setup and all run-loop exits now execute cleanup. PTY-10/11 prove normal and forced exits. |
| P1 | Visual mode was cleared or left stale inconsistently across search, cancel, file/screen/layout changes. | Search returns to Visual and extends its fixed anchor; cancel restores Visual; file/screen/layout changes clear predictably. `WF::cancel_restores_visual_selection_without_stale_draft`, `WF::visual_search_extends_the_fixed_anchor_and_chat_visual_yanks_messages`, `WF::file_switch_clears_visual_mode_and_split_unified_render_selection`. |
| P1 | Split mode highlighted only the cursor, not the Visual range. | Every selected canonical row now styles its applicable old/new cell. `WF::file_switch_clears_visual_mode_and_split_unified_render_selection`. |
| P1 | Visual `y` was unreachable as a one-key action. | Visual `y` executes immediately; `yy` remains Normal-mode yank. `WF::visual_yank_preserves_source_order_across_addition_and_context`. |
| P1 | `Ctrl-w h/j/k/l` required Control on the second key, contrary to the documented sequence. | A plain second direction is accepted. `app::tests::ctrl_w_then_plain_direction_moves_focus_as_documented`. |
| P1 | File-picker Enter and annotation-rail indexing were misleading. | Enter accepts the current file; rail movement is scoped to annotations in the displayed file. `app::tests::picker_enter_accepts_the_current_file_and_returns_focus_to_diff`. |
| P1 | Command entry and annotation composition were confined to the bottom status line. | Commands render in a top palette; annotation composers render beside the selected range and remain usable in narrow terminals. `WF::narrow_terminal_keeps_selection_composer_and_top_palette_visible`, PTY-03/08. |
| P1 | Export could retain an unrelated previous status after successfully queueing a batch. | Export now always reports the queued batch and optional written path. `WF::comment_export_queues_one_batch_and_acknowledges_delivery`. |
| P1 | Narrow command/Ask overlays leaked fragments of underlying panes, making their borders and content ambiguous. | Compact terminals use full-width cleared modal surfaces; the 40×12 adversarial reproductions are clean. |
| P1 | Review focus and advertised `Ctrl-W` arrow chords were not visibly reliable. | The header and focused pane titles show focus, arrows and letter chords share one path, and unavailable destinations report an explicit status. `app::tests::ctrl_w_then_plain_direction_moves_focus_as_documented`. |
| P1 | Wide CJK/emoji text wrapped by character count and clipped terminal cells. | Markdown wrapping now uses Ratatui's terminal-cell width, and snapshot extraction omits wide-character continuation cells. `chat_render::tests::wide_unicode_wraps_by_terminal_cells_without_clipping`. |
| P1 | Lagged/closed SDK subscriptions, stale deltas, and resumed pending work could strand or corrupt the local active turn. | Lagged/closed streams fail active and queued work visibly instead of hanging; resumed in-flight turns receive a synthetic local outbound; pre-turn stale events are quarantined. Corresponding `copilot::tests::*` race tests pass. |
| P1 | The checked-in Bzlmod lock was incomplete for strict consumers and the root alias could not analyze under `bazel test //...`. | The lockfile is refreshed, CI/release use `--lockfile_mode=error`, package visibility admits only the root alias, and the full aggregate Bazel gate passes. |

## Requirement traceability

| ID | v2 requirement | Classification | Evidence / observed gap |
|---|---|---|---|
| R-01 | Rust/ratatui/syntect/Copilot SDK/`gh`/git-plumbing harness architecture | Working end-to-end | Bazel build gate; live Copilot test; PTY-01. |
| R-02 | One active persistent Copilot session per Work Item | Working end-to-end | `copilot::tests::live_copilot_streams_and_resumes_persisted_history`; unique-active-session migration and PTY-05/07. |
| R-03 | Open a single local repository and diff it against a base | Working end-to-end | PTY-01 and PTY-04. |
| R-04 | Discover a multi-repository local workspace, omit repos without diffs, order by tracked activity | Partially working | Discovery/activity logic exists and `git::tests::detects_local_changes_and_activity` covers one repo; no complete multi-repo binary fixture yet. |
| R-05 | Remote PR and mixed local/remote Work Items | Untested | CLI accepts forms, and reference parsing is tested, but no disposable authenticated `gh` PR fixture was available. Not classified working. |
| R-06 | Per-repo base detection and `:base` override | Partially working | Detection has git integration coverage; command/effect exists. Multi-repo override has no full workflow test. |
| R-07 | Local `v0`, first-annotation/explicit/export snapshots without moving HEAD/index | Working end-to-end | `git::tests::snapshot_preserves_head_and_index_and_pins_the_worktree`; PTY-04 pinned `s1`. |
| R-08 | Remote bare cache, detached worktrees, immutable versions on new commits | Untested | Implemented in `remote.rs`; external `gh` workflow not exercised. |
| R-09 | Synthetic session root spanning every repo | Partially working | Root/symlink creation exists; no multi-repo session read test. |
| R-10 | Version picker with exact per-version counts and old-version reopening | Partially working | Storage queries and screen exist; no full old-version materialization workflow test. |
| R-11 | Annotation identity plus per-version placement | Working end-to-end | Temporary SQLite workflows persist and reload exact placements; migration/storage tests pass. |
| R-12 | Content snippet/hash anchoring and fuzzy carry-forward | Partially working | Exact, ambiguous, and missing-anchor unit tests pass; full remote-version carry-forward remains untested. |
| R-13 | Rename following and `≈` ambiguity review | Partially working | Rename parsing and re-anchor primitives exist; no complete rename-fetch workflow. |
| R-14 | Normal/Visual Ask grounded to exact range, queued serially, streamed inline | Working end-to-end | `WF::normal_line_ask_persists_queues_streams_and_completes_inline`; `WF::visual_range_ask_uses_exact_new_source_range`; PTY-05/06/07. |
| R-15 | Inline Ask follow-ups remain self-contained | Working end-to-end | `WF::annotation_edit_delete_undo_and_inline_follow_up_are_complete_workflows`; message envelope test coverage in `annotations.rs`. |
| R-16 | Comment persists locally and does not contact Copilot until export | Working end-to-end | `WF::normal_and_visual_comments_persist_without_contacting_agent`; PTY-04. |
| R-17 | `:fork` child session becomes active | Partially working | Command, queued control sequencing, SDK call, and storage handling exist; no authenticated fork smoke test. |
| R-18 | Structured local context generation, editing, acceptance, persistence | Partially working | Parser/editor/effects and controlled-agent shape exist; no complete attach/restart workflow test. |
| R-19 | Split and unified review layouts with inline/rail annotations | Working end-to-end | `WF::file_switch_clears_visual_mode_and_split_unified_render_selection`; PTY-01/04/08. |
| R-20 | Multi-repo grouped file/folder picker ordered by activity | Partially working | Repo grouping, collapse/expand, fuzzy filtering, and Enter work; folder hierarchy and complete activity-order workflow are absent. |
| R-21 | Full Chat panel with streaming, queue/activity/usage, model/session controls | Partially working | Streaming, queue/activity, usage, scrolling, composer, model capability picker, steering, and status paths are covered by deterministic `WF::*` tests. Durable queue recovery across process restart and character-wise terminal selection remain gaps. |
| R-22 | Top command palette with autocomplete and execution | Working end-to-end | `WF::narrow_terminal_keeps_selection_composer_and_top_palette_visible`; PTY-08. |
| R-23 | Settings screen | Partially working | Model, base, and context-step edits work; keybinding editing and several global preferences are informational/not implemented. |
| R-24 | Generated context editor six-field presentation | Partially working | Parser and editor render; complete accept-to-session workflow untested. |
| R-25 | Prune reviewed history, optional export, git/cache cleanup | Partially working | Review-scoped storage query and delete/export effects exist; no destructive workflow test, PR-state enrichment, or Copilot transcript deletion/path report. |
| R-26 | Markdown/JSON comment export and one structured session batch | Working end-to-end | Export format tests and `WF::comment_export_queues_one_batch_and_acknowledges_delivery`. |
| R-27 | Never silently resend; pending before send, sent atomically at response start, explicit restart recovery | Working end-to-end | `storage::tests::ask_delivery_ack_is_persisted_atomically_with_response_start`; `WF::failed_agent_delivery_is_pending_and_restart_requires_recovery_choice`. |
| R-28 | WAL, busy timeout, FKs, migrations, indexed SQLite model | Working end-to-end | Storage migration/concurrency tests; workflow tests use a real temporary SQLite file. |
| R-29 | Editing delivered content queues correction rather than rewriting history | Partially working | Effects implement correction messages; delivered-edit workflow not yet tested against a real SDK session. |
| R-30 | Lazy viewport syntax highlighting behind a trait, broad language support | Working end-to-end | `highlight::tests::recognizes_common_languages`; `highlight::tests::highlights_only_requested_lines_and_reuses_cache`; deterministic rendered frames. |
| R-31 | Read/search-only Ask permissions | Working in the audited local Copilot path | `copilot::tests::permission_handler_allows_reads_and_denies_shell_and_write`; LIVE-01 ran the authenticated bridge with its configured read-only permission handler. |
| R-32 | Streaming events and persisted-session resume | Working end-to-end | Deterministic delta/final/resync/sub-agent tests pass; LIVE-01 streamed a real response, exercised SIDE teardown, disconnected, resumed the persisted MAIN session, and reloaded history. |
| R-33 | Session deletion during prune or manual storage-path guidance | Not implemented | Current SDK path does not delete transcripts and prune does not yet provide a reliable CLI storage path. |
| R-34 | Interdiff view | Explicitly deferred by the specification | v1.x item in §11. |
| R-35 | Live file-watcher/re-anchor cadence while review remains open | Explicitly deferred by the specification | Open item in §11; manual `:sync` is present. |
| R-36 | Exact fuzzy threshold tuning and eager/lazy snapshot worktree policy | Explicitly deferred by the specification | Open items in §11. |

## Visual-mode traceability

| ID | Required Visual behavior | Classification | Evidence |
|---|---|---|---|
| V-01 | `v` enters Visual in diff | Working end-to-end | Multiple `WF::*`; PTY-02. |
| V-02 | Visible `VISUAL` mode indicator | Working end-to-end | `WF::visual_range_ask_uses_exact_new_source_range`; PTY-02. |
| V-03 | Fixed anchor while `j/k`, page, and search movement extend | Working end-to-end | `WF::visual_search_extends_the_fixed_anchor_and_chat_visual_yanks_messages`; range workflows exercise `j`. |
| V-04 | Every selected row has restrained visible styling | Working end-to-end | Styled-cell assertions in split/unified workflow test. |
| V-05 | Selection understandable in split and unified | Working end-to-end | `WF::file_switch_clears_visual_mode_and_split_unified_render_selection`. |
| V-06 | Fold/meta rows cannot become invalid anchors | Working end-to-end | `WF::folds_and_mixed_old_new_ranges_fail_safely`. |
| V-07 | `a` opens exact-range contextual Ask composer | Working end-to-end | `WF::visual_range_ask_uses_exact_new_source_range`; PTY-03 demonstrates contextual placement for the sibling composer. |
| V-08 | `c` opens exact-range contextual Comment composer | Working end-to-end | Comment workflow; PTY-03. |
| V-09 | Submission persists source lines, never viewport rows | Working end-to-end | New/old placement assertions across multiple non-1-based hunks. |
| V-10 | Annotation appears immediately at its anchor | Working end-to-end | Unified workflow frames and PTY-04. |
| V-11 | Ask queues and streams inline | Working end-to-end | Ask workflows and PTY-05/06. |
| V-12 | Comment remains local | Working end-to-end | Comment workflow checks zero agent commands; PTY-04. |
| V-13 | `y` copies complete selected source text in order | Working end-to-end | `WF::visual_yank_preserves_source_order_across_addition_and_context`. |
| V-14 | `Esc` clears Visual | Working end-to-end | File-switch/cancel workflows. |
| V-15 | File/repo/version/screen/layout transitions clear or preserve predictably | Working end-to-end for file/screen/layout; partially working for version | File/screen/layout tests pass. Old-version switching lacks full workflow coverage. |
| V-16 | Normal `a`/`c` uses current code line | Working end-to-end | Normal Ask and Comment workflows. |
| V-17 | Binary/empty/deleted/renamed/fold cases fail safely | Working end-to-end | `WF::binary_empty_and_renamed_files_have_explicit_safe_behavior`, deleted-line workflow, and fold/mixed-side workflow. |
| V-18 | Chat Visual selects visible meaningful text | Partially working | Chat Visual currently selects and yanks whole messages. Character-wise, line-wise, and block-wise rendered-text selection are not implemented; see [`chat-interaction-spec.md`](chat-interaction-spec.md). |
| V-19 | Footer exposes Visual actions | Working end-to-end | Frame assertions and PTY-02. |
| V-20 | Composer cancellation restores prior mode without stale draft | Working end-to-end | `WF::cancel_restores_visual_selection_without_stale_draft`. |

Source-line policy established by the audit:

- The reducer stores a canonical parsed-diff row selection only until submit.
- `anchor_from_diff` converts it to old/new source coordinates.
- Context plus additions use the new side; context plus deletions use the old
  side.
- A range containing both additions and deletions is rejected because one
  placement cannot truthfully represent both coordinate spaces.
- Metadata/fold rows are rejected for annotations and omitted from yanked
  source text.

## Keybinding matrix

| Key | Classification | Evidence / note |
|---|---|---|
| `j` / `k` | Working end-to-end | Visual, search, chat, picker, and workflow movement tests. |
| `h` / `l` diff files | Working end-to-end | File-switch workflow. |
| `C-u` / `C-d` half page | Untested | Reducer implementation exists; no complete rendered workflow evidence. |
| `C-f` / `C-b` full page | Untested | Reducer implementation exists; no complete rendered workflow evidence. |
| `o` / `O` on fold | Partially working | Fold detection/context setting exists; real diff re-fetch expansion is not in the full harness. |
| `gg` / `G` | Partially working | Reducer behavior exists; no complete rendered evidence. |
| `Ctrl-w h/j/k/l` | Partially working | Documented sequence fixed and reducer-tested; complete multi-pane render workflow is not yet recorded. |
| `v` | Working end-to-end | Visual workflow and PTY-02. |
| `y` | Working end-to-end | Visual diff/chat yank workflows. |
| `yy` | Untested | Implementation exists; no full effect/render evidence. |
| `/`, `n`, `N` | Working end-to-end for diff Visual search; partially working elsewhere | Visual search workflow; picker/chat traversal lacks complete evidence. |
| `*` | Partially working | Uses the first word on the current line because there is no horizontal text cursor. |
| `a` | Working end-to-end | Normal/Visual Ask workflows and PTY-05. |
| `c` | Working end-to-end | Normal/Visual Comment workflows and PTY-03/04. |
| `]a` / `[a` | Untested | Global wrap implementation exists; no complete workflow test. |
| `e` | Working end-to-end | Annotation edit workflow. |
| `dd` / `u` | Working end-to-end | Delete/undo persistence workflow. |
| `gm` | Partially working | Markdown generation is tested; external browser launch is not. |
| `za` | Untested | Fold state/render implementation exists; no complete workflow evidence. |
| `Tab`, `gc`, `gr` | Working end-to-end for `Tab`; partially working for prefixes | PTY-07 and headless Chat tests; `gc`/`gr` only reducer-tested. |
| `-`, `,e` | Partially working | Picker opens/closes and Enter is reducer-tested; full tree workflow evidence is absent. |
| `:` | Working end-to-end | PTY-08. |
| `q` overlays only | Working end-to-end | PTY-09/10/11 proves top-level `:q`/`:q!`; overlay close has reducer coverage. |

## Command matrix

| Command | Classification | Evidence / gap |
|---|---|---|
| `:diff split`, `:diff unified` | Working end-to-end | Split/unified workflow; PTY-08. |
| `:diff expand` | Partially working | Effect exists; no real re-diff workflow test. |
| `:model` | Partially working | Storage/agent command path exists; live model-switch not tested. |
| `:fork` | Partially working | SDK control path exists; authenticated fork not tested. |
| `:compact [instructions]` | Partially working | SDK control path exists; authenticated compaction not tested. |
| `:versions` | Partially working | Screen/query exists; materialization workflow not tested. |
| `:snapshot` | Working end-to-end | Snapshot Git test and PTY first-annotation snapshot; explicit command itself is not separately recorded. |
| `:generate-context` | Partially working | Controlled generation shape and editor exist; acceptance/restart test missing. |
| `:export [markdown|json]` | Working end-to-end | Export workflow and format tests. |
| `:prune` | Partially working | Review-scoped query/delete implementation; destructive workflow deliberately not run. |
| `:settings` | Partially working | Screen and three editable settings; remaining rows informational. |
| `:sync` | Untested | Requires local/remote repository mutation during an open review. |
| `:base ...` | Partially working | Effect exists; full multi-repo base-change workflow absent. |
| `:stop` / `:abort` | Working end-to-end at agent-event level | Deterministic abort test and reusable-session handling; no PTY capture. |
| `:q`, `:quit`, `:q!` | Working end-to-end | PTY-09/10/11. |

## Remaining limitations and blockers

- No requirement is labeled working solely from source inspection.
- No known P0 remains in the audited local/controlled-agent workflows.
- The main evidence blocker is remote behavior: a safe disposable,
  authenticated GitHub PR fixture was not available, so remote fetch,
  multi-PR version update, remote carry-forward, and old-version materializing
  remain explicitly **Untested** or **Partially working**.
- Session deletion/path reporting during prune is **Not implemented**.
- Settings keybinding editing, folder hierarchy in the picker, and several
  preference rows are incomplete.
- Interdiff and live file-watcher policy remain explicitly deferred by v2.

## Current TUI/chat UX addendum

This addendum records the implementation that was added after the original
diff/annotation audit. It is intentionally separate from the older requirement
matrix so that new evidence is not confused with the earlier PTY run.

| ID | Behavior | Current classification | Evidence / limitation |
|---|---|---|---|
| T-01 | Chat has explicit NORMAL, INSERT, COMMAND, SEARCH, and VISUAL modes with visible mode text | Working in deterministic frames | `testing::tests::command_palette_is_scrollable_selectable_and_unmistakably_modal`, `testing::tests::sticky_chat_composer_wraps_edits_preserves_and_explicitly_discards_drafts`, and the `ui-snapshot` gallery. |
| T-02 | Command palette selection and scrolling | Working in the deterministic harness and compiled PTY | Up/down selection, PageUp/PageDown, Tab completion, modal title/footer, and visible selection are covered by `WF::command_palette_is_scrollable_selectable_and_unmistakably_modal` and PTY-12. |
| T-03 | Chat scrolling by rendered rows, including wrapped single messages | Working in the deterministic harness | `WF::chat_scrolls_by_rendered_rows_and_pauses_live_following`; no separate mouse-drag selection automation exists. |
| T-04 | Sticky multiline composer with wrapping, independent scroll, editing, preserved drafts, and explicit discard | Working in the deterministic harness and compiled PTY | `WF::sticky_chat_composer_wraps_edits_preserves_and_explicitly_discards_drafts`; PTY-21 exercised a 21-row contextual composer and its independent row-range scroll. |
| T-05 | Cancel draft versus cancel active Copilot turn | Working in the deterministic harness and compiled PTY | Draft preservation/discard and deterministic abort/reusable-session paths pass. PTY-18 stopped an active response while preserving the draft in Insert mode. |
| T-06 | Markdown semantics in Chat and fenced-code rendering | Working in deterministic frames | `chat_render::*` tests and `WF::markdown_history_and_minimum_terminal_state_have_inspectable_frames`; renderer covers headings, lists, task lists, blockquotes, emphasis, inline code, wrapping, blank lines, and fences. |
| T-07 | Lazy cached syntax highlighting for diff lines and fenced code | Working in unit tests and deterministic frames | `highlight::tests::recognizes_common_languages`, `highlight::tests::highlights_only_requested_lines_and_reuses_cache`, and `chat_render::tests::highlights_fenced_code_with_a_language_specific_synthetic_path`. |
| T-08 | Durable Copilot progress, quiet warning, tool/skill visibility, and `:agent-status` timeline | Working in the deterministic harness and compiled PTY | The renderer and overlay expose lane, phase, elapsed time, last event, queue, outbound ID, operation detail, quiet diagnostics, and timeline. Tool, skill, subagent, hook, retry, and error activity are typed and visible. PTY-17 captured active, queued, and immediate-steering progress. |
| T-09 | `/side` isolated ephemeral conversation and `/main` restoration | Working in deterministic, compiled-PTY, and authenticated paths | `WF::side_conversation_is_visibly_isolated_and_main_transcript_is_restored`; PTY-20 returned from an active SIDE turn promptly, and LIVE-01 exercised real SIDE creation and SDK deletion. |
| T-10 | Deterministic state gallery for visual inspection | Working as headless command paths | `ui-snapshot --state all` exercises review, ask, command, composer, quiet, queue, side, model, markdown, and tiny states through the production renderer. `ui-script` drives resize, input, stream events, and snapshots. Gallery output is terminal text, not a committed image artifact. |
| T-11 | Copilot SDK decoupled behind a testable agent interface | Working for deterministic tests | `TuiHarness` uses a fake agent and injected lane/activity events; the production bridge remains the only path that starts the real Copilot CLI. |
| T-12 | Mouse wheel scrolling and exact terminal text selection | Partially working | The TUI handles wheel events for Chat/diff scrolling. Built-in Visual mode selects whole messages, not rendered lines or characters. Shift-drag is a terminal-emulator workaround rather than completion of the requested behavior; see [`chat-interaction-spec.md`](chat-interaction-spec.md). |

### Verification status for this addendum

The current checked-in regular-test baseline is:

```text
104 regular tests passed; 1 authenticated live Copilot test ignored by default
```

The regular baseline includes the reducer/effect, storage, rendering, SDK
adapter, deterministic UI, model-picker, queue/steering, liveness, and
MAIN/SIDE tests. The ignored test is the authenticated
`copilot::tests::live_copilot_streams_and_resumes_persisted_history`; it is not
part of the regular count. It was run separately with
`RQ_TUI_LIVE_COPILOT=1` and passed as LIVE-01. PTY-16 through PTY-21 were
captured from the current Bazel-built binary.

### Compiled PTY evidence

Earlier records are kept for reproducibility and design context. The current
pass is PTY-16 through PTY-21 in [`audit/pty-smoke.md`](audit/pty-smoke.md).

The second compiled-binary audit used the Bazel-built binary and a controlled
agent. It captured the following behaviors in a 100×28 tmux pane:

| ID | Action | Evidence |
|---|---|---|
| PTY-12 | Enter command mode and press Down repeatedly | The palette remained modal, showed a highlighted later command, shifted its visible list, and displayed a `13/20` selection count. The sticky bar also identified command mode. |
| PTY-13 | Submit `/side ...` from Chat | The transcript switched to a clearly labelled `SIDE` lane, with its own progress line and tool activity. MAIN messages were not shown in the SIDE view. |
| PTY-14 | Open `:agent-status` during SIDE | The overlay showed SIDE, connected state, phase, elapsed time, last SDK event, event count, queue depth, current operation/detail, outbound ID, and recent activity; `j/k` reached the timeline footer. |
| PTY-15 | Exit with `/main` and edit a long draft | MAIN returned without the SIDE transcript, and the bordered composer wrapped across multiple rows while retaining its cursor and sticky position. |

The current pass adds active-turn cancellation, queue cancellation, steering,
staged model selection, immediate SIDE exit, and oversized contextual-composer
scrolling. Quiet-warning behavior remains deterministic-gallery evidence
rather than a wall-clock PTY capture.
